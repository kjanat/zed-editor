use std::{
    borrow::Cow,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use collections::{HashMap, HashSet};
use fs::{FakeFs, Fs};
use futures::{FutureExt, StreamExt};
use gpui::{Entity, TestAppContext, UpdateGlobal as _};
use language::{
    Buffer, Capability, CodeLabel, DiagnosticSourceKind, FakeLspAdapter, HighlightId, Language,
    LanguageConfig, LanguageMatcher, LocalFile, OffsetRangeExt as _, Point, PointUtf16, json_lang,
    rust_lang,
};
use lsp::{LanguageServerId, LanguageServerName, LanguageServerSelector, Uri};
use parking_lot::Mutex;
use project::{
    DiagnosticSummary, Event, Project, WorktreeId,
    lsp_store::{
        log_store::{TestRpcLogHeaderState, TestRpcRequestTracker},
        *,
    },
};
use rpc::proto;
use serde_json::json;
use settings::{ScanSymlinksSetting, SettingsStore};
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
async fn test_open_buffer_via_lsp_loads_deno_virtual_documents(cx: &mut TestAppContext) {
    init_test(cx);

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/root"), json!({ "main.ts": "stat();" }))
        .await;

    let project = Project::test(fs, [path!("/root").as_ref()], cx).await;
    let language_registry = project.read_with(cx, |project, _| project.languages().clone());
    language_registry.add(Arc::new(Language::new(
        LanguageConfig {
            name: "TypeScript".into(),
            matcher: (LanguageMatcher {
                path_suffixes: vec!["ts".into()],
                ..LanguageMatcher::default()
            })
            .into(),
            ..LanguageConfig::default()
        },
        None,
    )));
    let mut fake_servers =
        language_registry.register_fake_lsp("TypeScript", FakeLspAdapter::default());

    let (main_buffer, _main_handle) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/root/main.ts"), cx)
        })
        .await
        .unwrap();
    let fake_server = fake_servers.next().await.unwrap();
    cx.run_until_parked();

    let adapter_uri: Uri = "deno:/https/jsr.io/%40kjanat/dreamcli/4.1.0/src/runtime/adapter.ts"
        .parse()
        .unwrap();
    let requested_uris = Arc::new(Mutex::new(Vec::new()));
    fake_server.set_request_handler::<deno_ext::VirtualTextDocument, _, _>({
        let requested_uris = requested_uris.clone();
        let adapter_uri = adapter_uri.clone();
        move |params, _| {
            requested_uris.lock().push(params.text_document.uri.clone());
            let text = (params.text_document.uri == adapter_uri)
                .then(|| "export function stat() {}\r\n".to_string());
            async move { Ok(text) }
        }
    });
    let server_id = project.read_with(cx, |project, cx| {
        project
            .lsp_store()
            .read(cx)
            .language_server_statuses()
            .next()
            .unwrap()
            .0
    });

    let buffer = project
        .update(cx, |project, cx| {
            project.open_local_buffer_via_lsp(adapter_uri.clone(), server_id, cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();

    buffer.read_with(cx, |buffer, cx| {
        assert_eq!(buffer.text(), "export function stat() {}\n");
        assert_eq!(buffer.capability(), Capability::ReadOnly);
        assert_eq!(
            buffer.language().map(|language| language.name()),
            Some("TypeScript".into())
        );
        let file = buffer.file().unwrap();
        assert_eq!(file.file_name(cx), "adapter.ts");
        assert_eq!(
            file.full_path(cx),
            PathBuf::from("deno:/https/jsr.io/@kjanat/dreamcli/4.1.0/src/runtime/adapter.ts")
        );
        assert_eq!(
            deno_ext::VirtualDocumentFile::from_dyn(buffer.file()).map(|file| file.uri()),
            Some(&adapter_uri)
        );
    });
    project.read_with(cx, |project, cx| {
        assert_eq!(project.worktrees(cx).count(), 1);
    });

    let reopened = project
        .update(cx, |project, cx| {
            project.open_local_buffer_via_lsp(adapter_uri.clone(), server_id, cx)
        })
        .await
        .unwrap();
    assert_eq!(reopened, buffer);
    assert_eq!(*requested_uris.lock(), vec![adapter_uri.clone()]);

    let missing_uri: Uri = "deno:/asset/missing.d.ts".parse().unwrap();
    let error = project
        .update(cx, |project, cx| {
            project.open_local_buffer_via_lsp(missing_uri.clone(), server_id, cx)
        })
        .await
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("has no document for deno:/asset/missing.d.ts"),
        "unexpected error: {error:#}"
    );
    project.read_with(cx, |project, cx| {
        assert_eq!(project.worktrees(cx).count(), 1);
    });

    let capabilities = |cx: &mut TestAppContext| {
        [&main_buffer, &buffer].map(|buffer| buffer.read_with(cx, |buffer, _| buffer.capability()))
    };
    project.update(cx, |project, cx| {
        project.mark_as_collab_for_testing();
        project.set_role(proto::ChannelRole::Guest, cx);
    });
    assert_eq!(
        capabilities(cx),
        [Capability::ReadOnly, Capability::ReadOnly]
    );
    project.update(cx, |project, cx| {
        project.set_role(proto::ChannelRole::Member, cx)
    });
    assert_eq!(
        capabilities(cx),
        [Capability::ReadWrite, Capability::ReadOnly]
    );
}

#[gpui::test]
async fn test_deno_virtual_documents_are_opened_in_their_server(cx: &mut TestAppContext) {
    init_test(cx);

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/root"), json!({ "main.ts": "stat();" }))
        .await;

    let project = Project::test(fs, [path!("/root").as_ref()], cx).await;
    let language_registry = project.read_with(cx, |project, _| project.languages().clone());
    language_registry.add(Arc::new(Language::new(
        LanguageConfig {
            name: "TypeScript".into(),
            matcher: (LanguageMatcher {
                path_suffixes: vec!["ts".into()],
                ..LanguageMatcher::default()
            })
            .into(),
            ..LanguageConfig::default()
        },
        None,
    )));
    let mut fake_servers = language_registry.register_fake_lsp(
        "TypeScript",
        FakeLspAdapter {
            capabilities: lsp::ServerCapabilities {
                hover_provider: Some(lsp::HoverProviderCapability::Simple(true)),
                definition_provider: Some(lsp::OneOf::Left(true)),
                ..lsp::ServerCapabilities::default()
            },
            ..FakeLspAdapter::default()
        },
    );

    let (main_buffer, _main_handle) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/root/main.ts"), cx)
        })
        .await
        .unwrap();
    let mut fake_server = fake_servers.next().await.unwrap();
    let main_opened = fake_server
        .receive_notification::<lsp::notification::DidOpenTextDocument>()
        .await;
    assert_eq!(
        main_opened.text_document.uri,
        Uri::from_file_path(path!("/root/main.ts")).unwrap()
    );

    let adapter_uri: Uri = "deno:/https/jsr.io/%40kjanat/dreamcli/4.1.0/src/runtime/adapter.ts"
        .parse()
        .unwrap();
    let lib_uri: Uri = "deno:/asset/lib.deno.ns.d.ts".parse().unwrap();
    let script_uri: Uri = "deno:/asset/script.mjs".parse().unwrap();
    fake_server.set_request_handler::<deno_ext::VirtualTextDocument, _, _>({
        let adapter_uri = adapter_uri.clone();
        let lib_uri = lib_uri.clone();
        let script_uri = script_uri.clone();
        move |params, _| {
            let uri = params.text_document.uri;
            let text = if uri == adapter_uri {
                Some("export function stat() {}\n".to_string())
            } else if uri == lib_uri {
                Some("declare namespace Deno {}\n".to_string())
            } else if uri == script_uri {
                Some("export {};\n".to_string())
            } else {
                None
            };
            async move { Ok(text) }
        }
    });
    let server_id = project.read_with(cx, |project, cx| {
        project
            .lsp_store()
            .read(cx)
            .language_server_statuses()
            .next()
            .unwrap()
            .0
    });

    let adapter = project
        .update(cx, |project, cx| {
            project.open_local_buffer_via_lsp(adapter_uri.clone(), server_id, cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();

    let adapter_handle = project.update(cx, |project, cx| {
        project.register_buffer_with_language_servers(&adapter, cx)
    });
    let adapter_opened = fake_server
        .receive_notification::<lsp::notification::DidOpenTextDocument>()
        .await;
    assert_eq!(
        adapter_opened.text_document,
        lsp::TextDocumentItem::new(
            adapter_uri.clone(),
            "typescript".to_string(),
            0,
            "export function stat() {}\n".to_string(),
        )
    );

    fake_server.notify::<lsp::notification::PublishDiagnostics>(lsp::PublishDiagnosticsParams {
        uri: adapter_uri.clone(),
        version: None,
        diagnostics: vec![lsp::Diagnostic {
            range: lsp::Range::new(lsp::Position::new(0, 16), lsp::Position::new(0, 20)),
            severity: Some(lsp::DiagnosticSeverity::WARNING),
            message: lsp::DiagnosticMessage::from("stat is deprecated"),
            ..lsp::Diagnostic::default()
        }],
    });
    cx.run_until_parked();
    adapter.read_with(cx, |adapter, _| {
        assert_eq!(
            adapter
                .buffer_diagnostics(Some(server_id))
                .iter()
                .map(|entry| entry.diagnostic.message.to_string())
                .collect::<Vec<_>>(),
            ["stat is deprecated"]
        );
    });

    fake_server.set_request_handler::<lsp::request::HoverRequest, _, _>({
        let adapter_uri = adapter_uri.clone();
        move |params, _| {
            assert_eq!(
                params.text_document_position_params.text_document.uri,
                adapter_uri
            );
            async move {
                Ok(Some(lsp::Hover {
                    contents: lsp::HoverContents::Scalar(lsp::MarkedString::String(
                        "Returns file information".to_string(),
                    )),
                    range: None,
                }))
            }
        }
    });
    let hovers = project
        .update(cx, |project, cx| {
            project.hover(&adapter, PointUtf16::new(0, 17), cx)
        })
        .await
        .unwrap();
    assert_eq!(
        hovers
            .iter()
            .flat_map(|hover| hover.contents.iter().map(|block| block.text.as_str()))
            .collect::<Vec<_>>(),
        ["Returns file information"]
    );

    fake_server.set_request_handler::<lsp::request::GotoDefinition, _, _>({
        let adapter_uri = adapter_uri.clone();
        let lib_uri = lib_uri.clone();
        move |params, _| {
            assert_eq!(
                params.text_document_position_params.text_document.uri,
                adapter_uri
            );
            let lib_uri = lib_uri.clone();
            async move {
                Ok(Some(lsp::GotoDefinitionResponse::Scalar(
                    lsp::Location::new(
                        lib_uri,
                        lsp::Range::new(lsp::Position::new(0, 18), lsp::Position::new(0, 22)),
                    ),
                )))
            }
        }
    });
    let definitions = project
        .update(cx, |project, cx| {
            project.definitions(&adapter, PointUtf16::new(0, 17), cx)
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(definitions.len(), 1);
    let target = &definitions[0].target;
    target.buffer.read_with(cx, |lib, cx| {
        assert_eq!(lib.text(), "declare namespace Deno {}\n");
        assert_eq!(
            lib.file().unwrap().full_path(cx),
            PathBuf::from("deno:/asset/lib.deno.ns.d.ts")
        );
        assert_eq!(
            target.range.to_point(lib),
            Point::new(0, 18)..Point::new(0, 22)
        );
    });

    let script = project
        .update(cx, |project, cx| {
            project.open_local_buffer_via_lsp(script_uri.clone(), server_id, cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();
    script.read_with(cx, |script, _| assert!(script.language().is_none()));
    let _script_handle = project.update(cx, |project, cx| {
        project.register_buffer_with_language_servers(&script, cx)
    });
    let javascript = Arc::new(Language::new(
        LanguageConfig {
            name: "JavaScript".into(),
            matcher: (LanguageMatcher {
                path_suffixes: vec!["mjs".into()],
                ..LanguageMatcher::default()
            })
            .into(),
            ..LanguageConfig::default()
        },
        None,
    ));
    language_registry.add(javascript.clone());
    let script_opened = fake_server
        .receive_notification::<lsp::notification::DidOpenTextDocument>()
        .await;
    assert_eq!(
        script_opened.text_document,
        lsp::TextDocumentItem::new(
            script_uri.clone(),
            "javascript".to_string(),
            0,
            "export {};\n".to_string(),
        )
    );

    project.update(cx, |project, cx| {
        project.restart_language_servers_for_buffers(
            vec![main_buffer.clone()],
            HashSet::default(),
            true,
            cx,
        )
    });
    let mut restarted_server = fake_servers.next().await.unwrap();
    let mut reopened_uris = Vec::new();
    while !reopened_uris.contains(&adapter_uri) {
        reopened_uris.push(
            restarted_server
                .receive_notification::<lsp::notification::DidOpenTextDocument>()
                .await
                .text_document
                .uri,
        );
    }

    project.update(cx, |project, cx| {
        project.set_language_for_buffer(&adapter, javascript, cx)
    });
    let retyped_closed = restarted_server
        .receive_notification::<lsp::notification::DidCloseTextDocument>()
        .await;
    assert_eq!(retyped_closed.text_document.uri, adapter_uri);
    let retyped_opened = loop {
        let opened = restarted_server
            .receive_notification::<lsp::notification::DidOpenTextDocument>()
            .await;
        if opened.text_document.uri == adapter_uri {
            break opened;
        }
    };
    assert_eq!(retyped_opened.text_document.language_id, "javascript");

    cx.update(|cx| {
        SettingsStore::update_global(cx, |settings, cx| {
            settings.update_user_settings(cx, |settings| {
                settings.project.lsp.0.insert(
                    "the-fake-language-server".into(),
                    settings::LspSettings {
                        initialization_options: Some(json!({ "reconfigured": true })),
                        ..Default::default()
                    },
                );
            });
        })
    });
    let mut reconfigured_server = fake_servers.next().await.unwrap();
    loop {
        let opened = reconfigured_server
            .receive_notification::<lsp::notification::DidOpenTextDocument>()
            .await;
        if opened.text_document.uri == adapter_uri {
            break;
        }
    }

    cx.update(|_| drop(adapter_handle));
    let adapter_closed = reconfigured_server
        .receive_notification::<lsp::notification::DidCloseTextDocument>()
        .await;
    assert_eq!(adapter_closed.text_document.uri, adapter_uri);
}

#[gpui::test]
async fn test_deno_virtual_documents_are_scoped_to_their_server(cx: &mut TestAppContext) {
    init_test(cx);

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/one"), json!({ "main.ts": "stat();" }))
        .await;
    fs.insert_tree(path!("/two"), json!({ "main.ts": "stat();" }))
        .await;

    let project = Project::test(fs, [path!("/one").as_ref(), path!("/two").as_ref()], cx).await;
    let language_registry = project.read_with(cx, |project, _| project.languages().clone());
    language_registry.add(Arc::new(Language::new(
        LanguageConfig {
            name: "TypeScript".into(),
            matcher: (LanguageMatcher {
                path_suffixes: vec!["ts".into()],
                ..LanguageMatcher::default()
            })
            .into(),
            ..LanguageConfig::default()
        },
        None,
    )));
    let mut fake_servers =
        language_registry.register_fake_lsp("TypeScript", FakeLspAdapter::default());

    let adapter_uri: Uri = "deno:/https/jsr.io/%40kjanat/dreamcli/4.1.0/src/runtime/adapter.ts"
        .parse()
        .unwrap();
    let mut handles = Vec::new();
    let mut server_ids = Vec::new();
    for (root, text) in [
        (path!("/one/main.ts"), "export const project = 1;\n"),
        (path!("/two/main.ts"), "export const project = 2;\n"),
    ] {
        handles.push(
            project
                .update(cx, |project, cx| {
                    project.open_local_buffer_with_lsp(root, cx)
                })
                .await
                .unwrap(),
        );
        let fake_server = fake_servers.next().await.unwrap();
        fake_server.set_request_handler::<deno_ext::VirtualTextDocument, _, _>(
            move |_, _| async move { Ok(Some(text.to_string())) },
        );
        server_ids.push(fake_server.server.server_id());
    }
    cx.run_until_parked();

    let mut buffers = Vec::new();
    for server_id in &server_ids {
        buffers.push(
            project
                .update(cx, |project, cx| {
                    project.open_local_buffer_via_lsp(adapter_uri.clone(), *server_id, cx)
                })
                .await
                .unwrap(),
        );
    }
    assert_ne!(buffers[0], buffers[1]);
    assert_eq!(
        buffers
            .iter()
            .map(|buffer| buffer.read_with(cx, |buffer, _| buffer.text()))
            .collect::<Vec<_>>(),
        ["export const project = 1;\n", "export const project = 2;\n"]
    );

    let reopened = project
        .update(cx, |project, cx| {
            project.open_local_buffer_via_lsp(adapter_uri.clone(), server_ids[0], cx)
        })
        .await
        .unwrap();
    assert_eq!(reopened, buffers[0]);
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
async fn test_open_buffer_via_lsp_maps_external_symlink_target(cx: &mut TestAppContext) {
    init_test(cx);
    cx.executor().allow_parking();

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/shared"),
        json!({ "pkg": { "def.rs": "pub fn def() {}", "other.rs": "pub fn other() {}" } }),
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

    project
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

    let buffer = project
        .update(cx, |project, cx| {
            project.open_local_buffer_via_lsp(
                Uri::from_file_path(path!("/shared/pkg/other.rs")).unwrap(),
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
            "pkg/other.rs".to_string(),
            PathBuf::from(path!("/project/pkg/other.rs"))
        )
    );
    assert_eq!(
        worktree_roots(&project, cx),
        vec![PathBuf::from(path!("/project"))]
    );
}

#[gpui::test]
async fn test_open_buffer_via_lsp_maps_external_symlink_target_with_scan_symlinks_always(
    cx: &mut TestAppContext,
) {
    init_test(cx);
    cx.executor().allow_parking();
    cx.update(|cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |settings| {
                settings.project.worktree.scan_symlinks = Some(ScanSymlinksSetting::Always);
            });
        });
    });

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/shared"),
        json!({ "pkg": { "other.rs": "pub fn other() {}" } }),
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
                Uri::from_file_path(path!("/shared/pkg/other.rs")).unwrap(),
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
            "pkg/other.rs".to_string(),
            PathBuf::from(path!("/project/pkg/other.rs"))
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
async fn test_document_highlights_do_not_resume_a_suspended_project(cx: &mut TestAppContext) {
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
                document_highlight_provider: Some(lsp::OneOf::Left(true)),
                hover_provider: Some(lsp::HoverProviderCapability::Simple(true)),
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
    servers.next().await.unwrap();
    drop(foreground);
    cx.run_until_parked();
    assert!(project.read_with(cx, |project, cx| project.runtime_is_suspended(cx)));

    let highlights = project
        .update(cx, |project, cx| {
            project.document_highlights(&buffer, 3, cx)
        })
        .await
        .unwrap();
    assert!(highlights.is_empty());
    cx.run_until_parked();
    assert!(servers.next().now_or_never().is_none());
    assert!(project.read_with(cx, |project, cx| project.runtime_is_suspended(cx)));

    let _hover = project.update(cx, |project, cx| project.hover(&buffer, 3, cx));
    servers.next().await.unwrap();
}

fn inactive_language_servers(
    store: &Entity<LspStore>,
    cx: &TestAppContext,
) -> Vec<(LanguageServerName, InactiveLanguageServerState)> {
    store.read_with(cx, |store, _| {
        let mut inactive = store
            .inactive_language_servers()
            .map(|(_, name, server)| (name.clone(), server.state.clone()))
            .collect::<Vec<_>>();
        inactive.sort_by(|(left, _), (right, _)| left.0.cmp(&right.0));
        inactive
    })
}

#[gpui::test]
async fn test_failed_language_server_is_recorded_until_it_starts(cx: &mut TestAppContext) {
    use std::sync::atomic::{AtomicUsize, Ordering};

    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/runtime"), json!({ "main.rs": "fn main() {}" }))
        .await;
    let project = Project::test(fs, [path!("/runtime").as_ref()], cx).await;
    let languages = project.read_with(cx, |project, _| project.languages().clone());
    languages.add(rust_lang());
    let starts = Arc::new(AtomicUsize::new(0));
    let mut servers = languages.register_fake_lsp(
        "Rust",
        FakeLspAdapter {
            name: "flaky-server",
            initializer: Some(Box::new({
                let starts = starts.clone();
                move |server| {
                    if starts.fetch_add(1, Ordering::SeqCst) == 0 {
                        server.set_request_handler::<lsp::request::Initialize, _, _>(
                            |_, _| async move { anyhow::bail!("server unavailable") },
                        );
                    }
                }
            })),
            ..Default::default()
        },
    );
    let (buffer, _handle) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/runtime/main.rs"), cx)
        })
        .await
        .unwrap();
    servers.next().await.unwrap();
    cx.run_until_parked();

    let store = project.read_with(cx, |project, _| project.lsp_store());
    let inactive = inactive_language_servers(&store, cx);
    assert_eq!(inactive.len(), 1);
    assert_eq!(
        inactive[0].0,
        LanguageServerName::new_static("flaky-server")
    );
    assert!(
        matches!(
            &inactive[0].1,
            InactiveLanguageServerState::Failed { error } if !error.is_empty()
        ),
        "the failed start should be recorded with its error, got {:?}",
        inactive[0].1
    );

    store.update(cx, |store, cx| {
        store.restart_language_servers_for_buffers(
            vec![buffer.clone()],
            HashSet::default(),
            true,
            cx,
        )
    });
    servers.next().await.unwrap();
    cx.run_until_parked();
    assert_eq!(inactive_language_servers(&store, cx), Vec::new());
}

#[gpui::test]
async fn test_starting_one_server_after_stop_all_leaves_the_others_stopped(
    cx: &mut TestAppContext,
) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/runtime"),
        json!({ "main.rs": "fn main() {}", "config.json": "{}" }),
    )
    .await;
    let project = Project::test(fs, [path!("/runtime").as_ref()], cx).await;
    let languages = project.read_with(cx, |project, _| project.languages().clone());
    languages.add(rust_lang());
    languages.add(json_lang());
    let mut json_servers = languages.register_fake_lsp(
        "JSON",
        FakeLspAdapter {
            name: "json-server",
            ..Default::default()
        },
    );
    let mut first_servers = languages.register_fake_lsp(
        "Rust",
        FakeLspAdapter {
            name: "first-server",
            ..Default::default()
        },
    );
    let mut second_servers = languages.register_fake_lsp(
        "Rust",
        FakeLspAdapter {
            name: "second-server",
            ..Default::default()
        },
    );
    let (_buffer, _handle) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/runtime/main.rs"), cx)
        })
        .await
        .unwrap();
    first_servers.next().await.unwrap();
    second_servers.next().await.unwrap();
    cx.run_until_parked();

    let store = project.read_with(cx, |project, _| project.lsp_store());
    store.update(cx, |store, cx| store.stop_all_language_servers(cx));
    cx.run_until_parked();
    assert_eq!(
        inactive_language_servers(&store, cx),
        [
            (
                LanguageServerName::new_static("first-server"),
                InactiveLanguageServerState::Stopped
            ),
            (
                LanguageServerName::new_static("second-server"),
                InactiveLanguageServerState::Stopped
            ),
        ]
    );

    store.update(cx, |store, cx| {
        store.restart_language_servers_for_buffers(
            Vec::new(),
            HashSet::from_iter([LanguageServerSelector::Name(
                LanguageServerName::new_static("second-server"),
            )]),
            true,
            cx,
        )
    });
    second_servers.next().await.unwrap();
    cx.run_until_parked();
    assert!(first_servers.next().now_or_never().is_none());
    assert_eq!(
        inactive_language_servers(&store, cx),
        [(
            LanguageServerName::new_static("first-server"),
            InactiveLanguageServerState::Stopped
        )]
    );

    let (_json_buffer, _json_handle) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/runtime/config.json"), cx)
        })
        .await
        .unwrap();
    json_servers.next().await.unwrap();
    cx.run_until_parked();
    assert!(first_servers.next().now_or_never().is_none());
}

#[gpui::test]
async fn test_stopped_server_can_start_only_with_an_open_file_it_handles(cx: &mut TestAppContext) {
    use std::{cell::Cell, rc::Rc};

    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/runtime"),
        json!({ "main.rs": "fn main() {}", "config.json": "{}" }),
    )
    .await;
    let project = Project::test(fs, [path!("/runtime").as_ref()], cx).await;
    let languages = project.read_with(cx, |project, _| project.languages().clone());
    languages.add(rust_lang());
    languages.add(json_lang());
    let _json_servers = languages.register_fake_lsp(
        "JSON",
        FakeLspAdapter {
            name: "json-server",
            ..Default::default()
        },
    );
    let mut rust_servers = languages.register_fake_lsp(
        "Rust",
        FakeLspAdapter {
            name: "rust-server",
            ..Default::default()
        },
    );
    let (rust_buffer, rust_handle) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/runtime/main.rs"), cx)
        })
        .await
        .unwrap();
    rust_servers.next().await.unwrap();
    cx.run_until_parked();

    let store = project.read_with(cx, |project, _| project.lsp_store());
    let changes = Rc::new(Cell::new(0));
    let _subscription = cx.update(|cx| {
        let changes = changes.clone();
        cx.subscribe(&store, move |_, event, _| {
            if matches!(event, LspStoreEvent::InactiveLanguageServersChanged) {
                changes.set(changes.get() + 1);
            }
        })
    });
    store.update(cx, |store, cx| store.stop_all_language_servers(cx));
    cx.run_until_parked();
    assert_eq!(changes.get(), 1);

    let worktree_id =
        rust_buffer.read_with(cx, |buffer, cx| buffer.file().unwrap().worktree_id(cx));
    let rust_server = LanguageServerName::new_static("rust-server");
    let json_server = LanguageServerName::new_static("json-server");
    let can_start = |worktree_id, name: &LanguageServerName, cx: &TestAppContext| {
        store.read_with(cx, |store, cx| {
            store.has_open_buffer_for_language_server(worktree_id, name, cx)
        })
    };
    assert!(can_start(Some(worktree_id), &rust_server, cx));
    assert!(can_start(None, &rust_server, cx));
    assert!(!can_start(
        Some(WorktreeId::from_proto(999)),
        &rust_server,
        cx
    ));
    assert!(!can_start(Some(worktree_id), &json_server, cx));

    let _json_buffer = project
        .update(cx, |project, cx| {
            project.open_local_buffer(path!("/runtime/config.json"), cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();
    assert!(!can_start(Some(worktree_id), &json_server, cx));

    cx.update(|_| drop(rust_handle));
    cx.run_until_parked();
    assert!(!can_start(Some(worktree_id), &rust_server, cx));
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
