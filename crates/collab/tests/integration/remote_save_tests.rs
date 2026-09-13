use call::ActiveCall;
use fs::Fs as _;
use gpui::{BackgroundExecutor, TestAppContext};
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
fn test_dwarf_panic_unwinding() {
    struct DropGuard<'a>(&'a std::cell::Cell<bool>);

    impl Drop for DropGuard<'_> {
        fn drop(&mut self) {
            self.0.set(true);
        }
    }

    let dropped = std::cell::Cell::new(false);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = DropGuard(&dropped);
        panic!("verify DWARF unwinding in the macOS test binary");
    }));
    assert!(result.is_err());
    assert!(dropped.get());
}

#[gpui::test]
async fn test_remote_save_detects_missed_file_replacement(
    executor: BackgroundExecutor,
    cx_a: &mut TestAppContext,
    cx_b: &mut TestAppContext,
) {
    let mut server = TestServer::start(executor.clone()).await;
    let client_a = server.create_client(cx_a, "user_a").await;
    let client_b = server.create_client(cx_b, "user_b").await;
    server
        .create_room(&mut [(&client_a, cx_a), (&client_b, cx_b)])
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
    project_b
        .update(cx_b, |project, cx| project.save_buffer(buffer.clone(), cx))
        .await
        .expect_err("remote save must detect the replacement");
    assert_eq!(fs.load(file_path).await.expect("read disk"), "replaced");
    assert!(buffer.read_with(cx_b, |buffer, _| buffer.has_conflict()));
    project_b
        .update(cx_b, |project, cx| project.save_buffer(buffer.clone(), cx))
        .await
        .expect("overwrite after conflict confirmation");
    assert_eq!(
        fs.load(file_path).await.expect("read disk"),
        "edited original"
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
}
