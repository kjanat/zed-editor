use call::ActiveCall;
use fs::Fs as _;
use gpui::{BackgroundExecutor, TestAppContext};
use rpc::ErrorExt as _;
use serde_json::json;
use std::path::Path;
use util::{path, rel_path::rel_path};

pub mod test_db;
pub mod test_server;
use test_server::TestServer;

#[ctor::ctor(unsafe)]
fn init_logger() {
    zlog::init_test();
}

#[cfg(target_os = "macos")]
#[test]
fn test_macos_panic_unwinding() {
    struct DropGuard<'a>(&'a std::cell::Cell<bool>);

    impl Drop for DropGuard<'_> {
        fn drop(&mut self) {
            self.0.set(true);
        }
    }

    let dropped = std::cell::Cell::new(false);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = DropGuard(&dropped);
        panic!("verify panic unwinding in the macOS test binary");
    }));
    assert!(result.is_err());
    assert!(dropped.get());
}

#[gpui::test(iterations = 20)]
async fn test_remote_save_detects_missed_file_replacement(
    executor: BackgroundExecutor,
    cx_a: &mut TestAppContext,
    cx_b: &mut TestAppContext,
    cx_c: &mut TestAppContext,
) {
    let mut server = TestServer::start(executor.clone()).await;
    let client_a = server.create_client(cx_a, "user_a").await;
    let client_b = server.create_client(cx_b, "user_b").await;
    let client_c = server.create_client(cx_c, "user_c").await;
    server
        .create_room(&mut [(&client_a, cx_a), (&client_b, cx_b), (&client_c, cx_c)])
        .await;
    let active_call = cx_a.read(ActiveCall::global);
    let fs = client_a.fs();
    fs.insert_tree(path!("/dir"), json!({ "a.txt": "original" }))
        .await;
    let (project_a, worktree_id) = client_a.build_local_project(path!("/dir"), cx_a).await;
    let project_id = active_call
        .update(cx_a, |call, cx| call.share_project(project_a.clone(), cx))
        .await
        .expect("share project");
    let project_b = client_b.join_remote_project(project_id, cx_b).await;
    let project_c = client_c.join_remote_project(project_id, cx_c).await;
    let buffer_c = project_c
        .update(cx_c, |project, cx| {
            project.open_buffer((worktree_id, rel_path("a.txt")), cx)
        })
        .await
        .expect("open buffer for another guest");
    let buffer = project_b
        .update(cx_b, |project, cx| {
            project.open_buffer((worktree_id, rel_path("a.txt")), cx)
        })
        .await
        .expect("open remote buffer");
    buffer.update(cx_b, |buffer, cx| {
        buffer.edit([(0..0, "edited ")], None, cx)
    });
    executor.run_until_parked();
    project_b.read_with(cx_b, |project, cx| {
        let entry = project
            .entry_for_path(
                &project::ProjectPath {
                    worktree_id,
                    path: rel_path("a.txt").into(),
                },
                cx,
            )
            .expect("joined worktree entry");
        assert_eq!(
            entry.device,
            Some(0),
            "joining must restore the persisted device"
        );
    });
    let file_path = Path::new(path!("/dir/a.txt"));
    let original = fs
        .metadata(file_path)
        .await
        .expect("metadata")
        .expect("file");
    fs.pause_events();
    fs.insert_file(file_path, b"replaced".to_vec()).await;
    fs.set_mtime(file_path, original.mtime)
        .expect("preserve timestamp");
    let first = project_b.update(cx_b, |project, cx| project.save_buffer(buffer.clone(), cx));
    let second = project_c.update(cx_c, |project, cx| {
        project.save_buffer(buffer_c.clone(), cx)
    });
    let (first, second) = futures::join!(first, second);
    for result in [first, second] {
        assert_eq!(
            result
                .expect_err("each guest must receive the conflict")
                .error_code(),
            rpc::ErrorCode::FileChangedOnDisk
        );
    }
    assert_eq!(fs.load(file_path).await.expect("read disk"), "replaced");
    assert!(buffer.read_with(cx_b, |buffer, _| buffer.has_conflict()));
    assert!(buffer_c.read_with(cx_c, |buffer, _| buffer.has_conflict()));
    project_b
        .update(cx_b, |project, cx| project.save_buffer(buffer.clone(), cx))
        .await
        .expect_err("ordinary retry is not overwrite confirmation");
    assert_eq!(
        fs.load(file_path).await.expect("read external file"),
        "replaced"
    );
    project_b
        .update(cx_b, |project, cx| {
            project.save_buffer_with_overwrite(buffer.clone(), true, cx)
        })
        .await
        .expect("overwrite after conflict confirmation");
    assert_eq!(
        fs.load(file_path).await.expect("read disk"),
        "edited original"
    );

    executor.run_until_parked();
    let host_buffer = project_a
        .update(cx_a, |project, cx| {
            project.open_buffer((worktree_id, rel_path("a.txt")), cx)
        })
        .await
        .expect("host buffer");
    let receipt = host_buffer.read_with(cx_a, |buffer, _| buffer.disk_state_for_save());
    assert_eq!(receipt.and_then(language::DiskState::device), Some(0));
    assert_eq!(
        buffer.read_with(cx_b, |buffer, _| buffer.disk_state_for_save()),
        receipt
    );
    assert_eq!(
        buffer_c.read_with(cx_c, |buffer, _| buffer.disk_state_for_save()),
        receipt
    );

    buffer.update(cx_b, |buffer, cx| {
        buffer.edit([(0..0, "discard ")], None, cx)
    });
    fs.insert_file(file_path, b"external again".to_vec()).await;
    project_b
        .update(cx_b, |project, cx| project.save_buffer(buffer.clone(), cx))
        .await
        .expect_err("detect the second replacement");
    assert!(buffer.read_with(cx_b, |buffer, _| buffer.has_conflict()));
    project_b
        .update(cx_b, |project, cx| {
            project.reload_buffers(collections::HashSet::from_iter([buffer.clone()]), true, cx)
        })
        .await
        .expect("discard remote edits");
    buffer.read_with(cx_b, |buffer, _| {
        assert_eq!(buffer.text(), "external again");
        assert!(!buffer.has_conflict());
        assert!(!buffer.is_dirty());
    });
    executor.run_until_parked();
    assert_eq!(
        buffer.read_with(cx_b, |buffer, _| buffer.disk_state_for_save()),
        host_buffer.read_with(cx_a, |buffer, _| buffer.disk_state_for_save()),
        "reload must install the host identity before its error response"
    );
    fs.insert_file(file_path, b"clean replacement".to_vec())
        .await;
    project_b
        .update(cx_b, |project, cx| project.save_buffer(buffer.clone(), cx))
        .await
        .expect_err("clean guest detects replacement");
    executor.run_until_parked();
    buffer.read_with(cx_b, |buffer, _| {
        assert_eq!(buffer.text(), "clean replacement");
        assert!(
            !buffer.has_conflict(),
            "late response must not undo reload recovery"
        );
        assert!(!buffer.is_dirty());
    });
    let destination = Path::new(path!("/dir/created-after-confirmation.txt"));
    fs.insert_file(destination, b"another process owns this".to_vec())
        .await;
    project_b
        .update(cx_b, |project, cx| {
            project.save_buffer_as_with_disk_state(
                buffer.clone(),
                (worktree_id, rel_path("created-after-confirmation.txt")).into(),
                Some(language::DiskState::New),
                cx,
            )
        })
        .await
        .expect_err("remote Save As must preserve confirmed absence");
    assert_eq!(
        fs.read_file_sync(destination)
            .expect("external destination"),
        b"another process owns this"
    );
    buffer.read_with(cx_b, |buffer, _| {
        assert_eq!(
            buffer.file().expect("original file").path().as_ref(),
            rel_path("a.txt")
        );
        assert!(!buffer.has_conflict());
    });
}
