use std::{
    borrow::Cow,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use collections::HashMap;
use fs::{FakeFs, Fs};
use futures::{FutureExt, StreamExt};
use gpui::{Entity, TestAppContext, UpdateGlobal as _};
use language::{
    Buffer, CodeLabel, DiagnosticSourceKind, FakeLspAdapter, HighlightId, LocalFile, rust_lang,
};
use lsp::{LanguageServerId, LanguageServerName, Uri};
use parking_lot::Mutex;
use project::{
    DiagnosticSummary, Event, Project,
    lsp_store::{
        log_store::{TestRpcLogHeaderState, TestRpcRequestTracker},
        *,
    },
};
use serde_json::json;
use unindent::Unindent;
use util::{path, rel_path::rel_path};

use crate::init_test;

#[gpui::test]
async fn test_diagnostic_batches_skip_paths_without_worktrees(cx: &mut TestAppContext) {
    init_test(cx);

    for skipped_index in 0..=2 {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/dir"), json!({ "a.rs": "one", "b.rs": "two" }))
            .await;
        let project = Project::test(fs, [Path::new(path!("/dir"))], cx).await;
        let lsp_store = project.read_with(cx, |project, _| project.lsp_store());
        let buffer_a = project
            .update(cx, |project, cx| {
                project.open_local_buffer(path!("/dir/a.rs"), cx)
            })
            .await
            .unwrap();
        let worktree_id =
            buffer_a.read_with(cx, |buffer, cx| buffer.file().unwrap().worktree_id(cx));
        let server_id = LanguageServerId(0);

        for message in [Some("error"), None] {
            cx.run_until_parked();
            project.read_with(cx, |project, cx| {
                assert_eq!(
                    project.get_open_buffer(&(worktree_id, rel_path("b.rs")).into(), cx),
                    None
                );
            });
            let mut events = cx.events(&project);
            let mut paths = vec![path!("/dir/a.rs"), path!("/dir/b.rs")];
            paths.insert(skipped_index, path!("/outside.rs"));
            let updates = paths
                .into_iter()
                .map(|path| DocumentDiagnosticsUpdate {
                    diagnostics: lsp::PublishDiagnosticsParams {
                        uri: Uri::from_file_path(path).unwrap(),
                        version: None,
                        diagnostics: message
                            .into_iter()
                            .map(|message| lsp::Diagnostic {
                                range: lsp::Range::new(
                                    lsp::Position::new(0, 0),
                                    lsp::Position::new(0, 3),
                                ),
                                severity: Some(lsp::DiagnosticSeverity::ERROR),
                                message: lsp::DiagnosticMessage::from(message),
                                ..lsp::Diagnostic::default()
                            })
                            .collect(),
                    },
                    result_id: None,
                    registration_id: None,
                    server_id,
                    disk_based_sources: Cow::Borrowed(&[]),
                })
                .collect();
            lsp_store.update(cx, |lsp_store, cx| {
                lsp_store
                    .merge_lsp_diagnostics(
                        DiagnosticSourceKind::Pushed,
                        updates,
                        |_, _, _| false,
                        cx,
                    )
                    .unwrap();
            });
            cx.run_until_parked();

            project.read_with(cx, |project, cx| {
                assert_eq!(
                    project.diagnostic_summary(false, cx),
                    DiagnosticSummary {
                        error_count: if message.is_some() { 2 } else { 0 },
                        warning_count: 0,
                    },
                    "skipped update at index {skipped_index}, message {message:?}"
                );
            });
            let diagnostic_events = std::iter::from_fn(|| events.next().now_or_never().flatten())
                .filter_map(|event| match event {
                    Event::DiagnosticsUpdated {
                        language_server_id,
                        paths,
                    } => Some((language_server_id, paths)),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(
                diagnostic_events,
                vec![(
                    server_id,
                    vec![
                        (worktree_id, rel_path("a.rs")).into(),
                        (worktree_id, rel_path("b.rs")).into(),
                    ],
                )],
                "skipped update at index {skipped_index}, message {message:?}"
            );

            let buffer_b = project
                .update(cx, |project, cx| {
                    project.open_local_buffer(path!("/dir/b.rs"), cx)
                })
                .await
                .unwrap();
            for buffer in [&buffer_a, &buffer_b] {
                buffer.read_with(cx, |buffer, _| {
                    assert_eq!(
                        buffer
                            .buffer_diagnostics(Some(server_id))
                            .iter()
                            .map(|entry| entry.diagnostic.message.to_string())
                            .collect::<Vec<_>>(),
                        message.into_iter().collect::<Vec<_>>()
                    );
                });
            }
        }
    }
}

#[gpui::test]
async fn test_removing_invisible_worktree_cleans_reused_lsp_bookkeeping(cx: &mut TestAppContext) {
    init_test(cx);
    cx.executor().allow_parking();

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/the-root"), json!({ "main.rs": "fn main() {}" }))
        .await;
    fs.insert_tree(
        path!("/the-registry"),
        json!({ "dep": { "src": { "dep.rs": "pub fn dep() {}" } } }),
    )
    .await;

    let project = Project::test(fs, [path!("/the-root").as_ref()], cx).await;
    let language_registry = project.read_with(cx, |project, _| project.languages().clone());
    language_registry.add(rust_lang());
    let mut fake_servers = language_registry.register_fake_lsp("Rust", FakeLspAdapter::default());

    let (_visible_buffer, _visible_handle) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/the-root/main.rs"), cx)
        })
        .await
        .unwrap();
    fake_servers.next().await.unwrap();
    cx.run_until_parked();

    let server_id = project.read_with(cx, |project, cx| {
        project
            .lsp_store()
            .read(cx)
            .language_server_statuses()
            .next()
            .unwrap()
            .0
    });
    let external_buffer = project
        .update(cx, |project, cx| {
            project.open_local_buffer_via_lsp(
                Uri::from_file_path(path!("/the-registry/dep/src/dep.rs")).unwrap(),
                server_id,
                cx,
            )
        })
        .await
        .unwrap();
    cx.run_until_parked();

    let invisible_worktree_id =
        external_buffer.read_with(cx, |buffer, cx| buffer.file().unwrap().worktree_id(cx));
    project.read_with(cx, |project, cx| {
        let worktree = project.worktree_for_id(invisible_worktree_id, cx).unwrap();
        assert!(!worktree.read(cx).is_visible());
        assert!(
            project
                .lsp_store()
                .read(cx)
                .has_language_server_seed_for_worktree(invisible_worktree_id)
        );
    });

    project.update(cx, |project, cx| {
        project.remove_worktree(invisible_worktree_id, cx);
    });
    cx.run_until_parked();

    project.read_with(cx, |project, cx| {
        let lsp_store = project.lsp_store();
        let lsp_store = lsp_store.read(cx);
        assert!(
            lsp_store
                .language_server_statuses()
                .any(|(status_server_id, _)| status_server_id == server_id)
        );
        assert!(!lsp_store.has_language_server_seed_for_worktree(invisible_worktree_id));
    });
}

#[gpui::test]
async fn test_open_buffer_via_lsp_case_variant_no_duplicate(cx: &mut TestAppContext) {
    init_test(cx);
    cx.executor().allow_parking();

    let fs = FakeFs::new(cx.executor());
    fs.set_case_sensitive(false);
    fs.insert_tree(
        path!("/root"),
        json!({ "src": { "main.rs": "fn main() {}" } }),
    )
    .await;

    let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
    let language_registry = project.read_with(cx, |project, _| project.languages().clone());
    language_registry.add(rust_lang());
    let mut fake_servers = language_registry.register_fake_lsp("Rust", FakeLspAdapter::default());

    project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/root/src/main.rs"), cx)
        })
        .await
        .unwrap();
    fake_servers.next().await.unwrap();
    cx.run_until_parked();

    let server_id = project.read_with(cx, |project, cx| {
        project
            .lsp_store()
            .read(cx)
            .language_server_statuses()
            .next()
            .unwrap()
            .0
    });

    project
        .update(cx, |project, cx| {
            project.open_local_buffer_via_lsp(
                Uri::from_file_path(path!("/root/SRC/main.rs")).unwrap(),
                server_id,
                cx,
            )
        })
        .await
        .unwrap();
    cx.run_until_parked();

    project.read_with(cx, |project, cx| {
        let worktree = project.worktrees(cx).next().unwrap();
        let entries: Vec<_> = worktree
            .read(cx)
            .snapshot()
            .entries(true, 0)
            .map(|entry| entry.path.as_unix_str().to_string())
            .collect();
        assert_eq!(entries, vec!["", "src", "src/main.rs"]);
    });
}

#[gpui::test]
async fn test_open_buffer_via_lsp_preserves_external_symlink_path(cx: &mut TestAppContext) {
    init_test(cx);
    cx.executor().allow_parking();

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/shared"),
        json!({ "pkg": { "def.rs": "pub fn def() {}" } }),
    )
    .await;
    fs.insert_tree(
        path!("/project"),
        json!({ "src": { "main.rs": "fn main() {}" } }),
    )
    .await;
    fs.create_symlink(
        path!("/project/pkg").as_ref(),
        PathBuf::from(path!("/shared/pkg")),
    )
    .await
    .unwrap();

    let (project, server_id) =
        project_with_rust_server(fs, path!("/project"), path!("/project/src/main.rs"), cx).await;

    let buffer = project
        .update(cx, |project, cx| {
            project.open_local_buffer_via_lsp(
                Uri::from_file_path(path!("/project/pkg/def.rs")).unwrap(),
                server_id,
                cx,
            )
        })
        .await
        .unwrap();
    cx.run_until_parked();

    assert_eq!(
        buffer_paths(&buffer, cx),
        (
            "pkg/def.rs".to_string(),
            PathBuf::from(path!("/project/pkg/def.rs"))
        )
    );
    assert_eq!(
        worktree_roots(&project, cx),
        vec![PathBuf::from(path!("/project"))]
    );
}

#[gpui::test]
async fn test_open_buffer_via_lsp_case_variant_in_unscanned_dir(cx: &mut TestAppContext) {
    init_test(cx);
    cx.executor().allow_parking();

    let fs = FakeFs::new(cx.executor());
    fs.set_case_sensitive(false);
    fs.insert_tree(
        path!("/root"),
        json!({
            ".gitignore": "ignored\n",
            "src": { "main.rs": "fn main() {}" },
            "ignored": { "lib.rs": "pub fn lib() {}" },
        }),
    )
    .await;

    let (project, server_id) =
        project_with_rust_server(fs, path!("/root"), path!("/root/src/main.rs"), cx).await;
    assert_eq!(
        worktree_entries(&project, cx),
        vec!["", ".gitignore", "ignored", "src", "src/main.rs"]
    );

    let buffer = project
        .update(cx, |project, cx| {
            project.open_local_buffer_via_lsp(
                Uri::from_file_path(path!("/root/IGNORED/LIB.rs")).unwrap(),
                server_id,
                cx,
            )
        })
        .await
        .unwrap();
    cx.run_until_parked();

    assert_eq!(
        buffer_paths(&buffer, cx),
        (
            "ignored/lib.rs".to_string(),
            PathBuf::from(path!("/root/ignored/lib.rs"))
        )
    );
    assert_eq!(
        worktree_roots(&project, cx),
        vec![PathBuf::from(path!("/root"))]
    );
    assert_eq!(
        worktree_entries(&project, cx),
        vec![
            "",
            ".gitignore",
            "ignored",
            "ignored/lib.rs",
            "src",
            "src/main.rs"
        ]
    );
}

#[test]
fn test_rpc_log_grouping_separates_timed_messages() {
    for (received, direction) in [(false, "Send"), (true, "Receive")] {
        let mut header_state = TestRpcLogHeaderState::new();

        assert_eq!(
            header_state.header_for_message(received, None),
            Some(format!("\n// {direction}:"))
        );
        assert_eq!(header_state.header_for_message(received, None), None);
        assert_eq!(
            header_state.header_for_message(received, Some(Duration::from_millis(53))),
            Some(format!("\n// {direction} (took 53.0ms):"))
        );
        assert_eq!(
            header_state.header_for_message(received, None),
            Some(format!("\n// {direction}:"))
        );
        assert_eq!(header_state.header_for_message(received, None), None);
    }
}

#[test]
fn test_rpc_request_tracker_distinguishes_request_directions() {
    let mut tracker = TestRpcRequestTracker::new();
    let started_at = Instant::now();

    assert_eq!(
        tracker.observe(
            false,
            r#"{"jsonrpc":"2.0","id":1,"method":"textDocument/hover"}"#,
            started_at,
        ),
        None
    );
    assert_eq!(
        tracker.observe(
            true,
            r#"{"jsonrpc":"2.0","id":1,"method":"workspace/configuration"}"#,
            started_at + Duration::from_millis(10),
        ),
        None
    );
    assert_eq!(
        tracker.observe(
            false,
            r#"{"jsonrpc":"2.0","id":1,"result":[]}"#,
            started_at + Duration::from_millis(30),
        ),
        Some(Duration::from_millis(20))
    );
    assert_eq!(
        tracker.observe(
            true,
            r#"{"jsonrpc":"2.0","id":1,"result":null}"#,
            started_at + Duration::from_millis(50),
        ),
        Some(Duration::from_millis(50))
    );
}

#[test]
fn test_rpc_request_tracker_decodes_ids_and_times_cancelled_requests() {
    let mut tracker = TestRpcRequestTracker::new();
    let started_at = Instant::now();

    tracker.observe(
        true,
        r#"{"jsonrpc":"2.0","id":"foo\u002fbar","method":"workspace/configuration"}"#,
        started_at,
    );
    assert_eq!(
        tracker.observe(
            false,
            r#"{"jsonrpc":"2.0","id":"foo/bar","result":[]}"#,
            started_at + Duration::from_millis(25),
        ),
        Some(Duration::from_millis(25))
    );

    tracker.observe(
        false,
        r#"{"jsonrpc":"2.0","id":7,"method":"textDocument/hover"}"#,
        started_at,
    );
    tracker.observe(
        false,
        r#"{"jsonrpc":"2.0","method":"$/cancelRequest","params":{"id":7}}"#,
        started_at + Duration::from_millis(1),
    );
    assert_eq!(tracker.pending_request_count(), 1);
    assert_eq!(
        tracker.observe(
            true,
            r#"{"jsonrpc":"2.0","id":7,"error":{"code":-32800,"message":"Request was cancelled"}}"#,
            started_at + Duration::from_millis(10),
        ),
        Some(Duration::from_millis(10))
    );
    assert_eq!(tracker.pending_request_count(), 0);
}

#[test]
fn test_rpc_request_tracker_bounds_unanswered_requests() {
    let mut tracker = TestRpcRequestTracker::new();
    let started_at = Instant::now();
    let max_pending_requests = TestRpcRequestTracker::max_pending_requests();

    for id in 0..=max_pending_requests {
        tracker.observe(
            false,
            &format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"textDocument/hover"}}"#),
            started_at + Duration::from_nanos(id as u64),
        );
    }

    assert_eq!(tracker.pending_request_count(), max_pending_requests);
    assert_eq!(
        tracker.observe(
            true,
            r#"{"jsonrpc":"2.0","id":0,"result":null}"#,
            started_at + Duration::from_secs(1),
        ),
        None
    );
    assert!(
        tracker
            .observe(
                true,
                r#"{"jsonrpc":"2.0","id":1,"result":null}"#,
                started_at + Duration::from_secs(1),
            )
            .is_some()
    );
}

#[test]
fn test_rpc_log_duration_proto_roundtrip() {
    let log_type = LanguageServerLogType::Rpc {
        received: true,
        elapsed: Some(Duration::from_micros(1234)),
    };

    assert_eq!(
        LanguageServerLogType::from_proto(log_type.to_proto()),
        log_type
    );
}

#[test]
fn test_glob_literal_prefix() {
    assert_eq!(glob_literal_prefix(Path::new("**/*.js")), Path::new(""));
    assert_eq!(
        glob_literal_prefix(Path::new("node_modules/**/*.js")),
        Path::new("node_modules")
    );
    assert_eq!(
        glob_literal_prefix(Path::new("foo/{bar,baz}.js")),
        Path::new("foo")
    );
    assert_eq!(
        glob_literal_prefix(Path::new("foo/bar/baz.js")),
        Path::new("foo/bar/baz.js")
    );

    #[cfg(target_os = "windows")]
    {
        assert_eq!(glob_literal_prefix(Path::new("**\\*.js")), Path::new(""));
        assert_eq!(
            glob_literal_prefix(Path::new("node_modules\\**/*.js")),
            Path::new("node_modules")
        );
        assert_eq!(
            glob_literal_prefix(Path::new("foo/{bar,baz}.js")),
            Path::new("foo")
        );
        assert_eq!(
            glob_literal_prefix(Path::new("foo\\bar\\baz.js")),
            Path::new("foo/bar/baz.js")
        );
    }
}

#[test]
fn test_multi_len_chars_normalization() {
    let mut label = CodeLabel::new(
        "myElˇ (parameter) myElˇ: {\n    foo: string;\n}".to_string(),
        0..6,
        vec![(0..6, HighlightId::new(1))],
    );
    ensure_uniform_list_compatible_label(&mut label);
    assert_eq!(
        label,
        CodeLabel::new(
            "myElˇ (parameter) myElˇ: { foo: string; }".to_string(),
            0..6,
            vec![(0..6, HighlightId::new(1))],
        )
    );
}

#[test]
fn test_completion_label_snippet_normalization() {
    for line_ending in ["\n", "\r\n", "\r"] {
        let text = "
            #[cfg(test)]
            mod tests {
                use super::*;

                #[test]
                fn test_name() {

                }
            }"
        .unindent()
        .replace('\n', line_ending);
        let name_start = text.find("test_name").expect("snippet has a test name");
        let text_len = text.len();
        let mut label = CodeLabel::new(
            text,
            0..text_len,
            vec![
                (0..12, HighlightId::new(1)),
                (name_start..name_start + 9, HighlightId::TABSTOP_REPLACE_ID),
            ],
        );

        ensure_uniform_list_compatible_label(&mut label);

        assert_eq!(
            label,
            CodeLabel::new(
                "#[cfg(test)] mod tests { use super::*; #[test] fn test_name() { } }".to_string(),
                0..67,
                vec![
                    (0..12, HighlightId::new(1)),
                    (50..59, HighlightId::TABSTOP_REPLACE_ID),
                ],
            ),
            "line ending: {line_ending:?}",
        );
    }
}

#[test]
fn test_completion_label_unicode_normalization() {
    for line_ending in ["\n", "\r\n", "\r"] {
        let text = "
            héllo {
                🦀value: 世界,
            }"
        .unindent()
        .replace('\n', line_ending);
        let value_start = text.find("🦀value").expect("label has a value");
        let type_start = text.find("世界").expect("label has a type");
        let text_len = text.len();
        let mut label = CodeLabel::new(
            text,
            value_start..value_start + 9,
            vec![
                (0..text_len, HighlightId::new(0)),
                (0..6, HighlightId::new(1)),
                (
                    value_start..value_start + 9,
                    HighlightId::TABSTOP_REPLACE_ID,
                ),
                (type_start..type_start + 6, HighlightId::new(2)),
            ],
        );

        ensure_uniform_list_compatible_label(&mut label);

        assert_eq!(
            label,
            CodeLabel::new(
                "héllo { 🦀value: 世界, }".to_string(),
                9..18,
                vec![
                    (0..29, HighlightId::new(0)),
                    (0..6, HighlightId::new(1)),
                    (9..18, HighlightId::TABSTOP_REPLACE_ID),
                    (20..26, HighlightId::new(2)),
                ],
            ),
            "line ending: {line_ending:?}",
        );
    }
}

#[test]
fn test_completion_label_whitespace_normalization() {
    for line_ending in ["\n", "\r\n", "\r", "\n\r", "\r\r\n"] {
        for before in ["", " ", " \t "] {
            for after in ["", " ", " \t "] {
                let mut label = CodeLabel::plain(format!("{before}{line_ending}{after}"), None);
                ensure_uniform_list_compatible_label(&mut label);
                assert_eq!(label, CodeLabel::plain(" ".to_string(), None));
            }
        }
    }

    for text in ["", " ", " \t ", "héllo  世界", "héllo\t世界"] {
        let mut label = CodeLabel::plain(text.to_string(), None);
        let expected = label.clone();
        ensure_uniform_list_compatible_label(&mut label);
        assert_eq!(label, expected);
    }
}

#[test]
fn test_trailing_newline_in_completion_documentation() {
    let doc =
        lsp::Documentation::String("Inappropriate argument value (of correct type).\n".to_string());
    let completion_doc: CompletionDocumentation = doc.into();
    assert!(
        matches!(completion_doc, CompletionDocumentation::SingleLine(s) if s == "Inappropriate argument value (of correct type).")
    );

    let doc = lsp::Documentation::String("  some value  \n".to_string());
    let completion_doc: CompletionDocumentation = doc.into();
    assert!(matches!(
        completion_doc,
        CompletionDocumentation::SingleLine(s) if s == "some value"
    ));
}

#[gpui::test]
async fn test_user_initialization_options_override_adapter_arrays(cx: &mut TestAppContext) {
    init_test(cx);

    let user_settings = serde_json::json!({
        "lsp": {
            "the-fake-language-server": {
                "initialization_options": {
                    "preview": {
                        "background": {
                            "enabled": true,
                            "args": ["--data-plane-host=127.0.0.1:23635", "--invert-colors=never"],
                        },
                    },
                    "plugins": ["user-plugin"],
                    "userOnly": ["user"],
                },
            },
        },
    });

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/the-root"),
        json!({
            ".zed": {
                "settings.json": user_settings.to_string(),
            },
            "main.rs": "fn main() {}",
        }),
    )
    .await;

    let project = Project::test(fs, [path!("/the-root").as_ref()], cx).await;
    let language_registry = project.read_with(cx, |project, _| project.languages().clone());
    language_registry.add(rust_lang());

    let sent_initialization_options = Arc::new(Mutex::new(None));
    let mut fake_servers = language_registry.register_fake_lsp(
        "Rust",
        FakeLspAdapter {
            name: "the-fake-language-server",
            initialization_options: Some(json!({
                "preview": {
                    "background": {
                        "args": ["--data-plane-host=127.0.0.1:23635", "--invert-colors=never"],
                        "partialRendering": true,
                    },
                },
                "plugins": ["default-plugin", "user-plugin"],
                "adapterOnly": [1, 2],
            })),
            initializer: Some(Box::new({
                let sent_initialization_options = sent_initialization_options.clone();
                move |fake_server| {
                    let sent_initialization_options = sent_initialization_options.clone();
                    fake_server.set_request_handler::<lsp::request::Initialize, _, _>(
                        move |params, _| {
                            *sent_initialization_options.lock() = params.initialization_options;
                            async move { Ok(lsp::InitializeResult::default()) }
                        },
                    );
                }
            })),
            ..FakeLspAdapter::default()
        },
    );
    cx.run_until_parked();

    project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/the-root/main.rs"), cx)
        })
        .await
        .unwrap();
    fake_servers.next().await.unwrap();
    cx.run_until_parked();

    assert_eq!(
        sent_initialization_options.lock().take(),
        Some(json!({
            "preview": {
                "background": {
                    "enabled": true,
                    "args": ["--data-plane-host=127.0.0.1:23635", "--invert-colors=never"],
                    "partialRendering": true,
                },
            },
            "plugins": ["user-plugin"],
            "adapterOnly": [1, 2],
            "userOnly": ["user"],
        })),
    );
}

#[gpui::test]
async fn test_other_adapters_lsp_configuration_contributions_are_unioned(cx: &mut TestAppContext) {
    init_test(cx);

    let user_settings = serde_json::json!({
        "lsp": {
            "the-fake-language-server": {
                "initialization_options": {
                    "languages": ["user-lang"],
                    "userOnly": true,
                },
            },
        },
    });

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/the-root"),
        json!({
            ".zed": {
                "settings.json": user_settings.to_string(),
            },
            "main.rs": "fn main() {}",
        }),
    )
    .await;

    let project = Project::test(fs, [path!("/the-root").as_ref()], cx).await;
    let language_registry = project.read_with(cx, |project, _| project.languages().clone());
    language_registry.add(rust_lang());

    let main_server_name = LanguageServerName("the-fake-language-server".into());
    for (language, server_name, plugin, lang, memory) in [
        ("Vue", "vue-language-server", "vue-plugin", "vue", 4096),
        (
            "Astro",
            "astro-language-server",
            "astro-plugin",
            "astro",
            2048,
        ),
    ] {
        let contribution = json!({
            "tsserver": {
                "globalPlugins": ["shared-plugin", plugin],
                "maxMemory": memory,
            },
            "languages": [lang],
        });
        language_registry.register_fake_lsp_adapter(
            language,
            FakeLspAdapter {
                name: server_name,
                additional_initialization_options: HashMap::from_iter([(
                    main_server_name.clone(),
                    contribution.clone(),
                )]),
                additional_workspace_configuration: HashMap::from_iter([(
                    main_server_name.clone(),
                    contribution,
                )]),
                ..FakeLspAdapter::default()
            },
        );
    }

    let sent_initialization_options = Arc::new(Mutex::new(None));
    let mut fake_servers = language_registry.register_fake_lsp(
        "Rust",
        FakeLspAdapter {
            name: "the-fake-language-server",
            initialization_options: Some(json!({
                "tsserver": {
                    "globalPlugins": ["default-plugin"],
                },
                "languages": ["default-lang"],
            })),
            initializer: Some(Box::new({
                let sent_initialization_options = sent_initialization_options.clone();
                move |fake_server| {
                    let sent_initialization_options = sent_initialization_options.clone();
                    fake_server.set_request_handler::<lsp::request::Initialize, _, _>(
                        move |params, _| {
                            *sent_initialization_options.lock() = params.initialization_options;
                            async move { Ok(lsp::InitializeResult::default()) }
                        },
                    );
                }
            })),
            ..FakeLspAdapter::default()
        },
    );
    cx.run_until_parked();

    project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/the-root/main.rs"), cx)
        })
        .await
        .unwrap();
    let mut fake_server = fake_servers.next().await.unwrap();
    let workspace_configuration = fake_server
        .receive_notification::<lsp::notification::DidChangeConfiguration>()
        .await
        .settings;
    cx.run_until_parked();

    assert_eq!(
        sent_initialization_options.lock().take(),
        Some(json!({
            "tsserver": {
                "globalPlugins": ["default-plugin", "shared-plugin", "astro-plugin", "vue-plugin"],
                "maxMemory": 4096,
            },
            "languages": ["user-lang"],
            "userOnly": true,
        })),
    );
    assert_eq!(
        workspace_configuration,
        json!({
            "tsserver": {
                "globalPlugins": ["shared-plugin", "astro-plugin", "vue-plugin"],
                "maxMemory": 4096,
            },
            "languages": ["astro", "vue"],
        }),
    );
}

#[gpui::test]
async fn test_initialization_options_contributions_without_own_options(cx: &mut TestAppContext) {
    init_test(cx);

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/the-root"), json!({ "main.rs": "fn main() {}" }))
        .await;

    let project = Project::test(fs, [path!("/the-root").as_ref()], cx).await;
    let language_registry = project.read_with(cx, |project, _| project.languages().clone());
    language_registry.add(rust_lang());

    let contribution = json!({
        "tsserver": {
            "globalPlugins": ["vue-plugin"],
        },
    });
    language_registry.register_fake_lsp_adapter(
        "Vue",
        FakeLspAdapter {
            name: "vue-language-server",
            additional_initialization_options: HashMap::from_iter([(
                LanguageServerName("the-fake-language-server".into()),
                contribution.clone(),
            )]),
            ..FakeLspAdapter::default()
        },
    );

    let sent_initialization_options = Arc::new(Mutex::new(None));
    let mut fake_servers = language_registry.register_fake_lsp(
        "Rust",
        FakeLspAdapter {
            name: "the-fake-language-server",
            initialization_options: None,
            initializer: Some(Box::new({
                let sent_initialization_options = sent_initialization_options.clone();
                move |fake_server| {
                    let sent_initialization_options = sent_initialization_options.clone();
                    fake_server.set_request_handler::<lsp::request::Initialize, _, _>(
                        move |params, _| {
                            *sent_initialization_options.lock() =
                                Some(params.initialization_options);
                            async move { Ok(lsp::InitializeResult::default()) }
                        },
                    );
                }
            })),
            ..FakeLspAdapter::default()
        },
    );
    cx.run_until_parked();

    project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/the-root/main.rs"), cx)
        })
        .await
        .unwrap();
    fake_servers.next().await.unwrap();
    cx.run_until_parked();

    assert_eq!(
        sent_initialization_options.lock().take(),
        Some(Some(contribution)),
    );
}

async fn project_with_rust_server(
    fs: Arc<FakeFs>,
    root: &str,
    first_file: &str,
    cx: &mut TestAppContext,
) -> (Entity<Project>, LanguageServerId) {
    let project = Project::test(fs, [root.as_ref()], cx).await;
    let language_registry = project.read_with(cx, |project, _| project.languages().clone());
    language_registry.add(rust_lang());
    let mut fake_servers = language_registry.register_fake_lsp("Rust", FakeLspAdapter::default());

    project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(first_file, cx)
        })
        .await
        .unwrap();
    fake_servers.next().await.unwrap();
    cx.run_until_parked();

    let server_id = project.read_with(cx, |project, cx| {
        project
            .lsp_store()
            .read(cx)
            .language_server_statuses()
            .next()
            .unwrap()
            .0
    });
    (project, server_id)
}

fn buffer_paths(buffer: &Entity<Buffer>, cx: &TestAppContext) -> (String, PathBuf) {
    buffer.read_with(cx, |buffer, cx| {
        let file = File::from_dyn(buffer.file()).unwrap();
        (file.path.as_unix_str().to_string(), file.abs_path(cx))
    })
}

fn worktree_roots(project: &Entity<Project>, cx: &TestAppContext) -> Vec<PathBuf> {
    project.read_with(cx, |project, cx| {
        project
            .worktrees(cx)
            .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
            .collect()
    })
}

fn worktree_entries(project: &Entity<Project>, cx: &TestAppContext) -> Vec<String> {
    project.read_with(cx, |project, cx| {
        let worktree = project.worktrees(cx).next().unwrap();
        worktree
            .read(cx)
            .snapshot()
            .entries(true, 0)
            .map(|entry| entry.path.as_unix_str().to_string())
            .collect()
    })
}

#[gpui::test(iterations = 10)]
async fn test_runtime_lease_preserves_buffers_and_manual_stop(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/runtime"), json!({ "main.rs": "fn main() {}" }))
        .await;
    let project = Project::test(fs, [path!("/runtime").as_ref()], cx).await;
    let languages = project.read_with(cx, |project, _| project.languages().clone());
    languages.add(rust_lang());
    let mut servers = languages.register_fake_lsp("Rust", FakeLspAdapter::default());
    let foreground = project.update(cx, |project, cx| project.acquire_runtime_lease(cx));
    let background = project.update(cx, |project, cx| project.acquire_runtime_lease(cx));
    let (buffer, _handle) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/runtime/main.rs"), cx)
        })
        .await
        .unwrap();
    let mut server = servers.next().await.unwrap();
    server
        .receive_notification::<lsp::notification::DidOpenTextDocument>()
        .await;
    let mut shutdowns = server
        .set_request_handler::<lsp::request::Shutdown, _, _>(|_, _| futures::future::ready(Ok(())));
    drop(foreground);
    cx.run_until_parked();
    assert!(!project.read_with(cx, |project, cx| project.runtime_is_suspended(cx)));
    assert!(shutdowns.next().now_or_never().is_none());
    drop(background);
    shutdowns.next().await.unwrap();
    cx.run_until_parked();
    assert!(project.read_with(cx, |project, cx| project.runtime_is_suspended(cx)));
    buffer.update(cx, |buffer, cx| {
        buffer.edit([(0..0, "// retained edit\n")], None, cx)
    });
    let foreground = project.update(cx, |project, cx| project.acquire_runtime_lease(cx));
    let mut resumed = servers.next().await.unwrap();
    let opened = resumed
        .receive_notification::<lsp::notification::DidOpenTextDocument>()
        .await;
    let expected_text = buffer.read_with(cx, |buffer, _| {
        buffer
            .line_ending()
            .apply("// retained edit\nfn main() {}".into())
    });
    assert_eq!(opened.text_document.text, expected_text);
    cx.run_until_parked();
    assert!(!project.read_with(cx, |project, cx| project.runtime_is_suspended(cx)));
    let store = project.read_with(cx, |project, _| project.lsp_store());
    store.update(cx, |store, cx| store.stop_all_language_servers(cx));
    cx.run_until_parked();
    drop(foreground);
    cx.run_until_parked();
    let _foreground = project.update(cx, |project, cx| project.acquire_runtime_lease(cx));
    cx.run_until_parked();
    assert!(!project.read_with(cx, |project, cx| project.runtime_is_suspended(cx)));
    assert!(servers.next().now_or_never().is_none());
}

#[gpui::test(iterations = 10)]
async fn test_runtime_resume_only_registers_buffers_with_live_lsp_handles(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/runtime"),
        json!({ "main.rs": "fn main() {}", "background.rs": "fn background() {}" }),
    )
    .await;
    let project = Project::test(fs, [path!("/runtime").as_ref()], cx).await;
    let languages = project.read_with(cx, |project, _| project.languages().clone());
    languages.add(rust_lang());
    let mut servers = languages.register_fake_lsp("Rust", FakeLspAdapter::default());
    let foreground = project.update(cx, |project, cx| project.acquire_runtime_lease(cx));
    let (buffer, handle) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/runtime/main.rs"), cx)
        })
        .await
        .unwrap();
    let background = project
        .update(cx, |project, cx| {
            project.open_local_buffer(path!("/runtime/background.rs"), cx)
        })
        .await
        .unwrap();
    let mut server = servers.next().await.unwrap();
    server
        .receive_notification::<lsp::notification::DidOpenTextDocument>()
        .await;
    drop(foreground);
    cx.run_until_parked();
    assert!(project.read_with(cx, |project, cx| project.runtime_is_suspended(cx)));
    let foreground = project.update(cx, |project, cx| project.acquire_runtime_lease(cx));
    let mut resumed = servers.next().await.unwrap();
    let opened = resumed
        .receive_notification::<lsp::notification::DidOpenTextDocument>()
        .await;
    assert_eq!(
        opened.text_document.uri,
        Uri::from_file_path(path!("/runtime/main.rs")).unwrap()
    );
    cx.run_until_parked();
    assert!(
        resumed
            .receive_notification::<lsp::notification::DidOpenTextDocument>()
            .now_or_never()
            .is_none()
    );
    cx.update(|_| drop(handle));
    resumed
        .receive_notification::<lsp::notification::DidCloseTextDocument>()
        .await;
    drop(foreground);
    cx.run_until_parked();
    let _foreground = project.update(cx, |project, cx| project.acquire_runtime_lease(cx));
    cx.run_until_parked();
    assert!(!project.read_with(cx, |project, cx| project.runtime_is_suspended(cx)));
    assert!(servers.next().now_or_never().is_none());
    assert_eq!(
        buffer.read_with(cx, |buffer, _| buffer.text()),
        "fn main() {}"
    );
    assert_eq!(
        background.read_with(cx, |buffer, _| buffer.text()),
        "fn background() {}"
    );
}

#[gpui::test(iterations = 10)]
async fn test_runtime_resume_tolerates_failed_optional_server(cx: &mut TestAppContext) {
    use std::sync::atomic::{AtomicUsize, Ordering};

    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/runtime"), json!({ "main.rs": "fn main() {}" }))
        .await;
    let project = Project::test(fs, [path!("/runtime").as_ref()], cx).await;
    let languages = project.read_with(cx, |project, _| project.languages().clone());
    languages.add(rust_lang());
    let requests = Arc::new(AtomicUsize::new(0));
    let mut servers = languages.register_fake_lsp(
        "Rust",
        FakeLspAdapter {
            capabilities: lsp::ServerCapabilities {
                rename_provider: Some(lsp::OneOf::Left(true)),
                ..Default::default()
            },
            initializer: Some(Box::new({
                let requests = requests.clone();
                move |server| {
                    let requests = requests.clone();
                    server.set_request_handler::<lsp::request::Rename, _, _>(move |_, _| {
                        requests.fetch_add(1, Ordering::SeqCst);
                        async move { Ok(None) }
                    });
                }
            })),
            ..Default::default()
        },
    );
    let starts = Arc::new(AtomicUsize::new(0));
    let mut optional_servers = languages.register_fake_lsp(
        "Rust",
        FakeLspAdapter {
            name: "optional-server",
            initializer: Some(Box::new({
                let starts = starts.clone();
                move |server| {
                    if starts.fetch_add(1, Ordering::SeqCst) == 1 {
                        server.set_request_handler::<lsp::request::Initialize, _, _>(
                            |_, _| async move { anyhow::bail!("optional server unavailable") },
                        );
                    }
                }
            })),
            ..Default::default()
        },
    );
    let foreground = project.update(cx, |project, cx| project.acquire_runtime_lease(cx));
    let (buffer, _handle) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/runtime/main.rs"), cx)
        })
        .await
        .unwrap();
    let mut server = servers.next().await.unwrap();
    server
        .receive_notification::<lsp::notification::DidOpenTextDocument>()
        .await;
    let mut optional = optional_servers.next().await.unwrap();
    optional
        .receive_notification::<lsp::notification::DidOpenTextDocument>()
        .await;
    drop(foreground);
    cx.run_until_parked();
    assert!(project.read_with(cx, |project, cx| project.runtime_is_suspended(cx)));

    let _foreground = project.update(cx, |project, cx| project.acquire_runtime_lease(cx));
    let mut resumed = servers.next().await.unwrap();
    resumed
        .receive_notification::<lsp::notification::DidOpenTextDocument>()
        .await;
    let _failed = optional_servers.next().await.unwrap();
    cx.run_until_parked();
    assert!(!project.read_with(cx, |project, cx| project.runtime_is_suspended(cx)));
    project
        .update(cx, |project, cx| {
            project.perform_rename(buffer.clone(), 3, "renamed".into(), None, cx)
        })
        .await
        .unwrap();
    assert_eq!(requests.load(Ordering::SeqCst), 1);

    let store = project.read_with(cx, |project, _| project.lsp_store());
    store
        .update(cx, |store, cx| store.resume_language_servers(cx))
        .await
        .unwrap();
    let mut retried = optional_servers.next().await.unwrap();
    retried
        .receive_notification::<lsp::notification::DidOpenTextDocument>()
        .await;
    assert_eq!(starts.load(Ordering::SeqCst), 3);
    assert!(servers.next().now_or_never().is_none());
}

#[gpui::test(iterations = 10)]
async fn test_runtime_resume_does_not_wait_for_optional_server(cx: &mut TestAppContext) {
    use std::sync::atomic::{AtomicUsize, Ordering};

    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/runtime"), json!({ "main.rs": "fn main() {}" }))
        .await;
    let project = Project::test(fs, [path!("/runtime").as_ref()], cx).await;
    let languages = project.read_with(cx, |project, _| project.languages().clone());
    languages.add(rust_lang());
    let requests = Arc::new(AtomicUsize::new(0));
    let mut servers = languages.register_fake_lsp(
        "Rust",
        FakeLspAdapter {
            capabilities: lsp::ServerCapabilities {
                rename_provider: Some(lsp::OneOf::Left(true)),
                ..Default::default()
            },
            initializer: Some(Box::new({
                let requests = requests.clone();
                move |server| {
                    let requests = requests.clone();
                    server.set_request_handler::<lsp::request::Rename, _, _>(move |_, _| {
                        requests.fetch_add(1, Ordering::SeqCst);
                        async move { Ok(None) }
                    });
                }
            })),
            ..Default::default()
        },
    );
    let starts = Arc::new(AtomicUsize::new(0));
    let optional_requests = Arc::new(AtomicUsize::new(0));
    let (release, wait) = futures::channel::oneshot::channel();
    let wait = Arc::new(Mutex::new(Some(wait)));
    let mut optional_servers = languages.register_fake_lsp(
        "Rust",
        FakeLspAdapter {
            name: "optional-server",
            initializer: Some(Box::new({
                let starts = starts.clone();
                let optional_requests = optional_requests.clone();
                move |server| {
                    let optional_requests = optional_requests.clone();
                    server.set_request_handler::<lsp::request::Rename, _, _>(move |_, _| {
                        optional_requests.fetch_add(1, Ordering::SeqCst);
                        async move { Ok(None) }
                    });
                    if starts.fetch_add(1, Ordering::SeqCst) == 1 {
                        let wait = wait.clone();
                        server.set_request_handler::<lsp::request::Initialize, _, _>(
                            move |_, _| {
                                let wait = wait.lock().take().unwrap();
                                async move {
                                    wait.await.unwrap();
                                    Ok(lsp::InitializeResult {
                                        capabilities: lsp::ServerCapabilities {
                                            rename_provider: Some(lsp::OneOf::Left(true)),
                                            ..Default::default()
                                        },
                                        ..Default::default()
                                    })
                                }
                            },
                        );
                    }
                }
            })),
            ..Default::default()
        },
    );
    let foreground = project.update(cx, |project, cx| project.acquire_runtime_lease(cx));
    let (buffer, _handle) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/runtime/main.rs"), cx)
        })
        .await
        .unwrap();
    let mut server = servers.next().await.unwrap();
    server
        .receive_notification::<lsp::notification::DidOpenTextDocument>()
        .await;
    let mut optional = optional_servers.next().await.unwrap();
    optional
        .receive_notification::<lsp::notification::DidOpenTextDocument>()
        .await;
    drop(foreground);
    cx.run_until_parked();
    assert!(project.read_with(cx, |project, cx| project.runtime_is_suspended(cx)));

    let _foreground = project.update(cx, |project, cx| project.acquire_runtime_lease(cx));
    let mut resumed = servers.next().await.unwrap();
    resumed
        .receive_notification::<lsp::notification::DidOpenTextDocument>()
        .await;
    let mut pending = optional_servers.next().await.unwrap();
    cx.run_until_parked();
    assert!(!project.read_with(cx, |project, cx| project.runtime_is_suspended(cx)));
    project
        .update(cx, |project, cx| {
            project.perform_rename(buffer.clone(), 3, "renamed".into(), None, cx)
        })
        .await
        .unwrap();
    assert_eq!(requests.load(Ordering::SeqCst), 1);

    let mut targeted_rename = project.update(cx, |project, cx| {
        project.perform_rename(
            buffer.clone(),
            3,
            "targeted".into(),
            Some(pending.server.server_id()),
            cx,
        )
    });
    cx.run_until_parked();
    assert!((&mut targeted_rename).now_or_never().is_none());
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    assert_eq!(optional_requests.load(Ordering::SeqCst), 0);
    release.send(()).unwrap();
    pending
        .receive_notification::<lsp::notification::DidOpenTextDocument>()
        .await;
    targeted_rename.await.unwrap();
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    assert_eq!(optional_requests.load(Ordering::SeqCst), 1);
    assert_eq!(starts.load(Ordering::SeqCst), 2);
    assert!(servers.next().now_or_never().is_none());
}

#[gpui::test(iterations = 10)]
async fn test_runtime_formatting_waits_only_for_selected_servers(cx: &mut TestAppContext) {
    use std::sync::atomic::{AtomicUsize, Ordering};

    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/runtime"), json!({ "main.rs": "fn main() {}" }))
        .await;
    let project = Project::test(fs, [path!("/runtime").as_ref()], cx).await;
    let languages = project.read_with(cx, |project, _| project.languages().clone());
    languages.add(rust_lang());
    let requests = Arc::new(AtomicUsize::new(0));
    let mut servers = languages.register_fake_lsp(
        "Rust",
        FakeLspAdapter {
            capabilities: lsp::ServerCapabilities {
                document_formatting_provider: Some(lsp::OneOf::Left(true)),
                ..Default::default()
            },
            initializer: Some(Box::new({
                let requests = requests.clone();
                move |server| {
                    let requests = requests.clone();
                    server.set_request_handler::<lsp::request::Formatting, _, _>(move |_, _| {
                        requests.fetch_add(1, Ordering::SeqCst);
                        async move { Ok(None) }
                    });
                }
            })),
            ..Default::default()
        },
    );
    let starts = Arc::new(AtomicUsize::new(0));
    let (release, wait) = futures::channel::oneshot::channel();
    let wait = Arc::new(Mutex::new(Some(wait)));
    let mut optional_servers = languages.register_fake_lsp(
        "Rust",
        FakeLspAdapter {
            name: "optional-server",
            capabilities: lsp::ServerCapabilities {
                document_formatting_provider: Some(lsp::OneOf::Left(true)),
                ..Default::default()
            },
            initializer: Some(Box::new({
                let starts = starts.clone();
                let requests = requests.clone();
                move |server| {
                    let requests = requests.clone();
                    server.set_request_handler::<lsp::request::Formatting, _, _>(move |_, _| {
                        requests.fetch_add(1, Ordering::SeqCst);
                        async move { Ok(None) }
                    });
                    if starts.fetch_add(1, Ordering::SeqCst) == 1 {
                        let wait = wait.clone();
                        server.set_request_handler::<lsp::request::Initialize, _, _>(
                            move |_, _| {
                                let wait = wait.lock().take().unwrap();
                                async move {
                                    wait.await.unwrap();
                                    Ok(lsp::InitializeResult {
                                        capabilities: lsp::ServerCapabilities {
                                            document_formatting_provider: Some(lsp::OneOf::Left(
                                                true,
                                            )),
                                            ..Default::default()
                                        },
                                        ..Default::default()
                                    })
                                }
                            },
                        );
                    }
                }
            })),
            ..Default::default()
        },
    );
    let foreground = project.update(cx, |project, cx| project.acquire_runtime_lease(cx));
    let (buffer, _handle) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/runtime/main.rs"), cx)
        })
        .await
        .unwrap();
    let mut server = servers.next().await.unwrap();
    server
        .receive_notification::<lsp::notification::DidOpenTextDocument>()
        .await;
    let mut optional = optional_servers.next().await.unwrap();
    optional
        .receive_notification::<lsp::notification::DidOpenTextDocument>()
        .await;
    drop(foreground);
    cx.run_until_parked();
    assert!(project.read_with(cx, |project, cx| project.runtime_is_suspended(cx)));

    let _foreground = project.update(cx, |project, cx| project.acquire_runtime_lease(cx));
    let mut resumed = servers.next().await.unwrap();
    resumed
        .receive_notification::<lsp::notification::DidOpenTextDocument>()
        .await;
    let mut pending = optional_servers.next().await.unwrap();
    cx.run_until_parked();
    assert!(!project.read_with(cx, |project, cx| project.runtime_is_suspended(cx)));
    use language::language_settings::{Formatter, FormatterList};
    use settings::{LanguageServerFormatterSpecifier, SettingsStore};
    for formatter in [
        Formatter::None,
        Formatter::LanguageServer(LanguageServerFormatterSpecifier::Specific {
            name: "the-fake-language-server".into(),
        }),
        Formatter::LanguageServer(LanguageServerFormatterSpecifier::Current),
    ] {
        cx.update(|cx| {
            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.project.all_languages.defaults.formatter =
                        Some(FormatterList::Single(formatter));
                });
            })
        });
        project
            .update(cx, |project, cx| {
                project.format(
                    collections::HashSet::from_iter([buffer.clone()]),
                    LspFormatTarget::Buffers,
                    false,
                    FormatTrigger::Manual,
                    cx,
                )
            })
            .await
            .unwrap();
    }
    assert_eq!(requests.load(Ordering::SeqCst), 2);
    cx.update(|cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |settings| {
                settings.project.all_languages.defaults.formatter =
                    Some(FormatterList::Single(Formatter::External {
                        command: "nonexistent-runtime-test-formatter".into(),
                        arguments: None,
                    }));
            });
        })
    });
    let external_error = project
        .update(cx, |project, cx| {
            project.format(
                collections::HashSet::from_iter([buffer.clone()]),
                LspFormatTarget::Buffers,
                false,
                FormatTrigger::Manual,
                cx,
            )
        })
        .await
        .unwrap_err();
    assert!(external_error.to_string().contains("external command"));

    cx.update(|cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |settings| {
                settings.project.all_languages.defaults.formatter = Some(FormatterList::Single(
                    Formatter::LanguageServer(LanguageServerFormatterSpecifier::Specific {
                        name: "optional-server".into(),
                    }),
                ));
            });
        })
    });
    cx.update(|cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |settings| {
                settings.project.all_languages.defaults.format_on_save =
                    Some(language::language_settings::FormatOnSave::Off);
            });
        })
    });
    project
        .update(cx, |project, cx| {
            project.format(
                collections::HashSet::from_iter([buffer.clone()]),
                LspFormatTarget::Buffers,
                false,
                FormatTrigger::Save,
                cx,
            )
        })
        .await
        .unwrap();
    assert_eq!(requests.load(Ordering::SeqCst), 2);
    let mut format = project.update(cx, |project, cx| {
        project.format(
            collections::HashSet::from_iter([buffer.clone()]),
            LspFormatTarget::Buffers,
            false,
            FormatTrigger::Manual,
            cx,
        )
    });
    cx.run_until_parked();
    assert!((&mut format).now_or_never().is_none());
    assert_eq!(requests.load(Ordering::SeqCst), 2);

    release.send(()).unwrap();
    pending
        .receive_notification::<lsp::notification::DidOpenTextDocument>()
        .await;
    format.await.unwrap();
    assert_eq!(requests.load(Ordering::SeqCst), 3);
    assert_eq!(starts.load(Ordering::SeqCst), 2);
    assert!(servers.next().now_or_never().is_none());
}

#[gpui::test(iterations = 10)]
async fn test_runtime_code_action_waits_for_its_originating_server(cx: &mut TestAppContext) {
    use std::sync::atomic::{AtomicUsize, Ordering};

    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/runtime"), json!({ "main.rs": "fn main() {}" }))
        .await;
    let project = Project::test(fs, [path!("/runtime").as_ref()], cx).await;
    let languages = project.read_with(cx, |project, _| project.languages().clone());
    languages.add(rust_lang());
    let requests = Arc::new(AtomicUsize::new(0));
    let mut servers = languages.register_fake_lsp(
        "Rust",
        FakeLspAdapter {
            capabilities: lsp::ServerCapabilities {
                code_action_provider: Some(lsp::CodeActionProviderCapability::Options(
                    lsp::CodeActionOptions {
                        resolve_provider: Some(true),
                        ..Default::default()
                    },
                )),
                ..Default::default()
            },
            initializer: Some(Box::new({
                let requests = requests.clone();
                move |server| {
                    let requests = requests.clone();
                    server.set_request_handler::<lsp::request::CodeActionResolveRequest, _, _>(
                        move |action, _| {
                            requests.fetch_add(1, Ordering::SeqCst);
                            async move { Ok(action) }
                        },
                    );
                }
            })),
            ..Default::default()
        },
    );
    let starts = Arc::new(AtomicUsize::new(0));
    let (release, wait) = futures::channel::oneshot::channel();
    let wait = Arc::new(Mutex::new(Some(wait)));
    let mut optional_servers = languages.register_fake_lsp(
        "Rust",
        FakeLspAdapter {
            name: "optional-server",
            capabilities: lsp::ServerCapabilities {
                code_action_provider: Some(lsp::CodeActionProviderCapability::Options(
                    lsp::CodeActionOptions {
                        resolve_provider: Some(true),
                        ..Default::default()
                    },
                )),
                ..Default::default()
            },
            initializer: Some(Box::new({
                let starts = starts.clone();
                let requests = requests.clone();
                move |server| {
                    let requests = requests.clone();
                    server.set_request_handler::<lsp::request::CodeActionResolveRequest, _, _>(
                        move |action, _| {
                            requests.fetch_add(1, Ordering::SeqCst);
                            async move { Ok(action) }
                        },
                    );
                    if starts.fetch_add(1, Ordering::SeqCst) == 1 {
                        let wait = wait.clone();
                        server.set_request_handler::<lsp::request::Initialize, _, _>(
                            move |_, _| {
                                let wait = wait.lock().take().unwrap();
                                async move {
                                    wait.await.unwrap();
                                    Ok(lsp::InitializeResult {
                                        capabilities: lsp::ServerCapabilities {
                                            code_action_provider: Some(
                                                lsp::CodeActionProviderCapability::Options(
                                                    lsp::CodeActionOptions {
                                                        resolve_provider: Some(true),
                                                        ..Default::default()
                                                    },
                                                ),
                                            ),
                                            ..Default::default()
                                        },
                                        ..Default::default()
                                    })
                                }
                            },
                        );
                    }
                }
            })),
            ..Default::default()
        },
    );
    let foreground = project.update(cx, |project, cx| project.acquire_runtime_lease(cx));
    let (buffer, _handle) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/runtime/main.rs"), cx)
        })
        .await
        .unwrap();
    let mut server = servers.next().await.unwrap();
    server
        .receive_notification::<lsp::notification::DidOpenTextDocument>()
        .await;
    let mut optional = optional_servers.next().await.unwrap();
    optional
        .receive_notification::<lsp::notification::DidOpenTextDocument>()
        .await;
    drop(foreground);
    cx.run_until_parked();
    assert!(project.read_with(cx, |project, cx| project.runtime_is_suspended(cx)));

    let _foreground = project.update(cx, |project, cx| project.acquire_runtime_lease(cx));
    let mut resumed = servers.next().await.unwrap();
    resumed
        .receive_notification::<lsp::notification::DidOpenTextDocument>()
        .await;
    let mut pending = optional_servers.next().await.unwrap();
    cx.run_until_parked();
    assert!(!project.read_with(cx, |project, cx| project.runtime_is_suspended(cx)));
    let range = buffer.read_with(cx, |buffer, _| {
        buffer.anchor_before(0)..buffer.anchor_after(3)
    });
    let action = project::CodeAction {
        server_id: pending.server.server_id(),
        range,
        lsp_action: project::LspAction::Action(Box::new(lsp::CodeAction {
            title: "Fix".into(),
            data: Some(json!({ "resolve": true })),
            ..Default::default()
        })),
        resolved: false,
    };
    let mut stale_action = action.clone();
    stale_action.server_id = optional.server.server_id();
    let stale_result = project
        .update(cx, |project, cx| {
            project.apply_code_action(buffer.clone(), stale_action, false, cx)
        })
        .await;
    assert!(
        stale_result
            .unwrap_err()
            .to_string()
            .contains("no longer available")
    );
    let mut apply = project.update(cx, |project, cx| {
        project.apply_code_action(buffer.clone(), action, false, cx)
    });
    cx.run_until_parked();
    assert!((&mut apply).now_or_never().is_none());
    assert_eq!(requests.load(Ordering::SeqCst), 0);

    release.send(()).unwrap();
    pending
        .receive_notification::<lsp::notification::DidOpenTextDocument>()
        .await;
    apply.await.unwrap();
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    assert_eq!(starts.load(Ordering::SeqCst), 2);
    assert!(servers.next().now_or_never().is_none());
}

#[gpui::test(iterations = 10)]
async fn test_runtime_definitions_wait_for_all_capable_servers(cx: &mut TestAppContext) {
    use std::sync::atomic::{AtomicUsize, Ordering};

    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/runtime"), json!({ "main.rs": "fn main() {}" }))
        .await;
    let project = Project::test(fs, [path!("/runtime").as_ref()], cx).await;
    let languages = project.read_with(cx, |project, _| project.languages().clone());
    languages.add(rust_lang());
    let requests = Arc::new(AtomicUsize::new(0));
    let mut servers = languages.register_fake_lsp(
        "Rust",
        FakeLspAdapter {
            capabilities: lsp::ServerCapabilities {
                definition_provider: Some(lsp::OneOf::Left(true)),
                ..Default::default()
            },
            initializer: Some(Box::new({
                let requests = requests.clone();
                move |server| {
                    let requests = requests.clone();
                    server.set_request_handler::<lsp::request::GotoDefinition, _, _>(
                        move |_, _| {
                            requests.fetch_add(1, Ordering::SeqCst);
                            async move { Ok(None) }
                        },
                    );
                }
            })),
            ..Default::default()
        },
    );
    let starts = Arc::new(AtomicUsize::new(0));
    let (release, wait) = futures::channel::oneshot::channel();
    let wait = Arc::new(Mutex::new(Some(wait)));
    let mut optional_servers = languages.register_fake_lsp(
        "Rust",
        FakeLspAdapter {
            name: "optional-server",
            capabilities: lsp::ServerCapabilities {
                definition_provider: Some(lsp::OneOf::Left(true)),
                ..Default::default()
            },
            initializer: Some(Box::new({
                let starts = starts.clone();
                let requests = requests.clone();
                move |server| {
                    let requests = requests.clone();
                    server.set_request_handler::<lsp::request::GotoDefinition, _, _>(
                        move |_, _| {
                            requests.fetch_add(1, Ordering::SeqCst);
                            async move { Ok(None) }
                        },
                    );
                    if starts.fetch_add(1, Ordering::SeqCst) == 1 {
                        let wait = wait.clone();
                        server.set_request_handler::<lsp::request::Initialize, _, _>(
                            move |_, _| {
                                let wait = wait.lock().take().unwrap();
                                async move {
                                    wait.await.unwrap();
                                    Ok(lsp::InitializeResult {
                                        capabilities: lsp::ServerCapabilities {
                                            definition_provider: Some(lsp::OneOf::Left(true)),
                                            ..Default::default()
                                        },
                                        ..Default::default()
                                    })
                                }
                            },
                        );
                    }
                }
            })),
            ..Default::default()
        },
    );
    let foreground = project.update(cx, |project, cx| project.acquire_runtime_lease(cx));
    let (buffer, _handle) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/runtime/main.rs"), cx)
        })
        .await
        .unwrap();
    let mut server = servers.next().await.unwrap();
    server
        .receive_notification::<lsp::notification::DidOpenTextDocument>()
        .await;
    let mut optional = optional_servers.next().await.unwrap();
    optional
        .receive_notification::<lsp::notification::DidOpenTextDocument>()
        .await;
    drop(foreground);
    cx.run_until_parked();
    assert!(project.read_with(cx, |project, cx| project.runtime_is_suspended(cx)));

    let _foreground = project.update(cx, |project, cx| project.acquire_runtime_lease(cx));
    let mut resumed = servers.next().await.unwrap();
    resumed
        .receive_notification::<lsp::notification::DidOpenTextDocument>()
        .await;
    let mut pending = optional_servers.next().await.unwrap();
    cx.run_until_parked();
    assert!(!project.read_with(cx, |project, cx| project.runtime_is_suspended(cx)));
    let mut definitions = project.update(cx, |project, cx| project.definitions(&buffer, 3, cx));
    cx.run_until_parked();
    assert!((&mut definitions).now_or_never().is_none());
    assert_eq!(requests.load(Ordering::SeqCst), 0);

    release.send(()).unwrap();
    pending
        .receive_notification::<lsp::notification::DidOpenTextDocument>()
        .await;
    definitions.await.unwrap();
    assert_eq!(requests.load(Ordering::SeqCst), 2);
    assert_eq!(starts.load(Ordering::SeqCst), 2);
    assert!(servers.next().now_or_never().is_none());
}

#[gpui::test(iterations = 10)]
async fn test_runtime_lease_reacquired_during_shutdown(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/runtime"), json!({ "main.rs": "fn main() {}" }))
        .await;
    let project = Project::test(fs, [path!("/runtime").as_ref()], cx).await;
    let languages = project.read_with(cx, |project, _| project.languages().clone());
    languages.add(rust_lang());
    let mut servers = languages.register_fake_lsp("Rust", FakeLspAdapter::default());
    let foreground = project.update(cx, |project, cx| project.acquire_runtime_lease(cx));
    let (_buffer, _handle) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/runtime/main.rs"), cx)
        })
        .await
        .unwrap();
    let mut server = servers.next().await.unwrap();
    server
        .receive_notification::<lsp::notification::DidOpenTextDocument>()
        .await;
    let (release, wait) = futures::channel::oneshot::channel();
    let mut wait = Some(wait);
    let (shutdown_started, shutdown_entered) = futures::channel::oneshot::channel();
    let mut shutdown_started = Some(shutdown_started);
    let mut shutdowns = server.set_request_handler::<lsp::request::Shutdown, _, _>(move |_, _| {
        let wait = wait.take().unwrap();
        shutdown_started.take().unwrap().send(()).unwrap();
        async move {
            wait.await.unwrap();
            Ok(())
        }
    });
    drop(foreground);
    shutdown_entered.await.unwrap();
    cx.run_until_parked();
    let foreground = project.update(cx, |project, cx| project.acquire_runtime_lease(cx));
    cx.run_until_parked();
    assert!(servers.next().now_or_never().is_none());
    release.send(()).unwrap();
    shutdowns.next().await.unwrap();
    let _resumed = servers.next().await.unwrap();
    cx.run_until_parked();
    assert!(!project.read_with(cx, |project, cx| project.runtime_is_suspended(cx)));
    drop(foreground);
    cx.run_until_parked();
    assert!(project.read_with(cx, |project, cx| project.runtime_is_suspended(cx)));
}

#[gpui::test(iterations = 10)]
async fn test_runtime_lease_keeps_pending_editor_requests_alive(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/runtime"), json!({ "main.rs": "fn main() {}" }))
        .await;
    let project = Project::test(fs, [path!("/runtime").as_ref()], cx).await;
    let languages = project.read_with(cx, |project, _| project.languages().clone());
    languages.add(rust_lang());
    let mut servers = languages.register_fake_lsp(
        "Rust",
        FakeLspAdapter {
            capabilities: lsp::ServerCapabilities {
                rename_provider: Some(lsp::OneOf::Left(true)),
                document_formatting_provider: Some(lsp::OneOf::Left(true)),
                ..Default::default()
            },
            ..Default::default()
        },
    );
    let foreground = project.update(cx, |project, cx| project.acquire_runtime_lease(cx));
    let (buffer, _handle) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/runtime/main.rs"), cx)
        })
        .await
        .unwrap();
    let mut server = servers.next().await.unwrap();
    server
        .receive_notification::<lsp::notification::DidOpenTextDocument>()
        .await;
    let (finish_rename, rename_wait) = futures::channel::oneshot::channel();
    let mut rename_wait = Some(rename_wait);
    server.set_request_handler::<lsp::request::Rename, _, _>(move |_, _| {
        let wait = rename_wait.take().unwrap();
        async move {
            wait.await.unwrap();
            Ok(None)
        }
    });
    let (finish_format, format_wait) = futures::channel::oneshot::channel();
    let mut format_wait = Some(format_wait);
    server.set_request_handler::<lsp::request::Formatting, _, _>(move |_, _| {
        let wait = format_wait.take().unwrap();
        async move {
            wait.await.unwrap();
            Ok(None)
        }
    });
    let mut shutdowns = server
        .set_request_handler::<lsp::request::Shutdown, _, _>(|_, _| futures::future::ready(Ok(())));
    let rename = project.update(cx, |project, cx| {
        project.perform_rename(buffer.clone(), 3, "renamed".into(), None, cx)
    });
    let format = project.update(cx, |project, cx| {
        project.format(
            [buffer.clone()].into_iter().collect(),
            LspFormatTarget::Buffers,
            true,
            FormatTrigger::Save,
            cx,
        )
    });
    cx.run_until_parked();
    drop(foreground);
    cx.run_until_parked();
    assert!(!project.read_with(cx, |project, cx| project.runtime_is_suspended(cx)));
    assert!(shutdowns.next().now_or_never().is_none());
    finish_rename.send(()).unwrap();
    rename.await.unwrap();
    cx.run_until_parked();
    assert!(!project.read_with(cx, |project, cx| project.runtime_is_suspended(cx)));
    assert!(shutdowns.next().now_or_never().is_none());
    finish_format.send(()).unwrap();
    format.await.unwrap();
    shutdowns.next().await.unwrap();
    cx.run_until_parked();
    assert!(project.read_with(cx, |project, cx| project.runtime_is_suspended(cx)));
}

#[gpui::test(iterations = 10)]
async fn test_runtime_lease_waits_for_resumed_server_initialization(cx: &mut TestAppContext) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/runtime"), json!({ "main.rs": "fn main() {}" }))
        .await;
    let project = Project::test(fs, [path!("/runtime").as_ref()], cx).await;
    let languages = project.read_with(cx, |project, _| project.languages().clone());
    languages.add(rust_lang());
    let starts = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(AtomicUsize::new(0));
    let (initialized, initialize_wait) = futures::channel::oneshot::channel();
    let initialize_wait = Arc::new(Mutex::new(Some(initialize_wait)));
    let mut servers = languages.register_fake_lsp(
        "Rust",
        FakeLspAdapter {
            capabilities: lsp::ServerCapabilities {
                rename_provider: Some(lsp::OneOf::Left(true)),
                ..Default::default()
            },
            initializer: Some(Box::new({
                let starts = starts.clone();
                let requests = requests.clone();
                move |server| {
                    if starts.fetch_add(1, Ordering::SeqCst) > 0 {
                        let initialize_wait = initialize_wait.clone();
                        server.set_request_handler::<lsp::request::Initialize, _, _>(
                            move |_, _| {
                                let wait = initialize_wait.lock().take().unwrap();
                                async move {
                                    wait.await.unwrap();
                                    Ok(lsp::InitializeResult {
                                        capabilities: lsp::ServerCapabilities {
                                            rename_provider: Some(lsp::OneOf::Left(true)),
                                            ..Default::default()
                                        },
                                        ..Default::default()
                                    })
                                }
                            },
                        );
                    }
                    let requests = requests.clone();
                    server.set_request_handler::<lsp::request::Rename, _, _>(move |_, _| {
                        requests.fetch_add(1, Ordering::SeqCst);
                        async move { Ok(None) }
                    });
                }
            })),
            ..Default::default()
        },
    );
    let foreground = project.update(cx, |project, cx| project.acquire_runtime_lease(cx));
    let (buffer, _handle) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/runtime/main.rs"), cx)
        })
        .await
        .unwrap();
    let mut server = servers.next().await.unwrap();
    server
        .receive_notification::<lsp::notification::DidOpenTextDocument>()
        .await;
    drop(foreground);
    cx.run_until_parked();
    assert!(project.read_with(cx, |project, cx| project.runtime_is_suspended(cx)));
    let rename = project.update(cx, |project, cx| {
        project.perform_rename(buffer.clone(), 3, "renamed".into(), None, cx)
    });
    cx.run_until_parked();
    assert_eq!(starts.load(Ordering::SeqCst), 2);
    assert_eq!(requests.load(Ordering::SeqCst), 0);
    assert!(!project.read_with(cx, |project, cx| project.runtime_is_suspended(cx)));
    initialized.send(()).unwrap();
    rename.await.unwrap();
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    cx.run_until_parked();
    assert!(project.read_with(cx, |project, cx| project.runtime_is_suspended(cx)));
}

#[gpui::test(iterations = 10)]
async fn test_signature_help_holds_runtime_lease_through_initialization_and_response(
    cx: &mut TestAppContext,
) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/runtime"), json!({ "main.rs": "fn main() {}" }))
        .await;
    let project = Project::test(fs, [path!("/runtime").as_ref()], cx).await;
    let languages = project.read_with(cx, |project, _| project.languages().clone());
    languages.add(rust_lang());
    let starts = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(AtomicUsize::new(0));
    let (initialized, initialize_wait) = futures::channel::oneshot::channel();
    let initialize_wait = Arc::new(Mutex::new(Some(initialize_wait)));
    let (respond, response_wait) = futures::channel::oneshot::channel();
    let response_wait = Arc::new(Mutex::new(Some(response_wait)));
    let mut servers = languages.register_fake_lsp(
        "Rust",
        FakeLspAdapter {
            capabilities: lsp::ServerCapabilities {
                signature_help_provider: Some(lsp::SignatureHelpOptions::default()),
                ..Default::default()
            },
            initializer: Some(Box::new({
                let starts = starts.clone();
                let requests = requests.clone();
                move |server| {
                    if starts.fetch_add(1, Ordering::SeqCst) > 0 {
                        let initialize_wait = initialize_wait.clone();
                        server.set_request_handler::<lsp::request::Initialize, _, _>(
                            move |_, _| {
                                let wait = initialize_wait.lock().take().unwrap();
                                async move {
                                    wait.await.unwrap();
                                    Ok(lsp::InitializeResult {
                                        capabilities: lsp::ServerCapabilities {
                                            signature_help_provider: Some(
                                                lsp::SignatureHelpOptions::default(),
                                            ),
                                            ..Default::default()
                                        },
                                        ..Default::default()
                                    })
                                }
                            },
                        );
                    }
                    let requests = requests.clone();
                    let response_wait = response_wait.clone();
                    server.set_request_handler::<lsp::request::SignatureHelpRequest, _, _>(
                        move |_, _| {
                            requests.fetch_add(1, Ordering::SeqCst);
                            let wait = response_wait.lock().take().unwrap();
                            async move {
                                wait.await.unwrap();
                                Ok(Some(lsp::SignatureHelp {
                                    signatures: vec![lsp::SignatureInformation {
                                        label: "main()".into(),
                                        documentation: None,
                                        parameters: None,
                                        active_parameter: None,
                                    }],
                                    active_signature: None,
                                    active_parameter: None,
                                }))
                            }
                        },
                    );
                }
            })),
            ..Default::default()
        },
    );
    let foreground = project.update(cx, |project, cx| project.acquire_runtime_lease(cx));
    let (buffer, _handle) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/runtime/main.rs"), cx)
        })
        .await
        .unwrap();
    let mut server = servers.next().await.unwrap();
    server
        .receive_notification::<lsp::notification::DidOpenTextDocument>()
        .await;
    drop(foreground);
    cx.run_until_parked();
    assert!(project.read_with(cx, |project, cx| project.runtime_is_suspended(cx)));
    let foreground = project.update(cx, |project, cx| project.acquire_runtime_lease(cx));
    let resumed = servers.next().await.unwrap();
    let mut shutdowns =
        resumed.set_request_handler::<lsp::request::Shutdown, _, _>(|_, _| async { Ok(()) });
    cx.run_until_parked();
    let mut help = project.update(cx, |project, cx| project.signature_help(&buffer, 3, cx));
    drop(foreground);
    cx.run_until_parked();
    assert_eq!(starts.load(Ordering::SeqCst), 2);
    assert_eq!(requests.load(Ordering::SeqCst), 0);
    assert!(!project.read_with(cx, |project, cx| project.runtime_is_suspended(cx)));
    assert!((&mut help).now_or_never().is_none());
    initialized.send(()).unwrap();
    cx.run_until_parked();
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    assert!(!project.read_with(cx, |project, cx| project.runtime_is_suspended(cx)));
    assert!(shutdowns.next().now_or_never().is_none());
    assert!((&mut help).now_or_never().is_none());
    respond.send(()).unwrap();
    assert_eq!(help.await.unwrap().len(), 1);
    shutdowns.next().await.unwrap();
    cx.run_until_parked();
    assert!(project.read_with(cx, |project, cx| project.runtime_is_suspended(cx)));
}

#[gpui::test(iterations = 10)]
async fn test_cancelled_signature_help_releases_runtime_lease(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/runtime"), json!({ "main.rs": "fn main() {}" }))
        .await;
    let project = Project::test(fs, [path!("/runtime").as_ref()], cx).await;
    let languages = project.read_with(cx, |project, _| project.languages().clone());
    languages.add(rust_lang());
    let mut servers = languages.register_fake_lsp(
        "Rust",
        FakeLspAdapter {
            capabilities: lsp::ServerCapabilities {
                signature_help_provider: Some(lsp::SignatureHelpOptions::default()),
                ..Default::default()
            },
            ..Default::default()
        },
    );
    let foreground = project.update(cx, |project, cx| project.acquire_runtime_lease(cx));
    let (buffer, _handle) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/runtime/main.rs"), cx)
        })
        .await
        .unwrap();
    let mut server = servers.next().await.unwrap();
    server
        .receive_notification::<lsp::notification::DidOpenTextDocument>()
        .await;
    let (started, request_started) = futures::channel::oneshot::channel();
    let mut started = Some(started);
    server.set_request_handler::<lsp::request::SignatureHelpRequest, _, _>(move |_, _| {
        started.take().unwrap().send(()).unwrap();
        futures::future::pending()
    });
    let mut shutdowns =
        server.set_request_handler::<lsp::request::Shutdown, _, _>(|_, _| async { Ok(()) });
    let help = project.update(cx, |project, cx| project.signature_help(&buffer, 3, cx));
    request_started.await.unwrap();
    drop(foreground);
    cx.run_until_parked();
    assert!(!project.read_with(cx, |project, cx| project.runtime_is_suspended(cx)));
    assert!(shutdowns.next().now_or_never().is_none());
    drop(help);
    shutdowns.next().await.unwrap();
    cx.run_until_parked();
    assert!(project.read_with(cx, |project, cx| project.runtime_is_suspended(cx)));
}

#[gpui::test(iterations = 10)]
async fn test_runtime_requests_wait_for_capable_servers_and_workspace_symbols(
    cx: &mut TestAppContext,
) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/runtime"), json!({ "main.rs": "fn main() {}" }))
        .await;
    let project = Project::test(fs, [path!("/runtime").as_ref()], cx).await;
    let languages = project.read_with(cx, |project, _| project.languages().clone());
    languages.add(rust_lang());
    let starts = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(AtomicUsize::new(0));
    let (initialized, initialize_wait) = futures::channel::oneshot::channel();
    let initialize_wait = Arc::new(Mutex::new(Some(initialize_wait)));
    let mut servers = languages.register_fake_lsp(
        "Rust",
        FakeLspAdapter {
            capabilities: lsp::ServerCapabilities {
                definition_provider: Some(lsp::OneOf::Left(true)),
                workspace_symbol_provider: Some(lsp::OneOf::Left(true)),
                ..Default::default()
            },
            initializer: Some(Box::new({
                let starts = starts.clone();
                let requests = requests.clone();
                move |server| {
                    if starts.fetch_add(1, Ordering::SeqCst) > 0 {
                        let initialize_wait = initialize_wait.clone();
                        server.set_request_handler::<lsp::request::Initialize, _, _>(
                            move |_, _| {
                                let wait = initialize_wait.lock().take().unwrap();
                                async move {
                                    wait.await.unwrap();
                                    Ok(lsp::InitializeResult {
                                        capabilities: lsp::ServerCapabilities {
                                            definition_provider: Some(lsp::OneOf::Left(true)),
                                            workspace_symbol_provider: Some(lsp::OneOf::Left(true)),
                                            ..Default::default()
                                        },
                                        ..Default::default()
                                    })
                                }
                            },
                        );
                    }
                    let requests = requests.clone();
                    server.set_request_handler::<lsp::request::GotoDefinition, _, _>(
                        move |_, _| {
                            requests.fetch_add(1, Ordering::SeqCst);
                            async move { Ok(None) }
                        },
                    );
                    server.set_request_handler::<lsp::request::WorkspaceSymbolRequest, _, _>(
                        |_, _| async move { Ok(None) },
                    );
                }
            })),
            ..Default::default()
        },
    );
    let mut incapable_servers = languages.register_fake_lsp(
        "Rust",
        FakeLspAdapter {
            name: "incapable",
            ..Default::default()
        },
    );
    let foreground = project.update(cx, |project, cx| project.acquire_runtime_lease(cx));
    let (buffer, _handle) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/runtime/main.rs"), cx)
        })
        .await
        .unwrap();
    let mut server = servers.next().await.unwrap();
    server
        .receive_notification::<lsp::notification::DidOpenTextDocument>()
        .await;
    let mut incapable = incapable_servers.next().await.unwrap();
    incapable
        .receive_notification::<lsp::notification::DidOpenTextDocument>()
        .await;
    drop(foreground);
    cx.run_until_parked();
    assert!(project.read_with(cx, |project, cx| project.runtime_is_suspended(cx)));
    let mut definitions = project.update(cx, |project, cx| project.definitions(&buffer, 3, cx));
    let mut symbols = project.update(cx, |project, cx| project.symbols("main", cx));
    let mut incapable = incapable_servers.next().await.unwrap();
    incapable
        .receive_notification::<lsp::notification::DidOpenTextDocument>()
        .await;
    cx.run_until_parked();
    assert_eq!(starts.load(Ordering::SeqCst), 2);
    assert_eq!(requests.load(Ordering::SeqCst), 0);
    assert!(!project.read_with(cx, |project, cx| project.runtime_is_suspended(cx)));
    assert!((&mut definitions).now_or_never().is_none());
    assert!((&mut symbols).now_or_never().is_none());
    initialized.send(()).unwrap();
    definitions.await.unwrap();
    symbols.await.unwrap();
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    cx.run_until_parked();
    assert!(project.read_with(cx, |project, cx| project.runtime_is_suspended(cx)));
}

#[gpui::test(iterations = 10)]
async fn test_suspension_discards_late_server_startup(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/runtime"), json!({ "main.rs": "fn main() {}" }))
        .await;
    let project = Project::test(fs, [path!("/runtime").as_ref()], cx).await;
    let languages = project.read_with(cx, |project, _| project.languages().clone());
    languages.add(rust_lang());
    let (release, wait) = futures::channel::oneshot::channel();
    let wait = Arc::new(Mutex::new(Some(wait)));
    let mut servers = languages.register_fake_lsp(
        "Rust",
        FakeLspAdapter {
            initializer: Some(Box::new(move |server| {
                let wait = wait.clone();
                server.set_request_handler::<lsp::request::Initialize, _, _>(move |_, _| {
                    let wait = wait.lock().take().unwrap();
                    async move {
                        wait.await.unwrap();
                        Ok(lsp::InitializeResult::default())
                    }
                });
            })),
            ..Default::default()
        },
    );
    let foreground = project.update(cx, |project, cx| project.acquire_runtime_lease(cx));
    let (buffer, _handle) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/runtime/main.rs"), cx)
        })
        .await
        .unwrap();
    let server = servers.next().await.unwrap();
    let server_id = server.server.server_id();
    let store = project.read_with(cx, |project, _| project.lsp_store());
    // Keep a startup waiter alive so shutdown cannot cancel initialization before it finishes.
    let definitions = store.update(cx, |store, cx| {
        store.definitions(&buffer, Default::default(), cx)
    });
    cx.run_until_parked();
    drop(foreground);
    cx.run_until_parked();
    release.send(()).unwrap();
    definitions.await.unwrap();
    cx.run_until_parked();
    assert!(project.read_with(cx, |project, cx| project.runtime_is_suspended(cx)));
    project.read_with(cx, |project, cx| {
        let store = project.lsp_store();
        let store = store.read(cx);
        assert!(store.language_server_for_id(server_id).is_none());
        assert!(store.language_server_statuses().next().is_none());
    });
}
