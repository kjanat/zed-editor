use crate::{
    Event, ExtensionIndex, ExtensionIndexEntry, ExtensionIndexLanguageEntry,
    ExtensionIndexThemeEntry, ExtensionManifest, ExtensionStore, GrammarManifestEntry,
    MAX_REMOTE_SYNC_ATTEMPTS, MAX_REMOTE_SYNC_RETRY_DELAY, RELOAD_DEBOUNCE_DURATION,
    REMOTE_SYNC_TIMEOUT, SchemaVersion,
    headless_host::{
        ExtensionVersion, HeadlessExtensionStore, LoadedExtension, STALE_UPLOAD_TTL,
        hash_directory_contents, remove_stale_uploads,
    },
    load_plugin_queries, remote_sync_retry_delay,
    wasm_host::WasmExtension,
};
use anyhow::Context as _;
use async_compression::futures::bufread::GzipEncoder;
use async_trait::async_trait;
use client::{AnyProtoClient, TypedEnvelope, proto};
use collections::{BTreeMap, HashMap, HashSet};
use extension::{
    BuildTaskTemplate, CodeLabel, Command, Completion, ContextServerConfiguration,
    DebugAdapterBinary, DebugRequest, DebugScenario, DebugTaskDefinition, Extension,
    ExtensionHostProxy, KeyValueStoreDelegate, LibManifestEntry, ProjectDelegate, SlashCommand,
    SlashCommandArgumentCompletion, SlashCommandOutput, StartDebuggingRequestArgumentsRequest,
    Symbol, WorktreeDelegate,
};
use fs::{FakeFs, Fs, RealFs, RemoveOptions};
use futures::{AsyncReadExt, FutureExt, StreamExt, io::BufReader};
use gpui::{
    AppContext as _, BackgroundExecutor, BorrowAppContext as _, Entity, EntityId, TaskExt,
    TestAppContext,
};
use http_client::{FakeHttpClient, HttpClientWithUrl, Response};
use language::{
    BinaryStatus, LanguageConfig, LanguageMatcher, LanguageName, LanguageRegistry, QueryFiles,
};
use language_extension::LspAccess;
use lsp::LanguageServerName;
use node_runtime::NodeRuntime;
use parking_lot::Mutex;
use project::{DEFAULT_COMPLETION_CONTEXT, Project};
use release_channel::AppVersion;
use remote::{ConnectionState, RemoteClient, RemoteClientEvent, RemoteConnectionOptions};
use reqwest_client::ReqwestClient;
use serde_json::json;
use settings::SettingsStore;
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime},
};
use task::{SpawnInTerminal, ZedDebugConfig};
use theme::ThemeRegistry;
use util::{
    path,
    paths::SanitizedPath,
    rel_path::{RelPath, rel_path_buf},
    test::TempTree,
};

#[cfg(test)]
#[ctor::ctor(unsafe)]
fn init_logger() {
    zlog::init_test();
}

#[gpui::test]
async fn test_load_plugin_queries(executor: BackgroundExecutor) {
    let fs = FakeFs::new(executor);
    fs.insert_tree(
        "/queries",
        json!({
            "highlights.scm": "highlight query",
            "outline.scm": "outline query",
            "highlights_extra.scm": "ignored query",
            "locals.scm": "unrelated query",
        }),
    )
    .await;

    let queries = load_plugin_queries(
        fs.clone(),
        Path::new("/queries"),
        Some(QueryFiles::HIGHLIGHTS),
    )
    .await;
    assert_eq!(queries.highlights.as_deref(), Some("highlight query"));
    assert!(queries.outline.is_none());
    assert!(queries.brackets.is_none());

    let queries = load_plugin_queries(fs, Path::new("/queries"), None).await;
    assert_eq!(queries.highlights.as_deref(), Some("highlight query"));
    assert_eq!(queries.outline.as_deref(), Some("outline query"));
    assert!(queries.brackets.is_none());
}

fn remote_sync_entry(id: &str, manifest_body: &str) -> ExtensionIndexEntry {
    let manifest = format!(
        r#"
        id = "{id}"
        name = "{id}"
        version = "1.0.0"
        schema_version = 0

        {manifest_body}
        "#
    );

    ExtensionIndexEntry {
        manifest: Arc::new(toml::from_str(&manifest).unwrap()),
        dev: false,
        source: None,
    }
}

fn remote_sync_language_entry(extension: &str, path: &str) -> ExtensionIndexLanguageEntry {
    ExtensionIndexLanguageEntry {
        extension: extension.into(),
        path: path.into(),
        matcher: LanguageMatcher::default().into(),
        hidden: false,
        grammar: None,
        query_files: None,
    }
}

fn remote_sync_extension_ids(index: &ExtensionIndex) -> Vec<String> {
    let mut extensions = index
        .extensions_to_sync_to_remote()
        .into_entries()
        .map(|(id, _)| id.to_string())
        .collect::<Vec<_>>();

    extensions.sort();

    extensions
}

#[test]
fn remote_sync_includes_language_dependencies() {
    let index = ExtensionIndex {
        extensions: [
            (
                "bar-language".into(),
                remote_sync_entry("bar-language", r#"languages = ["languages/bar"]"#),
            ),
            (
                "foo-lsp".into(),
                remote_sync_entry(
                    "foo-lsp",
                    r#"
                    [language_servers.foo]
                    language = "Foo"
                    "#,
                ),
            ),
            (
                "foo-language".into(),
                remote_sync_entry("foo-language", r#"languages = ["languages/foo"]"#),
            ),
        ]
        .into_iter()
        .collect(),
        languages: [
            (
                "Bar".into(),
                remote_sync_language_entry("bar-language", "languages/bar"),
            ),
            (
                "Foo".into(),
                remote_sync_language_entry("foo-language", "languages/foo"),
            ),
        ]
        .into_iter()
        .collect(),
        themes: BTreeMap::default(),
        icon_themes: BTreeMap::default(),
        language_providers: BTreeMap::default(),
    };

    assert_eq!(
        remote_sync_extension_ids(&index),
        ["foo-language", "foo-lsp"]
    );
}

#[test]
fn remote_sync_keeps_shared_language_dependency_once() {
    let index = ExtensionIndex {
        extensions: [
            (
                "aaa-lsp".into(),
                remote_sync_entry(
                    "aaa-lsp",
                    r#"
                    [language_servers.aaa]
                    language = "Foo"
                    "#,
                ),
            ),
            (
                "bbb-lsp".into(),
                remote_sync_entry(
                    "bbb-lsp",
                    r#"
                    [language_servers.bbb]
                    language = "Foo"
                    "#,
                ),
            ),
            (
                "zzz-language".into(),
                remote_sync_entry("zzz-language", r#"languages = ["languages/foo"]"#),
            ),
        ]
        .into_iter()
        .collect(),
        languages: [(
            "Foo".into(),
            remote_sync_language_entry("zzz-language", "languages/foo"),
        )]
        .into_iter()
        .collect(),
        themes: BTreeMap::default(),
        icon_themes: BTreeMap::default(),
        language_providers: BTreeMap::default(),
    };

    assert_eq!(
        remote_sync_extension_ids(&index),
        ["aaa-lsp", "bbb-lsp", "zzz-language"]
    );
}

#[test]
fn remote_sync_keeps_remote_loadable_extensions_without_language_dependency() {
    let index = ExtensionIndex {
        extensions: [(
            "foo".into(),
            remote_sync_entry(
                "foo",
                r#"
                [language_servers.foo]
                language = "Foo"
                "#,
            ),
        )]
        .into_iter()
        .collect(),
        languages: BTreeMap::default(),
        themes: BTreeMap::default(),
        icon_themes: BTreeMap::default(),
        language_providers: BTreeMap::default(),
    };

    assert_eq!(remote_sync_extension_ids(&index), ["foo"]);
}

#[test]
fn remote_sync_keeps_debug_adapters() {
    let index = ExtensionIndex {
        extensions: [(
            "foo".into(),
            remote_sync_entry(
                "foo",
                r#"
                [debug_adapters.foo]
                "#,
            ),
        )]
        .into_iter()
        .collect(),
        languages: BTreeMap::default(),
        themes: BTreeMap::default(),
        icon_themes: BTreeMap::default(),
        language_providers: BTreeMap::default(),
    };

    assert_eq!(remote_sync_extension_ids(&index), ["foo"]);
}

#[gpui::test]
async fn test_extension_store(cx: &mut TestAppContext) {
    init_test(cx);

    let fs = FakeFs::new(cx.executor());
    let http_client = FakeHttpClient::with_200_response();

    fs.insert_tree(
        "/the-extension-dir",
        json!({
            "installed": {
                "zed-monokai": {
                    "extension.json": r#"{
                        "id": "zed-monokai",
                        "name": "Zed Monokai",
                        "version": "2.0.0",
                        "themes": {
                            "Monokai Dark": "themes/monokai.json",
                            "Monokai Light": "themes/monokai.json",
                            "Monokai Pro Dark": "themes/monokai-pro.json",
                            "Monokai Pro Light": "themes/monokai-pro.json"
                        }
                    }"#,
                    "themes": {
                        "monokai.json": r#"{
                            "name": "Monokai",
                            "author": "Someone",
                            "themes": [
                                {
                                    "name": "Monokai Dark",
                                    "appearance": "dark",
                                    "style": {}
                                },
                                {
                                    "name": "Monokai Light",
                                    "appearance": "light",
                                    "style": {}
                                }
                            ]
                        }"#,
                        "monokai-pro.json": r#"{
                            "name": "Monokai Pro",
                            "author": "Someone",
                            "themes": [
                                {
                                    "name": "Monokai Pro Dark",
                                    "appearance": "dark",
                                    "style": {}
                                },
                                {
                                    "name": "Monokai Pro Light",
                                    "appearance": "light",
                                    "style": {}
                                }
                            ]
                        }"#,
                    }
                },
                "zed-ruby": {
                    "extension.json": r#"{
                        "id": "zed-ruby",
                        "name": "Zed Ruby",
                        "version": "1.0.0",
                        "grammars": {
                            "ruby": "grammars/ruby.wasm",
                            "embedded_template": "grammars/embedded_template.wasm"
                        },
                        "languages": {
                            "ruby": "languages/ruby",
                            "erb": "languages/erb"
                        }
                    }"#,
                    "grammars": {
                        "ruby.wasm": "",
                        "embedded_template.wasm": "",
                    },
                    "languages": {
                        "ruby": {
                            "config.toml": r#"
                                name = "Ruby"
                                grammar = "ruby"
                                path_suffixes = ["rb"]
                            "#,
                            "highlights.scm": "",
                            "outline.scm": "",
                        },
                        "erb": {
                            "config.toml": r#"
                                name = "ERB"
                                grammar = "embedded_template"
                                path_suffixes = ["erb"]
                            "#,
                            "highlights.scm": "",
                        }
                    },
                }
            }
        }),
    )
    .await;

    let mut expected_index = ExtensionIndex {
        extensions: [
            (
                "zed-ruby".into(),
                ExtensionIndexEntry {
                    manifest: Arc::new(ExtensionManifest {
                        id: "zed-ruby".into(),
                        name: "Zed Ruby".into(),
                        version: "1.0.0".into(),
                        schema_version: SchemaVersion::ZERO,
                        description: None,
                        authors: Vec::new(),
                        repository: None,
                        themes: Default::default(),
                        icon_themes: Vec::new(),
                        lib: Default::default(),
                        languages: vec![
                            rel_path_buf("languages/erb"),
                            rel_path_buf("languages/ruby"),
                        ],
                        grammars: [
                            ("embedded_template".into(), GrammarManifestEntry::default()),
                            ("ruby".into(), GrammarManifestEntry::default()),
                        ]
                        .into_iter()
                        .collect(),
                        language_servers: BTreeMap::default(),
                        context_servers: BTreeMap::default(),
                        slash_commands: BTreeMap::default(),
                        snippets: None,
                        capabilities: Vec::new(),
                        debug_adapters: Default::default(),
                        debug_locators: Default::default(),
                        language_model_providers: BTreeMap::default(),
                    }),
                    dev: false,
                    source: None,
                },
            ),
            (
                "zed-monokai".into(),
                ExtensionIndexEntry {
                    manifest: Arc::new(ExtensionManifest {
                        id: "zed-monokai".into(),
                        name: "Zed Monokai".into(),
                        version: "2.0.0".into(),
                        schema_version: SchemaVersion::ZERO,
                        description: None,
                        authors: vec![],
                        repository: None,
                        themes: vec![
                            rel_path_buf("themes/monokai-pro.json"),
                            rel_path_buf("themes/monokai.json"),
                        ],
                        icon_themes: Vec::new(),
                        lib: Default::default(),
                        languages: Default::default(),
                        grammars: BTreeMap::default(),
                        language_servers: BTreeMap::default(),
                        context_servers: BTreeMap::default(),
                        slash_commands: BTreeMap::default(),
                        snippets: None,
                        capabilities: Vec::new(),
                        debug_adapters: Default::default(),
                        debug_locators: Default::default(),
                        language_model_providers: BTreeMap::default(),
                    }),
                    dev: false,
                    source: None,
                },
            ),
        ]
        .into_iter()
        .collect(),
        languages: [
            (
                "ERB".into(),
                ExtensionIndexLanguageEntry {
                    extension: "zed-ruby".into(),
                    path: "languages/erb".into(),
                    grammar: Some("embedded_template".into()),
                    hidden: false,
                    matcher: (LanguageMatcher {
                        path_suffixes: vec!["erb".into()],
                        first_line_pattern: None,
                        ..LanguageMatcher::default()
                    })
                    .into(),
                    query_files: Some(QueryFiles::HIGHLIGHTS),
                },
            ),
            (
                "Ruby".into(),
                ExtensionIndexLanguageEntry {
                    extension: "zed-ruby".into(),
                    path: "languages/ruby".into(),
                    grammar: Some("ruby".into()),
                    hidden: false,
                    matcher: (LanguageMatcher {
                        path_suffixes: vec!["rb".into()],
                        first_line_pattern: None,
                        ..LanguageMatcher::default()
                    })
                    .into(),
                    query_files: Some(QueryFiles::HIGHLIGHTS | QueryFiles::OUTLINE),
                },
            ),
        ]
        .into_iter()
        .collect(),
        themes: [
            (
                "Monokai Dark".into(),
                ExtensionIndexThemeEntry {
                    extension: "zed-monokai".into(),
                    path: "themes/monokai.json".into(),
                },
            ),
            (
                "Monokai Light".into(),
                ExtensionIndexThemeEntry {
                    extension: "zed-monokai".into(),
                    path: "themes/monokai.json".into(),
                },
            ),
            (
                "Monokai Pro Dark".into(),
                ExtensionIndexThemeEntry {
                    extension: "zed-monokai".into(),
                    path: "themes/monokai-pro.json".into(),
                },
            ),
            (
                "Monokai Pro Light".into(),
                ExtensionIndexThemeEntry {
                    extension: "zed-monokai".into(),
                    path: "themes/monokai-pro.json".into(),
                },
            ),
        ]
        .into_iter()
        .collect(),
        icon_themes: BTreeMap::default(),
        language_providers: BTreeMap::default(),
    };

    let proxy = Arc::new(ExtensionHostProxy::new());
    let theme_registry = Arc::new(ThemeRegistry::new(Box::new(())));
    theme_extension::init(proxy.clone(), theme_registry.clone(), cx.executor());
    let language_registry = Arc::new(LanguageRegistry::test(cx.executor()));
    language_extension::init(LspAccess::Noop, proxy.clone(), language_registry.clone());
    let node_runtime = NodeRuntime::unavailable();

    let store = cx.new(|cx| {
        ExtensionStore::new(
            PathBuf::from("/the-extension-dir"),
            None,
            proxy.clone(),
            fs.clone(),
            http_client.clone(),
            http_client.clone(),
            None,
            node_runtime.clone(),
            cx,
        )
    });

    cx.executor().advance_clock(RELOAD_DEBOUNCE_DURATION);
    store.read_with(cx, |store, _| {
        let index = &store.extension_index;
        assert_eq!(index.extensions, expected_index.extensions);

        for ((actual_key, actual_language), (expected_key, expected_language)) in
            index.languages.iter().zip(expected_index.languages.iter())
        {
            assert_eq!(actual_key, expected_key);
            assert_eq!(actual_language.grammar, expected_language.grammar);
            assert_eq!(actual_language.matcher, expected_language.matcher);
            assert_eq!(actual_language.hidden, expected_language.hidden);
        }
        assert_eq!(index.themes, expected_index.themes);

        assert_eq!(
            language_registry.language_names(),
            [
                LanguageName::new_static("ERB"),
                LanguageName::new_static("Plain Text"),
                LanguageName::new_static("Ruby"),
            ]
        );
        assert_eq!(
            theme_registry.list_names(),
            [
                "Monokai Dark",
                "Monokai Light",
                "Monokai Pro Dark",
                "Monokai Pro Light",
                "One Dark",
            ]
        );
    });

    fs.insert_tree(
        "/the-extension-dir/installed/zed-gruvbox",
        json!({
            "extension.json": r#"{
                "id": "zed-gruvbox",
                "name": "Zed Gruvbox",
                "version": "1.0.0",
                "themes": {
                    "Gruvbox": "themes/gruvbox.json"
                }
            }"#,
            "themes": {
                "gruvbox.json": r#"{
                    "name": "Gruvbox",
                    "author": "Someone Else",
                    "themes": [
                        {
                            "name": "Gruvbox",
                            "appearance": "dark",
                            "style": {}
                        }
                    ]
                }"#,
            }
        }),
    )
    .await;

    expected_index.extensions.insert(
        "zed-gruvbox".into(),
        ExtensionIndexEntry {
            manifest: Arc::new(ExtensionManifest {
                id: "zed-gruvbox".into(),
                name: "Zed Gruvbox".into(),
                version: "1.0.0".into(),
                schema_version: SchemaVersion::ZERO,
                description: None,
                authors: vec![],
                repository: None,
                themes: vec![rel_path_buf("themes/gruvbox.json")],
                icon_themes: Vec::new(),
                lib: Default::default(),
                languages: Default::default(),
                grammars: BTreeMap::default(),
                language_servers: BTreeMap::default(),
                context_servers: BTreeMap::default(),
                slash_commands: BTreeMap::default(),
                snippets: None,
                capabilities: Vec::new(),
                debug_adapters: Default::default(),
                debug_locators: Default::default(),
                language_model_providers: BTreeMap::default(),
            }),
            dev: false,
            source: None,
        },
    );
    expected_index.themes.insert(
        "Gruvbox".into(),
        ExtensionIndexThemeEntry {
            extension: "zed-gruvbox".into(),
            path: "themes/gruvbox.json".into(),
        },
    );

    #[allow(clippy::let_underscore_future)]
    let _ = store.update(cx, |store, cx| store.reload(None, cx));

    cx.executor().advance_clock(RELOAD_DEBOUNCE_DURATION);
    store.read_with(cx, |store, _| {
        let index = &store.extension_index;

        for ((actual_key, actual_language), (expected_key, expected_language)) in
            index.languages.iter().zip(expected_index.languages.iter())
        {
            assert_eq!(actual_key, expected_key);
            assert_eq!(actual_language.grammar, expected_language.grammar);
            assert_eq!(actual_language.matcher, expected_language.matcher);
            assert_eq!(actual_language.hidden, expected_language.hidden);
        }

        assert_eq!(index.extensions, expected_index.extensions);
        assert_eq!(index.themes, expected_index.themes);

        assert_eq!(
            theme_registry.list_names(),
            [
                "Gruvbox",
                "Monokai Dark",
                "Monokai Light",
                "Monokai Pro Dark",
                "Monokai Pro Light",
                "One Dark",
            ]
        );
    });

    let prev_fs_metadata_call_count = fs.metadata_call_count();
    let prev_fs_read_dir_call_count = fs.read_dir_call_count();

    // Create new extension store, as if Zed were restarting.
    drop(store);
    let store = cx.new(|cx| {
        ExtensionStore::new(
            PathBuf::from("/the-extension-dir"),
            None,
            proxy,
            fs.clone(),
            http_client.clone(),
            http_client.clone(),
            None,
            node_runtime.clone(),
            cx,
        )
    });

    cx.executor().run_until_parked();
    store.read_with(cx, |store, _| {
        assert_eq!(store.extension_index.extensions, expected_index.extensions);
        assert_eq!(store.extension_index.themes, expected_index.themes);
        assert_eq!(
            store.extension_index.icon_themes,
            expected_index.icon_themes
        );

        for ((actual_key, actual_language), (expected_key, expected_language)) in store
            .extension_index
            .languages
            .iter()
            .zip(expected_index.languages.iter())
        {
            assert_eq!(actual_key, expected_key);
            assert_eq!(actual_language.grammar, expected_language.grammar);
            assert_eq!(actual_language.matcher, expected_language.matcher);
            assert_eq!(actual_language.hidden, expected_language.hidden);
        }

        assert_eq!(
            language_registry.language_names(),
            [
                LanguageName::new_static("ERB"),
                LanguageName::new_static("Plain Text"),
                LanguageName::new_static("Ruby"),
            ]
        );
        assert_eq!(
            language_registry.grammar_names(),
            ["embedded_template".into(), "ruby".into()]
        );
        assert_eq!(
            theme_registry.list_names(),
            [
                "Gruvbox",
                "Monokai Dark",
                "Monokai Light",
                "Monokai Pro Dark",
                "Monokai Pro Light",
                "One Dark",
            ]
        );

        // The on-disk manifest limits the number of FS calls that need to be made
        // on startup.
        assert_eq!(fs.read_dir_call_count(), prev_fs_read_dir_call_count);
        // Two cache freshness reads, three recovery passes with three journal probes,
        // and one probe for Git sources that may be missing from the cached index.
        assert_eq!(fs.metadata_call_count(), prev_fs_metadata_call_count + 12);
    });

    store.update(cx, |store, cx| {
        store
            .uninstall_extension("zed-ruby".into(), cx)
            .detach_and_log_err(cx);
    });

    cx.executor().advance_clock(RELOAD_DEBOUNCE_DURATION);
    expected_index.extensions.remove("zed-ruby");
    expected_index.languages.remove("Ruby");
    expected_index.languages.remove("ERB");

    store.read_with(cx, |store, _| {
        assert_eq!(store.extension_index.extensions, expected_index.extensions);
        assert_eq!(store.extension_index.themes, expected_index.themes);
        assert_eq!(
            store.extension_index.icon_themes,
            expected_index.icon_themes
        );

        for ((actual_key, actual_language), (expected_key, expected_language)) in store
            .extension_index
            .languages
            .iter()
            .zip(expected_index.languages.iter())
        {
            assert_eq!(actual_key, expected_key);
            assert_eq!(actual_language.grammar, expected_language.grammar);
            assert_eq!(actual_language.matcher, expected_language.matcher);
            assert_eq!(actual_language.hidden, expected_language.hidden);
        }

        assert_eq!(
            language_registry.language_names(),
            [LanguageName::new_static("Plain Text")]
        );
        assert_eq!(language_registry.grammar_names(), []);
    });
}

#[gpui::test]
async fn test_extension_store_with_test_extension(cx: &mut TestAppContext) {
    init_test(cx);
    cx.executor().allow_parking();

    let executor = cx.executor();
    async fn await_or_timeout<T>(
        executor: &BackgroundExecutor,
        what: &'static str,
        seconds: u64,
        future: impl std::future::Future<Output = T>,
    ) -> T {
        let timeout = executor.timer(std::time::Duration::from_secs(seconds));

        futures::select! {
            output = future.fuse() => output,
            _ = futures::FutureExt::fuse(timeout) => panic!(
            "[test_extension_store_with_test_extension] timed out after {seconds}s while {what}"
        )
        }
    }

    let root_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let cache_dir = root_dir.join("target");
    let test_extension_id = "test-extension";
    let test_extension_dir = root_dir.join("extensions").join(test_extension_id);

    let fs = RealFs::new(None, cx.executor());
    let extensions_tree = TempTree::new(json!({
        "installed": {},
        "work": {}
    }));
    let project_dir = TempTree::new(json!({
        "test.gleam": ""
    }));

    let extensions_dir = extensions_tree.path().canonicalize().unwrap();
    let project_dir = project_dir.path().canonicalize().unwrap();

    let project = await_or_timeout(
        &executor,
        "awaiting Project::test",
        5,
        Project::test(fs.clone(), [project_dir.as_path()], cx),
    )
    .await;

    let proxy = Arc::new(ExtensionHostProxy::new());
    let theme_registry = Arc::new(ThemeRegistry::new(Box::new(())));
    theme_extension::init(proxy.clone(), theme_registry.clone(), cx.executor());
    let language_registry = project.read_with(cx, |project, _cx| project.languages().clone());
    let lsp_store_id = project.read_with(cx, |project, _| project.lsp_store().entity_id());
    language_extension::init(
        LspAccess::ViaLspStore(
            project
                .update(cx, |project, _| project.lsp_store())
                .downgrade(),
        ),
        proxy.clone(),
        language_registry.clone(),
    );
    let node_runtime = NodeRuntime::unavailable();

    let (mut status_updates, _status_updates_subscription) =
        language_registry.language_server_binary_statuses();

    struct FakeLanguageServerVersion {
        version: String,
        binary_contents: String,
        http_request_count: usize,
    }

    let language_server_version = Arc::new(Mutex::new(FakeLanguageServerVersion {
        version: "v1.2.3".into(),
        binary_contents: "the-binary-contents".into(),
        http_request_count: 0,
    }));

    let extension_client = FakeHttpClient::create({
        let language_server_version = language_server_version.clone();
        move |request| {
            let language_server_version = language_server_version.clone();
            async move {
                let version = language_server_version.lock().version.clone();
                let binary_contents = language_server_version.lock().binary_contents.clone();

                let github_releases_uri = "https://api.github.com/repos/gleam-lang/gleam/releases";
                let asset_download_uri =
                    format!("https://fake-download.example.com/gleam-{version}");

                let uri = request.uri().to_string();
                if uri == github_releases_uri {
                    language_server_version.lock().http_request_count += 1;
                    Ok(Response::new(
                        json!([
                            {
                                "tag_name": version,
                                "prerelease": false,
                                "tarball_url": "",
                                "zipball_url": "",
                                "assets": [
                                    {
                                        "name": format!("gleam-{version}-aarch64-apple-darwin.tar.gz"),
                                        "browser_download_url": asset_download_uri
                                    },
                                    {
                                        "name": format!("gleam-{version}-x86_64-unknown-linux-musl.tar.gz"),
                                        "browser_download_url": asset_download_uri
                                    },
                                    {
                                        "name": format!("gleam-{version}-aarch64-unknown-linux-musl.tar.gz"),
                                        "browser_download_url": asset_download_uri
                                    },
                                    {
                                        "name": format!("gleam-{version}-x86_64-pc-windows-msvc.tar.gz"),
                                        "browser_download_url": asset_download_uri
                                    },
                                    {
                                        "name": format!("gleam-{version}-x86_64-unknown-freebsd.tar.gz"),
                                        "browser_download_url": asset_download_uri
                                    },
                                    {
                                        "name": format!("gleam-{version}-aarch64-unknown-freebsd.tar.gz"),
                                        "browser_download_url": asset_download_uri
                                    }
                                ]
                            }
                        ])
                        .to_string()
                        .into(),
                    ))
                } else if uri == asset_download_uri {
                    language_server_version.lock().http_request_count += 1;
                    let mut bytes = Vec::<u8>::new();
                    let mut archive = async_tar::Builder::new(&mut bytes);
                    let mut header = async_tar::Header::new_gnu();
                    header.set_size(binary_contents.len() as u64);
                    archive
                        .append_data(&mut header, "gleam", binary_contents.as_bytes())
                        .await
                        .unwrap();
                    archive.into_inner().await.unwrap();
                    let mut gzipped_bytes = Vec::new();
                    let mut encoder = GzipEncoder::new(BufReader::new(bytes.as_slice()));
                    encoder.read_to_end(&mut gzipped_bytes).await.unwrap();
                    Ok(Response::new(gzipped_bytes.into()))
                } else {
                    Ok(Response::builder().status(404).body("not found".into())?)
                }
            }
        }
    });
    let user_agent = cx.update(|cx| {
        format!(
            "Zed/{} ({}; {})",
            AppVersion::global(cx),
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    });
    let builder_client =
        Arc::new(ReqwestClient::user_agent(&user_agent).expect("Could not create HTTP client"));

    let extension_store = cx.new(|cx| {
        ExtensionStore::new(
            extensions_dir.clone(),
            Some(cache_dir),
            proxy,
            fs.clone(),
            extension_client.clone(),
            builder_client,
            None,
            node_runtime,
            cx,
        )
    });

    // Ensure that debounces fire.
    let mut events = cx.events(&extension_store);
    let executor = cx.executor();
    let _task = cx.executor().spawn(async move {
        while let Some(event) = events.next().await {
            if let Event::StartedReloading = event {
                executor.advance_clock(RELOAD_DEBOUNCE_DURATION);
            }
        }
    });

    extension_store.update(cx, |_, cx| {
        cx.subscribe(&extension_store, |_, _, event, _| {
            if matches!(event, Event::ExtensionFailedToLoad(_)) {
                panic!("extension failed to load");
            }
        })
        .detach();
    });

    let mut extension_events = cx.events(&cx.update(|cx| {
        extension::ExtensionEvents::try_global(cx)
            .expect("ExtensionEvents should be initialized in tests")
    }));

    let executor = cx.executor();
    await_or_timeout(
        &executor,
        "awaiting install_dev_extension",
        240,
        extension_store.update(cx, |store, cx| {
            store.install_dev_extension(test_extension_dir.clone(), cx)
        }),
    )
    .await
    .unwrap();

    await_or_timeout(
        &executor,
        "awaiting ExtensionsInstalledChanged",
        10,
        async {
            while let Some(event) = extension_events.next().await {
                if matches!(event, extension::Event::ExtensionsInstalledChanged) {
                    return;
                }
            }

            panic!(
                "[test_extension_store_with_test_extension] extension event stream ended before ExtensionsInstalledChanged"
            );
        },
    )
    .await;

    let mut fake_servers = language_registry.register_fake_lsp_server(
        LanguageServerName("gleam".into()),
        lsp::ServerCapabilities {
            completion_provider: Some(Default::default()),
            ..Default::default()
        },
        None,
    );
    cx.executor().run_until_parked();

    let mut project_events = cx.events(&project);
    let buffer_path = project_dir.join("test.gleam");
    let (buffer, _handle) = await_or_timeout(
        &executor,
        "awaiting open_local_buffer_with_lsp",
        5,
        project.update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(buffer_path.clone(), cx)
        }),
    )
    .await
    .unwrap();
    cx.executor().run_until_parked();

    let buffer_remote_id = buffer.read_with(cx, |buffer, _cx| buffer.remote_id());

    let fake_server = await_or_timeout(
        &executor,
        "awaiting first fake server spawn",
        10,
        fake_servers.next(),
    )
    .await
    .unwrap();

    let work_dir = extensions_dir.join(format!("work/{test_extension_id}"));
    let expected_server_path = work_dir.join("gleam-v1.2.3/gleam");
    let expected_binary_contents = language_server_version.lock().binary_contents.clone();

    // check that IO operations in extension work correctly
    assert!(work_dir.join("dir-created-with-rel-path").exists());
    assert!(work_dir.join("dir-created-with-abs-path").exists());
    assert!(work_dir.join("file-created-with-abs-path").exists());
    assert!(work_dir.join("file-created-with-rel-path").exists());

    assert_eq!(fake_server.binary.path, expected_server_path);
    assert_eq!(fake_server.binary.arguments, [OsString::from("lsp")]);
    assert_eq!(
        await_or_timeout(
            &executor,
            "awaiting fs.load(expected_server_path)",
            5,
            fs.load(&expected_server_path)
        )
        .await
        .unwrap(),
        expected_binary_contents
    );
    assert_eq!(language_server_version.lock().http_request_count, 2);
    assert_eq!(
        [
            await_or_timeout(
                &executor,
                "awaiting status_updates #1",
                5,
                status_updates.next()
            )
            .await
            .unwrap(),
            await_or_timeout(
                &executor,
                "awaiting status_updates #2",
                5,
                status_updates.next()
            )
            .await
            .unwrap(),
            await_or_timeout(
                &executor,
                "awaiting status_updates #3",
                5,
                status_updates.next()
            )
            .await
            .unwrap(),
            await_or_timeout(
                &executor,
                "awaiting status_updates #4",
                5,
                status_updates.next()
            )
            .await
            .unwrap(),
        ],
        [
            (
                Some(lsp_store_id),
                LanguageServerName::new_static("gleam"),
                BinaryStatus::Starting
            ),
            (
                Some(lsp_store_id),
                LanguageServerName::new_static("gleam"),
                BinaryStatus::CheckingForUpdate
            ),
            (
                Some(lsp_store_id),
                LanguageServerName::new_static("gleam"),
                BinaryStatus::Downloading
            ),
            (
                Some(lsp_store_id),
                LanguageServerName::new_static("gleam"),
                BinaryStatus::None
            )
        ]
    );

    // The extension creates custom labels for completion items.
    fake_server.set_request_handler::<lsp::request::Completion, _, _>(|_, _| async move {
        Ok(Some(lsp::CompletionResponse::Array(vec![
            lsp::CompletionItem {
                label: "foo".into(),
                kind: Some(lsp::CompletionItemKind::FUNCTION),
                detail: Some("fn() -> Result(Nil, Error)".into()),
                ..Default::default()
            },
            lsp::CompletionItem {
                label: "bar.baz".into(),
                kind: Some(lsp::CompletionItemKind::FUNCTION),
                detail: Some("fn(List(a)) -> a".into()),
                ..Default::default()
            },
            lsp::CompletionItem {
                label: "Quux".into(),
                kind: Some(lsp::CompletionItemKind::CONSTRUCTOR),
                detail: Some("fn(String) -> T".into()),
                ..Default::default()
            },
            lsp::CompletionItem {
                label: "my_string".into(),
                kind: Some(lsp::CompletionItemKind::CONSTANT),
                detail: Some("String".into()),
                ..Default::default()
            },
        ])))
    });

    // `register_fake_lsp_server` can yield a server instance before the client has fully registered
    // the buffer with the project LSP plumbing. Wait for the project to observe that registration
    // before issuing requests like completion.
    await_or_timeout(
        &executor,
        "awaiting LanguageServerBufferRegistered",
        5,
        async {
            while let Some(event) = project_events.next().await {
                if let project::Event::LanguageServerBufferRegistered { buffer_id, .. } = event {
                    if buffer_id == buffer_remote_id {
                        return;
                    }
                }
            }

            panic!(
                "[test_extension_store_with_test_extension] project event stream ended before buffer registration for {}",
                buffer_path.display()
            );
        },
    )
    .await;

    let completion_labels = await_or_timeout(
        &executor,
        "awaiting completions",
        5,
        project.update(cx, |project, cx| {
            project.completions(&buffer, 0, DEFAULT_COMPLETION_CONTEXT, cx)
        }),
    )
    .await
    .unwrap()
    .into_iter()
    .flat_map(|response| response.completions)
    .map(|c| c.label.text)
    .collect::<Vec<_>>();
    assert_eq!(
        completion_labels,
        [
            "foo: fn() -> Result(Nil, Error)".to_string(),
            "bar.baz: fn(List(a)) -> a".to_string(),
            "Quux: fn(String) -> T".to_string(),
            "my_string: String".to_string(),
        ]
    );

    let wasm_extension = extension_store.read_with(cx, |store, _| {
        store
            .wasm_extensions
            .iter()
            .find(|(manifest, _)| manifest.id.as_ref() == test_extension_id)
            .map(|(_, extension)| extension.clone())
            .expect("the test extension should be loaded")
    });
    let entries_before = resource_table_len(&wasm_extension).await;
    for _ in 0..20 {
        wasm_extension
            .language_server_workspace_configuration(
                LanguageServerName::new_static("gleam"),
                Arc::new(StubWorktree),
                lsp_store_id,
            )
            .await
            .unwrap();
    }
    assert_eq!(
        resource_table_len(&wasm_extension).await,
        entries_before,
        "each call must free the worktree it lends the extension"
    );

    // Simulate a new version of the language server being released
    language_server_version.lock().version = "v2.0.0".into();
    language_server_version.lock().binary_contents = "the-new-binary-contents".into();
    language_server_version.lock().http_request_count = 0;

    // Start a new instance of the language server.
    project.update(cx, |project, cx| {
        project.restart_language_servers_for_buffers(
            vec![buffer.clone()],
            HashSet::default(),
            true,
            cx,
        )
    });
    cx.executor().run_until_parked();

    // The extension has cached the binary path, and does not attempt
    // to reinstall it.
    let fake_server = await_or_timeout(
        &executor,
        "awaiting second fake server spawn",
        5,
        fake_servers.next(),
    )
    .await
    .unwrap();
    assert_eq!(fake_server.binary.path, expected_server_path);
    assert_eq!(
        await_or_timeout(
            &executor,
            "awaiting fs.load(expected_server_path) after restart",
            5,
            fs.load(&expected_server_path)
        )
        .await
        .unwrap(),
        expected_binary_contents
    );
    assert_eq!(language_server_version.lock().http_request_count, 0);

    // Reload the extension, clearing its cache.
    // Start a new instance of the language server.
    await_or_timeout(
        &executor,
        "awaiting extension_store.reload(test-extension)",
        5,
        extension_store.update(cx, |store, cx| {
            store.reload(Some("test-extension".into()), cx)
        }),
    )
    .await;
    cx.executor().run_until_parked();
    project.update(cx, |project, cx| {
        project.restart_language_servers_for_buffers(
            vec![buffer.clone()],
            HashSet::default(),
            true,
            cx,
        )
    });

    // The extension re-fetches the latest version of the language server.
    let fake_server = await_or_timeout(
        &executor,
        "awaiting third fake server spawn",
        5,
        fake_servers.next(),
    )
    .await
    .unwrap();
    let new_expected_server_path =
        extensions_dir.join(format!("work/{test_extension_id}/gleam-v2.0.0/gleam"));
    let expected_binary_contents = language_server_version.lock().binary_contents.clone();
    assert_eq!(fake_server.binary.path, new_expected_server_path);
    assert_eq!(fake_server.binary.arguments, [OsString::from("lsp")]);
    assert_eq!(
        await_or_timeout(
            &executor,
            "awaiting fs.load(new_expected_server_path)",
            5,
            fs.load(&new_expected_server_path)
        )
        .await
        .unwrap(),
        expected_binary_contents
    );

    // The old language server directory has been cleaned up.
    assert!(
        await_or_timeout(
            &executor,
            "awaiting fs.metadata(expected_server_path)",
            5,
            fs.metadata(&expected_server_path)
        )
        .await
        .unwrap()
        .is_none()
    );
}

async fn resource_table_len(extension: &WasmExtension) -> usize {
    extension
        .call(|_, store| {
            async move { anyhow::Ok(store.data_mut().table.iter_mut().count()) }.boxed()
        })
        .await
        .unwrap()
}

struct StubWorktree;

#[async_trait]
impl WorktreeDelegate for StubWorktree {
    fn id(&self) -> u64 {
        0
    }

    fn root_path(&self) -> String {
        String::new()
    }

    async fn read_text_file(&self, _: &RelPath) -> anyhow::Result<String> {
        anyhow::bail!("the stub worktree has no files")
    }

    async fn which(&self, _: String) -> Option<String> {
        None
    }

    async fn shell_env(&self) -> Vec<(String, String)> {
        Vec::new()
    }
}

fn init_test(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let store = SettingsStore::test(cx);
        cx.set_global(store);
        release_channel::init(semver::Version::new(0, 0, 0), cx);
        extension::init(cx);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        gpui_tokio::init(cx);
    });
}

#[gpui::test]
async fn test_register_remote_client_syncs_only_the_new_client(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    init_test(cx);
    let (store, _fs) = create_extension_store(cx);
    insert_remote_sync_index_entry(&store, cx);

    let (opts_a, _counter_a, sync_count_a) = setup_mock_remote(cx, server_cx);
    let (opts_b, _counter_b, sync_count_b) = setup_mock_remote(cx, server_cx);

    let client_a = RemoteClient::connect_mock(opts_a, cx).await;
    let client_b = RemoteClient::connect_mock(opts_b, cx).await;

    store.update(cx, |store, cx| {
        store.register_remote_client(client_a.clone(), cx)
    });
    cx.run_until_parked();
    assert_eq!(
        sync_count_a.load(Ordering::SeqCst),
        1,
        "registering a client should sync extensions to it once"
    );
    assert_eq!(sync_count_b.load(Ordering::SeqCst), 0);

    store.update(cx, |store, cx| {
        store.register_remote_client(client_b.clone(), cx)
    });
    cx.run_until_parked();
    assert_eq!(
        sync_count_a.load(Ordering::SeqCst),
        1,
        "registering a new client should not re-sync already-registered clients"
    );
    assert_eq!(
        sync_count_b.load(Ordering::SeqCst),
        1,
        "registering a client should sync extensions to it once"
    );

    store.update(cx, |store, cx| {
        store.register_remote_client(client_a.clone(), cx)
    });
    cx.run_until_parked();
    assert_eq!(
        sync_count_a.load(Ordering::SeqCst),
        1,
        "re-registering an already-registered client should be a no-op"
    );
}

#[gpui::test]
async fn test_register_remote_client_resyncs_extensions_on_reconnect(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    init_test(cx);
    let (store, _fs) = create_extension_store(cx);
    insert_remote_sync_index_entry(&store, cx);

    let (opts, _counter, sync_count) = setup_mock_remote(cx, server_cx);
    let (other_opts, _other_counter, other_sync_count) = setup_mock_remote(cx, server_cx);
    let client = RemoteClient::connect_mock(opts, cx).await;
    let other_client = RemoteClient::connect_mock(other_opts, cx).await;
    store.update(cx, |store, cx| {
        store.register_remote_client(client.clone(), cx);
        store.register_remote_client(other_client.clone(), cx);
    });
    cx.run_until_parked();
    assert_eq!(
        sync_count.load(Ordering::SeqCst),
        1,
        "registering a remote client should sync extensions to it once"
    );
    assert_eq!(other_sync_count.load(Ordering::SeqCst), 1);

    client.update(cx, |_client, cx| {
        cx.emit(RemoteClientEvent::Reconnected);
    });
    cx.run_until_parked();

    assert_eq!(
        sync_count.load(Ordering::SeqCst),
        2,
        "reconnecting should re-sync extensions to the remote client"
    );
    assert_eq!(
        other_sync_count.load(Ordering::SeqCst),
        1,
        "reconnecting one client should not re-sync other clients"
    );
}

#[gpui::test]
async fn test_register_remote_client_retries_failed_sync(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    init_test(cx);
    let (store, _fs) = create_extension_store(cx);
    insert_remote_sync_index_entry(&store, cx);

    let (opts, _counter, sync_count) = setup_mock_remote_with_failures(cx, server_cx, 1);
    let client = RemoteClient::connect_mock(opts, cx).await;

    store.update(cx, |store, cx| {
        store.register_remote_client(client.clone(), cx)
    });
    cx.run_until_parked();
    assert_eq!(
        sync_count.load(Ordering::SeqCst),
        1,
        "registering a client should attempt an initial sync"
    );

    cx.executor().advance_clock(remote_sync_retry_delay(0));
    cx.run_until_parked();
    assert_eq!(
        sync_count.load(Ordering::SeqCst),
        2,
        "a failed sync should be retried after the retry delay"
    );

    cx.executor().advance_clock(remote_sync_retry_delay(1));
    cx.run_until_parked();
    assert_eq!(
        sync_count.load(Ordering::SeqCst),
        2,
        "a successful sync should not be retried"
    );
}

#[gpui::test]
async fn test_persistent_sync_failure_retries_with_capped_backoff(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    init_test(cx);
    let (store, _fs) = create_extension_store(cx);
    insert_remote_sync_index_entry(&store, cx);

    let (opts, _counter, sync_count) = setup_mock_remote_with_failures(cx, server_cx, usize::MAX);
    let client = RemoteClient::connect_mock(opts, cx).await;

    store.update(cx, |store, cx| {
        store.register_remote_client(client.clone(), cx)
    });
    cx.run_until_parked();
    assert_eq!(sync_count.load(Ordering::SeqCst), 1);

    advance_through_sync_backoff(cx);
    assert_eq!(
        sync_count.load(Ordering::SeqCst),
        9,
        "every backoff period should trigger exactly one retry"
    );

    assert_eq!(remote_sync_retry_delay(8), MAX_REMOTE_SYNC_RETRY_DELAY);
    cx.executor().advance_clock(MAX_REMOTE_SYNC_RETRY_DELAY);
    cx.run_until_parked();
    assert_eq!(
        sync_count.load(Ordering::SeqCst),
        MAX_REMOTE_SYNC_ATTEMPTS,
        "a persistently failing sync should keep retrying once per capped backoff period until the attempt limit"
    );

    cx.executor().advance_clock(MAX_REMOTE_SYNC_RETRY_DELAY);
    cx.run_until_parked();
    assert_eq!(
        sync_count.load(Ordering::SeqCst),
        MAX_REMOTE_SYNC_ATTEMPTS,
        "after the attempt limit is reached, retries should pause until the next change"
    );

    client.update(cx, |_client, cx| {
        cx.emit(RemoteClientEvent::Reconnected);
    });
    cx.run_until_parked();
    assert_eq!(
        sync_count.load(Ordering::SeqCst),
        MAX_REMOTE_SYNC_ATTEMPTS + 1,
        "a connection change should resume syncing after retries were exhausted"
    );
}

#[gpui::test]
async fn test_register_remote_client_drops_stale_retry_after_successful_sync(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    init_test(cx);
    let (store, _fs) = create_extension_store(cx);

    let (opts, _counter, sync_count) = setup_mock_remote_with_failures(cx, server_cx, 1);
    let client = RemoteClient::connect_mock(opts, cx).await;

    store.update(cx, |store, cx| {
        store.register_remote_client(client.clone(), cx)
    });
    cx.run_until_parked();
    assert_eq!(
        sync_count.load(Ordering::SeqCst),
        1,
        "registering a client should attempt an initial sync"
    );

    client.update(cx, |_client, cx| {
        cx.emit(RemoteClientEvent::Reconnected);
    });
    cx.run_until_parked();
    assert_eq!(
        sync_count.load(Ordering::SeqCst),
        2,
        "reconnecting should trigger a sync"
    );

    cx.executor().advance_clock(remote_sync_retry_delay(0));
    cx.run_until_parked();
    assert_eq!(
        sync_count.load(Ordering::SeqCst),
        2,
        "a pending retry should be dropped once a later sync succeeded"
    );
}

#[gpui::test]
async fn test_register_remote_client_keeps_subscription_across_disconnect(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    init_test(cx);
    let (store, _fs) = create_extension_store(cx);

    let (opts, _counter, sync_count) = setup_mock_remote_with_failures(cx, server_cx, 1);
    let client = RemoteClient::connect_mock(opts, cx).await;

    store.update(cx, |store, cx| {
        store.register_remote_client(client.clone(), cx)
    });
    cx.run_until_parked();
    assert_eq!(
        sync_count.load(Ordering::SeqCst),
        1,
        "registering a client should attempt an initial sync"
    );

    client.update(cx, |_client, cx| {
        cx.emit(RemoteClientEvent::Disconnected {
            server_not_running: false,
        });
    });
    cx.run_until_parked();

    store.read_with(cx, |store, _cx| {
        assert_eq!(
            store.remote_clients.len(),
            1,
            "a disconnected client should stay registered so a later reconnect can resync"
        );
    });

    client.update(cx, |_client, cx| {
        cx.emit(RemoteClientEvent::Reconnected);
    });
    cx.run_until_parked();
    assert_eq!(
        sync_count.load(Ordering::SeqCst),
        2,
        "reconnecting after a disconnect should resync the client"
    );
}

#[gpui::test]
async fn test_register_remote_client_release_evicts_client(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    init_test(cx);
    let (store, _fs) = create_extension_store(cx);

    let (opts, _counter, _sync_count) = setup_mock_remote(cx, server_cx);
    let client = RemoteClient::connect_mock(opts, cx).await;

    store.update(cx, |store, cx| {
        store.register_remote_client(client.clone(), cx)
    });
    cx.run_until_parked();
    assert_eq!(
        store.read_with(cx, |store, _cx| store.remote_clients.len()),
        1
    );

    cx.update(move |_cx| drop(client));
    cx.run_until_parked();

    store.read_with(cx, |store, _cx| {
        assert_eq!(
            store.remote_clients.len(),
            0,
            "a released client should be evicted along with its subscriptions and reconciler"
        );
    });
}

#[gpui::test]
async fn test_register_remote_client_resyncs_after_simulated_reconnect(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    init_test(cx);
    let (store, _fs) = create_extension_store(cx);

    let (opts, _counter, sync_count) = setup_mock_remote(cx, server_cx);
    let client = RemoteClient::connect_mock(opts, cx).await;

    store.update(cx, |store, cx| {
        store.register_remote_client(client.clone(), cx)
    });
    cx.run_until_parked();
    assert_eq!(sync_count.load(Ordering::SeqCst), 1);

    client
        .update(cx, |client, cx| client.simulate_disconnect(cx))
        .detach();
    cx.run_until_parked();

    let mut reconnected_and_resynced = false;
    for _ in 0..30 {
        cx.executor().advance_clock(Duration::from_secs(10));
        cx.run_until_parked();
        let connected = client.read_with(cx, |client, _cx| {
            client.connection_state() == ConnectionState::Connected
        });
        if connected && sync_count.load(Ordering::SeqCst) >= 2 {
            reconnected_and_resynced = true;
            break;
        }
    }
    assert!(
        reconnected_and_resynced,
        "a simulated disconnect should reconnect and resync without deadlocking the reconciler"
    );
    assert_eq!(
        sync_count.load(Ordering::SeqCst),
        2,
        "a real reconnect should trigger exactly one resync"
    );
}

fn fail_headless_publications(
    store: &Entity<HeadlessExtensionStore>,
    extension_ids: &[&str],
    cx: &mut TestAppContext,
) -> crate::extension_transaction::ExtensionPublisher {
    use crate::extension_transaction::TransactionPhase;

    let extension_ids = extension_ids
        .iter()
        .map(|id| id.to_string())
        .collect::<Vec<_>>();
    let publisher = store.read_with(cx, |store, _| store.publisher.clone());
    publisher.set_hook(move |info| {
        let fail = info.phase == TransactionPhase::BeforePublish
            && extension_ids.contains(&info.extension_id);
        async move {
            anyhow::ensure!(!fail, "injected headless publication failure");
            Ok(())
        }
        .boxed()
    });
    publisher
}

#[gpui::test]
async fn test_headless_sync_extensions_notifications(cx: &mut TestAppContext) {
    init_test(cx);

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        "/extensions/foo-dev",
        json!({
            "extension.toml": r#"
id = "foo-dev"
name = "foo-dev"
version = "1.0.0"
schema_version = 1
languages = ["languages/foo"]
"#,
            "languages": {
                "foo": {
                    "config.toml": r#"
name = "Foo"
grammar = "foo"
path_suffixes = ["foo"]
"#
                }
            }
        }),
    )
    .await;

    let store = cx.update(|cx| {
        HeadlessExtensionStore::new(
            fs.clone(),
            FakeHttpClient::with_200_response(),
            PathBuf::from("/extensions"),
            Arc::new(ExtensionHostProxy::new()),
            NodeRuntime::unavailable(),
            cx,
        )
    });

    let installed_changed_count = Arc::new(AtomicUsize::new(0));
    cx.update(|cx| {
        let extension_events = extension::ExtensionEvents::try_global(cx)
            .expect("ExtensionEvents should be initialized in tests");
        let installed_changed_count = installed_changed_count.clone();
        cx.subscribe(&extension_events, move |_, event, _cx| {
            if matches!(event, extension::Event::ExtensionsInstalledChanged) {
                installed_changed_count.fetch_add(1, Ordering::SeqCst);
            }
        })
        .detach();
    });

    let dev_extension = || {
        vec![ExtensionVersion {
            id: "foo-dev".to_string(),
            version: "1.0.0".to_string(),
            dev: true,
            content_fingerprint: None,
        }]
    };

    let missing = store
        .update(cx, |store, cx| store.sync_extensions(dev_extension(), cx))
        .await
        .unwrap();
    cx.run_until_parked();
    assert_eq!(missing.len(), 1, "dev extensions should always be re-sent");
    assert_eq!(
        installed_changed_count.load(Ordering::SeqCst),
        1,
        "loading a not-yet-loaded extension should notify"
    );

    let missing = store
        .update(cx, |store, cx| store.sync_extensions(dev_extension(), cx))
        .await
        .unwrap();
    cx.run_until_parked();
    assert_eq!(missing.len(), 1, "dev extensions should always be re-sent");
    assert_eq!(
        installed_changed_count.load(Ordering::SeqCst),
        1,
        "re-syncing an unchanged dev extension should not reload it or notify"
    );

    fs.insert_file(
        "/extensions/foo-dev/languages/foo/config.toml",
        br#"
name = "Foo"
grammar = "foo"
path_suffixes = ["foo", "foo2"]
"#
        .to_vec(),
    )
    .await;

    let missing = store
        .update(cx, |store, cx| store.sync_extensions(dev_extension(), cx))
        .await
        .unwrap();
    cx.run_until_parked();
    assert_eq!(missing.len(), 1, "dev extensions should always be re-sent");
    assert_eq!(
        installed_changed_count.load(Ordering::SeqCst),
        2,
        "reloading a dev extension whose content changed should notify"
    );

    store
        .update(cx, |store, cx| store.sync_extensions(Vec::new(), cx))
        .await
        .unwrap();
    cx.run_until_parked();
    assert_eq!(
        installed_changed_count.load(Ordering::SeqCst),
        3,
        "unloading an extension should notify"
    );
}

#[gpui::test]
async fn test_headless_sync_skips_dev_extension_with_matching_fingerprint(cx: &mut TestAppContext) {
    init_test(cx);

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        "/extensions/foo-dev",
        json!({
            "extension.toml": r#"
id = "foo-dev"
name = "foo-dev"
version = "1.0.0"
schema_version = 1
languages = ["languages/foo"]
"#,
            "languages": {
                "foo": {
                    "config.toml": "name = \"Foo\"\ngrammar = \"foo\"\npath_suffixes = [\"foo\"]\n"
                }
            }
        }),
    )
    .await;

    let store = cx.update(|cx| {
        HeadlessExtensionStore::new(
            fs.clone(),
            FakeHttpClient::with_200_response(),
            PathBuf::from("/extensions"),
            Arc::new(ExtensionHostProxy::new()),
            NodeRuntime::unavailable(),
            cx,
        )
    });

    let dev_extension = |content_fingerprint: Option<u64>| {
        vec![ExtensionVersion {
            id: "foo-dev".to_string(),
            version: "1.0.0".to_string(),
            dev: true,
            content_fingerprint,
        }]
    };

    let missing = store
        .update(cx, |store, cx| {
            store.sync_extensions(dev_extension(None), cx)
        })
        .await
        .unwrap();
    assert_eq!(
        missing.len(),
        1,
        "a dev extension without a client fingerprint should be re-requested"
    );

    let fingerprint = store
        .read_with(cx, |store, _cx| {
            store
                .loaded_extensions
                .get("foo-dev")
                .and_then(|loaded| loaded.content_fingerprint)
        })
        .expect("loading a dev extension should record its content fingerprint");

    let missing = store
        .update(cx, |store, cx| {
            store.sync_extensions(dev_extension(Some(fingerprint)), cx)
        })
        .await
        .unwrap();
    assert_eq!(
        missing.len(),
        0,
        "a dev extension whose fingerprint matches the loaded content should not be re-requested"
    );

    let missing = store
        .update(cx, |store, cx| {
            store.sync_extensions(dev_extension(Some(fingerprint.wrapping_add(1))), cx)
        })
        .await
        .unwrap();
    assert_eq!(
        missing.len(),
        1,
        "a dev extension whose fingerprint differs from the loaded content should be re-requested"
    );
}

#[gpui::test]
async fn test_headless_sync_extensions_reports_failed_load_as_missing_again(
    cx: &mut TestAppContext,
) {
    init_test(cx);

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/extensions", json!({})).await;

    let store = cx.update(|cx| {
        HeadlessExtensionStore::new(
            fs.clone(),
            FakeHttpClient::with_200_response(),
            PathBuf::from("/extensions"),
            Arc::new(ExtensionHostProxy::new()),
            NodeRuntime::unavailable(),
            cx,
        )
    });

    let broken_extension = || {
        vec![ExtensionVersion {
            id: "broken".to_string(),
            version: "1.0.0".to_string(),
            dev: false,
            content_fingerprint: None,
        }]
    };

    let missing = store
        .update(cx, |store, cx| {
            store.sync_extensions(broken_extension(), cx)
        })
        .await
        .unwrap();
    assert_eq!(
        missing.len(),
        1,
        "a failed load should be reported as missing"
    );

    let loaded_count = store.read_with(cx, |store, _cx| store.loaded_extensions.len());
    assert_eq!(
        loaded_count, 0,
        "a failed load should not mark the extension as loaded"
    );

    let missing = store
        .update(cx, |store, cx| {
            store.sync_extensions(broken_extension(), cx)
        })
        .await
        .unwrap();
    assert_eq!(
        missing.len(),
        1,
        "an extension that failed to load should be reported as missing on the next sync too"
    );
}

#[gpui::test]
async fn test_headless_sync_uninstall_failure_does_not_block_other_extensions(
    cx: &mut TestAppContext,
) {
    init_test(cx);

    let extension_files = |id: &str| {
        json!({
            "extension.toml": format!(
                "id = \"{id}\"\nname = \"{id}\"\nversion = \"1.0.0\"\nschema_version = 1\nlanguages = [\"languages/lang\"]\n"
            ),
            "languages": {
                "lang": {
                    "config.toml": format!(
                        "name = \"{id}-lang\"\ngrammar = \"{id}\"\npath_suffixes = [\"{id}\"]\n"
                    )
                }
            }
        })
    };

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        "/extensions",
        json!({
            "ext-a": extension_files("ext-a"),
            "ext-b": extension_files("ext-b"),
            "ext-c": extension_files("ext-c"),
        }),
    )
    .await;

    let store = cx.update(|cx| {
        HeadlessExtensionStore::new(
            fs.clone(),
            FakeHttpClient::with_200_response(),
            PathBuf::from("/extensions"),
            Arc::new(ExtensionHostProxy::new()),
            NodeRuntime::unavailable(),
            cx,
        )
    });

    let extension = |id: &str| ExtensionVersion {
        id: id.to_string(),
        version: "1.0.0".to_string(),
        dev: false,
        content_fingerprint: None,
    };

    store
        .update(cx, |store, cx| {
            store.sync_extensions(vec![extension("ext-a"), extension("ext-b")], cx)
        })
        .await
        .unwrap();

    let _publisher = fail_headless_publications(&store, &["ext-a"], cx);

    let missing = store
        .update(cx, |store, cx| {
            store.sync_extensions(vec![extension("ext-b"), extension("ext-c")], cx)
        })
        .await
        .unwrap();
    assert_eq!(
        missing.len(),
        0,
        "a failed removal should not fail the sync or block loading new extensions"
    );

    store.read_with(cx, |store, _cx| {
        let mut ids = store.loaded_extensions.keys().cloned().collect::<Vec<_>>();
        ids.sort();
        assert_eq!(
            ids,
            vec![Arc::<str>::from("ext-b"), Arc::<str>::from("ext-c")],
            "the extension whose files failed to be removed should still be evicted, and the new extension loaded"
        );
    });
}

#[gpui::test]
async fn test_extension_index_change_sync_failure_is_retried(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    init_test(cx);
    let (store, fs) = create_extension_store(cx);

    let (opts, counter, sync_count) = setup_mock_remote(cx, server_cx);
    let client = RemoteClient::connect_mock(opts, cx).await;

    store.update(cx, |store, cx| {
        store.register_remote_client(client.clone(), cx)
    });
    cx.run_until_parked();
    assert_eq!(sync_count.load(Ordering::SeqCst), 1);

    counter.update(server_cx, |counter, _cx| counter.failures_remaining = 1);

    insert_remote_relevant_extension(&fs, "sync-ext").await;
    store.update(cx, |store, cx| drop(store.reload(None, cx)));
    cx.executor().advance_clock(RELOAD_DEBOUNCE_DURATION);
    cx.run_until_parked();
    assert_eq!(
        sync_count.load(Ordering::SeqCst),
        2,
        "an extension index change should sync extensions to registered clients"
    );

    cx.executor().advance_clock(remote_sync_retry_delay(0));
    cx.run_until_parked();
    assert_eq!(
        sync_count.load(Ordering::SeqCst),
        3,
        "a failed index-change sync should be retried after the retry delay"
    );

    cx.executor().advance_clock(remote_sync_retry_delay(1));
    cx.run_until_parked();
    assert_eq!(
        sync_count.load(Ordering::SeqCst),
        3,
        "a successful retry should not be retried again"
    );
}

#[gpui::test]
async fn test_register_remote_client_reconnect_does_not_start_parallel_retry_chain(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    init_test(cx);
    let (store, _fs) = create_extension_store(cx);

    let (opts, _counter, sync_count) = setup_mock_remote_with_failures(cx, server_cx, usize::MAX);
    let client = RemoteClient::connect_mock(opts, cx).await;

    store.update(cx, |store, cx| {
        store.register_remote_client(client.clone(), cx)
    });
    cx.run_until_parked();
    assert_eq!(sync_count.load(Ordering::SeqCst), 1);

    client.update(cx, |_client, cx| {
        cx.emit(RemoteClientEvent::Reconnected);
    });
    cx.run_until_parked();
    assert_eq!(
        sync_count.load(Ordering::SeqCst),
        2,
        "reconnecting should attempt a sync even while a retry is pending"
    );

    cx.executor().advance_clock(remote_sync_retry_delay(0));
    cx.run_until_parked();
    assert_eq!(
        sync_count.load(Ordering::SeqCst),
        3,
        "after a reconnect, each backoff period should trigger exactly one retry, not one per chain"
    );

    cx.executor().advance_clock(remote_sync_retry_delay(1));
    cx.run_until_parked();
    assert_eq!(
        sync_count.load(Ordering::SeqCst),
        4,
        "retries should stay serialized in a single chain"
    );
}

#[gpui::test]
async fn test_index_change_during_backoff_neither_preempts_nor_resets_it(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    init_test(cx);
    let (store, fs) = create_extension_store(cx);

    let (opts, _counter, sync_count) = setup_mock_remote_with_failures(cx, server_cx, usize::MAX);
    let client = RemoteClient::connect_mock(opts, cx).await;

    store.update(cx, |store, cx| {
        store.register_remote_client(client.clone(), cx)
    });
    cx.run_until_parked();
    assert_eq!(sync_count.load(Ordering::SeqCst), 1);

    insert_remote_relevant_extension(&fs, "sync-ext").await;
    store.update(cx, |store, cx| drop(store.reload(None, cx)));
    cx.executor().advance_clock(RELOAD_DEBOUNCE_DURATION);
    cx.run_until_parked();
    assert_eq!(
        sync_count.load(Ordering::SeqCst),
        1,
        "an index change should not preempt the pending backoff timer"
    );

    cx.executor().advance_clock(remote_sync_retry_delay(0));
    cx.run_until_parked();
    assert_eq!(
        sync_count.load(Ordering::SeqCst),
        2,
        "the pending retry should fire on schedule and pick up the index change"
    );

    cx.executor().advance_clock(remote_sync_retry_delay(0));
    cx.run_until_parked();
    assert_eq!(
        sync_count.load(Ordering::SeqCst),
        2,
        "an index change should not reset the backoff chain to the initial delay"
    );

    cx.executor().advance_clock(remote_sync_retry_delay(1));
    cx.run_until_parked();
    assert_eq!(
        sync_count.load(Ordering::SeqCst),
        3,
        "the backoff chain should continue growing across index changes"
    );
}

#[gpui::test]
async fn test_index_change_after_exhaustion_triggers_single_attempt(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    init_test(cx);
    let (store, fs) = create_extension_store(cx);

    let (opts, _counter, sync_count) = setup_mock_remote_with_failures(cx, server_cx, usize::MAX);
    let client = RemoteClient::connect_mock(opts, cx).await;

    store.update(cx, |store, cx| {
        store.register_remote_client(client.clone(), cx)
    });
    cx.run_until_parked();

    advance_through_sync_backoff(cx);
    cx.executor().advance_clock(MAX_REMOTE_SYNC_RETRY_DELAY);
    cx.run_until_parked();
    assert_eq!(sync_count.load(Ordering::SeqCst), MAX_REMOTE_SYNC_ATTEMPTS);

    insert_remote_relevant_extension(&fs, "sync-ext").await;
    store.update(cx, |store, cx| drop(store.reload(None, cx)));
    cx.executor().advance_clock(RELOAD_DEBOUNCE_DURATION);
    cx.run_until_parked();
    assert_eq!(
        sync_count.load(Ordering::SeqCst),
        MAX_REMOTE_SYNC_ATTEMPTS + 1,
        "after exhaustion, an index change should trigger exactly one attempt"
    );

    cx.executor().advance_clock(MAX_REMOTE_SYNC_RETRY_DELAY);
    cx.run_until_parked();
    assert_eq!(
        sync_count.load(Ordering::SeqCst),
        MAX_REMOTE_SYNC_ATTEMPTS + 1,
        "a failed post-exhaustion attempt should park again instead of restarting the chain"
    );

    client.update(cx, |_client, cx| {
        cx.emit(RemoteClientEvent::Reconnected);
    });
    cx.run_until_parked();
    assert_eq!(
        sync_count.load(Ordering::SeqCst),
        MAX_REMOTE_SYNC_ATTEMPTS + 2,
        "a reconnect should restart the retry chain after exhaustion"
    );

    cx.executor().advance_clock(remote_sync_retry_delay(0));
    cx.run_until_parked();
    assert_eq!(
        sync_count.load(Ordering::SeqCst),
        MAX_REMOTE_SYNC_ATTEMPTS + 3,
        "the restarted chain should retry with the initial delay"
    );
}

#[gpui::test]
async fn test_remote_sync_failed_install_is_retried(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    init_test(cx);
    let (store, _fs) = create_extension_store(cx);

    store.update(cx, |store, _cx| {
        for id in ["bar-lsp", "foo-lsp"] {
            let mut entry = remote_sync_entry(
                id,
                r#"
                [language_servers.foo]
                language = "Foo"
                "#,
            );
            entry.dev = true;
            store
                .extension_index
                .extensions
                .insert(Arc::from(id), entry);
        }
    });

    let (opts, _counter, sync_count, install_count) =
        setup_mock_remote_with_install_failures(cx, server_cx, 1);
    let client = RemoteClient::connect_mock(opts, cx).await;

    store.update(cx, |store, cx| {
        store.register_remote_client(client.clone(), cx)
    });
    cx.run_until_parked();
    assert_eq!(sync_count.load(Ordering::SeqCst), 1);
    assert_eq!(
        install_count.load(Ordering::SeqCst),
        2,
        "one failed install should not prevent installing the remaining extensions"
    );

    cx.executor().advance_clock(remote_sync_retry_delay(0));
    cx.run_until_parked();
    assert_eq!(
        sync_count.load(Ordering::SeqCst),
        2,
        "a sync with a failed extension install should be retried"
    );
    assert_eq!(
        install_count.load(Ordering::SeqCst),
        4,
        "the retried sync should reattempt all missing installs"
    );

    cx.executor().advance_clock(remote_sync_retry_delay(1));
    cx.run_until_parked();
    assert_eq!(
        sync_count.load(Ordering::SeqCst),
        2,
        "a sync whose installs all succeeded should not be retried"
    );
}

#[gpui::test]
async fn test_unchanged_dev_extension_is_not_reuploaded_on_resync(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    init_test(cx);
    let (store, _fs) = create_extension_store(cx);

    let insert_dev_entry = |store: &Entity<ExtensionStore>, cx: &mut TestAppContext, body: &str| {
        let mut entry = remote_sync_entry("foo-lsp", body);
        entry.dev = true;
        store.update(cx, |store, _cx| {
            store
                .extension_index
                .extensions
                .insert(Arc::from("foo-lsp"), entry);
        });
    };
    insert_dev_entry(&store, cx, "[language_servers.foo]\nlanguage = \"Foo\"");

    let (opts, _counter, sync_count, install_count) =
        setup_mock_remote_tracking_fingerprints(cx, server_cx);
    let client = RemoteClient::connect_mock(opts, cx).await;

    store.update(cx, |store, cx| {
        store.register_remote_client(client.clone(), cx)
    });
    cx.run_until_parked();
    assert_eq!(sync_count.load(Ordering::SeqCst), 1);
    assert_eq!(
        install_count.load(Ordering::SeqCst),
        1,
        "the initial sync should upload the dev extension"
    );

    client.update(cx, |_client, cx| {
        cx.emit(RemoteClientEvent::Reconnected);
    });
    cx.run_until_parked();
    assert_eq!(sync_count.load(Ordering::SeqCst), 2);
    assert_eq!(
        install_count.load(Ordering::SeqCst),
        1,
        "a resync with unchanged dev extension content should not re-upload it"
    );

    insert_dev_entry(
        &store,
        cx,
        "[language_servers.foo]\nlanguage = \"Foo\"\n\n[language_servers.bar]\nlanguage = \"Bar\"",
    );
    client.update(cx, |_client, cx| {
        cx.emit(RemoteClientEvent::Reconnected);
    });
    cx.run_until_parked();
    assert_eq!(sync_count.load(Ordering::SeqCst), 3);
    assert_eq!(
        install_count.load(Ordering::SeqCst),
        2,
        "changed dev extension content should be re-uploaded"
    );
}

#[gpui::test]
async fn test_remote_sync_hang_times_out_and_is_retried(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    init_test(cx);
    let (store, _fs) = create_extension_store(cx);

    let (opts, _counter, sync_count) = setup_mock_remote_hanging(cx, server_cx);
    let client = RemoteClient::connect_mock(opts, cx).await;

    store.update(cx, |store, cx| {
        store.register_remote_client(client.clone(), cx)
    });
    cx.run_until_parked();
    assert_eq!(
        sync_count.load(Ordering::SeqCst),
        1,
        "registering a client should attempt an initial sync"
    );

    cx.executor().advance_clock(REMOTE_SYNC_TIMEOUT);
    cx.run_until_parked();
    cx.executor().advance_clock(remote_sync_retry_delay(0));
    cx.run_until_parked();
    assert_eq!(
        sync_count.load(Ordering::SeqCst),
        2,
        "a hung sync should time out and be retried"
    );
}

#[gpui::test]
async fn test_headless_failed_reload_keeps_other_extensions_languages(cx: &mut TestAppContext) {
    init_test(cx);

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        "/extensions",
        json!({
            "ext-a": {
                "extension.toml": r#"
id = "ext-a"
name = "ext-a"
version = "1.0.0"
schema_version = 1
languages = ["languages/shared"]
"#,
                "languages": {
                    "shared": {
                        "config.toml": "name = \"Shared\"\ngrammar = \"shared\"\npath_suffixes = [\"shared\"]\n"
                    }
                }
            },
            "ext-b": {
                "extension.toml": r#"
id = "ext-b"
name = "ext-b"
version = "1.0.0"
schema_version = 1
languages = ["languages/foo"]
"#,
                "languages": {
                    "foo": {
                        "config.toml": "name = \"Foo\"\ngrammar = \"foo\"\npath_suffixes = [\"foo\"]\n"
                    }
                }
            }
        }),
    )
    .await;

    let language_registry = Arc::new(LanguageRegistry::test(cx.executor()));
    let proxy = Arc::new(ExtensionHostProxy::new());
    language_extension::init(LspAccess::Noop, proxy.clone(), language_registry.clone());

    let store = cx.update(|cx| {
        HeadlessExtensionStore::new(
            fs.clone(),
            FakeHttpClient::with_200_response(),
            PathBuf::from("/extensions"),
            proxy,
            NodeRuntime::unavailable(),
            cx,
        )
    });

    let extensions = || {
        vec![
            ExtensionVersion {
                id: "ext-a".to_string(),
                version: "1.0.0".to_string(),
                dev: false,
                content_fingerprint: None,
            },
            ExtensionVersion {
                id: "ext-b".to_string(),
                version: "1.0.0".to_string(),
                dev: true,
                content_fingerprint: None,
            },
        ]
    };

    store
        .update(cx, |store, cx| store.sync_extensions(extensions(), cx))
        .await
        .unwrap();
    assert_eq!(
        language_registry.language_names(),
        vec![
            LanguageName::new("Foo"),
            LanguageName::new("Plain Text"),
            LanguageName::new("Shared"),
        ],
    );

    fs.insert_tree(
        "/extensions/ext-b/languages",
        json!({
            "shared": {
                "config.toml": "name = \"Shared\"\ngrammar = \"shared\"\npath_suffixes = [\"shared\"]\n"
            },
            "broken": {
                "config.toml": "not valid toml ["
            }
        }),
    )
    .await;
    fs.insert_file(
        "/extensions/ext-b/extension.toml",
        br#"
id = "ext-b"
name = "ext-b"
version = "1.0.0"
schema_version = 1
languages = ["languages/foo", "languages/shared", "languages/broken"]
"#
        .to_vec(),
    )
    .await;

    let missing = store
        .update(cx, |store, cx| store.sync_extensions(extensions(), cx))
        .await
        .unwrap();
    assert_eq!(
        missing.len(),
        1,
        "a failed dev extension reload should be reported as missing"
    );
    assert_eq!(
        language_registry.language_names(),
        vec![
            LanguageName::new("Foo"),
            LanguageName::new("Plain Text"),
            LanguageName::new("Shared"),
        ],
        "a failed reload of one extension should not deregister languages owned by other extensions"
    );
}

#[gpui::test]
async fn test_headless_dev_reload_deregisters_dropped_languages(cx: &mut TestAppContext) {
    init_test(cx);

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        "/extensions/foo-dev",
        json!({
            "extension.toml": r#"
id = "foo-dev"
name = "foo-dev"
version = "1.0.0"
schema_version = 1
languages = ["languages/foo", "languages/bar"]
"#,
            "languages": {
                "foo": {
                    "config.toml": "name = \"Foo\"\ngrammar = \"foo\"\npath_suffixes = [\"foo\"]\n"
                },
                "bar": {
                    "config.toml": "name = \"Bar\"\ngrammar = \"bar\"\npath_suffixes = [\"bar\"]\n"
                }
            }
        }),
    )
    .await;

    let language_registry = Arc::new(LanguageRegistry::test(cx.executor()));
    let proxy = Arc::new(ExtensionHostProxy::new());
    language_extension::init(LspAccess::Noop, proxy.clone(), language_registry.clone());

    let store = cx.update(|cx| {
        HeadlessExtensionStore::new(
            fs.clone(),
            FakeHttpClient::with_200_response(),
            PathBuf::from("/extensions"),
            proxy,
            NodeRuntime::unavailable(),
            cx,
        )
    });

    let dev_extension = || {
        vec![ExtensionVersion {
            id: "foo-dev".to_string(),
            version: "1.0.0".to_string(),
            dev: true,
            content_fingerprint: None,
        }]
    };

    store
        .update(cx, |store, cx| store.sync_extensions(dev_extension(), cx))
        .await
        .unwrap();
    assert_eq!(
        language_registry.language_names(),
        vec![
            LanguageName::new("Bar"),
            LanguageName::new("Foo"),
            LanguageName::new("Plain Text"),
        ],
    );

    fs.insert_file(
        "/extensions/foo-dev/extension.toml",
        br#"
id = "foo-dev"
name = "foo-dev"
version = "1.0.0"
schema_version = 1
languages = ["languages/foo"]
"#
        .to_vec(),
    )
    .await;

    store
        .update(cx, |store, cx| store.sync_extensions(dev_extension(), cx))
        .await
        .unwrap();
    assert_eq!(
        language_registry.language_names(),
        vec![LanguageName::new("Foo"), LanguageName::new("Plain Text")],
        "a dev reload that drops a language should deregister it"
    );
    store.read_with(cx, |store, _cx| {
        assert_eq!(
            store
                .loaded_extensions
                .get("foo-dev")
                .map(|extension| extension.languages.len()),
            Some(1)
        );
    });
}

#[gpui::test]
async fn test_headless_uninstall_restores_surviving_extensions_language_config(
    cx: &mut TestAppContext,
) {
    init_test(cx);

    let shared_language_extension = |id: &str, suffix: &str| {
        json!({
            "extension.toml": format!(
                "id = \"{id}\"\nname = \"{id}\"\nversion = \"1.0.0\"\nschema_version = 1\nlanguages = [\"languages/shared\"]\n"
            ),
            "languages": {
                "shared": {
                    "config.toml": format!(
                        "name = \"Shared\"\ngrammar = \"shared\"\npath_suffixes = [\"{suffix}\"]\n"
                    )
                }
            }
        })
    };

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        "/extensions",
        json!({
            "ext-a": shared_language_extension("ext-a", "shared-a"),
            "ext-c": shared_language_extension("ext-c", "shared-c"),
        }),
    )
    .await;

    let language_registry = Arc::new(LanguageRegistry::test(cx.executor()));
    let proxy = Arc::new(ExtensionHostProxy::new());
    language_extension::init(LspAccess::Noop, proxy.clone(), language_registry.clone());

    let store = cx.update(|cx| {
        HeadlessExtensionStore::new(
            fs.clone(),
            FakeHttpClient::with_200_response(),
            PathBuf::from("/extensions"),
            proxy,
            NodeRuntime::unavailable(),
            cx,
        )
    });

    let extension = |id: &str| ExtensionVersion {
        id: id.to_string(),
        version: "1.0.0".to_string(),
        dev: false,
        content_fingerprint: None,
    };

    store
        .update(cx, |store, cx| {
            store.sync_extensions(vec![extension("ext-a"), extension("ext-c")], cx)
        })
        .await
        .unwrap();

    store
        .update(cx, |store, cx| {
            store.sync_extensions(vec![extension("ext-a")], cx)
        })
        .await
        .unwrap();
    assert_eq!(
        language_registry.language_name_for_extension("shared-a"),
        Some(LanguageName::new("Shared")),
        "uninstalling an extension should restore the surviving extension's config for a shared language"
    );
    assert_eq!(
        language_registry.language_name_for_extension("shared-c"),
        None,
        "the uninstalled extension's config should no longer be registered"
    );
}

#[gpui::test]
async fn test_headless_failed_reload_restores_previous_language_config(cx: &mut TestAppContext) {
    init_test(cx);

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        "/extensions/foo-dev",
        json!({
            "extension.toml": r#"
id = "foo-dev"
name = "foo-dev"
version = "1.0.0"
schema_version = 1
languages = ["languages/foo"]
"#,
            "languages": {
                "foo": {
                    "config.toml": "name = \"Foo\"\ngrammar = \"foo\"\npath_suffixes = [\"foo\"]\n"
                }
            }
        }),
    )
    .await;

    let language_registry = Arc::new(LanguageRegistry::test(cx.executor()));
    let proxy = Arc::new(ExtensionHostProxy::new());
    language_extension::init(LspAccess::Noop, proxy.clone(), language_registry.clone());

    let store = cx.update(|cx| {
        HeadlessExtensionStore::new(
            fs.clone(),
            FakeHttpClient::with_200_response(),
            PathBuf::from("/extensions"),
            proxy,
            NodeRuntime::unavailable(),
            cx,
        )
    });

    let dev_extension = || {
        vec![ExtensionVersion {
            id: "foo-dev".to_string(),
            version: "1.0.0".to_string(),
            dev: true,
            content_fingerprint: None,
        }]
    };

    store
        .update(cx, |store, cx| store.sync_extensions(dev_extension(), cx))
        .await
        .unwrap();
    assert_eq!(
        language_registry.language_name_for_extension("foo"),
        Some(LanguageName::new("Foo")),
    );

    fs.insert_tree(
        "/extensions/foo-dev/languages",
        json!({
            "foo": {
                "config.toml": "name = \"Foo\"\ngrammar = \"foo\"\npath_suffixes = [\"foo2\"]\n"
            },
            "broken": {
                "config.toml": "not valid toml ["
            }
        }),
    )
    .await;
    fs.insert_file(
        "/extensions/foo-dev/extension.toml",
        br#"
id = "foo-dev"
name = "foo-dev"
version = "1.0.0"
schema_version = 1
languages = ["languages/foo", "languages/broken"]
"#
        .to_vec(),
    )
    .await;

    let missing = store
        .update(cx, |store, cx| store.sync_extensions(dev_extension(), cx))
        .await
        .unwrap();
    assert_eq!(
        missing.len(),
        1,
        "a failed dev extension reload should be reported as missing"
    );
    assert_eq!(
        language_registry.language_name_for_extension("foo"),
        Some(LanguageName::new("Foo")),
        "a failed reload should restore the previously registered language config"
    );
    assert_eq!(
        language_registry.language_name_for_extension("foo2"),
        None,
        "the failed reload's partially applied language config should be rolled back"
    );
    store.read_with(cx, |store, _cx| {
        let loaded = store
            .loaded_extensions
            .get("foo-dev")
            .expect("a failed reload should keep the extension loaded");
        assert_eq!(
            loaded.version.as_ref(),
            "1.0.0",
            "a failed reload should keep the previously loaded version"
        );
        assert_eq!(
            loaded.languages.len(),
            1,
            "a failed reload should not duplicate or drop language registrations"
        );
    });
}

#[gpui::test]
async fn test_headless_failed_reinstall_restores_previous_registrations(cx: &mut TestAppContext) {
    init_test(cx);

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        "/extensions/foo-dev",
        json!({
            "extension.toml": r#"
id = "foo-dev"
name = "foo-dev"
version = "1.0.0"
schema_version = 1
languages = ["languages/foo"]
"#,
            "languages": {
                "foo": {
                    "config.toml": "name = \"Foo\"\ngrammar = \"foo\"\npath_suffixes = [\"foo\"]\n"
                }
            }
        }),
    )
    .await;
    fs.insert_tree(
        "/staged/foo-dev",
        json!({
            "extension.toml": r#"
id = "foo-dev"
name = "foo-dev"
version = "1.0.0"
schema_version = 1
languages = ["languages/foo"]
"#,
            "languages": {
                "foo": {
                    "config.toml": "not valid toml ["
                }
            }
        }),
    )
    .await;

    let language_registry = Arc::new(LanguageRegistry::test(cx.executor()));
    let proxy = Arc::new(ExtensionHostProxy::new());
    language_extension::init(LspAccess::Noop, proxy.clone(), language_registry.clone());

    let store = cx.update(|cx| {
        HeadlessExtensionStore::new(
            fs.clone(),
            FakeHttpClient::with_200_response(),
            PathBuf::from("/extensions"),
            proxy,
            NodeRuntime::unavailable(),
            cx,
        )
    });

    let dev_extension = ExtensionVersion {
        id: "foo-dev".to_string(),
        version: "1.0.0".to_string(),
        dev: true,
        content_fingerprint: None,
    };

    store
        .update(cx, |store, cx| {
            store.sync_extensions(vec![dev_extension.clone()], cx)
        })
        .await
        .unwrap();
    assert_eq!(
        language_registry.language_name_for_extension("foo"),
        Some(LanguageName::new("Foo")),
    );

    let install_result = store
        .update(cx, |store, cx| {
            store.install_extension(dev_extension, PathBuf::from("/staged/foo-dev"), cx)
        })
        .await;
    assert!(
        install_result.is_err(),
        "installing a broken extension should fail"
    );

    assert_eq!(
        language_registry.language_name_for_extension("foo"),
        Some(LanguageName::new("Foo")),
        "a failed reinstall should leave the previous registrations untouched"
    );
    store.read_with(cx, |store, _cx| {
        assert_eq!(
            store
                .loaded_extensions
                .get("foo-dev")
                .map(|extension| extension.languages.len()),
            Some(1),
            "a failed reinstall should keep the previously loaded languages"
        );
        assert_eq!(
            store
                .loaded_extensions
                .get("foo-dev")
                .map(|extension| extension.version.as_ref()),
            Some("1.0.0"),
            "a failed reinstall should keep the previous version loaded"
        );
    });
    assert!(
        fs.is_dir(Path::new("/extensions/foo-dev")).await,
        "a failed reinstall should not touch the installed files"
    );
    assert!(
        !fs.is_dir(Path::new("/staged/foo-dev")).await,
        "a failed reinstall should clean up the staged upload"
    );
}

#[gpui::test]
async fn test_remove_stale_uploads_removes_only_stale_entries(cx: &mut TestAppContext) {
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        "/uploads/stale-upload",
        json!({
            "extension.toml": "id = \"stale\""
        }),
    )
    .await;
    fs.set_next_mtime(SystemTime::now() + STALE_UPLOAD_TTL * 2);
    fs.insert_tree(
        "/uploads/far-future-upload",
        json!({
            "extension.toml": "id = \"future\""
        }),
    )
    .await;
    fs.set_next_mtime(SystemTime::now());
    fs.insert_tree(
        "/uploads/fresh-upload",
        json!({
            "extension.toml": "id = \"fresh\""
        }),
    )
    .await;

    let fs_trait_object = fs.clone() as Arc<dyn Fs>;
    remove_stale_uploads(&fs_trait_object, Path::new("/uploads")).await;

    assert!(
        !fs.is_dir(Path::new("/uploads/stale-upload")).await,
        "an upload directory older than the TTL should be removed"
    );
    assert!(
        !fs.is_dir(Path::new("/uploads/far-future-upload")).await,
        "an upload directory with an mtime far in the future should be treated as stale"
    );
    assert!(
        fs.is_dir(Path::new("/uploads/fresh-upload")).await,
        "a recently modified upload directory should be kept"
    );
}

#[gpui::test]
async fn test_headless_dev_reload_replaces_registrations(cx: &mut TestAppContext) {
    init_test(cx);

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/extensions", json!({})).await;

    let language_registry = Arc::new(LanguageRegistry::test(cx.executor()));
    let proxy = Arc::new(ExtensionHostProxy::new());
    language_extension::init(LspAccess::Noop, proxy.clone(), language_registry.clone());

    let store = cx.update(|cx| {
        HeadlessExtensionStore::new(
            fs.clone(),
            FakeHttpClient::with_200_response(),
            PathBuf::from("/extensions"),
            proxy,
            NodeRuntime::unavailable(),
            cx,
        )
    });

    let loaded_extension = || LoadedExtension {
        version: "1.0.0".into(),
        languages: vec![(
            LanguageName::new("Foo"),
            LanguageConfig {
                name: LanguageName::new("Foo"),
                ..LanguageConfig::default()
            },
        )],
        language_servers: vec![(
            LanguageServerName("foo-lsp".into()),
            LanguageName::new("Foo"),
        )],
        debug_adapters: Vec::new(),
        debug_locators: Vec::new(),
        wasm_extension: Some(Arc::new(FakeExtension)),
        content_fingerprint: None,
    };
    let foo = LanguageName::new("Foo");

    let removal_tasks = store.update(cx, |store, cx| {
        store.commit_extension("foo-ext".into(), Some(loaded_extension()), cx)
    });
    assert_eq!(
        removal_tasks.len(),
        0,
        "the first load should not stop any language servers"
    );
    assert_eq!(language_registry.lsp_adapters(&foo).len(), 1);
    assert_eq!(
        language_registry.language_names(),
        vec![LanguageName::new("Foo"), LanguageName::new("Plain Text")],
    );

    for _ in 0..2 {
        let removal_tasks = store.update(cx, |store, cx| {
            store.commit_extension("foo-ext".into(), Some(loaded_extension()), cx)
        });
        assert_eq!(
            removal_tasks.len(),
            1,
            "a reload should stop the previously running language server"
        );
        for removal in removal_tasks {
            removal.await.unwrap();
        }
        assert_eq!(
            language_registry.lsp_adapters(&foo).len(),
            1,
            "a reload should not accumulate duplicate language server registrations"
        );
        assert_eq!(
            language_registry.language_names(),
            vec![LanguageName::new("Foo"), LanguageName::new("Plain Text")],
            "a reload should not accumulate duplicate language registrations"
        );
    }

    let removal_tasks = store.update(cx, |store, cx| {
        store.commit_extension("foo-ext".into(), None, cx)
    });
    assert_eq!(removal_tasks.len(), 1);
    for removal in removal_tasks {
        removal.await.unwrap();
    }
    assert_eq!(
        language_registry.lsp_adapters(&foo).len(),
        0,
        "an uninstall should deregister the language server"
    );
    assert_eq!(
        language_registry.language_names(),
        vec![LanguageName::new("Plain Text")],
        "an uninstall should deregister the language"
    );
}

#[gpui::test]
async fn test_headless_failed_promotion_preserves_files_and_registrations(cx: &mut TestAppContext) {
    init_test(cx);

    let extension_files = |suffix: &str| {
        json!({
            "extension.toml": r#"
id = "foo"
name = "foo"
version = "1.0.0"
schema_version = 1
languages = ["languages/foo"]
"#,
            "languages": {
                "foo": {
                    "config.toml": format!(
                        "name = \"Foo\"\ngrammar = \"foo\"\npath_suffixes = [\"{suffix}\"]\n"
                    )
                }
            }
        })
    };

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/extensions/foo", extension_files("foo"))
        .await;
    fs.insert_tree("/staged/foo", extension_files("foo2")).await;

    let languages = Arc::new(LanguageRegistry::test(cx.executor()));
    let proxy = Arc::new(ExtensionHostProxy::new());
    language_extension::init(LspAccess::Noop, proxy.clone(), languages.clone());

    let store = cx.update(|cx| {
        HeadlessExtensionStore::new(
            fs.clone(),
            FakeHttpClient::with_200_response(),
            PathBuf::from("/extensions"),
            proxy,
            NodeRuntime::unavailable(),
            cx,
        )
    });

    let extension = || ExtensionVersion {
        id: "foo".to_string(),
        version: "1.0.0".to_string(),
        dev: false,
        content_fingerprint: None,
    };

    store
        .update(cx, |store, cx| store.sync_extensions(vec![extension()], cx))
        .await
        .unwrap();
    store.read_with(cx, |store, _cx| {
        assert_eq!(
            store
                .loaded_extensions
                .get("foo")
                .map(|extension| extension.version.as_ref()),
            Some("1.0.0")
        );
    });

    let original_config = fs
        .load(Path::new("/extensions/foo/languages/foo/config.toml"))
        .await
        .expect("original language config");
    let publisher = store.read_with(cx, |store, _| store.publisher.clone());
    publisher.set_hook({
        let fs = fs.clone();
        move |info| {
            let fs = fs.clone();
            async move {
                if info.phase == crate::extension_transaction::TransactionPhase::BeforePublish {
                    fs.atomic_write(info.installed, "promotion obstruction".into())
                        .await?;
                }
                Ok(())
            }
            .boxed()
        }
    });
    let install_result = store
        .update(cx, |store, cx| {
            store.install_extension(extension(), PathBuf::from("/staged/foo"), cx)
        })
        .await;
    assert!(
        install_result.is_err(),
        "a failed promotion should report the error"
    );
    publisher.clear_hook();
    assert_eq!(
        fs.load(Path::new("/extensions/foo/languages/foo/config.toml"))
            .await
            .expect("preserved language config"),
        original_config
    );
    assert_eq!(
        languages.language_name_for_extension("foo"),
        Some(LanguageName::new("Foo"))
    );
    assert!(languages.language_name_for_extension("foo2").is_none());

    store.read_with(cx, |store, _cx| {
        assert_eq!(
            store
                .loaded_extensions
                .get("foo")
                .map(|extension| extension.version.as_ref()),
            Some("1.0.0"),
            "a failed promotion must retain the previously loaded extension"
        );
    });

    let missing = store
        .update(cx, |store, cx| store.sync_extensions(vec![extension()], cx))
        .await
        .unwrap();
    assert_eq!(
        missing.len(),
        0,
        "a later sync should find the preserved extension installed"
    );
    store.read_with(cx, |store, _cx| {
        assert_eq!(
            store
                .loaded_extensions
                .get("foo")
                .map(|extension| extension.version.as_ref()),
            Some("1.0.0"),
            "a later sync should keep the preserved extension"
        );
    });
}

#[gpui::test]
async fn test_cancelled_headless_install_commits_before_queued_sync(cx: &mut TestAppContext) {
    use crate::extension_transaction::TransactionPhase;

    init_test(cx);
    let files = |version: &str, suffix: &str| {
        json!({
            "extension.toml": format!("id = \"foo\"\nname = \"foo\"\nversion = \"{version}\"\nschema_version = 1\nlanguages = [\"languages/foo\"]\n"),
            "languages": { "foo": { "config.toml": format!("name = \"Foo\"\npath_suffixes = [\"{suffix}\"]\n") } }
        })
    };
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/extensions/foo", files("1.0.0", "old"))
        .await;
    fs.insert_tree("/staged/foo", files("2.0.0", "new")).await;
    let languages = Arc::new(LanguageRegistry::test(cx.executor()));
    let proxy = Arc::new(ExtensionHostProxy::new());
    language_extension::init(LspAccess::Noop, proxy.clone(), languages.clone());
    let store = cx.update(|cx| {
        HeadlessExtensionStore::new(
            fs.clone(),
            FakeHttpClient::with_200_response(),
            PathBuf::from("/extensions"),
            proxy,
            NodeRuntime::unavailable(),
            cx,
        )
    });
    let extension = |version: &str| ExtensionVersion {
        id: "foo".into(),
        version: version.into(),
        dev: false,
        content_fingerprint: None,
    };
    store
        .update(cx, |store, cx| {
            store.sync_extensions(vec![extension("1.0.0")], cx)
        })
        .await
        .expect("load original extension");

    let publisher = store.read_with(cx, |store, _| store.publisher.clone());
    let (reached, reached_receiver) = futures::channel::oneshot::channel();
    let reached = Arc::new(Mutex::new(Some(reached)));
    let (release, release_receiver) = futures::channel::oneshot::channel();
    let release_receiver = Arc::new(Mutex::new(Some(release_receiver)));
    publisher.set_hook(move |info| {
        let reached = reached.clone();
        let release_receiver = release_receiver.clone();
        async move {
            if info.phase == TransactionPhase::BeforePublish {
                reached
                    .lock()
                    .take()
                    .expect("one headless publication")
                    .send(())
                    .expect("publication observer");
                let release_receiver = release_receiver.lock().take().expect("one release");
                release_receiver
                    .await
                    .expect("release headless publication");
            }
            Ok(())
        }
        .boxed()
    });
    let install = store.update(cx, |store, cx| {
        store.install_extension(extension("2.0.0"), PathBuf::from("/staged/foo"), cx)
    });
    reached_receiver
        .await
        .expect("headless publication reached");
    drop(install);
    let mut sync = Box::pin(store.update(cx, |store, cx| {
        store.sync_extensions(vec![extension("2.0.0")], cx)
    }));
    cx.run_until_parked();
    assert!(
        sync.as_mut().now_or_never().is_none(),
        "sync waits for filesystem and registry commit"
    );
    assert_eq!(
        languages.language_name_for_extension("old"),
        Some(LanguageName::new("Foo"))
    );
    assert!(languages.language_name_for_extension("new").is_none());
    publisher.clear_hook();
    release
        .send(())
        .expect("publication survives RPC cancellation");
    assert!(
        sync.await
            .expect("sync after canceled installation")
            .is_empty()
    );
    store.read_with(cx, |store, _| {
        assert_eq!(
            store
                .loaded_extensions
                .get("foo")
                .map(|loaded| loaded.version.as_ref()),
            Some("2.0.0")
        )
    });
    assert_eq!(
        languages.language_name_for_extension("new"),
        Some(LanguageName::new("Foo"))
    );
    assert!(languages.language_name_for_extension("old").is_none());
    assert!(
        fs.load(Path::new("/extensions/foo/extension.toml"))
            .await
            .expect("published manifest")
            .contains("2.0.0")
    );
    assert!(
        !fs.is_dir(Path::new(
            "/extensions.transactions/staging/git-source.previous"
        ))
        .await
    );
}

#[gpui::test]
async fn test_headless_sync_recovers_interrupted_publication(cx: &mut TestAppContext) {
    init_test(cx);
    let files = |suffix: &str| {
        json!({
            "extension.toml": "id = \"foo\"\nname = \"foo\"\nversion = \"1.0.0\"\nschema_version = 1\nlanguages = [\"languages/foo\"]\n",
            "languages": { "foo": { "config.toml": format!("name = \"Foo\"\npath_suffixes = [\"{suffix}\"]\n") } }
        })
    };
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/extensions/foo", files("uncommitted"))
        .await;
    fs.insert_tree(
        "/extensions.transactions/staging/git-source.previous",
        json!({
            "extension_id": "foo",
            "state.toml": toml::to_string(&crate::extension_transaction::GitSourceBackupState {
                checkout_existed: false, installed_existed: true, record: None,
            }).expect("recovery journal state"),
            "installed": files("original"),
        }),
    )
    .await;
    let languages = Arc::new(LanguageRegistry::test(cx.executor()));
    let proxy = Arc::new(ExtensionHostProxy::new());
    language_extension::init(LspAccess::Noop, proxy.clone(), languages.clone());
    let store = cx.update(|cx| {
        HeadlessExtensionStore::new(
            fs.clone(),
            FakeHttpClient::with_200_response(),
            PathBuf::from("/extensions"),
            proxy,
            NodeRuntime::unavailable(),
            cx,
        )
    });
    let missing = store
        .update(cx, |store, cx| {
            store.sync_extensions(
                vec![ExtensionVersion {
                    id: "foo".into(),
                    version: "1.0.0".into(),
                    dev: false,
                    content_fingerprint: None,
                }],
                cx,
            )
        })
        .await
        .expect("recover before loading remote extensions");
    assert!(missing.is_empty());
    assert_eq!(
        languages.language_name_for_extension("original"),
        Some(LanguageName::new("Foo"))
    );
    assert!(
        languages
            .language_name_for_extension("uncommitted")
            .is_none()
    );
    assert!(
        fs.load(Path::new("/extensions/foo/languages/foo/config.toml"))
            .await
            .expect("recovered language config")
            .contains("original")
    );
    assert!(
        !fs.is_dir(Path::new(
            "/extensions.transactions/staging/git-source.previous"
        ))
        .await
    );
}

#[gpui::test]
async fn test_headless_failed_replacement_keeps_uninstall_retryable(cx: &mut TestAppContext) {
    init_test(cx);

    let extension_files = |suffix: &str| {
        json!({
            "extension.toml": r#"
id = "foo"
name = "foo"
version = "1.0.0"
schema_version = 1
languages = ["languages/foo"]
"#,
            "languages": {
                "foo": {
                    "config.toml": format!(
                        "name = \"Foo\"\ngrammar = \"foo\"\npath_suffixes = [\"{suffix}\"]\n"
                    )
                }
            }
        })
    };

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/extensions/foo", extension_files("foo"))
        .await;
    fs.insert_tree("/staged/foo", extension_files("foo2")).await;

    let store = cx.update(|cx| {
        HeadlessExtensionStore::new(
            fs.clone(),
            FakeHttpClient::with_200_response(),
            PathBuf::from("/extensions"),
            Arc::new(ExtensionHostProxy::new()),
            NodeRuntime::unavailable(),
            cx,
        )
    });

    let extension = || ExtensionVersion {
        id: "foo".to_string(),
        version: "1.0.0".to_string(),
        dev: false,
        content_fingerprint: None,
    };

    store
        .update(cx, |store, cx| store.sync_extensions(vec![extension()], cx))
        .await
        .unwrap();

    let publisher = fail_headless_publications(&store, &["foo"], cx);
    let install_result = store
        .update(cx, |store, cx| {
            store.install_extension(extension(), PathBuf::from("/staged/foo"), cx)
        })
        .await;
    assert!(
        install_result.is_err(),
        "a failed replacement should report the error"
    );

    store
        .update(cx, |store, cx| store.sync_extensions(Vec::new(), cx))
        .await
        .unwrap();
    assert!(
        fs.is_dir(Path::new("/extensions/foo")).await,
        "the cleanup retry should leave the files while removal keeps failing"
    );

    publisher.clear_hook();
    store
        .update(cx, |store, cx| store.sync_extensions(Vec::new(), cx))
        .await
        .unwrap();
    assert!(
        !fs.is_dir(Path::new("/extensions/foo")).await,
        "a later sync should retry and complete the cleanup of a failed install"
    );
}

#[gpui::test]
async fn test_headless_unchanged_reinstall_skips_reload(cx: &mut TestAppContext) {
    init_test(cx);

    let extension_files = |suffix: &str| {
        json!({
            "extension.toml": r#"
id = "foo-dev"
name = "foo-dev"
version = "1.0.0"
schema_version = 1
languages = ["languages/foo"]
"#,
            "languages": {
                "foo": {
                    "config.toml": format!(
                        "name = \"Foo\"\ngrammar = \"foo\"\npath_suffixes = [\"{suffix}\"]\n"
                    )
                }
            }
        })
    };

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/extensions/foo-dev", extension_files("foo"))
        .await;
    fs.insert_tree("/staged/foo-dev", extension_files("foo"))
        .await;

    let store = cx.update(|cx| {
        HeadlessExtensionStore::new(
            fs.clone(),
            FakeHttpClient::with_200_response(),
            PathBuf::from("/extensions"),
            Arc::new(ExtensionHostProxy::new()),
            NodeRuntime::unavailable(),
            cx,
        )
    });

    let installed_changed_count = Arc::new(AtomicUsize::new(0));
    cx.update(|cx| {
        let extension_events = extension::ExtensionEvents::try_global(cx)
            .expect("ExtensionEvents should be initialized in tests");
        let installed_changed_count = installed_changed_count.clone();
        cx.subscribe(&extension_events, move |_, event, _cx| {
            if matches!(event, extension::Event::ExtensionsInstalledChanged) {
                installed_changed_count.fetch_add(1, Ordering::SeqCst);
            }
        })
        .detach();
    });

    let dev_extension = || ExtensionVersion {
        id: "foo-dev".to_string(),
        version: "1.0.0".to_string(),
        dev: true,
        content_fingerprint: None,
    };

    store
        .update(cx, |store, cx| {
            store.sync_extensions(vec![dev_extension()], cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();
    assert_eq!(installed_changed_count.load(Ordering::SeqCst), 1);

    store
        .update(cx, |store, cx| {
            store.install_extension(dev_extension(), PathBuf::from("/staged/foo-dev"), cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();
    assert_eq!(
        installed_changed_count.load(Ordering::SeqCst),
        1,
        "reinstalling identical content should not reload the extension or notify"
    );
    assert!(
        !fs.is_dir(Path::new("/staged/foo-dev")).await,
        "a skipped install should clean up the staged upload"
    );
    assert!(
        fs.is_dir(Path::new("/extensions/foo-dev")).await,
        "a skipped install should leave the installed files untouched"
    );
    store.read_with(cx, |store, _cx| {
        assert_eq!(
            store
                .loaded_extensions
                .get("foo-dev")
                .map(|extension| extension.version.as_ref()),
            Some("1.0.0"),
            "a skipped install should keep the extension loaded"
        );
    });

    fs.insert_tree("/staged/foo-dev", extension_files("foo2"))
        .await;
    store
        .update(cx, |store, cx| {
            store.install_extension(dev_extension(), PathBuf::from("/staged/foo-dev"), cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();
    assert_eq!(
        installed_changed_count.load(Ordering::SeqCst),
        2,
        "reinstalling changed content should reload the extension and notify"
    );
}

#[gpui::test]
async fn test_headless_failed_uninstall_is_retried_on_later_syncs(cx: &mut TestAppContext) {
    init_test(cx);

    let extension_files = |id: &str| {
        json!({
            "extension.toml": format!(
                "id = \"{id}\"\nname = \"{id}\"\nversion = \"1.0.0\"\nschema_version = 1\nlanguages = [\"languages/lang\"]\n"
            ),
            "languages": {
                "lang": {
                    "config.toml": format!(
                        "name = \"{id}-lang\"\ngrammar = \"{id}\"\npath_suffixes = [\"{id}\"]\n"
                    )
                }
            }
        })
    };

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        "/extensions",
        json!({
            "ext-a": extension_files("ext-a"),
            "ext-b": extension_files("ext-b"),
            "ext-c": extension_files("ext-c"),
        }),
    )
    .await;

    let store = cx.update(|cx| {
        HeadlessExtensionStore::new(
            fs.clone(),
            FakeHttpClient::with_200_response(),
            PathBuf::from("/extensions"),
            Arc::new(ExtensionHostProxy::new()),
            NodeRuntime::unavailable(),
            cx,
        )
    });

    let extension = |id: &str| ExtensionVersion {
        id: id.to_string(),
        version: "1.0.0".to_string(),
        dev: false,
        content_fingerprint: None,
    };

    store
        .update(cx, |store, cx| {
            store.sync_extensions(
                vec![extension("ext-a"), extension("ext-b"), extension("ext-c")],
                cx,
            )
        })
        .await
        .unwrap();

    let publisher = fail_headless_publications(&store, &["ext-a", "ext-c"], cx);
    store
        .update(cx, |store, cx| {
            store.sync_extensions(vec![extension("ext-b")], cx)
        })
        .await
        .unwrap();
    store.read_with(cx, |store, _cx| {
        assert_eq!(
            store.loaded_extensions.keys().cloned().collect::<Vec<_>>(),
            vec![Arc::<str>::from("ext-b")],
            "failed removals should still evict the extensions"
        );
    });
    assert!(
        fs.is_dir(Path::new("/extensions/ext-a")).await,
        "a failed removal should leave the files on disk"
    );

    let missing = store
        .update(cx, |store, cx| {
            store.sync_extensions(vec![extension("ext-b"), extension("ext-c")], cx)
        })
        .await
        .unwrap();
    assert_eq!(
        missing.len(),
        0,
        "an extension pending removal that the client re-requests should be reloaded from the intact files instead"
    );
    store.read_with(cx, |store, _cx| {
        let mut ids = store.loaded_extensions.keys().cloned().collect::<Vec<_>>();
        ids.sort();
        assert_eq!(
            ids,
            vec![Arc::<str>::from("ext-b"), Arc::<str>::from("ext-c")]
        );
    });
    assert!(
        fs.is_dir(Path::new("/extensions/ext-a")).await,
        "a removal that keeps failing should keep the files on disk"
    );

    publisher.clear_hook();
    store
        .update(cx, |store, cx| {
            store.sync_extensions(vec![extension("ext-b"), extension("ext-c")], cx)
        })
        .await
        .unwrap();
    assert!(
        !fs.is_dir(Path::new("/extensions/ext-a")).await,
        "a later sync should retry and complete the failed removal"
    );
}

#[gpui::test]
async fn test_index_change_without_remote_relevant_extensions_does_not_sync(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    init_test(cx);
    let (store, fs) = create_extension_store(cx);

    let (opts, _counter, sync_count) = setup_mock_remote(cx, server_cx);
    let client = RemoteClient::connect_mock(opts, cx).await;

    store.update(cx, |store, cx| {
        store.register_remote_client(client.clone(), cx)
    });
    cx.run_until_parked();
    assert_eq!(sync_count.load(Ordering::SeqCst), 1);

    fs.insert_tree(
        "/extensions/installed/plain-ext",
        json!({
            "extension.toml": "id = \"plain-ext\"\nname = \"plain-ext\"\nversion = \"1.0.0\"\nschema_version = 1\n",
        }),
    )
    .await;
    store.update(cx, |store, cx| drop(store.reload(None, cx)));
    cx.executor().advance_clock(RELOAD_DEBOUNCE_DURATION);
    cx.run_until_parked();
    assert_eq!(
        sync_count.load(Ordering::SeqCst),
        1,
        "an index change that does not affect remote-synced extensions should not trigger a sync"
    );

    insert_remote_relevant_extension(&fs, "sync-ext").await;
    store.update(cx, |store, cx| drop(store.reload(None, cx)));
    cx.executor().advance_clock(RELOAD_DEBOUNCE_DURATION);
    cx.run_until_parked();
    assert_eq!(
        sync_count.load(Ordering::SeqCst),
        2,
        "an index change that affects remote-synced extensions should trigger a sync"
    );
}

#[gpui::test]
async fn test_uninstalling_extension_restores_surviving_extensions_language(
    cx: &mut TestAppContext,
) {
    init_test(cx);

    let fs = FakeFs::new(cx.executor());
    let language_registry = Arc::new(LanguageRegistry::test(cx.executor()));
    let proxy = Arc::new(ExtensionHostProxy::new());
    language_extension::init(LspAccess::Noop, proxy.clone(), language_registry.clone());

    insert_language_extension(&fs, "ext-a", "Shared", "shared-a").await;
    insert_language_extension(&fs, "ext-b", "Shared", "shared-b").await;

    let store = create_extension_store_with(fs.clone(), proxy, cx);
    assert_eq!(
        language_registry.language_name_for_extension("shared-b"),
        Some(LanguageName::new("Shared")),
        "the extension scanned last should own the language in the index"
    );

    fs.remove_dir(
        Path::new("/extensions/installed/ext-b"),
        RemoveOptions {
            recursive: true,
            ignore_if_not_exists: false,
        },
    )
    .await
    .unwrap();
    store.update(cx, |store, cx| drop(store.reload(None, cx)));
    cx.executor().advance_clock(RELOAD_DEBOUNCE_DURATION);
    cx.run_until_parked();

    assert_eq!(
        language_registry.language_name_for_extension("shared-a"),
        Some(LanguageName::new("Shared")),
        "uninstalling the owning extension should re-register the language from the surviving extension"
    );
    assert_eq!(
        language_registry.language_name_for_extension("shared-b"),
        None,
        "the uninstalled extension's language config should be gone"
    );
}

#[gpui::test]
async fn test_reload_all_reloads_unchanged_extensions(cx: &mut TestAppContext) {
    init_test(cx);

    let fs = FakeFs::new(cx.executor());
    let language_registry = Arc::new(LanguageRegistry::test(cx.executor()));
    let proxy = Arc::new(ExtensionHostProxy::new());
    language_extension::init(LspAccess::Noop, proxy.clone(), language_registry.clone());

    insert_language_extension(&fs, "ext-a", "Alpha", "alpha").await;
    insert_language_extension(&fs, "ext-b", "Beta", "beta").await;

    let store = create_extension_store_with(fs, proxy, cx);
    let installed_changed_count = Arc::new(AtomicUsize::new(0));
    cx.update(|cx| {
        let extension_events = extension::ExtensionEvents::try_global(cx)
            .expect("ExtensionEvents should be initialized in tests");
        let installed_changed_count = installed_changed_count.clone();
        cx.subscribe(&extension_events, move |_, event, _cx| {
            if matches!(event, extension::Event::ExtensionsInstalledChanged) {
                installed_changed_count.fetch_add(1, Ordering::SeqCst);
            }
        })
        .detach();
    });

    let registry_version = language_registry.version();
    store.update(cx, |store, cx| drop(store.reload(None, cx)));
    cx.executor().advance_clock(RELOAD_DEBOUNCE_DURATION);
    cx.run_until_parked();
    assert_eq!(installed_changed_count.load(Ordering::SeqCst), 0);
    assert_eq!(language_registry.version(), registry_version);

    store.update(cx, |store, cx| drop(store.reload_all(cx)));
    cx.executor().advance_clock(RELOAD_DEBOUNCE_DURATION);
    cx.run_until_parked();
    assert_eq!(installed_changed_count.load(Ordering::SeqCst), 1);
    assert!(language_registry.version() > registry_version);
    for suffix in ["alpha", "beta"] {
        assert!(
            language_registry
                .language_name_for_extension(suffix)
                .is_some(),
            "{suffix} should be registered again after the reload"
        );
    }
    assert!(
        store.read_with(cx, |store, _| store.modified_extensions.is_empty()),
        "the reload should consume the extensions it marked as modified"
    );
}

#[gpui::test]
async fn test_extension_cannot_shadow_language_registered_outside_extensions(
    cx: &mut TestAppContext,
) {
    init_test(cx);

    let fs = FakeFs::new(cx.executor());
    let language_registry = Arc::new(LanguageRegistry::test(cx.executor()));
    let proxy = Arc::new(ExtensionHostProxy::new());
    language_extension::init(LspAccess::Noop, proxy.clone(), language_registry.clone());

    language_registry.register_test_language(LanguageConfig {
        name: LanguageName::new("Shared"),
        matcher: Arc::new(LanguageMatcher {
            path_suffixes: vec!["builtin".into()],
            ..LanguageMatcher::default()
        }),
        ..LanguageConfig::default()
    });

    let store = create_extension_store_with(fs.clone(), proxy, cx);

    insert_language_extension(&fs, "ext-a", "Shared", "shadowed").await;
    store.update(cx, |store, cx| drop(store.reload(None, cx)));
    cx.executor().advance_clock(RELOAD_DEBOUNCE_DURATION);
    cx.run_until_parked();

    assert_eq!(
        language_registry.language_name_for_extension("builtin"),
        Some(LanguageName::new("Shared")),
        "an extension must not replace a language registered outside of extensions"
    );
    assert_eq!(
        language_registry.language_name_for_extension("shadowed"),
        None,
        "the shadowing extension's language config should not be registered"
    );

    fs.remove_dir(
        Path::new("/extensions/installed/ext-a"),
        RemoveOptions {
            recursive: true,
            ignore_if_not_exists: false,
        },
    )
    .await
    .unwrap();
    store.update(cx, |store, cx| drop(store.reload(None, cx)));
    cx.executor().advance_clock(RELOAD_DEBOUNCE_DURATION);
    cx.run_until_parked();

    assert_eq!(
        language_registry.language_name_for_extension("builtin"),
        Some(LanguageName::new("Shared")),
        "uninstalling the shadowing extension must not remove the language it never registered"
    );
}

#[gpui::test]
async fn test_headless_extension_cannot_shadow_language_registered_outside_extensions(
    cx: &mut TestAppContext,
) {
    init_test(cx);

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        "/extensions/ext-a",
        json!({
            "extension.toml": r#"
id = "ext-a"
name = "ext-a"
version = "1.0.0"
schema_version = 1
languages = ["languages/shared"]
"#,
            "languages": {
                "shared": {
                    "config.toml": "name = \"Shared\"\npath_suffixes = [\"shadowed\"]\n"
                }
            }
        }),
    )
    .await;

    let language_registry = Arc::new(LanguageRegistry::test(cx.executor()));
    let proxy = Arc::new(ExtensionHostProxy::new());
    language_extension::init(LspAccess::Noop, proxy.clone(), language_registry.clone());

    language_registry.register_test_language(LanguageConfig {
        name: LanguageName::new("Shared"),
        matcher: Arc::new(LanguageMatcher {
            path_suffixes: vec!["builtin".into()],
            ..LanguageMatcher::default()
        }),
        ..LanguageConfig::default()
    });

    let store = cx.update(|cx| {
        HeadlessExtensionStore::new(
            fs.clone(),
            FakeHttpClient::with_200_response(),
            PathBuf::from("/extensions"),
            proxy,
            NodeRuntime::unavailable(),
            cx,
        )
    });

    let extension = || {
        vec![ExtensionVersion {
            id: "ext-a".to_string(),
            version: "1.0.0".to_string(),
            dev: false,
            content_fingerprint: None,
        }]
    };

    store
        .update(cx, |store, cx| store.sync_extensions(extension(), cx))
        .await
        .unwrap();
    assert_eq!(
        language_registry.language_name_for_extension("builtin"),
        Some(LanguageName::new("Shared")),
        "an extension must not replace a language registered outside of extensions"
    );
    assert_eq!(
        language_registry.language_name_for_extension("shadowed"),
        None,
        "the shadowing extension's language config should not be registered"
    );

    store
        .update(cx, |store, cx| store.sync_extensions(Vec::new(), cx))
        .await
        .unwrap();
    assert_eq!(
        language_registry.language_name_for_extension("builtin"),
        Some(LanguageName::new("Shared")),
        "unloading the shadowing extension must not remove the language it never registered"
    );
}

#[gpui::test]
async fn test_hash_directory_contents_hashes_symlinks_by_target(cx: &mut TestAppContext) {
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        "/targets",
        json!({
            "target-1": "one",
            "target-2": "two",
        }),
    )
    .await;
    fs.insert_tree(
        "/dir",
        json!({
            "file.txt": "contents",
        }),
    )
    .await;
    fs.insert_symlink("/dir/link", PathBuf::from("/targets/target-1"))
        .await;

    let fs_trait_object = fs.clone() as Arc<dyn Fs>;
    let first = hash_directory_contents(&fs_trait_object, Path::new("/dir"))
        .await
        .unwrap();
    let second = hash_directory_contents(&fs_trait_object, Path::new("/dir"))
        .await
        .unwrap();
    assert_eq!(
        first, second,
        "fingerprinting a directory containing a symlink should be deterministic"
    );

    fs.insert_symlink("/dir/link", PathBuf::from("/targets/target-2"))
        .await;
    let third = hash_directory_contents(&fs_trait_object, Path::new("/dir"))
        .await
        .unwrap();
    assert_ne!(
        first, third,
        "changing a symlink's target should change the fingerprint"
    );
}

struct SyncRequestCounter {
    sync_count: Arc<AtomicUsize>,
    failures_remaining: usize,
    installed_fingerprints: HashMap<String, Option<u64>>,
}

fn setup_mock_remote(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) -> (
    RemoteConnectionOptions,
    Entity<SyncRequestCounter>,
    Arc<AtomicUsize>,
) {
    setup_mock_remote_with_failures(cx, server_cx, 0)
}

fn setup_mock_remote_with_failures(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
    failures: usize,
) -> (
    RemoteConnectionOptions,
    Entity<SyncRequestCounter>,
    Arc<AtomicUsize>,
) {
    let sync_count = Arc::new(AtomicUsize::new(0));
    let counter = server_cx.new(|_| SyncRequestCounter {
        sync_count: sync_count.clone(),
        failures_remaining: failures,
        installed_fingerprints: HashMap::default(),
    });
    let (opts, server_client, _) = RemoteClient::fake_server(cx, server_cx);
    register_server_handlers(&server_client, counter.clone());
    (opts, counter, sync_count)
}

fn setup_mock_remote_with_install_failures(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
    failures: usize,
) -> (
    RemoteConnectionOptions,
    Entity<SyncRequestCounter>,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
) {
    let sync_count = Arc::new(AtomicUsize::new(0));
    let install_count = Arc::new(AtomicUsize::new(0));
    let counter = server_cx.new(|_| SyncRequestCounter {
        sync_count: sync_count.clone(),
        failures_remaining: failures,
        installed_fingerprints: HashMap::default(),
    });
    let (opts, server_client, _) = RemoteClient::fake_server(cx, server_cx);
    register_ping_handler(&server_client, &counter);
    register_sync_handler(
        &server_client,
        &counter,
        SyncHandlerBehavior {
            fail_from_counter: false,
            echo_missing_extensions: true,
        },
    );
    server_client.add_request_handler::<proto::InstallExtension, SyncRequestCounter, _, _>(
        counter.downgrade(),
        {
            let install_count = install_count.clone();
            move |counter, _envelope: TypedEnvelope<proto::InstallExtension>, mut cx| {
                let install_count = install_count.clone();
                async move {
                    install_count.fetch_add(1, Ordering::SeqCst);
                    let should_fail = counter.update(&mut cx, |counter, _cx| {
                        if counter.failures_remaining > 0 {
                            counter.failures_remaining -= 1;
                            true
                        } else {
                            false
                        }
                    });
                    anyhow::ensure!(!should_fail, "simulated install failure");
                    Ok(proto::Ack {})
                }
            }
        },
    );
    (opts, counter, sync_count, install_count)
}

fn setup_mock_remote_tracking_fingerprints(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) -> (
    RemoteConnectionOptions,
    Entity<SyncRequestCounter>,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
) {
    let sync_count = Arc::new(AtomicUsize::new(0));
    let install_count = Arc::new(AtomicUsize::new(0));
    let counter = server_cx.new(|_| SyncRequestCounter {
        sync_count: sync_count.clone(),
        failures_remaining: 0,
        installed_fingerprints: HashMap::default(),
    });
    let (opts, server_client, _) = RemoteClient::fake_server(cx, server_cx);
    register_ping_handler(&server_client, &counter);
    server_client.add_request_handler::<proto::SyncExtensions, SyncRequestCounter, _, _>(
        counter.downgrade(),
        |counter, envelope: TypedEnvelope<proto::SyncExtensions>, mut cx| async move {
            let missing_extensions = counter.update(&mut cx, |counter, _cx| {
                counter.sync_count.fetch_add(1, Ordering::SeqCst);
                envelope
                    .payload
                    .extensions
                    .into_iter()
                    .filter(|extension| {
                        let installed = counter
                            .installed_fingerprints
                            .get(&extension.id)
                            .copied()
                            .flatten();
                        extension.content_fingerprint.is_none()
                            || installed != extension.content_fingerprint
                    })
                    .collect()
            });
            Ok(proto::SyncExtensionsResponse {
                missing_extensions,
                tmp_dir: "/remote-tmp".to_string(),
            })
        },
    );
    server_client.add_request_handler::<proto::InstallExtension, SyncRequestCounter, _, _>(
        counter.downgrade(),
        {
            let install_count = install_count.clone();
            move |counter, envelope: TypedEnvelope<proto::InstallExtension>, mut cx| {
                let install_count = install_count.clone();
                async move {
                    install_count.fetch_add(1, Ordering::SeqCst);
                    if let Some(extension) = envelope.payload.extension {
                        counter.update(&mut cx, |counter, _cx| {
                            counter
                                .installed_fingerprints
                                .insert(extension.id, extension.content_fingerprint);
                        });
                    }
                    Ok(proto::Ack {})
                }
            }
        },
    );
    (opts, counter, sync_count, install_count)
}

fn setup_mock_remote_hanging(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) -> (
    RemoteConnectionOptions,
    Entity<SyncRequestCounter>,
    Arc<AtomicUsize>,
) {
    let sync_count = Arc::new(AtomicUsize::new(0));
    let counter = server_cx.new(|_| SyncRequestCounter {
        sync_count: sync_count.clone(),
        failures_remaining: 0,
        installed_fingerprints: HashMap::default(),
    });
    let (opts, server_client, _) = RemoteClient::fake_server(cx, server_cx);
    register_ping_handler(&server_client, &counter);
    server_client.add_request_handler::<proto::SyncExtensions, SyncRequestCounter, _, _>(
        counter.downgrade(),
        |counter, _envelope: TypedEnvelope<proto::SyncExtensions>, mut cx| async move {
            counter.update(&mut cx, |counter, _cx| {
                counter.sync_count.fetch_add(1, Ordering::SeqCst)
            });
            drop(counter);
            futures::future::pending::<()>().await;
            Ok(proto::SyncExtensionsResponse {
                missing_extensions: Vec::new(),
                tmp_dir: String::new(),
            })
        },
    );
    (opts, counter, sync_count)
}

fn register_ping_handler(server_client: &AnyProtoClient, counter: &Entity<SyncRequestCounter>) {
    server_client.add_request_handler::<proto::Ping, SyncRequestCounter, _, _>(
        counter.downgrade(),
        |_counter, _envelope: TypedEnvelope<proto::Ping>, _cx| async move { Ok(proto::Ack {}) },
    );
}

fn register_server_handlers(server_client: &AnyProtoClient, counter: Entity<SyncRequestCounter>) {
    register_ping_handler(server_client, &counter);
    register_sync_handler(
        server_client,
        &counter,
        SyncHandlerBehavior {
            fail_from_counter: true,
            echo_missing_extensions: false,
        },
    );
}

#[derive(Clone, Copy)]
struct SyncHandlerBehavior {
    fail_from_counter: bool,
    echo_missing_extensions: bool,
}

fn register_sync_handler(
    server_client: &AnyProtoClient,
    counter: &Entity<SyncRequestCounter>,
    behavior: SyncHandlerBehavior,
) {
    server_client.add_request_handler::<proto::SyncExtensions, SyncRequestCounter, _, _>(
        counter.downgrade(),
        move |counter, envelope: TypedEnvelope<proto::SyncExtensions>, mut cx| async move {
            let should_fail = counter.update(&mut cx, |counter, _cx| {
                counter.sync_count.fetch_add(1, Ordering::SeqCst);
                if behavior.fail_from_counter && counter.failures_remaining > 0 {
                    counter.failures_remaining -= 1;
                    true
                } else {
                    false
                }
            });
            anyhow::ensure!(!should_fail, "simulated sync failure");
            Ok(proto::SyncExtensionsResponse {
                missing_extensions: if behavior.echo_missing_extensions {
                    envelope.payload.extensions
                } else {
                    Vec::new()
                },
                tmp_dir: "/remote-tmp".to_string(),
            })
        },
    );
}

fn create_extension_store(cx: &mut TestAppContext) -> (Entity<ExtensionStore>, Arc<FakeFs>) {
    let fs = FakeFs::new(cx.executor());
    let store = create_extension_store_with(fs.clone(), Arc::new(ExtensionHostProxy::new()), cx);
    (store, fs)
}

fn insert_remote_sync_index_entry(store: &Entity<ExtensionStore>, cx: &mut TestAppContext) {
    store.update(cx, |store, _cx| {
        store.extension_index.extensions.insert(
            Arc::from("foo-lsp"),
            remote_sync_entry(
                "foo-lsp",
                r#"
                [language_servers.foo]
                language = "Foo"
                "#,
            ),
        );
    });
}

fn advance_through_sync_backoff(cx: &mut TestAppContext) {
    for attempts in 0..8 {
        cx.executor()
            .advance_clock(remote_sync_retry_delay(attempts));
        cx.run_until_parked();
    }
}

fn create_extension_store_with(
    fs: Arc<FakeFs>,
    proxy: Arc<ExtensionHostProxy>,
    cx: &mut TestAppContext,
) -> Entity<ExtensionStore> {
    create_extension_store_at(fs, proxy, PathBuf::from("/extensions"), cx)
}

fn create_extension_store_at(
    fs: Arc<FakeFs>,
    proxy: Arc<ExtensionHostProxy>,
    directory: PathBuf,
    cx: &mut TestAppContext,
) -> Entity<ExtensionStore> {
    let http_client = FakeHttpClient::with_200_response();
    let node_runtime = NodeRuntime::unavailable();

    let store = cx.new(|cx| {
        ExtensionStore::new(
            directory,
            None,
            proxy,
            fs,
            http_client.clone(),
            http_client,
            None,
            node_runtime,
            cx,
        )
    });
    cx.run_until_parked();
    cx.executor().advance_clock(RELOAD_DEBOUNCE_DURATION);
    cx.run_until_parked();

    store
}

async fn insert_remote_relevant_extension(fs: &Arc<FakeFs>, id: &str) {
    fs.insert_tree(
        format!("/extensions/installed/{id}"),
        json!({
            "extension.toml": format!(
                "id = \"{id}\"\nname = \"{id}\"\nversion = \"1.0.0\"\nschema_version = 1\n\n[language_servers.{id}-lsp]\nlanguage = \"{id}-lang\"\n"
            ),
        }),
    )
    .await;
}

async fn insert_language_extension(fs: &Arc<FakeFs>, id: &str, language: &str, suffix: &str) {
    fs.insert_tree(
        format!("/extensions/installed/{id}"),
        json!({
            "extension.toml": format!(
                "id = \"{id}\"\nname = \"{id}\"\nversion = \"1.0.0\"\nschema_version = 1\nlanguages = [\"languages/lang\"]\n"
            ),
            "languages": {
                "lang": {
                    "config.toml": format!(
                        "name = \"{language}\"\npath_suffixes = [\"{suffix}\"]\n"
                    )
                }
            }
        }),
    )
    .await;
}

async fn insert_collision_extension(fs: &Arc<FakeFs>, id: &str, language: &str) {
    insert_language_extension(fs, id, language, "collision").await;
    fs.insert_tree(
        format!("/extensions/installed/{id}"),
        json!({
            "extension.toml": format!(
                "id = \"{id}\"\nname = \"{id}\"\nversion = \"1.0.0\"\nschema_version = 1\n[grammars.shared]\nrepository = \"https://example.com/grammar\"\nrev = \"same-revision\"\n"
            ),
            "grammars": {"shared.wasm": ""},
        }),
    ).await;
}

#[gpui::test]
async fn test_grammar_ownership_through_extension_store_reload_and_uninstall(
    cx: &mut TestAppContext,
) {
    let result: anyhow::Result<()> = async {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        let proxy = Arc::new(ExtensionHostProxy::new());
        let registry = Arc::new(LanguageRegistry::test(cx.executor()));
        language_extension::init(LspAccess::Noop, proxy.clone(), registry.clone());
        let tree = TempTree::new(json!({"installed": {"html": {"grammars": {}}, "html-jinja": {"grammars": {}}}}));
        for (extension, name, binary, injections) in [
            ("html", "HTML", include_bytes!("../../language/test_data/grammars/html.wasm").as_slice(), ""),
            ("html-jinja", "HTML-Jinja", include_bytes!("../../language/test_data/grammars/html-jinja.wasm").as_slice(), "((jinja) @content (#set! \"language\" \"Jinja-Inline\"))"),
        ] {
            let directory = tree.path().join("installed").join(extension);
            std::fs::write(directory.join("grammars/html.wasm"), binary)?;
            fs.insert_tree(&directory, json!({
                "extension.toml": format!("id = \"{extension}\"\nname = \"{name}\"\nversion = \"1.0.0\"\nschema_version = 1\n[grammars.html]\nrepository = \"https://example.com/{extension}\"\nrev = \"revision\"\n"),
                "grammars": {"html.wasm": ""},
                "languages": {"html": {
                    "config.toml": format!("name = \"{name}\"\ngrammar = \"html\"\npath_suffixes = [\"{extension}\"]\n"),
                    "injections.scm": injections,
                }},
            })).await;
        }
        let store = create_extension_store_at(fs.clone(), proxy.clone(), tree.path().into(), cx);
        assert!(store.update(cx, |store, _| store.take_language_collision_warnings()).is_empty());
        drop(store);
        let store = create_extension_store_at(fs.clone(), proxy, tree.path().into(), cx);

        store.update(cx, |store, cx| store.reload(Some("html".into()), cx)).await;
        let jinja = registry.language_for_name("HTML-Jinja").await?;
        let html = registry.language_for_name("HTML").await?;
        assert_ne!(jinja.grammar().context("Jinja grammar")?.ts_language, html.grammar().context("HTML grammar")?.ts_language);
        assert!(store.update(cx, |store, _| store.take_language_collision_warnings()).is_empty());

        fs.remove_dir(&tree.path().join("installed/html"), RemoveOptions {recursive: true, ..Default::default()}).await?;
        store.update(cx, |store, cx| store.reload(None, cx)).await;
        registry.reload();
        registry.language_for_name("HTML-Jinja").await?;
        assert!(!registry.language_names().contains(&"HTML".into()));
        fs.remove_dir(&tree.path().join("installed/html-jinja"), RemoveOptions {recursive: true, ..Default::default()}).await?;
        store.update(cx, |store, cx| store.reload(None, cx)).await;
        assert!(!registry.grammar_names().iter().any(|name| name.as_ref() == "html"));
        Ok(())
    }.await;
    assert!(result.is_ok(), "{result:?}");
}

#[gpui::test]
async fn test_language_collision_startup_and_cached_index(cx: &mut TestAppContext) {
    let result: anyhow::Result<()> = async {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        let proxy = Arc::new(ExtensionHostProxy::new());
        let registry = Arc::new(LanguageRegistry::test(cx.executor()));
        language_extension::init(LspAccess::Noop, proxy.clone(), registry);
        insert_collision_extension(&fs, "alpha", "Shared").await;
        insert_collision_extension(&fs, "beta", "Shared").await;
        let store = create_extension_store_with(fs.clone(), proxy.clone(), cx);
        let warnings = store.update(cx, |store, _| store.take_language_collision_warnings());
        assert_eq!(warnings.len(), 1);
        let warning = warnings.first().context("missing collision")?;
        assert_eq!(
            warning.providers,
            ("alpha (alpha, v1.0.0)".into(), "beta (beta, v1.0.0)".into())
        );
        assert_eq!(warning.resources.len(), 1);
        assert!(
            warning
                .resources
                .iter()
                .any(|resource| resource.contains("language \"Shared\""))
        );
        assert!(
            store
                .update(cx, |store, _| store.take_language_collision_warnings())
                .is_empty()
        );
        let reads = fs.read_dir_call_count();
        drop(store);

        let store = create_extension_store_with(fs.clone(), proxy.clone(), cx);
        assert_eq!(
            fs.read_dir_call_count(),
            reads,
            "startup should use the cached provider declarations"
        );
        assert_eq!(
            store.update(cx, |store, _| store.take_language_collision_warnings()),
            warnings
        );
        drop(store);

        let mut old_index: serde_json::Value =
            serde_json::from_str(&fs.load(Path::new("/extensions/index.json")).await?)?;
        old_index
            .as_object_mut()
            .context("expected index object")?
            .remove("language_providers");
        let content = serde_json::to_string(&old_index)?;
        fs.save(
            Path::new("/extensions/index.json"),
            &content.as_str().into(),
            Default::default(),
        )
        .await?;
        let store = create_extension_store_with(fs.clone(), proxy, cx);
        assert!(
            fs.read_dir_call_count() > reads,
            "legacy indexes must be rebuilt to recover shadowed providers"
        );
        assert_eq!(
            store.update(cx, |store, _| store.take_language_collision_warnings()),
            warnings
        );
        Ok(())
    }
    .await;
    assert!(result.is_ok(), "{result:?}");
}

#[gpui::test]
async fn test_language_collision_install_reload_and_uninstall(cx: &mut TestAppContext) {
    let result: anyhow::Result<()> = async {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        let proxy = Arc::new(ExtensionHostProxy::new());
        let registry = Arc::new(LanguageRegistry::test(cx.executor()));
        language_extension::init(LspAccess::Noop, proxy.clone(), registry.clone());
        insert_collision_extension(&fs, "alpha", "Shared").await;
        let store = create_extension_store_with(fs.clone(), proxy, cx);
        store
            .update(cx, |store, cx| store.reload(Some("alpha".into()), cx))
            .await;
        assert!(
            store
                .update(cx, |store, _| store.take_language_collision_warnings())
                .is_empty()
        );

        insert_collision_extension(&fs, "beta", "Shared").await;
        store.update(cx, |store, cx| store.reload(None, cx)).await;
        assert_eq!(
            store
                .update(cx, |store, _| store.take_language_collision_warnings())
                .len(),
            1
        );

        // Reloading one provider must preserve the other provider's grammar.
        store
            .update(cx, |store, cx| store.reload(Some("alpha".into()), cx))
            .await;
        assert!(
            registry
                .grammar_names()
                .iter()
                .any(|name| name.as_ref() == "shared")
        );
        assert!(
            store
                .update(cx, |store, _| store.take_language_collision_warnings())
                .is_empty()
        );

        fs.remove_dir(
            Path::new("/extensions/installed/alpha"),
            RemoveOptions {
                recursive: true,
                ..Default::default()
            },
        )
        .await?;
        store.update(cx, |store, cx| store.reload(None, cx)).await;
        assert!(
            registry
                .grammar_names()
                .iter()
                .any(|name| name.as_ref() == "shared")
        );
        assert!(
            store
                .update(cx, |store, _| store.take_language_collision_warnings())
                .is_empty()
        );

        insert_collision_extension(&fs, "alpha", "Shared").await;
        store.update(cx, |store, cx| store.reload(None, cx)).await;
        assert!(
            store
                .update(cx, |store, _| store.take_language_collision_warnings())
                .is_empty()
        );
        insert_collision_extension(&fs, "gamma", "Shared").await;
        store.update(cx, |store, cx| store.reload(None, cx)).await;
        assert_eq!(
            store
                .update(cx, |store, _| store.take_language_collision_warnings())
                .len(),
            2
        );
        Ok(())
    }
    .await;
    assert!(result.is_ok(), "{result:?}");
}

#[gpui::test]
async fn test_language_collision_native_and_pending_warning_removal(cx: &mut TestAppContext) {
    let result: anyhow::Result<()> = async {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        let proxy = Arc::new(ExtensionHostProxy::new());
        let registry = Arc::new(LanguageRegistry::test(cx.executor()));
        let language = language::json_lang();
        registry.register_native_grammars([(
            "shared",
            language
                .grammar()
                .context("missing JSON grammar")?
                .ts_language
                .clone(),
        )]);
        language_extension::init(LspAccess::Noop, proxy.clone(), registry.clone());
        insert_collision_extension(&fs, "alpha", "Plain Text").await;
        let store = create_extension_store_with(fs.clone(), proxy, cx);
        let warnings = store.update(cx, |store, _| store.take_language_collision_warnings());
        assert_eq!(warnings.len(), 1);
        let warning = warnings.first().context("missing native collision")?;
        assert_eq!(warning.resources.len(), 1);
        assert!(
            warning
                .resources
                .iter()
                .all(|resource| resource.contains("active provider: Zed built-in support"))
        );
        assert!(registry.is_native_language(&"Plain Text".into()));
        assert!(registry.is_native_grammar("shared"));

        insert_collision_extension(&fs, "beta", "Plain Text").await;
        store.update(cx, |store, cx| store.reload(None, cx)).await;
        fs.remove_dir(
            Path::new("/extensions/installed/beta"),
            RemoveOptions {
                recursive: true,
                ..Default::default()
            },
        )
        .await?;
        store.update(cx, |store, cx| store.reload(None, cx)).await;
        assert!(
            store
                .update(cx, |store, _| store.take_language_collision_warnings())
                .is_empty(),
            "removed providers must not produce stale pending notifications"
        );
        assert!(registry.is_native_grammar("shared"));
        Ok(())
    }
    .await;
    assert!(result.is_ok(), "{result:?}");
}

#[gpui::test]
async fn test_grammar_collision_sources(cx: &mut TestAppContext) {
    let result: anyhow::Result<()> = async {
        init_test(cx);
        for different_source in [
            "repository = \"https://example.com/other\"\nrev = \"same-revision\"",
            "repository = \"https://example.com/grammar\"\nrev = \"other-revision\"",
            "repository = \"https://example.com/grammar\"\nrev = \"same-revision\"\npath = \"subdirectory\"",
        ] {
            let fs = FakeFs::new(cx.executor());
            let proxy = Arc::new(ExtensionHostProxy::new());
            let registry = Arc::new(LanguageRegistry::test(cx.executor()));
            language_extension::init(LspAccess::Noop, proxy.clone(), registry);
            insert_collision_extension(&fs, "alpha", "Alpha").await;
            insert_collision_extension(&fs, "beta", "Beta").await;
            let store = create_extension_store_with(fs.clone(), proxy.clone(), cx);
            assert!(store.update(cx, |store, _| store.take_language_collision_warnings()).is_empty());
            drop(store);
            let store = create_extension_store_with(fs.clone(), proxy, cx);
            assert!(store.update(cx, |store, _| store.take_language_collision_warnings()).is_empty(), "cached identical sources must not warn");

            for id in ["gamma", "delta"] {
                insert_collision_extension(&fs, id, id).await;
                fs.insert_file(format!("/extensions/installed/{id}/extension.toml"), format!(
                    "id = \"{id}\"\nname = \"{id}\"\nversion = \"1.0.0\"\nschema_version = 1\n[grammars.shared]\n{different_source}\n"
                ).into_bytes()).await;
            }
            store.update(cx, |store, cx| store.reload(None, cx)).await;
            let warnings = store.update(cx, |store, _| store.take_language_collision_warnings());
            assert!(warnings.is_empty(), "different sources are safe when their grammars are isolated");

            for id in ["gamma", "delta"] {
                insert_collision_extension(&fs, id, id).await;
            }
            store.update(cx, |store, cx| store.reload(None, cx)).await;
            assert!(store.read_with(cx, |store, _| store.active_language_collision_warnings().is_empty()), "matching sources must resolve delivered warnings");
            assert!(store.update(cx, |store, _| store.take_language_collision_warnings()).is_empty());
            fs.insert_file("/extensions/installed/gamma/extension.toml", format!(
                "id = \"gamma\"\nname = \"gamma\"\nversion = \"1.0.0\"\nschema_version = 1\n[grammars.shared]\n{different_source}\n"
            ).into_bytes()).await;
            store.update(cx, |store, cx| store.reload(None, cx)).await;
            let warnings = store.update(cx, |store, _| store.take_language_collision_warnings());
            assert!(warnings.is_empty(), "changing one provider's source must preserve isolation");
        }
        Ok(())
    }.await;
    assert!(result.is_ok(), "{result:?}");
}

struct FakeExtension;

#[async_trait]
impl Extension for FakeExtension {
    fn manifest(&self) -> Arc<ExtensionManifest> {
        Arc::new(ExtensionManifest {
            id: "fake-extension".into(),
            name: "Fake Extension".to_string(),
            version: "1.0.0".into(),
            schema_version: SchemaVersion(1),
            description: None,
            repository: None,
            authors: Vec::new(),
            lib: LibManifestEntry::default(),
            themes: Vec::new(),
            icon_themes: Vec::new(),
            languages: Vec::new(),
            grammars: BTreeMap::default(),
            language_servers: BTreeMap::default(),
            context_servers: BTreeMap::default(),
            slash_commands: BTreeMap::default(),
            snippets: None,
            capabilities: Vec::new(),
            debug_adapters: BTreeMap::default(),
            debug_locators: BTreeMap::default(),
            language_model_providers: BTreeMap::default(),
        })
    }

    fn work_dir(&self) -> Arc<Path> {
        Arc::from(Path::new("/fake-extension-work-dir"))
    }

    async fn language_server_command(
        &self,
        _language_server_id: LanguageServerName,
        _language_name: LanguageName,
        _worktree: Arc<dyn WorktreeDelegate>,
        _language_server_status_source: EntityId,
    ) -> anyhow::Result<Command> {
        anyhow::bail!("not supported by FakeExtension")
    }

    async fn language_server_initialization_options(
        &self,
        _language_server_id: LanguageServerName,
        _language_name: LanguageName,
        _worktree: Arc<dyn WorktreeDelegate>,
        _language_server_status_source: EntityId,
    ) -> anyhow::Result<Option<String>> {
        anyhow::bail!("not supported by FakeExtension")
    }

    async fn language_server_workspace_configuration(
        &self,
        _language_server_id: LanguageServerName,
        _worktree: Arc<dyn WorktreeDelegate>,
        _language_server_status_source: EntityId,
    ) -> anyhow::Result<Option<String>> {
        anyhow::bail!("not supported by FakeExtension")
    }

    async fn language_server_initialization_options_schema(
        &self,
        _language_server_id: LanguageServerName,
        _worktree: Arc<dyn WorktreeDelegate>,
        _language_server_status_source: EntityId,
    ) -> anyhow::Result<Option<String>> {
        anyhow::bail!("not supported by FakeExtension")
    }

    async fn language_server_workspace_configuration_schema(
        &self,
        _language_server_id: LanguageServerName,
        _worktree: Arc<dyn WorktreeDelegate>,
        _language_server_status_source: EntityId,
    ) -> anyhow::Result<Option<String>> {
        anyhow::bail!("not supported by FakeExtension")
    }

    async fn language_server_additional_initialization_options(
        &self,
        _language_server_id: LanguageServerName,
        _target_language_server_id: LanguageServerName,
        _worktree: Arc<dyn WorktreeDelegate>,
        _language_server_status_source: EntityId,
    ) -> anyhow::Result<Option<String>> {
        anyhow::bail!("not supported by FakeExtension")
    }

    async fn language_server_additional_workspace_configuration(
        &self,
        _language_server_id: LanguageServerName,
        _target_language_server_id: LanguageServerName,
        _worktree: Arc<dyn WorktreeDelegate>,
        _language_server_status_source: EntityId,
    ) -> anyhow::Result<Option<String>> {
        anyhow::bail!("not supported by FakeExtension")
    }

    async fn labels_for_completions(
        &self,
        _language_server_id: LanguageServerName,
        _completions: Vec<Completion>,
    ) -> anyhow::Result<Vec<Option<CodeLabel>>> {
        anyhow::bail!("not supported by FakeExtension")
    }

    async fn labels_for_symbols(
        &self,
        _language_server_id: LanguageServerName,
        _symbols: Vec<Symbol>,
    ) -> anyhow::Result<Vec<Option<CodeLabel>>> {
        anyhow::bail!("not supported by FakeExtension")
    }

    async fn complete_slash_command_argument(
        &self,
        _command: SlashCommand,
        _arguments: Vec<String>,
    ) -> anyhow::Result<Vec<SlashCommandArgumentCompletion>> {
        anyhow::bail!("not supported by FakeExtension")
    }

    async fn run_slash_command(
        &self,
        _command: SlashCommand,
        _arguments: Vec<String>,
        _worktree: Option<Arc<dyn WorktreeDelegate>>,
    ) -> anyhow::Result<SlashCommandOutput> {
        anyhow::bail!("not supported by FakeExtension")
    }

    async fn context_server_command(
        &self,
        _context_server_id: Arc<str>,
        _project: Arc<dyn ProjectDelegate>,
    ) -> anyhow::Result<Command> {
        anyhow::bail!("not supported by FakeExtension")
    }

    async fn context_server_configuration(
        &self,
        _context_server_id: Arc<str>,
        _project: Arc<dyn ProjectDelegate>,
    ) -> anyhow::Result<Option<ContextServerConfiguration>> {
        anyhow::bail!("not supported by FakeExtension")
    }

    async fn suggest_docs_packages(&self, _provider: Arc<str>) -> anyhow::Result<Vec<String>> {
        anyhow::bail!("not supported by FakeExtension")
    }

    async fn index_docs(
        &self,
        _provider: Arc<str>,
        _package_name: Arc<str>,
        _kv_store: Arc<dyn KeyValueStoreDelegate>,
    ) -> anyhow::Result<()> {
        anyhow::bail!("not supported by FakeExtension")
    }

    async fn get_dap_binary(
        &self,
        _dap_name: Arc<str>,
        _config: DebugTaskDefinition,
        _user_installed_path: Option<PathBuf>,
        _worktree: Arc<dyn WorktreeDelegate>,
    ) -> anyhow::Result<DebugAdapterBinary> {
        anyhow::bail!("not supported by FakeExtension")
    }

    async fn dap_request_kind(
        &self,
        _dap_name: Arc<str>,
        _config: serde_json::Value,
    ) -> anyhow::Result<StartDebuggingRequestArgumentsRequest> {
        anyhow::bail!("not supported by FakeExtension")
    }

    async fn dap_config_to_scenario(
        &self,
        _config: ZedDebugConfig,
    ) -> anyhow::Result<DebugScenario> {
        anyhow::bail!("not supported by FakeExtension")
    }

    async fn dap_locator_create_scenario(
        &self,
        _locator_name: String,
        _build_config_template: BuildTaskTemplate,
        _resolved_label: String,
        _debug_adapter_name: String,
    ) -> anyhow::Result<Option<DebugScenario>> {
        anyhow::bail!("not supported by FakeExtension")
    }

    async fn run_dap_locator(
        &self,
        _locator_name: String,
        _config: SpawnInTerminal,
    ) -> anyhow::Result<DebugRequest> {
        anyhow::bail!("not supported by FakeExtension")
    }
}

fn git_in(directory: &Path, args: &[&str]) -> String {
    let output = futures::executor::block_on(
        util::command::new_command("git")
            .args(args)
            .current_dir(directory)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .output(),
    )
    .expect("git should run");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

struct GitExtensionRemote {
    _tree: TempTree,
    url: String,
    bare: PathBuf,
    seed: PathBuf,
    subdirectory: Option<&'static str>,
}

fn tree_root(tree: &TempTree) -> PathBuf {
    SanitizedPath::new(tree.path()).as_path().to_path_buf()
}

impl GitExtensionRemote {
    fn new(extension_id: &str, subdirectory: Option<&'static str>) -> Self {
        let tree = TempTree::new(json!({ "remote.git": {}, "seed": {} }));
        let root = tree_root(&tree);
        let bare = root.join("remote.git");
        let seed = root.join("seed");
        git_in(
            &bare,
            &["-c", "init.defaultBranch=master", "init", "--bare"],
        );
        git_in(&seed, &["-c", "init.defaultBranch=master", "init"]);
        let remote = Self {
            _tree: tree,
            url: bare.to_string_lossy().into_owned(),
            bare,
            seed,
            subdirectory,
        };
        remote.publish(extension_id, "0.1.0");
        remote
    }

    fn publish(&self, extension_id: &str, version: &str) -> String {
        self.publish_with_manifest(extension_id, version, "")
    }

    fn publish_with_manifest(&self, extension_id: &str, version: &str, extra: &str) -> String {
        let directory = match self.subdirectory {
            Some(subdirectory) => self.seed.join(subdirectory),
            None => self.seed.clone(),
        };
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join("extension.toml"),
            format!(
                "id = \"{extension_id}\"\nname = \"Git Extension\"\nversion = \"{version}\"\nschema_version = 1\n{extra}"
            ),
        )
        .unwrap();
        git_in(&self.seed, &["add", "-A"]);
        git_in(&self.seed, &["commit", "-m", version]);
        git_in(
            &self.seed,
            &["push", &self.bare.to_string_lossy(), "HEAD:master"],
        );
        git_in(&self.seed, &["rev-parse", "HEAD"])
    }

    fn tag(&self, name: &str) {
        git_in(&self.seed, &["tag", "--annotate", name, "--message", name]);
        git_in(
            &self.seed,
            &[
                "push",
                &self.bare.to_string_lossy(),
                &format!("refs/tags/{name}"),
            ],
        );
    }

    fn push_ref(&self, local_ref: &str, remote_ref: &str) {
        git_in(
            &self.seed,
            &[
                "push",
                &self.bare.to_string_lossy(),
                &format!("{local_ref}:{remote_ref}"),
            ],
        );
    }

    fn source(&self) -> crate::GitExtensionSource {
        crate::GitExtensionSource {
            url: self.url.clone(),
            reference: Some("master".into()),
            path: self.subdirectory.map(str::to_string),
        }
    }
}

async fn git_extension_store(
    cx: &mut TestAppContext,
) -> (TempTree, PathBuf, Entity<ExtensionStore>, gpui::Task<()>) {
    git_extension_store_with_http(cx, FakeHttpClient::with_200_response()).await
}

fn marketplace_serving(
    extension_id: &'static str,
    version: &'static str,
) -> Arc<HttpClientWithUrl> {
    marketplace_serving_with_theme(extension_id, version, None)
}

#[derive(Clone, Copy)]
enum MarketplaceThemeFixture {
    DeclaredMalformed,
    LegacyDiscoveredMalformed,
    LegacyMissingDeclared,
}

fn marketplace_serving_with_theme(
    extension_id: &'static str,
    version: &'static str,
    theme: Option<MarketplaceThemeFixture>,
) -> Arc<HttpClientWithUrl> {
    FakeHttpClient::create(move |request| async move {
        if !request
            .uri()
            .path()
            .ends_with(&format!("/extensions/{extension_id}/download"))
        {
            return Ok(Response::builder().status(404).body("not found".into())?);
        }
        let (manifest_name, manifest) = match theme {
            Some(MarketplaceThemeFixture::LegacyDiscoveredMalformed) => (
                "extension.json",
                json!({"name": "Marketplace Extension", "version": version}).to_string(),
            ),
            Some(MarketplaceThemeFixture::LegacyMissingDeclared) => (
                "extension.json",
                json!({"name": "Marketplace Extension", "version": version,
                    "themes": {"Missing": "missing.json"}})
                .to_string(),
            ),
            _ => {
                let mut manifest = format!(
                    "id = \"{extension_id}\"\nname = \"Marketplace Extension\"\nversion = \"{version}\"\nschema_version = 1\n"
                );
                if theme.is_some() {
                    manifest.push_str("themes = [\"themes/theme.json\"]\n");
                }
                ("extension.toml", manifest)
            }
        };
        let mut bytes = Vec::<u8>::new();
        let mut archive = async_tar::Builder::new(&mut bytes);
        let mut header = async_tar::Header::new_gnu();
        header.set_size(manifest.len() as u64);
        header.set_mode(0o644);
        archive
            .append_data(&mut header, manifest_name, manifest.as_bytes())
            .await?;
        if matches!(
            theme,
            Some(
                MarketplaceThemeFixture::DeclaredMalformed
                    | MarketplaceThemeFixture::LegacyDiscoveredMalformed
            )
        ) {
            let theme = "{invalid";
            let mut header = async_tar::Header::new_gnu();
            header.set_size(theme.len() as u64);
            header.set_mode(0o644);
            archive
                .append_data(&mut header, "themes/theme.json", theme.as_bytes())
                .await?;
        }
        archive.into_inner().await?;
        let mut gzipped_bytes = Vec::new();
        GzipEncoder::new(BufReader::new(bytes.as_slice()))
            .read_to_end(&mut gzipped_bytes)
            .await?;
        Ok(Response::new(gzipped_bytes.into()))
    })
}

#[derive(Clone, Copy, Default)]
struct MarketplaceGrammarFixture {
    id: &'static str,
    provides_html: bool,
    consumes_html: bool,
    malformed_query: bool,
    malformed_theme: bool,
}

fn marketplace_serving_grammar_batch(
    extensions: Vec<MarketplaceGrammarFixture>,
) -> (Arc<HttpClientWithUrl>, Arc<AtomicUsize>) {
    let downloads = Arc::new(AtomicUsize::new(0));
    let client = FakeHttpClient::create({
        let downloads = downloads.clone();
        move |request| {
            let fixture = extensions
                .iter()
                .find(|fixture| {
                    request
                        .uri()
                        .path()
                        .ends_with(&format!("/extensions/{}/download", fixture.id))
                })
                .copied();
            let downloads = downloads.clone();
            async move {
                let Some(fixture) = fixture else {
                    return Ok(Response::builder().status(404).body("not found".into())?);
                };
                downloads.fetch_add(1, Ordering::SeqCst);
                let mut manifest = format!(
                    "id = \"{}\"\nname = \"{}\"\nversion = \"2.0.0\"\nschema_version = 1\n",
                    fixture.id, fixture.id,
                );
                let mut files = Vec::new();
                if fixture.consumes_html {
                    manifest.push_str("languages = [\"languages/html\"]\n");
                    files.push((
                        "languages/html/config.toml",
                        format!("name = \"Language {}\"\ngrammar = \"html\"\n", fixture.id,)
                            .into_bytes(),
                    ));
                    let query = if fixture.malformed_query {
                        "("
                    } else {
                        "(tag_name) @tag"
                    };
                    files.push(("languages/html/highlights.scm", query.as_bytes().to_vec()));
                }
                if fixture.malformed_theme {
                    manifest.push_str("themes = [\"themes/broken.json\"]\n");
                    files.push(("themes/broken.json", b"{invalid".to_vec()));
                }
                if fixture.provides_html {
                    manifest.push_str("[grammars.html]\nrepository = \"https://example.com/html\"\nrev = \"fixture\"\n");
                    files.push((
                        "grammars/html.wasm",
                        include_bytes!("../../language/test_data/grammars/html.wasm").to_vec(),
                    ));
                }
                files.push(("extension.toml", manifest.into_bytes()));
                let mut bytes = Vec::new();
                let mut archive = async_tar::Builder::new(&mut bytes);
                for (path, contents) in files {
                    let mut header = async_tar::Header::new_gnu();
                    header.set_size(contents.len() as u64);
                    header.set_mode(0o644);
                    archive
                        .append_data(&mut header, path, contents.as_slice())
                        .await?;
                }
                archive.into_inner().await?;
                let mut compressed = Vec::new();
                GzipEncoder::new(BufReader::new(bytes.as_slice()))
                    .read_to_end(&mut compressed)
                    .await?;
                Ok(Response::new(compressed.into()))
            }
        }
    });
    (client, downloads)
}

async fn git_extension_store_with_http(
    cx: &mut TestAppContext,
    http_client: Arc<HttpClientWithUrl>,
) -> (TempTree, PathBuf, Entity<ExtensionStore>, gpui::Task<()>) {
    init_test(cx);
    cx.executor().allow_parking();
    let tree = TempTree::new(json!({ "installed": {}, "work": {}, "build": {} }));
    let extensions_dir = tree_root(&tree);
    let (store, debounce) = extension_store_in(&extensions_dir, http_client, cx).await;
    (tree, extensions_dir, store, debounce)
}

async fn extension_store_in(
    extensions_dir: &Path,
    http_client: Arc<HttpClientWithUrl>,
    cx: &mut TestAppContext,
) -> (Entity<ExtensionStore>, gpui::Task<()>) {
    let fs = RealFs::new(None, cx.executor());
    let extensions_dir = extensions_dir.to_path_buf();
    let store = cx.new(|cx| {
        ExtensionStore::new(
            extensions_dir.clone(),
            Some(extensions_dir.join("build")),
            Arc::new(ExtensionHostProxy::new()),
            fs,
            http_client.clone(),
            http_client,
            None,
            NodeRuntime::unavailable(),
            cx,
        )
    });
    let mut events = cx.events(&store);
    let executor = cx.executor();
    let debounce = cx.executor().spawn(async move {
        while let Some(event) = events.next().await {
            if let Event::StartedReloading = event {
                executor.advance_clock(RELOAD_DEBOUNCE_DURATION);
            }
        }
    });
    let initial_load = store.read_with(cx, |store, _| store.initial_index_load.clone());
    initial_load.await;
    let initial_maintenance = store.read_with(cx, |store, _| store.initial_maintenance.clone());
    initial_maintenance.await;
    (store, debounce)
}

fn installed_source(
    store: &Entity<ExtensionStore>,
    extension_id: &str,
    cx: &mut TestAppContext,
) -> Option<crate::InstalledGitSource> {
    store.read_with(cx, |store, _| {
        store
            .extension_index
            .extensions
            .get(extension_id)
            .and_then(|entry| entry.source.clone())
    })
}

fn installed_version(
    store: &Entity<ExtensionStore>,
    extension_id: &str,
    cx: &mut TestAppContext,
) -> Option<Arc<str>> {
    store.read_with(cx, |store, _| {
        store
            .extension_index
            .extensions
            .get(extension_id)
            .map(|entry| entry.manifest.version.clone())
    })
}

#[gpui::test]
async fn test_git_source_namespace_collisions_preserve_existing_install(cx: &mut TestAppContext) {
    for (existing_id, colliding_id) in [("foo", "foo.toml"), ("foo.toml", "foo")] {
        let (_tree, extensions_dir, store, _debounce) = git_extension_store(cx).await;
        let remote = GitExtensionRemote::new(existing_id, None);
        store
            .update(cx, |store, cx| {
                store.install_git_extension(existing_id.into(), remote.source(), cx)
            })
            .await
            .expect("install first source");
        let original_source = installed_source(&store, existing_id, cx);
        let record_path = extensions_dir
            .join("sources")
            .join(format!("{existing_id}.toml"));
        let original_record = std::fs::read(&record_path).expect("first source record");
        let manifest_path = extensions_dir
            .join("sources")
            .join(existing_id)
            .join("extension.toml");
        let original_manifest = std::fs::read(&manifest_path).expect("first checkout manifest");
        let colliding_remote = GitExtensionRemote::new(colliding_id, None);
        let error = store
            .update(cx, |store, cx| {
                store.install_git_extension(colliding_id.into(), colliding_remote.source(), cx)
            })
            .await
            .expect_err("colliding source paths must not replace another extension");
        assert!(format!("{error:#}").contains("collides"), "{error:#}");
        let error = store
            .update(cx, |store, cx| {
                store.uninstall_extension(colliding_id.into(), cx)
            })
            .await
            .expect_err("removing the colliding id must not delete another source");
        assert!(format!("{error:#}").contains("collides"), "{error:#}");
        assert_eq!(
            std::fs::read(&record_path).expect("preserved record"),
            original_record
        );
        assert_eq!(
            std::fs::read(&manifest_path).expect("preserved checkout"),
            original_manifest
        );
        assert_eq!(installed_source(&store, existing_id, cx), original_source);
        assert!(
            extensions_dir
                .join("installed")
                .join(existing_id)
                .join("extension.toml")
                .is_file()
        );
        assert!(!extensions_dir.join("installed").join(colliding_id).exists());
        assert!(!extensions_dir.join("staging/git-source.previous").exists());
    }
}

#[gpui::test]
async fn test_install_git_extension(cx: &mut TestAppContext) {
    let (_tree, extensions_dir, store, _debounce) = git_extension_store(cx).await;
    let remote = GitExtensionRemote::new("git-extension", None);
    let commit = git_in(&remote.seed, &["rev-parse", "HEAD"]);

    store
        .update(cx, |store, cx| {
            store.install_git_extension("git-extension".into(), remote.source(), cx)
        })
        .await
        .unwrap();

    let entry = store.read_with(cx, |store, _| {
        store.extension_index.extensions["git-extension"].clone()
    });
    assert!(entry.dev);
    let source = entry
        .source
        .expect("the index should record the git source");
    assert_eq!(source.commit, commit);
    assert_eq!(source.url, remote.url);
    let installed = std::fs::symlink_metadata(extensions_dir.join("installed/git-extension"))
        .expect("the extension should be installed");
    assert!(installed.file_type().is_symlink());
    let record: crate::InstalledGitSource = toml::from_str(
        &std::fs::read_to_string(extensions_dir.join("sources/git-extension.toml")).unwrap(),
    )
    .unwrap();
    assert_eq!(record, source);
}

#[gpui::test]
async fn test_install_git_extension_rejects_mismatched_id(cx: &mut TestAppContext) {
    let (_tree, extensions_dir, store, _debounce) = git_extension_store(cx).await;
    let remote = GitExtensionRemote::new("git-extension", None);

    let result = store
        .update(cx, |store, cx| {
            store.install_git_extension("other".into(), remote.source(), cx)
        })
        .await;

    let error = result.expect_err("a mismatched id should fail the install");
    assert!(
        format!("{error:#}").contains("declares extension id git-extension, expected other"),
        "{error:#}"
    );
    assert!(!extensions_dir.join("installed/other").exists());
    assert!(!extensions_dir.join("sources/other").exists());
    assert!(!extensions_dir.join("sources/other.toml").exists());
    store.read_with(cx, |store, _| {
        assert!(!store.extension_index.extensions.contains_key("other"));
    });
}

#[gpui::test]
async fn test_install_git_extension_from_subdirectory(cx: &mut TestAppContext) {
    let (_tree, extensions_dir, store, _debounce) = git_extension_store(cx).await;
    let remote = GitExtensionRemote::new("git-extension", Some("editors/ext"));

    store
        .update(cx, |store, cx| {
            store.install_git_extension("git-extension".into(), remote.source(), cx)
        })
        .await
        .unwrap();

    assert_eq!(
        std::fs::canonicalize(extensions_dir.join("installed/git-extension")).unwrap(),
        std::fs::canonicalize(extensions_dir.join("sources/git-extension/editors/ext")).unwrap()
    );
    assert_eq!(
        installed_source(&store, "git-extension", cx)
            .and_then(|source| source.path)
            .as_deref(),
        Some("editors/ext")
    );
}

#[gpui::test]
async fn test_install_git_extension_rejects_paths_outside_its_directories(cx: &mut TestAppContext) {
    let (_tree, extensions_dir, store, _debounce) = git_extension_store(cx).await;
    let remote = GitExtensionRemote::new("git-extension", None);
    let outside = TempTree::new(json!({ "keep": "" }));

    for extension_id in [
        outside.path().to_string_lossy().into_owned(),
        "../outside".to_string(),
        "nested/git-extension".to_string(),
    ] {
        let error = store
            .update(cx, |store, cx| {
                store.install_git_extension(extension_id.as_str().into(), remote.source(), cx)
            })
            .await
            .expect_err("an id with path components should fail the install");
        assert!(
            format!("{error:#}").contains("must be a single path component"),
            "{extension_id}: {error:#}"
        );
    }

    let mut escaping = remote.source();
    escaping.path = Some("../..".into());
    let error = store
        .update(cx, |store, cx| {
            store.install_git_extension("git-extension".into(), escaping, cx)
        })
        .await
        .expect_err("a path leaving the repository should fail the install");
    assert!(
        format!("{error:#}").contains("must stay inside its repository"),
        "{error:#}"
    );

    assert!(outside.path().join("keep").exists());
    assert!(!extensions_dir.join("outside").exists());
    assert!(!extensions_dir.join("sources").exists());
}

#[gpui::test]
async fn test_failed_git_replacement_keeps_installed_extension(cx: &mut TestAppContext) {
    let (_tree, extensions_dir, store, _debounce) = git_extension_store(cx).await;
    let remote = GitExtensionRemote::new("git-extension", None);
    store
        .update(cx, |store, cx| {
            store.install_git_extension("git-extension".into(), remote.source(), cx)
        })
        .await
        .unwrap();
    let installed = installed_source(&store, "git-extension", cx);

    let replacement = GitExtensionRemote::new("other-extension", None);
    let error = store
        .update(cx, |store, cx| {
            store.install_git_extension("git-extension".into(), replacement.source(), cx)
        })
        .await
        .expect_err("a replacement declaring another id should fail");
    assert!(
        format!("{error:#}")
            .contains("declares extension id other-extension, expected git-extension"),
        "{error:#}"
    );

    assert!(installed.is_some());
    assert_eq!(installed_source(&store, "git-extension", cx), installed);
    let manifest =
        std::fs::read_to_string(extensions_dir.join("installed/git-extension/extension.toml"))
            .expect("the installed extension should still resolve");
    assert!(manifest.contains("id = \"git-extension\""), "{manifest}");
    let record: crate::InstalledGitSource = toml::from_str(
        &std::fs::read_to_string(extensions_dir.join("sources/git-extension.toml")).unwrap(),
    )
    .unwrap();
    assert_eq!(Some(record), installed);
    assert_eq!(
        std::fs::read_dir(extensions_dir.join("staging"))
            .unwrap()
            .count(),
        0
    );
}

#[gpui::test]
async fn test_failed_git_source_commit_restores_previous_checkout(cx: &mut TestAppContext) {
    let (_tree, extensions_dir, store, _debounce) = git_extension_store(cx).await;
    let remote = GitExtensionRemote::new("git-extension", None);
    store
        .update(cx, |store, cx| {
            store.install_git_extension("git-extension".into(), remote.source(), cx)
        })
        .await
        .unwrap();

    remote.publish("git-extension", "0.2.0");
    let record_path = extensions_dir.join("sources/git-extension.toml");
    std::fs::remove_file(&record_path).unwrap();
    std::fs::create_dir(&record_path).unwrap();
    store
        .update(cx, |store, cx| {
            store.install_git_extension("git-extension".into(), remote.source(), cx)
        })
        .await
        .expect_err("a record that cannot be written should fail the install");

    let manifest =
        std::fs::read_to_string(extensions_dir.join("installed/git-extension/extension.toml"))
            .expect("the installed extension should still resolve");
    assert!(manifest.contains("version = \"0.1.0\""), "{manifest}");
    assert!(record_path.is_dir());
    assert_eq!(
        std::fs::read_dir(extensions_dir.join("staging"))
            .unwrap()
            .count(),
        0
    );
}

#[gpui::test]
async fn test_git_source_commit_invalidates_the_cached_index(cx: &mut TestAppContext) {
    use crate::extension_transaction::TransactionPhase;

    let (_tree, extensions_dir, store, debounce) = git_extension_store(cx).await;
    let remote = GitExtensionRemote::new("git-extension", None);
    store
        .update(cx, |store, cx| {
            store.install_git_extension("git-extension".into(), remote.source(), cx)
        })
        .await
        .expect("initial git install");
    let index_path = extensions_dir.join("index.json");
    assert!(index_path.exists());
    let commit = remote.publish("git-extension", "0.2.0");
    let publisher = store.read_with(cx, |store, _| store.publisher.clone());
    let (committed_tx, committed_rx) = futures::channel::oneshot::channel();
    let committed_tx = Arc::new(Mutex::new(Some(committed_tx)));
    let (release_tx, release_rx) = futures::channel::oneshot::channel();
    let release_rx = Arc::new(Mutex::new(Some(release_rx)));
    publisher.set_hook(move |info| {
        let committed_tx = committed_tx.clone();
        let release_rx = release_rx.clone();
        async move {
            if info.phase == TransactionPhase::AfterCommit {
                assert!(
                    !info.index.exists(),
                    "old cached index invalidated before commit"
                );
                committed_tx
                    .lock()
                    .take()
                    .expect("one commit")
                    .send(())
                    .expect("test awaits commit");
                let release_rx = release_rx.lock().take().expect("one commit release");
                release_rx.await.expect("release committed publication");
            }
            Ok(())
        }
        .boxed()
    });
    let install = store.update(cx, |store, cx| {
        store.install_git_extension("git-extension".into(), remote.source(), cx)
    });
    committed_rx.await.expect("public install committed");
    assert!(!index_path.exists());
    publisher.clear_hook();
    drop((install, store, debounce));
    cx.run_until_parked();
    assert!(
        publisher.lock().now_or_never().is_none(),
        "publication retains the gate after cancellation"
    );
    release_tx
        .send(())
        .expect("publication survives caller drop");
    publisher
        .recover()
        .await
        .expect("wait for committed cleanup");

    let (restarted, _restarted_debounce) =
        extension_store_in(&extensions_dir, FakeHttpClient::with_200_response(), cx).await;
    assert_eq!(
        installed_version(&restarted, "git-extension", cx).as_deref(),
        Some("0.2.0")
    );
    assert_eq!(
        installed_source(&restarted, "git-extension", cx).map(|source| source.commit),
        Some(commit)
    );
    assert!(!extensions_dir.join("staging/git-source.previous").exists());
}

#[gpui::test]
async fn test_committed_git_update_succeeds_when_final_write_reports_an_error(
    cx: &mut TestAppContext,
) {
    use crate::extension_transaction::TransactionPhase;

    let (_tree, extensions_dir, store, _debounce) = git_extension_store(cx).await;
    let remote = GitExtensionRemote::new("git-extension", None);
    store
        .update(cx, |store, cx| {
            store.install_git_extension("git-extension".into(), remote.source(), cx)
        })
        .await
        .expect("initial Git install");
    let commit = remote.publish("git-extension", "0.2.0");
    let (publisher, fs) =
        store.read_with(cx, |store, _| (store.publisher.clone(), store.fs.clone()));
    publisher.set_hook(move |info| {
        let fs = fs.clone();
        async move {
            if info.phase == TransactionPhase::BeforeCommit {
                fs.atomic_write(info.backup.join("committed"), String::new())
                    .await?;
                anyhow::bail!("injected flush failure after publishing commit marker");
            }
            Ok(())
        }
        .boxed()
    });
    store
        .update(cx, |store, cx| {
            store.install_git_extension("git-extension".into(), remote.source(), cx)
        })
        .await
        .expect("visible commit marker determines successful outcome");
    publisher.clear_hook();
    assert_eq!(
        installed_version(&store, "git-extension", cx).as_deref(),
        Some("0.2.0")
    );
    assert_eq!(
        installed_source(&store, "git-extension", cx).map(|source| source.commit),
        Some(commit)
    );
    assert!(git_failures(&store, cx).is_empty());
    assert!(!extensions_dir.join("staging/git-source.previous").exists());
}

#[derive(Clone, Copy, Debug)]
enum LifecycleReplacement {
    Git,
    Local,
    Marketplace,
    Remove,
}

#[gpui::test]
async fn test_public_extension_replacements_restore_previous_install(cx: &mut TestAppContext) {
    use crate::extension_transaction::TransactionPhase;

    let (_tree, extensions_dir, store, _debounce) =
        git_extension_store_with_http(cx, marketplace_serving("git-extension", "2.0.0")).await;
    let remote = GitExtensionRemote::new("git-extension", None);
    store
        .update(cx, |store, cx| {
            store.install_git_extension("git-extension".into(), remote.source(), cx)
        })
        .await
        .expect("initial git install");
    let original_source = installed_source(&store, "git-extension", cx);
    let installed = extensions_dir.join("installed/git-extension");
    let record = extensions_dir.join("sources/git-extension.toml");
    let original_target = std::fs::read_link(&installed).expect("original installed link");
    let original_manifest =
        std::fs::read(installed.join("extension.toml")).expect("original manifest");
    let original_record = std::fs::read(&record).expect("original source record");
    let local = local_dev_extension("git-extension");
    remote.publish("git-extension", "0.2.0");
    let publisher = store.read_with(cx, |store, _| store.publisher.clone());

    for replacement in [
        LifecycleReplacement::Git,
        LifecycleReplacement::Local,
        LifecycleReplacement::Marketplace,
        LifecycleReplacement::Remove,
    ] {
        for phase in [
            TransactionPhase::BeforeBackup,
            TransactionPhase::BeforePublish,
            TransactionPhase::BeforeRecord,
            TransactionPhase::BeforeCommit,
        ] {
            let reached = Arc::new(AtomicUsize::new(0));
            publisher.set_hook({
                let reached = reached.clone();
                move |info| {
                    let reached = reached.clone();
                    async move {
                        if info.phase == phase {
                            reached.fetch_add(1, Ordering::SeqCst);
                            assert_eq!(info.extension_id, "git-extension");
                            assert!(info.checkout.ends_with("sources/git-extension"));
                            assert!(info.record.ends_with("sources/git-extension.toml"));
                            assert_eq!(
                                info.candidate.is_none(),
                                matches!(replacement, LifecycleReplacement::Remove)
                            );
                            if phase == TransactionPhase::BeforeBackup {
                                assert!(info.installed.join("extension.toml").is_file());
                                assert!(info.checkout.join("extension.toml").is_file());
                                assert!(info.record.is_file());
                                assert!(!info.backup.join("installed").exists());
                            } else {
                                assert!(
                                    info.backup
                                        .join("installed")
                                        .symlink_metadata()?
                                        .file_type()
                                        .is_symlink()
                                );
                            }
                            if phase == TransactionPhase::BeforePublish
                                && !matches!(replacement, LifecycleReplacement::Remove)
                            {
                                assert!(info.installed.symlink_metadata().is_err());
                                // Force the production promotion syscall to fail after backup.
                                std::fs::write(info.installed, "occupied")?;
                                return Ok(());
                            }
                            anyhow::bail!("injected publication failure at {phase:?}");
                        }
                        Ok(())
                    }
                    .boxed()
                }
            });
            let result = store
                .update(cx, |store, cx| match replacement {
                    LifecycleReplacement::Git => {
                        store.install_git_extension("git-extension".into(), remote.source(), cx)
                    }
                    LifecycleReplacement::Local => {
                        store.install_dev_extension(tree_root(&local), cx)
                    }
                    LifecycleReplacement::Marketplace => {
                        store.install_latest_extension_task("git-extension".into(), cx)
                    }
                    LifecycleReplacement::Remove => {
                        store.uninstall_extension("git-extension".into(), cx)
                    }
                })
                .await;
            publisher.clear_hook();
            assert!(result.is_err(), "{replacement:?} at {phase:?}");
            assert_eq!(
                reached.load(Ordering::SeqCst),
                1,
                "{replacement:?} at {phase:?}"
            );
            assert_eq!(
                std::fs::read_link(&installed).expect("restored link"),
                original_target
            );
            assert_eq!(
                std::fs::read(installed.join("extension.toml")).expect("restored checkout"),
                original_manifest
            );
            assert_eq!(
                std::fs::read(&record).expect("restored record"),
                original_record
            );
            store.update(cx, |store, cx| store.reload(None, cx)).await;
            assert_eq!(
                installed_source(&store, "git-extension", cx),
                original_source
            );
            assert_eq!(
                installed_version(&store, "git-extension", cx).as_deref(),
                Some("0.1.0")
            );
            assert!(!extensions_dir.join("staging/git-source.previous").exists());
            assert!(local.path().join("extension.toml").is_file());
        }
    }
}

#[gpui::test]
async fn test_failed_git_replacement_preserves_non_git_install(cx: &mut TestAppContext) {
    use crate::extension_transaction::TransactionPhase;

    for previous_local in [false, true] {
        let (_tree, extensions_dir, store, _debounce) =
            git_extension_store_with_http(cx, marketplace_serving("git-extension", "2.0.0")).await;
        let local = local_dev_extension("git-extension");
        store
            .update(cx, |store, cx| {
                if previous_local {
                    store.install_dev_extension(tree_root(&local), cx)
                } else {
                    store.install_latest_extension_task("git-extension".into(), cx)
                }
            })
            .await
            .expect("initial non-git install");
        let installed = extensions_dir.join("installed/git-extension");
        let original_manifest =
            std::fs::read(installed.join("extension.toml")).expect("original manifest");
        let original_target = std::fs::canonicalize(&installed).expect("original installed path");
        let publisher = store.read_with(cx, |store, _| store.publisher.clone());
        publisher.set_hook(|info| {
            async move {
                if info.phase == TransactionPhase::BeforeCommit {
                    anyhow::bail!("reject replacement after source record publication");
                }
                Ok(())
            }
            .boxed()
        });
        let remote = GitExtensionRemote::new("git-extension", None);
        store
            .update(cx, |store, cx| {
                store.install_git_extension("git-extension".into(), remote.source(), cx)
            })
            .await
            .expect_err("failed git replacement restores non-git ownership");
        publisher.clear_hook();
        assert_eq!(
            std::fs::canonicalize(&installed).expect("restored path"),
            original_target
        );
        assert_eq!(
            std::fs::read(installed.join("extension.toml")).expect("restored manifest"),
            original_manifest
        );
        assert_eq!(
            std::fs::symlink_metadata(&installed)
                .expect("restored entry")
                .file_type()
                .is_symlink(),
            previous_local
        );
        assert!(!extensions_dir.join("sources/git-extension").exists());
        assert!(!extensions_dir.join("sources/git-extension.toml").exists());
        assert_eq!(installed_source(&store, "git-extension", cx), None);
        assert!(local.path().join("extension.toml").is_file());
    }
}

#[gpui::test]
async fn test_managed_rebuild_preserves_installed_commit_and_reloads(cx: &mut TestAppContext) {
    use crate::extension_transaction::TransactionPhase;

    let (_tree, extensions_dir, store, _debounce) = git_extension_store(cx).await;
    let remote = GitExtensionRemote::new("git-extension", None);
    store
        .update(cx, |store, cx| {
            store.install_git_extension("git-extension".into(), remote.source(), cx)
        })
        .await
        .expect("initial git install");
    let installed = extensions_dir.join("installed/git-extension");
    let original_source = installed_source(&store, "git-extension", cx);
    let record = extensions_dir.join("sources/git-extension.toml");
    let original_record = std::fs::read(&record).expect("original source record");
    let original_manifest =
        std::fs::read(installed.join("extension.toml")).expect("original manifest");
    remote.publish("git-extension", "0.2.0");
    let publisher = store.read_with(cx, |store, _| store.publisher.clone());
    publisher.set_hook(|info| {
        async move {
            if info.phase == TransactionPhase::BeforeCommit {
                anyhow::bail!("managed rebuild publication fails");
            }
            Ok(())
        }
        .boxed()
    });
    store
        .update(cx, |store, cx| {
            store.install_dev_extension(installed.clone(), cx)
        })
        .await
        .expect_err("managed rebuild must roll back on failure");
    publisher.clear_hook();
    assert_eq!(
        std::fs::read(&record).expect("restored record"),
        original_record
    );
    assert_eq!(
        std::fs::read(installed.join("extension.toml")).expect("restored checkout"),
        original_manifest
    );
    assert_eq!(
        installed_source(&store, "git-extension", cx),
        original_source
    );

    let mut events = cx.events(&store);
    store
        .update(cx, |store, cx| {
            store.install_dev_extension(installed.clone(), cx)
        })
        .await
        .expect("managed rebuild at the installed commit");
    let mut reloaded = false;
    while let Some(Some(event)) = events.next().now_or_never() {
        reloaded |= matches!(event, Event::ExtensionsUpdated);
    }
    assert!(
        reloaded,
        "unchanged manifest still reloads rebuilt resources"
    );
    assert_eq!(
        installed_source(&store, "git-extension", cx),
        original_source
    );
    assert_eq!(
        std::fs::read(&record).expect("same installed commit"),
        original_record
    );
    assert_eq!(
        std::fs::read(installed.join("extension.toml")).expect("installed revision remains"),
        original_manifest
    );
    assert_eq!(
        installed_version(&store, "git-extension", cx).as_deref(),
        Some("0.1.0")
    );
}

#[gpui::test]
async fn test_public_git_install_defers_failed_cleanup(cx: &mut TestAppContext) {
    use crate::extension_transaction::TransactionPhase;

    let (_tree, extensions_dir, store, _debounce) = git_extension_store(cx).await;
    let remote = GitExtensionRemote::new("git-extension", None);
    store
        .update(cx, |store, cx| {
            store.install_git_extension("git-extension".into(), remote.source(), cx)
        })
        .await
        .expect("initial git install");
    let commit = remote.publish("git-extension", "0.2.0");
    let publisher = store.read_with(cx, |store, _| store.publisher.clone());
    publisher.set_hook(|info| {
        async move {
            if info.phase == TransactionPhase::BeforeCleanup {
                anyhow::bail!("injected cleanup failure");
            }
            Ok(())
        }
        .boxed()
    });
    store
        .update(cx, |store, cx| {
            store.install_git_extension("git-extension".into(), remote.source(), cx)
        })
        .await
        .expect("committed install survives cleanup failure");
    publisher.clear_hook();
    assert_eq!(
        installed_version(&store, "git-extension", cx).as_deref(),
        Some("0.2.0")
    );
    assert_eq!(
        installed_source(&store, "git-extension", cx)
            .expect("new source")
            .commit,
        commit
    );
    // Reader recovery may finish deferred cleanup before the next public mutation.
    store
        .update(cx, |store, cx| {
            store.uninstall_extension("git-extension".into(), cx)
        })
        .await
        .expect("uninstall recovers deferred cleanup");
    assert!(!extensions_dir.join("installed/git-extension").exists());
    assert!(!extensions_dir.join("sources/git-extension.toml").exists());
    assert!(!extensions_dir.join("staging/git-source.previous").exists());
}

#[gpui::test]
async fn test_marketplace_preparation_failure_keeps_git_install(cx: &mut TestAppContext) {
    use MarketplaceThemeFixture::*;

    for (block_staging, theme) in [
        (false, None),
        (true, None),
        (false, Some(DeclaredMalformed)),
        (false, Some(LegacyDiscoveredMalformed)),
        (false, Some(LegacyMissingDeclared)),
    ] {
        let client = if theme.is_some() {
            marketplace_serving_with_theme("git-extension", "2.0.0", theme)
        } else if block_staging {
            marketplace_serving("git-extension", "2.0.0")
        } else {
            FakeHttpClient::with_200_response()
        };
        let (_tree, extensions_dir, store, _debounce) =
            git_extension_store_with_http(cx, client).await;
        let remote = GitExtensionRemote::new("git-extension", None);
        store
            .update(cx, |store, cx| {
                store.install_git_extension("git-extension".into(), remote.source(), cx)
            })
            .await
            .expect("initial git install");
        let installed = extensions_dir.join("installed/git-extension");
        let record = extensions_dir.join("sources/git-extension.toml");
        let original_target = std::fs::read_link(&installed).expect("original link");
        let original_record = std::fs::read(&record).expect("original record");
        let staging = extensions_dir.join("staging");
        if block_staging {
            std::fs::remove_dir(&staging).expect("initial staging directory empty");
            std::fs::write(&staging, "occupied").expect("block staging creation");
        }
        let error = store
            .update(cx, |store, cx| {
                store.install_latest_extension_task("git-extension".into(), cx)
            })
            .await
            .expect_err("preparation failure must preserve installed extension");
        if let Some(theme) = theme {
            let expected_context = match theme {
                LegacyMissingDeclared => "reading theme missing.json",
                DeclaredMalformed | LegacyDiscoveredMalformed => {
                    "validating theme themes/theme.json"
                }
            };
            assert!(
                format!("{error:#}").contains(expected_context),
                "expected {expected_context}: {error:#}"
            );
        }
        assert_eq!(
            std::fs::read_link(&installed).expect("old link remains"),
            original_target
        );
        assert_eq!(
            std::fs::read(&record).expect("old record remains"),
            original_record
        );
        assert!(installed.join("extension.toml").is_file());
        assert_eq!(
            installed_version(&store, "git-extension", cx).as_deref(),
            Some("0.1.0")
        );
        if block_staging {
            std::fs::remove_file(&staging).expect("remove staging obstruction");
            std::fs::create_dir(&staging).expect("restore staging directory");
        }
    }
}

#[gpui::test]
async fn test_marketplace_install_and_remove_use_the_provided_filesystem(cx: &mut TestAppContext) {
    init_test(cx);
    let (store, fs) = create_extension_store(cx);
    store.update(cx, |store, _| {
        store.http_client = marketplace_serving("virtual-extension", "2.0.0");
    });
    let mut events = cx.events(&store);
    let executor = cx.executor();
    let _debounce = cx.executor().spawn(async move {
        while let Some(event) = events.next().await {
            if matches!(event, Event::StartedReloading) {
                executor.advance_clock(RELOAD_DEBOUNCE_DURATION);
            }
        }
    });
    store
        .update(cx, |store, cx| {
            store.install_latest_extension_task("virtual-extension".into(), cx)
        })
        .await
        .expect("archive staging and publication work entirely in FakeFs");
    assert_eq!(
        installed_version(&store, "virtual-extension", cx).as_deref(),
        Some("2.0.0")
    );
    let installed = Path::new("/extensions/installed/virtual-extension");
    assert!(
        fs.load(&installed.join("extension.toml"))
            .await
            .expect("manifest extracted into FakeFs")
            .contains("2.0.0")
    );
    assert!(
        fs.read_dir(Path::new("/extensions/staging"))
            .await
            .expect("virtual staging directory")
            .next()
            .await
            .is_none(),
        "successful publication cleans staging"
    );
    store
        .update(cx, |store, cx| {
            store.uninstall_extension("virtual-extension".into(), cx)
        })
        .await
        .expect("transactional removal uses FakeFs");
    assert!(
        fs.metadata(installed)
            .await
            .expect("virtual installed metadata")
            .is_none()
    );
    assert_eq!(installed_version(&store, "virtual-extension", cx), None);
    assert!(
        fs.read_dir(Path::new("/extensions/staging"))
            .await
            .expect("virtual staging directory")
            .next()
            .await
            .is_none(),
        "removal cleans journal"
    );
}

#[gpui::test]
async fn test_manual_marketplace_install_retires_git_source(cx: &mut TestAppContext) {
    let (_tree, extensions_dir, store, _debounce) =
        git_extension_store_with_http(cx, marketplace_serving("git-extension", "2.0.0")).await;
    let remote = GitExtensionRemote::new("git-extension", None);
    store
        .update(cx, |store, cx| {
            store.install_git_extension("git-extension".into(), remote.source(), cx)
        })
        .await
        .expect("initial git install");
    store
        .update(cx, |store, cx| {
            store.install_latest_extension_task("git-extension".into(), cx)
        })
        .await
        .expect("manual marketplace replacement");
    assert_eq!(
        installed_version(&store, "git-extension", cx).as_deref(),
        Some("2.0.0")
    );
    assert_eq!(installed_source(&store, "git-extension", cx), None);
    assert!(
        !std::fs::symlink_metadata(extensions_dir.join("installed/git-extension"))
            .expect("marketplace directory")
            .file_type()
            .is_symlink()
    );
    assert!(!extensions_dir.join("sources/git-extension").exists());
    assert!(!extensions_dir.join("sources/git-extension.toml").exists());
}

#[gpui::test]
async fn test_cancelled_publication_recovers_before_queued_reload(cx: &mut TestAppContext) {
    use crate::extension_transaction::TransactionPhase;

    let (_tree, extensions_dir, store, debounce) =
        git_extension_store_with_http(cx, marketplace_serving("git-extension", "2.0.0")).await;
    let remote = GitExtensionRemote::new("git-extension", None);
    store
        .update(cx, |store, cx| {
            store.install_git_extension("git-extension".into(), remote.source(), cx)
        })
        .await
        .expect("initial git install");
    let original_source = installed_source(&store, "git-extension", cx);
    let original_record =
        std::fs::read(extensions_dir.join("sources/git-extension.toml")).expect("original record");
    let publisher = store.read_with(cx, |store, _| store.publisher.clone());
    let (reached_tx, reached_rx) = futures::channel::oneshot::channel();
    let reached_tx = Arc::new(Mutex::new(Some(reached_tx)));
    let (release_tx, release_rx) = futures::channel::oneshot::channel();
    let release_rx = Arc::new(Mutex::new(Some(release_rx)));
    let executor = cx.executor();
    let fs = store.read_with(cx, |store, _| store.fs.clone());
    publisher.set_hook(move |info| {
        let reached_tx = reached_tx.clone();
        let release_rx = release_rx.clone();
        let executor = executor.clone();
        let fs = fs.clone();
        async move {
            if info.phase == TransactionPhase::BeforePublish {
                let candidate = info.candidate.expect("marketplace staging candidate");
                reached_tx
                    .lock()
                    .take()
                    .expect("one publication")
                    .send(candidate.clone())
                    .expect("test waits for publication");
                let release_rx = release_rx.lock().take().expect("one mutation release");
                let (completed_tx, completed_rx) = futures::channel::oneshot::channel();
                // Like RealFs's blocking worker, this mutation outlives a cancelled waiter.
                executor
                    .spawn(async move {
                        release_rx.await.expect("release delayed filesystem write");
                        let result = fs
                            .atomic_write(candidate.join("late-write"), "completed".into())
                            .await;
                        completed_tx
                            .send(result)
                            .expect("transaction still awaits filesystem worker");
                    })
                    .detach();
                completed_rx.await.expect("filesystem worker completed")?;
                anyhow::bail!("injected failure after delayed filesystem write");
            }
            Ok(())
        }
        .boxed()
    });
    let install = store.update(cx, |store, cx| {
        store.install_latest_extension_task("git-extension".into(), cx)
    });
    let candidate = reached_rx.await.expect("publication reached after backup");
    assert!(!extensions_dir.join("installed/git-extension").exists());
    let mut reload = Box::pin(store.update(cx, |store, cx| store.reload(None, cx)));
    cx.run_until_parked();
    assert!(
        reload.as_mut().now_or_never().is_none(),
        "reload waits for stable filesystem state"
    );
    assert_eq!(
        installed_source(&store, "git-extension", cx),
        original_source
    );
    publisher.clear_hook();
    drop(install);
    let mut recovery = Box::pin(publisher.recover());
    cx.run_until_parked();
    assert!(candidate.is_dir(), "cancelled caller must not drop staging");
    assert!(
        recovery.as_mut().now_or_never().is_none(),
        "next recovery waits for the in-flight mutation"
    );
    assert!(
        reload.as_mut().now_or_never().is_none(),
        "reader remains blocked after cancellation"
    );
    release_tx.send(()).expect("release filesystem worker");
    recovery.await.expect("recovery after completed rollback");
    reload.await;
    assert!(
        !candidate.exists(),
        "staging cleaned after mutation completes"
    );
    assert_eq!(
        installed_source(&store, "git-extension", cx),
        original_source
    );
    assert_eq!(
        installed_version(&store, "git-extension", cx).as_deref(),
        Some("0.1.0")
    );
    assert_eq!(
        std::fs::read(extensions_dir.join("sources/git-extension.toml")).expect("recovered record"),
        original_record
    );
    assert!(
        extensions_dir
            .join("installed/git-extension/extension.toml")
            .is_file()
    );
    assert!(!extensions_dir.join("staging/git-source.previous").exists());
    drop((store, debounce));
    let (restarted, _debounce) =
        extension_store_in(&extensions_dir, FakeHttpClient::with_200_response(), cx).await;
    assert_eq!(
        installed_source(&restarted, "git-extension", cx),
        original_source
    );
    assert_eq!(
        installed_version(&restarted, "git-extension", cx).as_deref(),
        Some("0.1.0")
    );
}

#[gpui::test]
async fn test_cancelled_index_rebuild_finishes_cache_write_before_uninstall(
    cx: &mut TestAppContext,
) {
    let (_tree, extensions_dir, store, _debounce) = git_extension_store(cx).await;
    let remote = GitExtensionRemote::new("git-extension", None);
    store
        .update(cx, |store, cx| {
            store.install_git_extension("git-extension".into(), remote.source(), cx)
        })
        .await
        .expect("initial Git install");
    let index_path = extensions_dir.join("index.json");
    let old_index = std::fs::read_to_string(&index_path).expect("initial cached index");
    let (publisher, fs) =
        store.read_with(cx, |store, _| (store.publisher.clone(), store.fs.clone()));
    let guard = publisher
        .lock_for_read()
        .await
        .expect("lock for index rebuild");
    let (started_tx, started_rx) = futures::channel::oneshot::channel();
    let started_tx = Arc::new(Mutex::new(Some(started_tx)));
    let (release_tx, release_rx) = futures::channel::oneshot::channel();
    let release_rx = Arc::new(Mutex::new(Some(release_rx)));
    let executor = cx.executor();
    publisher.set_index_save_hook({
        let index_path = index_path.clone();
        move || {
            let started_tx = started_tx.clone();
            let release_rx = release_rx.clone();
            let executor = executor.clone();
            let fs = fs.clone();
            let index_path = index_path.clone();
            let old_index = old_index.clone();
            async move {
                let release_rx = release_rx.lock().take().expect("one delayed cache write");
                let (completed_tx, completed_rx) = futures::channel::oneshot::channel();
                executor
                    .spawn(async move {
                        release_rx.await.expect("release cache write");
                        let result = fs.atomic_write(index_path, old_index).await;
                        completed_tx
                            .send(result)
                            .expect("cache writer retains its waiter");
                    })
                    .detach();
                started_tx
                    .lock()
                    .take()
                    .expect("one cache save")
                    .send(())
                    .expect("test waits for cache write");
                completed_rx.await.expect("cache write completed")
            }
            .boxed()
        }
    });
    let rebuild = store.update(cx, |store, cx| store.rebuild_extension_index(guard, cx));
    started_rx.await.expect("cache save worker started");
    publisher.clear_index_save_hook();
    drop(rebuild);
    let mut uninstall = Box::pin(store.update(cx, |store, cx| {
        store.uninstall_extension("git-extension".into(), cx)
    }));
    cx.run_until_parked();
    assert!(
        uninstall.as_mut().now_or_never().is_none(),
        "uninstall waits for the cancelled rebuild's cache write"
    );
    assert!(
        extensions_dir
            .join("installed/git-extension/extension.toml")
            .is_file()
    );
    release_tx
        .send(())
        .expect("cache write remains alive after caller cancellation");
    uninstall
        .await
        .expect("uninstall after cache writer completes");
    let index: ExtensionIndex =
        serde_json::from_slice(&std::fs::read(&index_path).expect("rebuilt cache"))
            .expect("valid cache");
    assert!(!index.extensions.contains_key("git-extension"));
    assert!(installed_version(&store, "git-extension", cx).is_none());
    assert!(!extensions_dir.join("installed/git-extension").exists());
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum LocalRelinkInterruption {
    LinkCreated,
    RecordRemoved,
    Committed,
}

#[gpui::test]
async fn test_restart_recovers_local_dev_replacement_after_link_created(cx: &mut TestAppContext) {
    check_restart_recovers_local_dev_replacement(cx, LocalRelinkInterruption::LinkCreated).await;
}

#[gpui::test]
async fn test_restart_recovers_local_dev_replacement_after_record_removed(cx: &mut TestAppContext) {
    check_restart_recovers_local_dev_replacement(cx, LocalRelinkInterruption::RecordRemoved).await;
}

#[gpui::test]
async fn test_restart_recovers_local_dev_replacement_after_commit(cx: &mut TestAppContext) {
    check_restart_recovers_local_dev_replacement(cx, LocalRelinkInterruption::Committed).await;
}

async fn check_restart_recovers_local_dev_replacement(
    cx: &mut TestAppContext,
    interruption: LocalRelinkInterruption,
) {
    use LocalRelinkInterruption::*;
    let (_tree, extensions_dir, store, debounce) = git_extension_store(cx).await;
    let remote = GitExtensionRemote::new("git-extension", None);
    store
        .update(cx, |store, cx| {
            store.install_git_extension("git-extension".into(), remote.source(), cx)
        })
        .await
        .expect("initial git install");
    let local = local_dev_extension("git-extension");
    let fs = store.read_with(cx, |store, _| store.fs.clone());
    let live = extensions_dir.join("sources/git-extension");
    let installed = extensions_dir.join("installed/git-extension");
    let record_path = extensions_dir.join("sources/git-extension.toml");
    let old_record = std::fs::read_to_string(&record_path).expect("existing git source record");
    let old_target = std::fs::read_link(&installed).expect("existing git link");
    drop((store, debounce));
    let backup = extensions_dir.join("staging/git-source.previous");
    std::fs::create_dir_all(&backup).expect("transaction journal");
    std::fs::write(backup.join("extension_id"), "git-extension").expect("journal extension id");
    std::fs::write(
        backup.join("state.toml"),
        toml::to_string(&crate::GitSourceBackupState {
            checkout_existed: true,
            installed_existed: true,
            record: Some(old_record.clone()),
        })
        .expect("journal serialization"),
    )
    .expect("journal state");
    std::fs::rename(&live, backup.join("checkout")).expect("back up checkout");
    std::fs::rename(&installed, backup.join("installed")).expect("back up git link");
    fs.create_symlink(&installed, tree_root(&local))
        .await
        .expect("publish local link");
    if interruption != LinkCreated {
        std::fs::remove_file(&record_path).expect("remove git source record");
    }
    if interruption == Committed {
        std::fs::write(backup.join("committed"), "").expect("commit local replacement");
    }
    let (restarted, _debounce) =
        extension_store_in(&extensions_dir, FakeHttpClient::with_200_response(), cx).await;
    if interruption == Committed {
        assert!(!record_path.exists());
        assert!(!live.exists());
        assert_eq!(
            std::fs::canonicalize(&installed).expect("local installed link"),
            std::fs::canonicalize(local.path()).expect("local source")
        );
        assert_eq!(installed_source(&restarted, "git-extension", cx), None);
    } else {
        assert_eq!(
            std::fs::read_to_string(&record_path).expect("restored source record"),
            old_record
        );
        assert_eq!(
            std::fs::read_link(&installed).expect("restored git link"),
            old_target
        );
        assert!(live.is_dir());
        assert!(installed_source(&restarted, "git-extension", cx).is_some());
    }
    assert_eq!(
        installed_version(&restarted, "git-extension", cx).as_deref(),
        Some(if interruption == Committed {
            "9.0.0"
        } else {
            "0.1.0"
        }),
        "{interruption:?}"
    );
    assert!(
        local.path().join("extension.toml").is_file(),
        "recovery must not remove external local sources"
    );
    assert!(!backup.exists(), "{interruption:?}");
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum GitRelinkInterruption {
    CheckoutReplaced,
    InstalledMovedOut,
    LinkCreated,
    RecordWritten,
    Committed,
    CheckoutRestored,
    InstalledRestored,
}

#[gpui::test]
async fn test_restart_recovers_git_relink_after_checkout_replaced(cx: &mut TestAppContext) {
    check_restart_recovers_git_relink(cx, GitRelinkInterruption::CheckoutReplaced).await;
}

#[gpui::test]
async fn test_restart_recovers_git_relink_after_installed_moved_out(cx: &mut TestAppContext) {
    check_restart_recovers_git_relink(cx, GitRelinkInterruption::InstalledMovedOut).await;
}

#[gpui::test]
async fn test_restart_recovers_git_relink_after_link_created(cx: &mut TestAppContext) {
    check_restart_recovers_git_relink(cx, GitRelinkInterruption::LinkCreated).await;
}

#[gpui::test]
async fn test_restart_recovers_git_relink_after_record_written(cx: &mut TestAppContext) {
    check_restart_recovers_git_relink(cx, GitRelinkInterruption::RecordWritten).await;
}

#[gpui::test]
async fn test_restart_recovers_git_relink_after_commit(cx: &mut TestAppContext) {
    check_restart_recovers_git_relink(cx, GitRelinkInterruption::Committed).await;
}

#[gpui::test]
async fn test_restart_recovers_git_relink_after_checkout_restored(cx: &mut TestAppContext) {
    check_restart_recovers_git_relink(cx, GitRelinkInterruption::CheckoutRestored).await;
}

#[gpui::test]
async fn test_restart_recovers_git_relink_after_installed_restored(cx: &mut TestAppContext) {
    check_restart_recovers_git_relink(cx, GitRelinkInterruption::InstalledRestored).await;
}

async fn check_restart_recovers_git_relink(
    cx: &mut TestAppContext,
    interruption: GitRelinkInterruption,
) {
    use GitRelinkInterruption::*;
    let (_tree, extensions_dir, store, debounce) = git_extension_store(cx).await;
    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|settings, cx| {
            settings.update_user_settings(cx, |content| {
                content
                    .extension
                    .auto_update_extensions
                    .insert("git-extension".into(), false);
            });
        });
    });
    let remote = GitExtensionRemote::new("git-extension", None);
    store
        .update(cx, |store, cx| {
            store.install_git_extension("git-extension".into(), remote.source(), cx)
        })
        .await
        .expect("initial git extension install should succeed");
    let fs = store.read_with(cx, |store, _| store.fs.clone());
    let live = extensions_dir.join("sources/git-extension");
    let installed = extensions_dir.join("installed/git-extension");
    let record_path = extensions_dir.join("sources/git-extension.toml");
    let old_record = std::fs::read_to_string(&record_path)
        .expect("extension fixture contents should be readable");
    let old_manifest = std::fs::read_to_string(live.join("extension.toml"))
        .expect("extension fixture contents should be readable");
    let old_target = std::fs::read_link(&installed).expect("installed extension link should exist");
    let mut new_source = installed_source(&store, "git-extension", cx)
        .expect("installed extension should have a source record");
    new_source.path = Some("nested".into());
    let new_manifest = old_manifest.replace("0.1.0", "0.2.0");
    std::fs::create_dir_all(remote.seed.join("nested")).expect("replacement extension directory");
    std::fs::write(remote.seed.join("nested/extension.toml"), &new_manifest)
        .expect("replacement extension manifest");
    new_source.commit = remote.publish("git-extension", "0.2.0");
    new_source.checkout_tree = Some(git_in(&remote.seed, &["rev-parse", "HEAD^{tree}"]));
    let new_record = toml::to_string(&new_source).expect("source record should serialize");
    let recovery_marker = "recovery-checkout-marker";
    std::fs::write(live.join(recovery_marker), "previous checkout")
        .expect("mark previous checkout");
    std::fs::write(remote.seed.join(recovery_marker), "replacement checkout")
        .expect("mark replacement checkout");
    drop((store, debounce));

    let backup = extensions_dir.join("staging/git-source.previous");
    std::fs::create_dir_all(&backup).expect("transaction fixture directory should be created");
    std::fs::write(backup.join("extension_id"), "git-extension")
        .expect("transaction fixture should be written");
    std::fs::write(
        backup.join("state.toml"),
        toml::to_string(&crate::GitSourceBackupState {
            checkout_existed: true,
            installed_existed: true,
            record: Some(old_record.clone()),
        })
        .expect("transaction state fixture should be written"),
    )
    .expect("transaction state fixture should be written");
    std::fs::rename(&live, backup.join("checkout")).expect("transaction fixture entry should move");
    std::fs::rename(&remote.seed, &live).expect("move the prepared Git checkout into place");
    if interruption != CheckoutReplaced {
        std::fs::rename(&installed, backup.join("installed"))
            .expect("transaction fixture entry should move");
    }
    if !matches!(interruption, CheckoutReplaced | InstalledMovedOut) {
        fs.create_symlink(&installed, live.join("nested"))
            .await
            .expect("replacement link should be created");
    }
    if matches!(
        interruption,
        RecordWritten | Committed | CheckoutRestored | InstalledRestored
    ) {
        std::fs::write(&record_path, &new_record).expect("transaction fixture should be written");
    }
    if interruption == Committed {
        std::fs::write(backup.join("committed"), "")
            .expect("transaction fixture should be written");
    }
    if matches!(interruption, CheckoutRestored | InstalledRestored) {
        std::fs::remove_dir_all(&live).expect("replacement checkout should be removed");
        std::fs::rename(backup.join("checkout"), &live)
            .expect("transaction fixture entry should move");
    }
    if interruption == InstalledRestored {
        fs.remove_file(
            &installed,
            RemoveOptions {
                recursive: false,
                ignore_if_not_exists: false,
            },
        )
        .await
        .expect("replacement link should be removed");
        std::fs::rename(backup.join("installed"), &installed)
            .expect("transaction fixture entry should move");
    }

    let (restarted, _debounce) =
        extension_store_in(&extensions_dir, FakeHttpClient::with_200_response(), cx).await;
    let committed = interruption == Committed;
    assert_eq!(
        std::fs::read_to_string(live.join(recovery_marker)).expect("retained checkout marker"),
        if committed {
            "replacement checkout"
        } else {
            "previous checkout"
        },
        "recovery must preserve the checkout rather than reinstall it: {interruption:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&record_path)
            .expect("extension fixture contents should be readable"),
        if committed { &new_record } else { &old_record }.as_str(),
        "{interruption:?}"
    );
    assert_eq!(
        std::fs::read_to_string(installed.join("extension.toml"))
            .expect("extension fixture contents should be readable"),
        if committed {
            &new_manifest
        } else {
            &old_manifest
        }
        .as_str(),
        "{interruption:?}"
    );
    if !committed {
        assert_eq!(
            std::fs::read_link(&installed).expect("installed extension link should exist"),
            old_target,
            "{interruption:?}"
        );
    }
    assert_eq!(
        installed_version(&restarted, "git-extension", cx).as_deref(),
        Some(if committed { "0.2.0" } else { "0.1.0" }),
        "{interruption:?}"
    );
    assert!(!backup.exists(), "{interruption:?}");
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum GitSourceSwapInterruption {
    BackupCreated,
    CheckoutMovedOut,
    NewCheckoutMovedIn,
    FirstCheckoutMovedIn,
    RecordWritten,
}

fn interrupt_git_source_swap(
    extensions_dir: &Path,
    interruption: GitSourceSwapInterruption,
    new_checkout: &Path,
    new_record: &str,
) {
    use GitSourceSwapInterruption::*;
    let live = extensions_dir.join("sources/git-extension");
    let backup = extensions_dir.join("staging/git-source.previous");
    std::fs::create_dir_all(&backup).unwrap();
    std::fs::write(backup.join("extension_id"), "git-extension").unwrap();
    if matches!(
        interruption,
        CheckoutMovedOut | NewCheckoutMovedIn | RecordWritten
    ) {
        std::fs::rename(&live, backup.join("checkout")).unwrap();
    }
    if interruption != BackupCreated {
        std::fs::write(backup.join("record.toml"), new_record).unwrap();
    }
    if matches!(
        interruption,
        NewCheckoutMovedIn | FirstCheckoutMovedIn | RecordWritten
    ) {
        std::fs::rename(new_checkout, &live).expect("move the prepared Git checkout into place");
    }
    if interruption == RecordWritten {
        std::fs::write(
            extensions_dir.join("sources/git-extension.toml"),
            new_record,
        )
        .unwrap();
    }
}

#[gpui::test]
async fn test_restart_finishes_git_source_swap_after_backup_created(cx: &mut TestAppContext) {
    check_restart_finishes_git_source_swap(cx, GitSourceSwapInterruption::BackupCreated).await;
}

#[gpui::test]
async fn test_restart_finishes_git_source_swap_after_checkout_moved_out(cx: &mut TestAppContext) {
    check_restart_finishes_git_source_swap(cx, GitSourceSwapInterruption::CheckoutMovedOut).await;
}

#[gpui::test]
async fn test_restart_finishes_git_source_swap_after_new_checkout_moved_in(
    cx: &mut TestAppContext,
) {
    check_restart_finishes_git_source_swap(cx, GitSourceSwapInterruption::NewCheckoutMovedIn).await;
}

#[gpui::test]
async fn test_restart_finishes_git_source_swap_after_first_checkout_moved_in(
    cx: &mut TestAppContext,
) {
    check_restart_finishes_git_source_swap(cx, GitSourceSwapInterruption::FirstCheckoutMovedIn)
        .await;
}

#[gpui::test]
async fn test_restart_finishes_git_source_swap_after_record_written(cx: &mut TestAppContext) {
    check_restart_finishes_git_source_swap(cx, GitSourceSwapInterruption::RecordWritten).await;
}

async fn check_restart_finishes_git_source_swap(
    cx: &mut TestAppContext,
    interruption: GitSourceSwapInterruption,
) {
    use GitSourceSwapInterruption::*;
    let (_tree, extensions_dir, store, debounce) = git_extension_store(cx).await;
    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|settings, cx| {
            settings.update_user_settings(cx, |content| {
                content
                    .extension
                    .auto_update_extensions
                    .insert("git-extension".into(), false);
            });
        });
    });
    let remote = GitExtensionRemote::new("git-extension", None);
    store
        .update(cx, |store, cx| {
            store.install_git_extension("git-extension".into(), remote.source(), cx)
        })
        .await
        .unwrap();
    let record_path = extensions_dir.join("sources/git-extension.toml");
    let old_record = std::fs::read_to_string(&record_path).unwrap();
    let mut new_source =
        installed_source(&store, "git-extension", cx).expect("installed Git source");
    new_source.commit = remote.publish("git-extension", "0.2.0");
    // This metadata-only fixture has no build outputs, so its committed tree is
    // also the validated checkout tree. Keep the record and real checkout paired.
    new_source.checkout_tree = Some(git_in(&remote.seed, &["rev-parse", "HEAD^{tree}"]));
    let new_record = toml::to_string(&new_source).expect("replacement source record");
    let recovery_marker = "recovery-checkout-marker";
    std::fs::write(
        extensions_dir
            .join("sources/git-extension")
            .join(recovery_marker),
        "previous checkout",
    )
    .expect("mark previous checkout");
    std::fs::write(remote.seed.join(recovery_marker), "replacement checkout")
        .expect("mark replacement checkout");
    if interruption == FirstCheckoutMovedIn {
        store
            .update(cx, |store, cx| {
                store.uninstall_extension("git-extension".into(), cx)
            })
            .await
            .unwrap();
    }
    drop((store, debounce));
    interrupt_git_source_swap(&extensions_dir, interruption, &remote.seed, &new_record);

    let (restarted, _restarted_debounce) =
        extension_store_in(&extensions_dir, FakeHttpClient::with_200_response(), cx).await;
    let expected = match interruption {
        FirstCheckoutMovedIn => None,
        RecordWritten => Some(("0.2.0", &new_record)),
        _ => Some(("0.1.0", &old_record)),
    };
    assert_eq!(
        installed_version(&restarted, "git-extension", cx).as_deref(),
        expected.map(|(version, _)| version),
        "{interruption:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&record_path).ok().as_ref(),
        expected.map(|(_, record)| record),
        "{interruption:?}"
    );
    assert_eq!(
        extensions_dir.join("sources/git-extension").exists(),
        expected.is_some(),
        "{interruption:?}"
    );
    assert_eq!(
        std::fs::read_to_string(
            extensions_dir
                .join("sources/git-extension")
                .join(recovery_marker)
        )
        .ok()
        .as_deref(),
        match interruption {
            FirstCheckoutMovedIn => None,
            RecordWritten => Some("replacement checkout"),
            _ => Some("previous checkout"),
        },
        "recovery must preserve the checkout rather than reinstall it: {interruption:?}"
    );
    assert_eq!(
        std::fs::read_dir(extensions_dir.join("staging"))
            .unwrap()
            .count(),
        0,
        "{interruption:?}"
    );
}

#[gpui::test]
async fn test_git_install_restores_an_interrupted_swap_first(cx: &mut TestAppContext) {
    let (_tree, extensions_dir, store, _debounce) = git_extension_store(cx).await;
    let remote = GitExtensionRemote::new("git-extension", None);
    store
        .update(cx, |store, cx| {
            store.install_git_extension("git-extension".into(), remote.source(), cx)
        })
        .await
        .unwrap();
    let old_record =
        std::fs::read_to_string(extensions_dir.join("sources/git-extension.toml")).unwrap();
    let old_manifest =
        std::fs::read_to_string(extensions_dir.join("sources/git-extension/extension.toml"))
            .unwrap();
    let mut new_source =
        installed_source(&store, "git-extension", cx).expect("installed Git source");
    let commit = remote.publish("git-extension", "0.2.0");
    new_source.commit = commit.clone();
    new_source.checkout_tree = Some(git_in(&remote.seed, &["rev-parse", "HEAD^{tree}"]));
    let new_record = toml::to_string(&new_source).expect("replacement source record");
    interrupt_git_source_swap(
        &extensions_dir,
        GitSourceSwapInterruption::NewCheckoutMovedIn,
        &remote.seed,
        &new_record,
    );

    let other = GitExtensionRemote::new("other-extension", None);
    store
        .update(cx, |store, cx| {
            store.install_git_extension("git-extension".into(), other.source(), cx)
        })
        .await
        .expect_err("a source declaring another id should fail the install");
    assert_eq!(
        std::fs::read_to_string(extensions_dir.join("installed/git-extension/extension.toml"))
            .unwrap(),
        old_manifest
    );
    assert_eq!(
        std::fs::read_to_string(extensions_dir.join("sources/git-extension.toml")).unwrap(),
        old_record
    );
    assert_eq!(
        std::fs::read_dir(extensions_dir.join("staging"))
            .unwrap()
            .count(),
        0
    );

    store
        .update(cx, |store, cx| {
            store.install_git_extension("git-extension".into(), remote.source(), cx)
        })
        .await
        .unwrap();

    assert_eq!(
        installed_version(&store, "git-extension", cx).as_deref(),
        Some("0.2.0")
    );
    assert_eq!(
        installed_source(&store, "git-extension", cx).map(|source| source.commit),
        Some(commit)
    );
    assert_eq!(
        std::fs::read_dir(extensions_dir.join("staging"))
            .unwrap()
            .count(),
        0
    );
}

#[gpui::test]
async fn test_git_extension_on_annotated_tag_is_not_refetched(cx: &mut TestAppContext) {
    let (_tree, extensions_dir, store, _debounce) = git_extension_store(cx).await;
    let remote = GitExtensionRemote::new("git-extension", None);
    let commit = git_in(&remote.seed, &["rev-parse", "HEAD"]);
    remote.tag("v0.1.0");
    let mut source = remote.source();
    source.reference = Some("v0.1.0".into());
    store
        .update(cx, |store, cx| {
            store.install_git_extension("git-extension".into(), source, cx)
        })
        .await
        .unwrap();
    assert_eq!(
        installed_source(&store, "git-extension", cx).map(|source| source.commit),
        Some(commit)
    );
    let marker = extensions_dir.join("sources/git-extension/marker");
    std::fs::write(&marker, "").unwrap();

    store
        .update(cx, |store, cx| store.check_for_git_updates(cx))
        .await
        .unwrap();

    assert!(
        marker.exists(),
        "an unchanged annotated tag should not be fetched again"
    );
}

#[gpui::test]
async fn test_git_option_like_ref_reports_failure_without_replacing_install(
    cx: &mut TestAppContext,
) {
    let (_tree, extensions_dir, store, _debounce) = git_extension_store(cx).await;
    let remote = GitExtensionRemote::new("git-extension", None);
    store
        .update(cx, |store, cx| {
            store.install_git_extension("git-extension".into(), remote.source(), cx)
        })
        .await
        .expect("install branch before testing invalid ref");
    let original = installed_source(&store, "git-extension", cx).expect("installed Git source");
    let record_path = extensions_dir.join("sources/git-extension.toml");
    let original_record = std::fs::read(&record_path).expect("original source record");
    remote.tag("v0.1.0");
    remote.publish("git-extension", "0.2.0");

    let mut source = remote.source();
    source.reference = Some("--tags".into());
    store
        .update(cx, |store, cx| {
            store.install_git_extensions(vec![("git-extension".into(), source)], cx)
        })
        .await;

    let failures = git_failures(&store, cx);
    assert_eq!(failures.len(), 1);
    assert!(failures.iter().all(|failure| {
        failure.extension_id.as_ref() == "git-extension" && failure.message.contains("--tags")
    }));
    assert_eq!(
        installed_source(&store, "git-extension", cx),
        Some(original.clone())
    );
    assert_eq!(
        installed_version(&store, "git-extension", cx).as_deref(),
        Some("0.1.0")
    );
    assert_eq!(
        std::fs::read(&record_path).expect("preserved record"),
        original_record
    );
    assert_eq!(
        git_in(
            &extensions_dir.join("sources/git-extension"),
            &["rev-parse", "HEAD"]
        ),
        original.commit
    );

    for reference in [original.commit.clone(), "v0.1.0".into()] {
        let mut source = remote.source();
        source.reference = Some(reference.clone());
        store
            .update(cx, |store, cx| {
                store.install_git_extensions(vec![("git-extension".into(), source)], cx)
            })
            .await;
        assert!(git_failures(&store, cx).is_empty());
        let installed = installed_source(&store, "git-extension", cx).expect("valid source");
        assert_eq!(installed.reference.as_deref(), Some(reference.as_str()));
        assert_eq!(installed.commit, original.commit);
        assert_eq!(
            installed_version(&store, "git-extension", cx).as_deref(),
            Some("0.1.0")
        );
    }
}

#[gpui::test]
async fn test_git_repository_url_is_not_parsed_as_option(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let remote = GitExtensionRemote::new("git-extension", None);
    let fs: Arc<dyn Fs> = RealFs::new(None, cx.executor());
    let source = crate::GitExtensionSource {
        url: "--get-url".into(),
        reference: None,
        path: None,
    };
    let error = crate::remote_commit(&cx.executor(), &fs, &remote.seed, &source)
        .await
        .expect_err("an option-like repository must not turn a remote check into --get-url");
    assert!(format!("{error:#}").contains("git ls-remote -- --get-url"));
}

#[gpui::test]
async fn test_relative_git_url_updates_from_preparation_directory(cx: &mut TestAppContext) {
    let (_tree, extensions_dir, store, _debounce) = git_extension_store(cx).await;
    let mut remote = GitExtensionRemote::new("git-extension", Some("editors/ext"));
    let staging = extensions_dir.join("staging");
    std::fs::create_dir_all(&staging).expect("staging directory");
    let bare = staging.join("relative-remote.git");
    std::fs::rename(&remote.bare, &bare).expect("relative remote fixture");
    remote.bare = bare;
    remote.url = "../relative-remote.git".into();
    let source = remote.source();
    store
        .update(cx, |store, cx| {
            store.install_git_extension("git-extension".into(), source.clone(), cx)
        })
        .await
        .expect("install relative Git source");
    let marker = extensions_dir.join("sources/git-extension/unchanged-marker");
    std::fs::write(&marker, "preserve unchanged checkout").expect("checkout marker");
    store
        .update(cx, |store, cx| store.check_for_git_updates(cx))
        .await
        .expect("check unchanged relative source");
    assert!(git_failures(&store, cx).is_empty());
    assert!(marker.exists(), "unchanged source must not rebuild");

    let commit = remote.publish("git-extension", "0.2.0");
    store
        .update(cx, |store, cx| store.check_for_git_updates(cx))
        .await
        .expect("update relative source");
    assert!(git_failures(&store, cx).is_empty());
    let installed = installed_source(&store, "git-extension", cx).expect("updated Git source");
    assert!(
        installed.matches(&source),
        "configured relative identity is preserved"
    );
    assert_eq!(installed.commit, commit);
    assert_eq!(
        installed_version(&store, "git-extension", cx).as_deref(),
        Some("0.2.0")
    );
}

#[gpui::test]
async fn test_pinned_relative_git_url_repairs_installed_commit(cx: &mut TestAppContext) {
    let (_tree, extensions_dir, store, _debounce) = git_extension_store(cx).await;
    let mut remote = GitExtensionRemote::new("git-extension", None);
    let staging = extensions_dir.join("staging");
    std::fs::create_dir_all(&staging).expect("staging directory");
    let bare = staging.join("relative-remote.git");
    std::fs::rename(&remote.bare, &bare).expect("relative remote fixture");
    remote.bare = bare;
    remote.url = "../relative-remote.git".into();
    store
        .update(cx, |store, cx| {
            store.install_git_extension("git-extension".into(), remote.source(), cx)
        })
        .await
        .expect("install relative Git source");
    let installed = installed_source(&store, "git-extension", cx);
    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|settings, cx| {
            settings.update_user_settings(cx, |content| {
                content
                    .extension
                    .auto_update_extensions
                    .insert("git-extension".into(), false);
            });
        });
    });
    remote.publish("git-extension", "0.2.0");
    let fs = store.read_with(cx, |store, _| store.fs.clone());
    fs.remove_file(
        &extensions_dir.join("installed/git-extension"),
        RemoveOptions::default(),
    )
    .await
    .expect("remove pinned extension link");
    store
        .update(cx, |store, cx| store.check_for_git_updates(cx))
        .await
        .expect("repair pinned relative source");
    assert!(git_failures(&store, cx).is_empty());
    assert_eq!(installed_source(&store, "git-extension", cx), installed);
    assert_eq!(
        installed_version(&store, "git-extension", cx).as_deref(),
        Some("0.1.0")
    );
    assert!(
        extensions_dir
            .join("installed/git-extension/extension.toml")
            .is_file()
    );
}

#[gpui::test]
async fn test_git_source_replacement_started_before_update_check(cx: &mut TestAppContext) {
    replace_git_source_during_update_check(cx, false).await;
}

#[gpui::test]
async fn test_git_source_replacement_started_after_update_check(cx: &mut TestAppContext) {
    replace_git_source_during_update_check(cx, true).await;
}

async fn replace_git_source_during_update_check(cx: &mut TestAppContext, update_first: bool) {
    let (_tree, extensions_dir, store, _debounce) = git_extension_store(cx).await;
    let first = GitExtensionRemote::new("git-extension", None);
    store
        .update(cx, |store, cx| {
            store.install_git_extension("git-extension".into(), first.source(), cx)
        })
        .await
        .unwrap();
    first.publish("git-extension", "0.2.0");
    let second = GitExtensionRemote::new("git-extension", None);
    let second_commit = git_in(&second.seed, &["rev-parse", "HEAD"]);

    let start_install = |cx: &mut TestAppContext| {
        store.update(cx, |store, cx| {
            store.install_git_extension("git-extension".into(), second.source(), cx)
        })
    };
    let (install, update) = if update_first {
        let update = store.update(cx, |store, cx| store.check_for_git_updates(cx));
        (start_install(cx), update)
    } else {
        let install = start_install(cx);
        (
            install,
            store.update(cx, |store, cx| store.check_for_git_updates(cx)),
        )
    };
    install.await.unwrap();
    update.await.unwrap();

    let installed = installed_source(&store, "git-extension", cx).unwrap();
    assert_eq!(installed.url, second.url);
    assert_eq!(installed.commit, second_commit);
    assert_eq!(
        installed_version(&store, "git-extension", cx).as_deref(),
        Some("0.1.0")
    );
    let record: crate::InstalledGitSource = toml::from_str(
        &std::fs::read_to_string(extensions_dir.join("sources/git-extension.toml")).unwrap(),
    )
    .unwrap();
    assert_eq!(record, installed);
}

#[gpui::test]
async fn test_git_extension_update_check(cx: &mut TestAppContext) {
    let (_tree, extensions_dir, store, _debounce) = git_extension_store(cx).await;
    let remote = GitExtensionRemote::new("git-extension", None);
    store
        .update(cx, |store, cx| {
            store.install_git_extension("git-extension".into(), remote.source(), cx)
        })
        .await
        .unwrap();
    let fetch_head = extensions_dir.join("sources/git-extension/.git/FETCH_HEAD");
    let fetched_at = std::fs::metadata(&fetch_head).unwrap().modified().unwrap();

    store
        .update(cx, |store, cx| store.check_for_git_updates(cx))
        .await
        .unwrap();
    assert_eq!(
        std::fs::metadata(&fetch_head).unwrap().modified().unwrap(),
        fetched_at,
        "an unchanged remote should not be fetched again"
    );
    assert_eq!(
        installed_version(&store, "git-extension", cx).as_deref(),
        Some("0.1.0")
    );

    let second_commit = remote.publish("git-extension", "0.2.0");
    store
        .update(cx, |store, cx| store.check_for_git_updates(cx))
        .await
        .unwrap();
    assert_eq!(
        installed_source(&store, "git-extension", cx).map(|source| source.commit),
        Some(second_commit.clone())
    );
    assert_eq!(
        installed_version(&store, "git-extension", cx).as_deref(),
        Some("0.2.0")
    );

    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|settings, cx| {
            settings.update_user_settings(cx, |content| {
                content
                    .extension
                    .auto_update_extensions
                    .insert("git-extension".into(), false);
            });
        });
    });
    remote.publish("git-extension", "0.3.0");
    store
        .update(cx, |store, cx| store.check_for_git_updates(cx))
        .await
        .unwrap();
    assert_eq!(
        installed_source(&store, "git-extension", cx).map(|source| source.commit),
        Some(second_commit)
    );
    assert_eq!(
        installed_version(&store, "git-extension", cx).as_deref(),
        Some("0.2.0")
    );
}

#[gpui::test]
async fn test_git_extension_source_survives_index_rebuild(cx: &mut TestAppContext) {
    let (_tree, extensions_dir, store, _debounce) = git_extension_store(cx).await;
    let remote = GitExtensionRemote::new("git-extension", None);
    store
        .update(cx, |store, cx| {
            store.install_git_extension("git-extension".into(), remote.source(), cx)
        })
        .await
        .unwrap();
    let source = installed_source(&store, "git-extension", cx);

    std::fs::remove_file(extensions_dir.join("index.json")).unwrap();
    store.update(cx, |store, cx| store.reload(None, cx)).await;

    assert!(source.is_some());
    assert_eq!(installed_source(&store, "git-extension", cx), source);
}

#[gpui::test]
async fn test_uninstall_git_extension_removes_clone(cx: &mut TestAppContext) {
    let (_tree, extensions_dir, store, _debounce) = git_extension_store(cx).await;
    let remote = GitExtensionRemote::new("git-extension", None);
    store
        .update(cx, |store, cx| {
            store.install_git_extension("git-extension".into(), remote.source(), cx)
        })
        .await
        .unwrap();

    store
        .update(cx, |store, cx| {
            store.uninstall_extension("git-extension".into(), cx)
        })
        .await
        .unwrap();

    assert!(std::fs::symlink_metadata(extensions_dir.join("installed/git-extension")).is_err());
    assert!(!extensions_dir.join("sources/git-extension").exists());
    assert!(!extensions_dir.join("sources/git-extension.toml").exists());
    store.read_with(cx, |store, _| {
        assert!(
            !store
                .extension_index
                .extensions
                .contains_key("git-extension")
        );
    });
}

#[gpui::test]
async fn test_install_git_extensions_continues_after_a_failure(cx: &mut TestAppContext) {
    let (_tree, _extensions_dir, store, _debounce) = git_extension_store(cx).await;
    let first = GitExtensionRemote::new("first-extension", None);
    let second = GitExtensionRemote::new("second-extension", None);

    store
        .update(cx, |store, cx| {
            store.install_git_extensions(
                vec![
                    ("mismatched".into(), first.source()),
                    ("second-extension".into(), second.source()),
                ],
                cx,
            )
        })
        .await;

    store.read_with(cx, |store, _| {
        assert!(!store.extension_index.extensions.contains_key("mismatched"));
        assert!(
            store
                .extension_index
                .extensions
                .contains_key("second-extension")
        );
    });
}

#[gpui::test]
async fn test_git_extensions_to_install_follow_settings(cx: &mut TestAppContext) {
    let (_tree, _extensions_dir, store, _debounce) = git_extension_store(cx).await;
    let remote = GitExtensionRemote::new("git-extension", None);
    store
        .update(cx, |store, cx| {
            store.install_git_extension("git-extension".into(), remote.source(), cx)
        })
        .await
        .unwrap();

    let mut content: settings::ExtensionSettingsContent = serde_json::from_value(json!({
        "auto_install_extensions": {
            "html": true,
            "git-extension": { "git": remote.url, "ref": "master" },
            "zed-ldap": { "git": "https://example.com/zed-ldap" },
            "turborepo": {
                "git": "https://example.com/zed-turborepo",
                "ref": "master",
                "path": "crates/turbo-zed"
            }
        }
    }))
    .unwrap();
    for extension_id in crate::SUPPRESSED_EXTENSIONS.iter() {
        content.auto_install_extensions.insert(
            (*extension_id).into(),
            settings::AutoInstallExtensionContent::Git {
                git: format!("https://example.com/{extension_id}"),
                reference: None,
                path: None,
            },
        );
    }
    assert_eq!(
        content.auto_install_extensions["turborepo"],
        settings::AutoInstallExtensionContent::Git {
            git: "https://example.com/zed-turborepo".into(),
            reference: Some("master".into()),
            path: Some("crates/turbo-zed".into()),
        }
    );
    let extension_settings = crate::ExtensionSettings {
        auto_install_extensions: content.auto_install_extensions,
        ..Default::default()
    };
    assert!(extension_settings.should_auto_install("zed-ldap"));
    assert_eq!(
        extension_settings
            .marketplace_extensions()
            .map(|id| id.as_ref())
            .collect::<Vec<_>>(),
        ["html"]
    );

    let mut to_install = store.read_with(cx, |store, _| {
        store
            .git_extensions_to_install(&extension_settings)
            .into_iter()
            .map(|(extension_id, _)| extension_id)
            .collect::<Vec<_>>()
    });
    to_install.sort();
    assert_eq!(to_install, [Arc::from("turborepo"), Arc::from("zed-ldap")]);

    let mut moved = extension_settings;
    moved.auto_install_extensions.insert(
        "git-extension".into(),
        settings::AutoInstallExtensionContent::Git {
            git: remote.url.clone(),
            reference: Some("release".into()),
            path: None,
        },
    );
    let to_install = store.read_with(cx, |store, _| {
        store
            .git_extensions_to_install(&moved)
            .into_iter()
            .map(|(extension_id, _)| extension_id)
            .collect::<Vec<_>>()
    });
    assert!(to_install.contains(&Arc::from("git-extension")));
}

fn marketplace_settings(extension_id: &str) -> crate::ExtensionSettings {
    crate::ExtensionSettings {
        auto_install_extensions: HashMap::from_iter([(
            Arc::from(extension_id),
            settings::AutoInstallExtensionContent::Marketplace(true),
        )]),
        ..Default::default()
    }
}

#[gpui::test]
async fn test_auto_install_publishes_independent_candidates_while_preparation_is_stalled(
    cx: &mut TestAppContext,
) {
    let (client, _) = marketplace_serving_grammar_batch(vec![
        MarketplaceGrammarFixture {
            id: "stalled",
            ..Default::default()
        },
        MarketplaceGrammarFixture {
            id: "independent-marketplace",
            ..Default::default()
        },
    ]);
    let (client, release, started) = stall_marketplace_request(client, "stalled");
    let (_tree, _extensions_dir, store, _debounce) =
        git_extension_store_with_http(cx, client).await;
    let remote = GitExtensionRemote::new("independent-git", None);
    let batch = store.update(cx, |store, cx| {
        store.install_auto_extensions(
            vec![
                "stalled".into(),
                "independent-marketplace".into(),
                "missing".into(),
            ],
            vec![("independent-git".into(), remote.source())],
            cx,
        )
    });
    started.await.expect("stalled preparation started");
    cx.condition(&store, |store, _| {
        store
            .installed_extensions()
            .contains_key("independent-marketplace")
            && store.installed_extensions().contains_key("independent-git")
            && !store.outstanding_operations().contains_key("missing")
    })
    .await;
    assert_eq!(
        installed_version(&store, "independent-marketplace", cx).as_deref(),
        Some("2.0.0")
    );
    assert_eq!(
        installed_version(&store, "independent-git", cx).as_deref(),
        Some("0.1.0")
    );
    assert!(installed_version(&store, "stalled", cx).is_none());
    assert!(installed_version(&store, "missing", cx).is_none());
    assert!(store.read_with(cx, |store, _| {
        store.outstanding_operations().contains_key("stalled")
    }));
    release.send(()).expect("stalled request still waits");
    batch.await;
    assert_eq!(
        installed_version(&store, "stalled", cx).as_deref(),
        Some("2.0.0")
    );
    assert!(store.read_with(cx, |store, _| store.outstanding_operations().is_empty()));
}

#[gpui::test]
async fn test_auto_install_consumers_wait_for_unknown_grammar_exports(cx: &mut TestAppContext) {
    for removes_existing_provider in [false, true] {
        let (client, _) = marketplace_serving_grammar_batch(vec![MarketplaceGrammarFixture {
            id: "provider",
            provides_html: true,
            ..Default::default()
        }]);
        let (_tree, extensions_dir, store, _debounce) =
            git_extension_store_with_http(cx, client).await;
        let registry = Arc::new(LanguageRegistry::test(cx.executor()));
        language_extension::init(
            LspAccess::Noop,
            store.read_with(cx, |store, _| store.proxy.clone()),
            registry,
        );
        store
            .update(cx, |store, cx| {
                store.install_latest_extension_task("provider".into(), cx)
            })
            .await
            .expect("existing html provider");
        let remote = GitExtensionRemote::new("consumer", None);
        store
            .update(cx, |store, cx| {
                store.install_git_extension("consumer".into(), remote.source(), cx)
            })
            .await
            .expect("existing consumer");
        let original_source = installed_source(&store, "consumer", cx);
        let record = extensions_dir.join("sources/consumer.toml");
        let original_record = std::fs::read(&record).expect("original consumer record");
        let unknown_id = if removes_existing_provider {
            "provider"
        } else {
            "other-provider"
        };
        let (client, _) = marketplace_serving_grammar_batch(vec![
            MarketplaceGrammarFixture {
                id: "consumer",
                consumes_html: true,
                ..Default::default()
            },
            MarketplaceGrammarFixture {
                id: unknown_id,
                provides_html: !removes_existing_provider,
                ..Default::default()
            },
            MarketplaceGrammarFixture {
                id: "independent",
                ..Default::default()
            },
        ]);
        let (client, release, started) = stall_marketplace_request(client, unknown_id);
        store.update(cx, |store, _| store.http_client = client);
        let batch = store.update(cx, |store, cx| {
            store.install_marketplace_extensions(
                vec!["consumer".into(), unknown_id.into(), "independent".into()],
                cx,
            )
        });
        started.await.expect("unknown provider request started");
        cx.condition(&store, |store, _| {
            store.installed_extensions().contains_key("independent")
        })
        .await;
        cx.run_until_parked();
        assert_eq!(installed_source(&store, "consumer", cx), original_source);
        assert_eq!(
            installed_version(&store, "consumer", cx).as_deref(),
            Some("0.1.0"),
            "consumer must wait for unknown exports"
        );
        release.send(()).expect("unknown provider still waits");
        batch.await;
        assert_eq!(installed_source(&store, "consumer", cx), original_source);
        assert_eq!(
            std::fs::read(&record).expect("preserved record"),
            original_record
        );
        assert_eq!(
            installed_version(&store, "consumer", cx).as_deref(),
            Some("0.1.0"),
            "consumer rejects the final missing or ambiguous grammar"
        );
        assert_eq!(
            extensions_dir
                .join("installed/provider/grammars/html.wasm")
                .is_file(),
            !removes_existing_provider
        );
        assert_eq!(
            installed_version(&store, unknown_id, cx).as_deref(),
            Some("2.0.0")
        );
        assert!(store.read_with(cx, |store, _| store.outstanding_operations().is_empty()));
    }
}

#[gpui::test]
async fn test_auto_install_timeout_releases_consumers_of_installed_grammars(
    cx: &mut TestAppContext,
) {
    for stalled_id in ["unrelated", "provider"] {
        let (client, _) = marketplace_serving_grammar_batch(vec![
            MarketplaceGrammarFixture {
                id: "provider",
                provides_html: true,
                ..Default::default()
            },
            MarketplaceGrammarFixture {
                id: "consumer",
                consumes_html: true,
                ..Default::default()
            },
            MarketplaceGrammarFixture {
                id: "unrelated",
                ..Default::default()
            },
            MarketplaceGrammarFixture {
                id: "independent",
                ..Default::default()
            },
        ]);
        let (_tree, extensions_dir, store, _debounce) =
            git_extension_store_with_http(cx, client.clone()).await;
        let registry = Arc::new(LanguageRegistry::test(cx.executor()));
        language_extension::init(
            LspAccess::Noop,
            store.read_with(cx, |store, _| store.proxy.clone()),
            registry,
        );
        store
            .update(cx, |store, cx| {
                store.install_latest_extension_task("provider".into(), cx)
            })
            .await
            .expect("installed grammar provider");
        let provider_path = extensions_dir.join("installed/provider/grammars/html.wasm");
        let original_grammar = std::fs::read(&provider_path).expect("installed grammar");
        let (gated_client, release, started) =
            stall_marketplace_request(client.clone(), stalled_id);
        let (prepared, prepared_receiver) = futures::channel::oneshot::channel();
        store.update(cx, |store, _| {
            store.http_client = gated_client;
            store.auto_install_prepared_observer = Some(("consumer".into(), prepared));
        });
        let batch = store.update(cx, |store, cx| {
            store.install_marketplace_extensions(
                vec!["consumer".into(), stalled_id.into(), "independent".into()],
                cx,
            )
        });
        started.await.expect("preparation stalled");
        cx.condition(&store, |store, _| {
            store.installed_extensions().contains_key("independent")
        })
        .await;
        prepared_receiver
            .await
            .expect("consumer waits with preparation complete");
        cx.run_until_parked();
        assert!(installed_version(&store, "consumer", cx).is_none());
        assert!(store.read_with(cx, |store, _| {
            store.outstanding_operations().contains_key(stalled_id)
        }));

        cx.executor()
            .advance_clock(crate::EXTENSION_PREPARATION_TIMEOUT);
        batch.await;
        cx.run_until_parked();
        assert!(
            release.send(()).is_err(),
            "deadline cancels the stalled HTTP future"
        );
        assert_eq!(
            installed_version(&store, "consumer", cx).as_deref(),
            Some("2.0.0")
        );
        assert_eq!(
            std::fs::read(&provider_path).expect("surviving provider"),
            original_grammar
        );
        assert!(store.read_with(cx, |store, _| store.outstanding_operations().is_empty()));

        store.update(cx, |store, _| store.http_client = client);
        store
            .update(cx, |store, cx| {
                store.install_marketplace_extensions(vec![stalled_id.into()], cx)
            })
            .await;
        assert_eq!(
            installed_version(&store, stalled_id, cx).as_deref(),
            Some("2.0.0")
        );
        assert!(store.read_with(cx, |store, _| store.outstanding_operations().is_empty()));
    }
}

#[gpui::test]
async fn test_cancelled_auto_install_releases_stalled_preparation(cx: &mut TestAppContext) {
    let (client, _) = marketplace_serving_grammar_batch(vec![MarketplaceGrammarFixture {
        id: "stalled",
        ..Default::default()
    }]);
    let (client, release, started) = stall_marketplace_request(client, "stalled");
    let (_tree, extensions_dir, store, _debounce) = git_extension_store_with_http(cx, client).await;
    let batch = store.update(cx, |store, cx| {
        store.install_marketplace_extensions(vec!["stalled".into()], cx)
    });
    started.await.expect("preparation waits on response");
    drop(batch);
    cx.condition(&store, |store, _| store.outstanding_operations().is_empty())
        .await;
    cx.run_until_parked();
    assert!(
        release.send(()).is_err(),
        "cancelled task drops the HTTP future"
    );
    assert!(installed_version(&store, "stalled", cx).is_none());
    assert!(!extensions_dir.join("installed/stalled").exists());
}

fn stall_marketplace_request(
    client: Arc<HttpClientWithUrl>,
    extension_id: &'static str,
) -> (
    Arc<HttpClientWithUrl>,
    futures::channel::oneshot::Sender<()>,
    futures::channel::oneshot::Receiver<()>,
) {
    use http_client::HttpClient as _;

    let (release_tx, release_rx) = futures::channel::oneshot::channel();
    let (started_tx, started_rx) = futures::channel::oneshot::channel();
    let release_rx = Arc::new(Mutex::new(Some(release_rx)));
    let started_tx = Arc::new(Mutex::new(Some(started_tx)));
    let gated = FakeHttpClient::create(move |request| {
        let client = client.clone();
        let release_rx = release_rx.clone();
        let started_tx = started_tx.clone();
        async move {
            if request
                .uri()
                .path()
                .ends_with(&format!("/extensions/{extension_id}/download"))
            {
                let started = started_tx.lock().take().context("one stalled request")?;
                let release = release_rx.lock().take().context("one stalled response")?;
                started
                    .send(())
                    .map_err(|_| anyhow::anyhow!("test stopped waiting for request"))?;
                release.await.context("stalled request was cancelled")?;
            }
            client.send(request).await
        }
    });
    (gated, release_tx, started_rx)
}

#[gpui::test]
async fn test_marketplace_batch_installs_grammar_provider_before_consumer(cx: &mut TestAppContext) {
    use crate::extension_transaction::TransactionPhase;

    for order in [["consumer", "provider"], ["provider", "consumer"]] {
        let (client, downloads) = marketplace_serving_grammar_batch(vec![
            MarketplaceGrammarFixture {
                id: "consumer",
                consumes_html: true,
                ..Default::default()
            },
            MarketplaceGrammarFixture {
                id: "provider",
                provides_html: true,
                ..Default::default()
            },
        ]);
        let (_tree, _extensions_dir, store, _debounce) =
            git_extension_store_with_http(cx, client).await;
        let registry = Arc::new(LanguageRegistry::test(cx.executor()));
        language_extension::init(
            LspAccess::Noop,
            store.read_with(cx, |store, _| store.proxy.clone()),
            registry.clone(),
        );
        let published = Arc::new(Mutex::new(Vec::new()));
        let publisher = store.read_with(cx, |store, _| store.publisher.clone());
        publisher.set_hook({
            let published = published.clone();
            move |info| {
                let published = published.clone();
                async move {
                    if info.phase == TransactionPhase::BeforePublish {
                        published.lock().push(info.extension_id);
                    }
                    Ok(())
                }
                .boxed()
            }
        });
        store
            .update(cx, |store, cx| {
                store.install_marketplace_extensions(order.into_iter().map(Arc::from).collect(), cx)
            })
            .await;
        publisher.clear_hook();
        assert_eq!(
            *published.lock(),
            ["provider", "consumer"],
            "input order: {order:?}"
        );
        assert_eq!(
            downloads.load(Ordering::SeqCst),
            2,
            "each archive downloaded once"
        );
        for extension_id in order {
            assert_eq!(
                installed_version(&store, extension_id, cx).as_deref(),
                Some("2.0.0")
            );
        }
        registry
            .language_for_name("Language consumer")
            .await
            .expect("consumer queries load using newly installed provider");
        assert!(store.read_with(cx, |store, _| store.outstanding_operations().is_empty()));
    }
}

#[derive(Clone, Copy)]
enum GrammarBatchSource {
    Git,
    Marketplace,
}

async fn assert_auto_install_grammar_batch(
    provider_source: GrammarBatchSource,
    consumer_source: GrammarBatchSource,
    consumer_first: bool,
    cx: &mut TestAppContext,
) {
    use crate::extension_transaction::TransactionPhase;

    let grammar_repository = GitExtensionRemote::new("grammar-fixture", None);
    let mut remotes = Vec::new();
    let mut git_sources = Vec::new();
    let mut marketplace_fixtures = Vec::new();
    let mut marketplace_ids = Vec::new();
    for (id, source) in [("provider", provider_source), ("consumer", consumer_source)] {
        match source {
            GrammarBatchSource::Marketplace => {
                marketplace_ids.push(Arc::from(id));
                marketplace_fixtures.push(MarketplaceGrammarFixture {
                    id,
                    provides_html: id == "provider",
                    consumes_html: id == "consumer",
                    ..Default::default()
                });
            }
            GrammarBatchSource::Git => {
                let remote = GitExtensionRemote::new(id, None);
                let declaration = if id == "provider" {
                    std::fs::create_dir(remote.seed.join("grammars")).expect("grammar directory");
                    std::fs::write(
                        remote.seed.join("grammars/html.wasm"),
                        include_bytes!("../../language/test_data/grammars/html.wasm"),
                    )
                    .expect("precompiled grammar");
                    format!(
                        "[grammars.html]\nrepository = {}\nrev = \"master\"\n",
                        toml::Value::String(grammar_repository.url.clone())
                    )
                } else {
                    std::fs::create_dir_all(remote.seed.join("languages/html"))
                        .expect("language directory");
                    std::fs::write(
                        remote.seed.join("languages/html/config.toml"),
                        "name = \"Language consumer\"\ngrammar = \"html\"\n",
                    )
                    .expect("language config");
                    std::fs::write(
                        remote.seed.join("languages/html/highlights.scm"),
                        "(tag_name) @tag",
                    )
                    .expect("language query");
                    String::new()
                };
                remote.publish_with_manifest(id, "2.0.0", &declaration);
                git_sources.push((Arc::from(id), remote.source()));
                remotes.push(remote);
            }
        }
    }
    if consumer_first {
        git_sources.reverse();
        marketplace_ids.reverse();
    }
    let expected_downloads = marketplace_ids.len();
    let (client, downloads) = marketplace_serving_grammar_batch(marketplace_fixtures);
    let (_tree, extensions_dir, store, _debounce) = git_extension_store_with_http(cx, client).await;
    // No parser sources means the bundled WASM is current. An empty compiler
    // satisfies SDK discovery but fails if this fixture unexpectedly compiles.
    let compiler = extensions_dir
        .join("build/wasi-sdk/bin")
        .join(format!("clang{}", std::env::consts::EXE_SUFFIX));
    std::fs::create_dir_all(compiler.parent().expect("compiler directory"))
        .expect("cached SDK directory");
    std::fs::write(compiler, []).expect("unused compiler fixture");
    let registry = Arc::new(LanguageRegistry::test(cx.executor()));
    language_extension::init(
        LspAccess::Noop,
        store.read_with(cx, |store, _| store.proxy.clone()),
        registry.clone(),
    );
    let published = Arc::new(Mutex::new(Vec::new()));
    let publisher = store.read_with(cx, |store, _| store.publisher.clone());
    publisher.set_hook({
        let published = published.clone();
        move |info| {
            let published = published.clone();
            async move {
                if info.phase == TransactionPhase::BeforePublish {
                    published.lock().push(info.extension_id);
                }
                Ok(())
            }
            .boxed()
        }
    });
    store
        .update(cx, |store, cx| {
            if marketplace_ids.is_empty() {
                store.install_git_extensions(git_sources, cx)
            } else {
                store.install_auto_extensions(marketplace_ids, git_sources, cx)
            }
        })
        .await;
    publisher.clear_hook();
    assert_eq!(*published.lock(), ["provider", "consumer"]);
    assert_eq!(downloads.load(Ordering::SeqCst), expected_downloads);
    assert!(git_failures(&store, cx).is_empty());
    for id in ["provider", "consumer"] {
        assert_eq!(installed_version(&store, id, cx).as_deref(), Some("2.0.0"));
    }
    registry
        .language_for_name("Language consumer")
        .await
        .expect("consumer queries load through the newly installed grammar");
    assert!(store.read_with(cx, |store, _| store.outstanding_operations().is_empty()));
}

#[gpui::test]
async fn test_git_batch_installs_grammar_provider_before_consumer(cx: &mut TestAppContext) {
    for consumer_first in [true, false] {
        assert_auto_install_grammar_batch(
            GrammarBatchSource::Git,
            GrammarBatchSource::Git,
            consumer_first,
            cx,
        )
        .await;
    }
}

#[gpui::test]
async fn test_git_consumer_waits_for_marketplace_grammar_provider(cx: &mut TestAppContext) {
    assert_auto_install_grammar_batch(
        GrammarBatchSource::Marketplace,
        GrammarBatchSource::Git,
        true,
        cx,
    )
    .await;
}

#[gpui::test]
async fn test_marketplace_consumer_waits_for_git_grammar_provider(cx: &mut TestAppContext) {
    assert_auto_install_grammar_batch(
        GrammarBatchSource::Git,
        GrammarBatchSource::Marketplace,
        true,
        cx,
    )
    .await;
}

#[gpui::test]
async fn test_git_batch_reports_grammar_cycles_without_publishing(cx: &mut TestAppContext) {
    let (_tree, _extensions_dir, store, _debounce) = git_extension_store(cx).await;
    let mut remotes = Vec::new();
    let mut sources = Vec::new();
    for (id, dependency) in [("alpha", "beta"), ("beta", "alpha")] {
        let remote = GitExtensionRemote::new(id, None);
        std::fs::create_dir_all(remote.seed.join("languages/test")).expect("language directory");
        std::fs::write(
            remote.seed.join("languages/test/config.toml"),
            format!("name = \"{id}\"\ngrammar = \"{dependency}\"\n"),
        )
        .expect("language config");
        remote.publish_with_manifest(
            id,
            "2.0.0",
            &format!("[grammars.{id}]\nrepository = \"unused\"\nrev = \"unused\"\n"),
        );
        sources.push((Arc::from(id), remote.source()));
        remotes.push(remote);
    }
    store
        .update(cx, |store, cx| store.install_git_extensions(sources, cx))
        .await;
    let failures = git_failures(&store, cx);
    assert_eq!(failures.len(), 2);
    for failure in failures {
        assert!(failure.message.contains("cyclic"), "{failure:?}");
    }
    for id in ["alpha", "beta"] {
        assert!(installed_version(&store, id, cx).is_none());
    }
    assert!(store.read_with(cx, |store, _| store.outstanding_operations().is_empty()));
}

#[gpui::test]
async fn test_failed_marketplace_provider_keeps_existing_git_consumer(cx: &mut TestAppContext) {
    let (client, downloads) = marketplace_serving_grammar_batch(vec![MarketplaceGrammarFixture {
        id: "provider",
        provides_html: true,
        malformed_theme: true,
        ..Default::default()
    }]);
    let (_tree, extensions_dir, store, _debounce) = git_extension_store_with_http(cx, client).await;
    let registry = Arc::new(LanguageRegistry::test(cx.executor()));
    language_extension::init(
        LspAccess::Noop,
        store.read_with(cx, |store, _| store.proxy.clone()),
        registry,
    );
    let remote = GitExtensionRemote::new("consumer", None);
    store
        .update(cx, |store, cx| {
            store.install_git_extension("consumer".into(), remote.source(), cx)
        })
        .await
        .expect("original consumer");
    let original_source = installed_source(&store, "consumer", cx);
    let record = extensions_dir.join("sources/consumer.toml");
    let original_record = std::fs::read(&record).expect("original record");
    let installed = extensions_dir.join("installed/consumer");
    let original_target = std::fs::read_link(&installed).expect("original link");
    let original_manifest =
        std::fs::read(installed.join("extension.toml")).expect("original manifest");
    std::fs::create_dir_all(remote.seed.join("languages/html")).expect("language directory");
    std::fs::write(
        remote.seed.join("languages/html/config.toml"),
        "name = \"Consumer\"\ngrammar = \"html\"\n",
    )
    .expect("language config");
    remote.publish("consumer", "2.0.0");
    store
        .update(cx, |store, cx| {
            store.install_auto_extensions(
                vec!["provider".into()],
                vec![("consumer".into(), remote.source())],
                cx,
            )
        })
        .await;
    assert_eq!(downloads.load(Ordering::SeqCst), 1);
    let failures = git_failures(&store, cx);
    assert_eq!(failures.len(), 1);
    assert!(
        failures
            .first()
            .expect("consumer failure")
            .message
            .contains("no such grammar html"),
        "{failures:?}"
    );
    assert_eq!(installed_source(&store, "consumer", cx), original_source);
    assert_eq!(
        std::fs::read(&record).expect("preserved record"),
        original_record
    );
    assert_eq!(
        std::fs::read_link(&installed).expect("preserved link"),
        original_target
    );
    assert_eq!(
        std::fs::read(installed.join("extension.toml")).expect("preserved manifest"),
        original_manifest
    );
    assert_eq!(
        installed_version(&store, "consumer", cx).as_deref(),
        Some("0.1.0")
    );
    assert!(installed_version(&store, "provider", cx).is_none());
    assert!(store.read_with(cx, |store, _| store.outstanding_operations().is_empty()));
}

#[gpui::test]
async fn test_marketplace_batch_waits_for_removed_grammar_exports(cx: &mut TestAppContext) {
    let (client, _) = marketplace_serving_grammar_batch(vec![MarketplaceGrammarFixture {
        id: "provider",
        provides_html: true,
        ..Default::default()
    }]);
    let (_tree, extensions_dir, store, _debounce) = git_extension_store_with_http(cx, client).await;
    let registry = Arc::new(LanguageRegistry::test(cx.executor()));
    language_extension::init(
        LspAccess::Noop,
        store.read_with(cx, |store, _| store.proxy.clone()),
        registry,
    );
    store
        .update(cx, |store, cx| {
            store.install_latest_extension_task("provider".into(), cx)
        })
        .await
        .expect("initial provider exports html");
    let provider_grammar = extensions_dir.join("installed/provider/grammars/html.wasm");
    assert!(provider_grammar.is_file());
    let remote = GitExtensionRemote::new("consumer", None);
    store
        .update(cx, |store, cx| {
            store.install_git_extension("consumer".into(), remote.source(), cx)
        })
        .await
        .expect("existing consumer install");
    let original_source = installed_source(&store, "consumer", cx);
    let installed = extensions_dir.join("installed/consumer");
    let original_target = std::fs::read_link(&installed).expect("original consumer link");
    let original_manifest =
        std::fs::read(installed.join("extension.toml")).expect("original consumer manifest");
    let record = extensions_dir.join("sources/consumer.toml");
    let original_record = std::fs::read(&record).expect("original consumer source record");
    let (client, downloads) = marketplace_serving_grammar_batch(vec![
        MarketplaceGrammarFixture {
            id: "consumer",
            consumes_html: true,
            ..Default::default()
        },
        MarketplaceGrammarFixture {
            id: "provider",
            ..Default::default()
        },
    ]);
    store.update(cx, |store, _| store.http_client = client);
    store
        .update(cx, |store, cx| {
            store.install_marketplace_extensions(vec!["consumer".into(), "provider".into()], cx)
        })
        .await;
    assert_eq!(downloads.load(Ordering::SeqCst), 2);
    assert!(
        !provider_grammar.exists(),
        "provider replacement removed its grammar"
    );
    assert_eq!(
        installed_version(&store, "provider", cx).as_deref(),
        Some("2.0.0")
    );
    assert_eq!(installed_source(&store, "consumer", cx), original_source);
    assert_eq!(
        installed_version(&store, "consumer", cx).as_deref(),
        Some("0.1.0")
    );
    assert_eq!(
        std::fs::read_link(&installed).expect("consumer link preserved"),
        original_target
    );
    assert_eq!(
        std::fs::read(installed.join("extension.toml")).expect("consumer checkout preserved"),
        original_manifest
    );
    assert_eq!(
        std::fs::read(&record).expect("consumer source record preserved"),
        original_record
    );
    assert!(store.read_with(cx, |store, _| store.outstanding_operations().is_empty()));
}

#[derive(Clone, Copy, Debug)]
enum MarketplaceDependencyFailure {
    MalformedConsumer,
    RejectedProvider,
    AmbiguousProviders,
}

#[gpui::test]
async fn test_marketplace_batch_keeps_existing_consumer_when_validation_fails(
    cx: &mut TestAppContext,
) {
    use MarketplaceDependencyFailure::*;

    for failure in [MalformedConsumer, RejectedProvider, AmbiguousProviders] {
        let mut fixtures = vec![
            MarketplaceGrammarFixture {
                id: "consumer",
                consumes_html: true,
                malformed_query: matches!(failure, MalformedConsumer),
                ..Default::default()
            },
            MarketplaceGrammarFixture {
                id: "provider",
                provides_html: true,
                malformed_theme: matches!(failure, RejectedProvider),
                ..Default::default()
            },
        ];
        if matches!(failure, AmbiguousProviders) {
            fixtures.push(MarketplaceGrammarFixture {
                id: "other-provider",
                provides_html: true,
                ..Default::default()
            });
        }
        let ids = fixtures
            .iter()
            .map(|fixture| Arc::from(fixture.id))
            .collect::<Vec<_>>();
        let (client, downloads) = marketplace_serving_grammar_batch(fixtures);
        let (_tree, extensions_dir, store, _debounce) =
            git_extension_store_with_http(cx, client).await;
        let registry = Arc::new(LanguageRegistry::test(cx.executor()));
        language_extension::init(
            LspAccess::Noop,
            store.read_with(cx, |store, _| store.proxy.clone()),
            registry,
        );
        let remote = GitExtensionRemote::new("consumer", None);
        store
            .update(cx, |store, cx| {
                store.install_git_extension("consumer".into(), remote.source(), cx)
            })
            .await
            .expect("existing consumer install");
        let original_source = installed_source(&store, "consumer", cx);
        let installed = extensions_dir.join("installed/consumer");
        let original_target = std::fs::read_link(&installed).expect("original consumer link");
        let original_manifest =
            std::fs::read(installed.join("extension.toml")).expect("original consumer manifest");
        let record = extensions_dir.join("sources/consumer.toml");
        let original_record = std::fs::read(&record).expect("original source record");
        let count = ids.len();
        store
            .update(cx, |store, cx| {
                store.install_marketplace_extensions(ids, cx)
            })
            .await;
        assert_eq!(downloads.load(Ordering::SeqCst), count, "{failure:?}");
        assert_eq!(
            installed_source(&store, "consumer", cx),
            original_source,
            "{failure:?}"
        );
        assert_eq!(
            installed_version(&store, "consumer", cx).as_deref(),
            Some("0.1.0"),
            "{failure:?}"
        );
        assert_eq!(
            std::fs::read_link(&installed).expect("consumer link remains"),
            original_target
        );
        assert_eq!(
            std::fs::read(installed.join("extension.toml")).expect("consumer checkout remains"),
            original_manifest
        );
        assert_eq!(
            std::fs::read(&record).expect("consumer record remains"),
            original_record
        );
        assert_eq!(
            installed_version(&store, "provider", cx).is_some(),
            !matches!(failure, RejectedProvider)
        );
        if matches!(failure, AmbiguousProviders) {
            assert_eq!(
                installed_version(&store, "other-provider", cx).as_deref(),
                Some("2.0.0")
            );
        }
        assert!(store.read_with(cx, |store, _| store.outstanding_operations().is_empty()));
    }
}

#[gpui::test]
async fn test_marketplace_setting_replaces_git_install(cx: &mut TestAppContext) {
    let (_tree, extensions_dir, store, _debounce) =
        git_extension_store_with_http(cx, marketplace_serving("git-extension", "2.0.0")).await;
    let remote = GitExtensionRemote::new("git-extension", None);
    store
        .update(cx, |store, cx| {
            store.install_git_extension("git-extension".into(), remote.source(), cx)
        })
        .await
        .unwrap();

    remote.publish("other-extension", "0.2.0");
    store
        .update(cx, |store, cx| store.check_for_git_updates(cx))
        .await
        .expect("update check should report the failed update");
    assert_eq!(git_failures(&store, cx).len(), 1);

    let settings = marketplace_settings("git-extension");
    let to_install = store.read_with(cx, |store, _| {
        store.marketplace_extensions_to_install(&settings)
    });
    assert_eq!(to_install, [Arc::from("git-extension")]);
    store
        .update(cx, |store, cx| {
            store.install_marketplace_extensions(to_install, cx)
        })
        .await;

    let entry = store.read_with(cx, |store, _| {
        store.extension_index.extensions["git-extension"].clone()
    });
    assert!(!entry.dev);
    assert_eq!(entry.source, None);
    assert_eq!(entry.manifest.version.as_ref(), "2.0.0");
    assert!(git_failures(&store, cx).is_empty());
    store.read_with(cx, |store, _| {
        assert!(store.undelivered_git_extension_failures.is_empty());
    });
    assert!(!extensions_dir.join("sources/git-extension").exists());
    assert!(!extensions_dir.join("sources/git-extension.toml").exists());
    assert!(
        store
            .read_with(cx, |store, _| store
                .marketplace_extensions_to_install(&settings))
            .is_empty()
    );
}

#[gpui::test]
async fn test_failed_marketplace_switch_keeps_git_install(cx: &mut TestAppContext) {
    let (_tree, extensions_dir, store, _debounce) =
        git_extension_store_with_http(cx, marketplace_serving("other-extension", "2.0.0")).await;
    let remote = GitExtensionRemote::new("git-extension", None);
    store
        .update(cx, |store, cx| {
            store.install_git_extension("git-extension".into(), remote.source(), cx)
        })
        .await
        .unwrap();
    let installed = installed_source(&store, "git-extension", cx);

    let settings = marketplace_settings("git-extension");
    let to_install = store.read_with(cx, |store, _| {
        store.marketplace_extensions_to_install(&settings)
    });
    store
        .update(cx, |store, cx| {
            store.install_marketplace_extensions(to_install, cx)
        })
        .await;

    assert!(installed.is_some());
    assert_eq!(installed_source(&store, "git-extension", cx), installed);
    assert!(extensions_dir.join("sources/git-extension.toml").exists());
    assert!(
        extensions_dir
            .join("installed/git-extension/extension.toml")
            .exists()
    );
}

fn git_failures(
    store: &Entity<ExtensionStore>,
    cx: &mut TestAppContext,
) -> Vec<crate::GitExtensionFailure> {
    store.read_with(cx, |store, _| {
        store
            .active_git_extension_failures()
            .values()
            .cloned()
            .collect()
    })
}

fn stall_extension_build(
    store: &Entity<ExtensionStore>,
    extension_id: &'static str,
    cx: &mut TestAppContext,
) -> (
    futures::channel::oneshot::Receiver<PathBuf>,
    futures::channel::oneshot::Receiver<()>,
) {
    let (started, started_receiver) = futures::channel::oneshot::channel();
    let (cancelled, cancelled_receiver) = futures::channel::oneshot::channel();
    let started = Arc::new(Mutex::new(Some(started)));
    let cancelled = Arc::new(Mutex::new(Some(cancelled)));
    store.read_with(cx, |store, _| {
        store.builder.set_precompile_hook(move |directory| {
            let directory = directory.to_path_buf();
            let started = started.clone();
            let cancelled = cancelled.clone();
            async move {
                let manifest: toml::Value =
                    toml::from_str(&std::fs::read_to_string(directory.join("extension.toml"))?)?;
                if manifest.get("id").and_then(toml::Value::as_str) == Some(extension_id) {
                    let _cancelled = util::defer(move || {
                        cancelled
                            .lock()
                            .take()
                            .expect("one build cancellation")
                            .send(())
                            .expect("build cancellation observer");
                    });
                    started
                        .lock()
                        .take()
                        .expect("one stalled build")
                        .send(directory)
                        .expect("build observer");
                    futures::future::pending::<()>().await;
                }
                Ok(())
            }
            .boxed()
        });
    });
    (started_receiver, cancelled_receiver)
}

#[gpui::test]
async fn test_git_build_timeout_preserves_install_and_releases_batch(cx: &mut TestAppContext) {
    let (client, _) = marketplace_serving_grammar_batch(vec![MarketplaceGrammarFixture {
        id: "later",
        ..Default::default()
    }]);
    let (_tree, extensions_dir, store, _debounce) =
        git_extension_store_with_http(cx, client.clone()).await;
    let remote = GitExtensionRemote::new("blocked", None);
    store
        .update(cx, |store, cx| {
            store.install_git_extension("blocked".into(), remote.source(), cx)
        })
        .await
        .expect("initial Git install");
    let independent = local_dev_extension("independent");
    store
        .update(cx, |store, cx| {
            store.install_dev_extension(tree_root(&independent), cx)
        })
        .await
        .expect("independent local install");
    let source = installed_source(&store, "blocked", cx);
    let manifest_path = extensions_dir.join("installed/blocked/extension.toml");
    let original_manifest = std::fs::read(&manifest_path).expect("original manifest");
    remote.publish("blocked", "0.2.0");
    let (build_started, build_cancelled) = stall_extension_build(&store, "blocked", cx);
    let (client, release, request_started) = stall_marketplace_request(client, "later");
    let (prepared, preparation_done) = futures::channel::oneshot::channel();
    store.update(cx, |store, _| {
        store.http_client = client;
        store.auto_install_prepared_observer = Some(("later".into(), prepared));
    });
    let batch = store.update(cx, |store, cx| {
        store.install_auto_extensions(
            vec!["later".into()],
            vec![("blocked".into(), remote.source())],
            cx,
        )
    });
    let staging = build_started.await.expect("build owns source lock");
    request_started.await.expect("later download waits");
    let mut uninstall = Box::pin(store.update(cx, |store, cx| {
        store.uninstall_extension("independent".into(), cx)
    }));
    cx.run_until_parked();
    assert!(uninstall.as_mut().now_or_never().is_none());
    release.send(()).expect("release later preparation");
    preparation_done
        .await
        .expect("later preparation completed before clock advance");
    cx.executor().advance_clock(crate::EXTENSION_BUILD_TIMEOUT);
    batch.await;
    uninstall
        .await
        .expect("uninstall proceeds after build deadline");
    build_cancelled.await.expect("deadline cancels compilation");
    assert!(!staging.exists());
    assert_eq!(installed_source(&store, "blocked", cx), source);
    assert_eq!(
        std::fs::read(&manifest_path).expect("preserved manifest"),
        original_manifest
    );
    assert_eq!(
        installed_version(&store, "later", cx).as_deref(),
        Some("2.0.0")
    );
    assert!(installed_version(&store, "independent", cx).is_none());
    let failures = git_failures(&store, cx);
    assert_eq!(failures.len(), 1);
    assert!(
        failures[0].message.contains("compilation timed out"),
        "{}",
        failures[0].message
    );
    assert!(store.read_with(cx, |store, _| store.outstanding_operations().is_empty()));
    store.read_with(cx, |store, _| {
        store
            .builder
            .set_precompile_hook(|_| async { Ok(()) }.boxed());
    });
    store
        .update(cx, |store, cx| {
            store.install_git_extensions(vec![("blocked".into(), remote.source())], cx)
        })
        .await;
    assert_eq!(
        installed_version(&store, "blocked", cx).as_deref(),
        Some("0.2.0")
    );
    assert!(git_failures(&store, cx).is_empty());
}

#[gpui::test]
async fn test_cancelled_git_build_keeps_staging_until_teardown(cx: &mut TestAppContext) {
    let (_tree, _extensions_dir, store, _debounce) = git_extension_store(cx).await;
    let remote = GitExtensionRemote::new("blocked", None);
    let (started, cancelled) = stall_extension_build(&store, "blocked", cx);
    let install = store.update(cx, |store, cx| {
        store.install_git_extensions(vec![("blocked".into(), remote.source())], cx)
    });
    let staging = started.await.expect("compilation started");
    drop(install);
    cx.condition(&store, |store, _| store.outstanding_operations().is_empty())
        .await;
    assert!(staging.is_dir(), "detached build retains its checkout");
    cx.executor().advance_clock(crate::EXTENSION_BUILD_TIMEOUT);
    cancelled
        .await
        .expect("cancelled owner still reaches build deadline");
    cx.run_until_parked();
    assert!(!staging.exists());
    assert!(installed_version(&store, "blocked", cx).is_none());
}

#[gpui::test]
async fn test_local_build_timeout_preserves_git_install(cx: &mut TestAppContext) {
    let (_tree, _extensions_dir, store, _debounce) = git_extension_store(cx).await;
    let remote = GitExtensionRemote::new("blocked", None);
    store
        .update(cx, |store, cx| {
            store.install_git_extension("blocked".into(), remote.source(), cx)
        })
        .await
        .expect("initial Git install");
    let source = installed_source(&store, "blocked", cx);
    let local = local_dev_extension("blocked");
    let (started, cancelled) = stall_extension_build(&store, "blocked", cx);
    let install = store.update(cx, |store, cx| {
        store.install_dev_extension(tree_root(&local), cx)
    });
    started.await.expect("local compilation started");
    cx.executor().advance_clock(crate::EXTENSION_BUILD_TIMEOUT);
    let error = install.await.expect_err("local build deadline");
    cancelled.await.expect("local compilation cancelled");
    assert!(format!("{error:#}").contains("compilation timed out"));
    assert_eq!(installed_source(&store, "blocked", cx), source);
    assert!(tree_root(&local).join("extension.toml").exists());
    assert!(store.read_with(cx, |store, _| store.outstanding_operations().is_empty()));
}

struct GitCommandStall {
    command: &'static str,
    reached: Mutex<Option<futures::channel::oneshot::Sender<()>>>,
    cancelled: std::sync::atomic::AtomicBool,
}

static GIT_COMMAND_STALLS: std::sync::LazyLock<Mutex<HashMap<PathBuf, Arc<GitCommandStall>>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::default()));

struct GitCommandStallGuard {
    directory: PathBuf,
    stall: Arc<GitCommandStall>,
}

impl GitCommandStallGuard {
    fn new(
        directory: &Path,
        command: &'static str,
    ) -> (Self, futures::channel::oneshot::Receiver<()>) {
        let (sender, receiver) = futures::channel::oneshot::channel();
        let stall = Arc::new(GitCommandStall {
            command,
            reached: Mutex::new(Some(sender)),
            cancelled: std::sync::atomic::AtomicBool::new(false),
        });
        assert!(
            GIT_COMMAND_STALLS
                .lock()
                .insert(directory.to_path_buf(), stall.clone())
                .is_none()
        );
        (
            Self {
                directory: directory.to_path_buf(),
                stall,
            },
            receiver,
        )
    }
}

impl Drop for GitCommandStallGuard {
    fn drop(&mut self) {
        GIT_COMMAND_STALLS.lock().remove(&self.directory);
    }
}

pub(super) async fn git_command_test_hook(directory: &Path, arguments: &[&str]) {
    let stall = GIT_COMMAND_STALLS.lock().iter().find_map(|(root, stall)| {
        (directory.starts_with(root) && arguments.first() == Some(&stall.command))
            .then(|| stall.clone())
    });
    if let Some(stall) = stall {
        stall
            .reached
            .lock()
            .take()
            .expect("one stalled Git command")
            .send(())
            .expect("test awaits Git command");
        let _cancelled = util::defer(move || stall.cancelled.store(true, Ordering::SeqCst));
        futures::future::pending::<()>().await;
    }
}

#[gpui::test]
async fn test_cancelled_git_command_retains_scratch_until_worker_finishes(cx: &mut TestAppContext) {
    let scratch = Arc::new(tempfile::tempdir().expect("Git scratch directory"));
    let directory = scratch.path().to_path_buf();
    let (stall, reached) = GitCommandStallGuard::new(&directory, "fetch");
    let executor = cx.executor();
    let mut command = Box::pin(crate::run_git(
        &executor,
        &directory,
        &["fetch"],
        crate::GitCommandOwner::Scratch(scratch.clone()),
    ));
    assert!(command.as_mut().now_or_never().is_none());
    reached.await.expect("Git worker owns scratch");
    drop(scratch);
    drop(command);
    assert!(
        directory.is_dir(),
        "worker must retain cancelled caller's scratch"
    );
    cx.run_until_parked();
    assert!(stall.stall.cancelled.load(Ordering::SeqCst));
    assert!(!directory.exists(), "finished worker releases scratch");
}

#[gpui::test]
async fn test_cancelled_git_command_retains_checkout_gate_until_worker_finishes(
    cx: &mut TestAppContext,
) {
    let directory = tempfile::tempdir().expect("Git fixture directory");
    let gate = Arc::new(futures::lock::Mutex::new(()));
    let guard = Arc::new(gate.clone().lock_owned().await);
    let (stall, reached) = GitCommandStallGuard::new(directory.path(), "diff");
    let executor = cx.executor();
    let mut command = Box::pin(crate::run_git(
        &executor,
        directory.path(),
        &["diff"],
        crate::GitCommandOwner::Checkout(guard.clone()),
    ));
    assert!(command.as_mut().now_or_never().is_none());
    reached.await.expect("Git worker owns checkout gate");
    drop(guard);
    drop(command);
    assert!(
        gate.try_lock().is_none(),
        "worker must retain cancelled caller's gate"
    );
    cx.run_until_parked();
    assert!(stall.stall.cancelled.load(Ordering::SeqCst));
    assert!(
        gate.try_lock().is_some(),
        "finished worker releases checkout gate"
    );
}

#[gpui::test]
async fn test_git_network_timeout_preserves_install_and_releases_queued_uninstall(
    cx: &mut TestAppContext,
) {
    for command in ["ls-remote", "fetch"] {
        let (_tree, extensions_dir, store, _debounce) = git_extension_store(cx).await;
        let remote = GitExtensionRemote::new("blocked", None);
        std::fs::write(remote.seed.join("tracked.txt"), "tracked resource")
            .expect("tracked fixture");
        remote.publish("blocked", "0.1.1");
        store
            .update(cx, |store, cx| {
                store.install_git_extension("blocked".into(), remote.source(), cx)
            })
            .await
            .expect("initial Git install");
        let independent = local_dev_extension("independent");
        store
            .update(cx, |store, cx| {
                store.install_dev_extension(tree_root(&independent), cx)
            })
            .await
            .expect("independent local install");
        let source = installed_source(&store, "blocked", cx);
        let record_path = extensions_dir.join("sources/blocked.toml");
        let original_record = std::fs::read(&record_path).expect("original record");
        let manifest_path = extensions_dir.join("installed/blocked/extension.toml");
        let original_manifest = std::fs::read(&manifest_path).expect("original manifest");
        if command == "fetch" {
            std::fs::remove_file(extensions_dir.join("sources/blocked/tracked.txt"))
                .expect("trigger repair fetch");
        }
        let (stall, reached) = GitCommandStallGuard::new(&extensions_dir, command);
        let update = store.update(cx, |store, cx| store.check_for_git_updates(cx));
        reached.await.expect("network command owns the source lock");
        let mut uninstall = Box::pin(store.update(cx, |store, cx| {
            store.uninstall_extension("independent".into(), cx)
        }));
        cx.run_until_parked();
        assert!(
            uninstall.as_mut().now_or_never().is_none(),
            "uninstall waits behind stalled {command}"
        );
        cx.executor().advance_clock(crate::GIT_COMMAND_TIMEOUT);
        update
            .await
            .expect("update records timeout without aborting the check");
        uninstall
            .await
            .expect("uninstall proceeds after Git deadline");
        assert!(
            stall.stall.cancelled.load(Ordering::SeqCst),
            "deadline drops command future"
        );
        let failures = git_failures(&store, cx);
        let failure = failures.first().expect("Git timeout notification");
        assert_eq!(failures.len(), 1);
        assert_eq!(failure.extension_id.as_ref(), "blocked");
        assert!(failure.message.contains("timed out"), "{}", failure.message);
        assert!(failure.message.contains(command), "{}", failure.message);
        assert_eq!(
            std::fs::read(&record_path).expect("preserved record"),
            original_record
        );
        assert_eq!(
            std::fs::read(&manifest_path).expect("preserved manifest"),
            original_manifest
        );
        assert_eq!(installed_source(&store, "blocked", cx), source);
        assert!(installed_version(&store, "independent", cx).is_none());
        assert!(store.read_with(cx, |store, _| store.outstanding_operations().is_empty()));
        assert_eq!(
            std::fs::read_dir(extensions_dir.join("staging"))
                .expect("staging directory")
                .count(),
            0
        );
    }
}

#[gpui::test]
async fn test_git_install_failures_are_reported_until_resolved(cx: &mut TestAppContext) {
    let (_tree, _extensions_dir, store, _debounce) = git_extension_store(cx).await;
    let wrong = GitExtensionRemote::new("other-extension", None);

    store
        .update(cx, |store, cx| {
            store.install_git_extensions(vec![("git-extension".into(), wrong.source())], cx)
        })
        .await;
    let failures = git_failures(&store, cx);
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].extension_id.as_ref(), "git-extension");
    assert_eq!(failures[0].url, wrong.url);
    assert!(
        failures[0]
            .message
            .contains("declares extension id other-extension, expected git-extension"),
        "{}",
        failures[0].message
    );
    let delivered = store.update(cx, |store, _| store.take_git_extension_failures());
    assert_eq!(delivered, failures);
    assert!(
        store
            .update(cx, |store, _| store.take_git_extension_failures())
            .is_empty(),
        "a failure should be delivered once"
    );

    let right = GitExtensionRemote::new("git-extension", None);
    store
        .update(cx, |store, cx| {
            store.install_git_extensions(vec![("git-extension".into(), right.source())], cx)
        })
        .await;
    assert!(git_failures(&store, cx).is_empty());
}

#[gpui::test]
async fn test_git_update_preserves_rejected_source_replacement_failure(cx: &mut TestAppContext) {
    let (_tree, _extensions_dir, store, _debounce) = git_extension_store(cx).await;
    let remote = GitExtensionRemote::new("git-extension", None);
    let wrong = GitExtensionRemote::new("other-extension", None);
    store
        .update(cx, |store, cx| {
            store.install_git_extension("git-extension".into(), remote.source(), cx)
        })
        .await
        .expect("initial install should succeed");
    let original = installed_source(&store, "git-extension", cx);

    for source in [
        wrong.source(),
        crate::GitExtensionSource {
            reference: Some("missing-branch".into()),
            ..remote.source()
        },
        crate::GitExtensionSource {
            path: Some("missing-directory".into()),
            ..remote.source()
        },
    ] {
        store
            .update(cx, |store, cx| {
                store.install_git_extensions(vec![("git-extension".into(), source.clone())], cx)
            })
            .await;
        let failures = git_failures(&store, cx);
        assert_eq!(failures.len(), 1);

        let source_lock = store.read_with(cx, |store, _| store.git_source_lock.clone());
        let source_guard = source_lock.lock().await;
        let update = store.update(cx, |store, cx| store.check_for_git_updates(cx));
        cx.run_until_parked();
        cx.update(|cx| {
            cx.update_global::<SettingsStore, _>(|settings, cx| {
                settings.update_user_settings(cx, |content| {
                    content.extension.auto_install_extensions.insert(
                        "git-extension".into(),
                        settings::AutoInstallExtensionContent::Git {
                            git: source.url.clone(),
                            reference: source.reference.clone(),
                            path: source.path.clone(),
                        },
                    );
                });
            });
        });
        drop(source_guard);
        update.await.expect("update check should succeed");

        assert_eq!(installed_source(&store, "git-extension", cx), original);
        assert_eq!(git_failures(&store, cx), failures);
        assert_eq!(
            store.update(cx, |store, _| store.take_git_extension_failures()),
            failures
        );
    }
}

#[gpui::test]
async fn test_git_errors_redact_url_credentials(cx: &mut TestAppContext) {
    let (_tree, extensions_dir, store, _debounce) = git_extension_store(cx).await;
    let url = "https://kjanat:ghp_hunter2hunter2hunter2@127.0.0.1:9/zed-ldap.git";

    let directory = extensions_dir.join("work");
    git_in(&directory, &["init"]);
    git_in(&directory, &["remote", "add", "origin", url]);
    let publisher = store.read_with(cx, |store, _| store.publisher.clone());
    let owner = crate::GitCommandOwner::Checkout(Arc::new(
        publisher
            .lock_for_read()
            .await
            .expect("fixture filesystem guard"),
    ));
    let error = crate::run_git(
        &cx.executor(),
        &directory,
        &["remote", "add", "origin", url],
        owner,
    )
    .await
    .expect_err("a second origin should fail");
    let error = format!("{error:#}");
    assert!(!error.contains("hunter2"), "{error}");
    assert!(error.contains("[REDACTED]"), "{error}");

    let mut source = GitExtensionRemote::new("git-extension", None).source();
    source.url = url.to_string();
    store
        .update(cx, |store, cx| {
            store.install_git_extensions(vec![("git-extension".into(), source)], cx)
        })
        .await;
    let failures = git_failures(&store, cx);
    assert_eq!(failures.len(), 1);
    for text in [&failures[0].url, &failures[0].message] {
        assert!(!text.contains("hunter2"), "{text}");
    }
    assert!(
        failures[0].url.contains("[REDACTED]"),
        "{}",
        failures[0].url
    );
}

#[gpui::test]
async fn test_git_theme_validation_checks_declared_resources(executor: BackgroundExecutor) {
    let valid_theme = r#"{
        // Theme resources support comments and trailing commas.
        "name": "Test family", "author": "Test author",
        "themes": [{"name": "Test theme", "appearance": "dark", "style": {},}],
    }"#;
    for resource_kind in ["themes", "icon_themes"] {
        let fs = FakeFs::new(executor.clone());
        let root = Path::new(path!("/extension"));
        fs.insert_tree(
            root,
            json!({
                "extension.toml": format!(
                    "id = \"test\"\nname = \"Test\"\nversion = \"1.0.0\"\nschema_version = 1\n{resource_kind} = [\"custom/resource.json\"]\n"
                ),
                "custom": {"resource.json": valid_theme},
            }),
        )
        .await;
        let filesystem: Arc<dyn Fs> = fs.clone();
        let manifest = ExtensionManifest::load(filesystem.clone(), root)
            .await
            .expect("valid manifest");
        crate::validate_git_theme_resources(&filesystem, root, &manifest)
            .await
            .expect("valid JSONC resource");

        let resource = root.join("custom/resource.json");
        for invalid in ["{", "{}"] {
            fs.insert_file(&resource, invalid.as_bytes().to_vec()).await;
            let error = crate::validate_git_theme_resources(&filesystem, root, &manifest)
                .await
                .expect_err("invalid syntax or schema must fail validation");
            assert!(format!("{error:#}").contains("custom/resource.json"));
        }
        fs.remove_file(&resource, RemoveOptions::default())
            .await
            .expect("remove declared resource");
        let error = crate::validate_git_theme_resources(&filesystem, root, &manifest)
            .await
            .expect_err("missing declared resource must fail validation");
        assert!(format!("{error:#}").contains("custom/resource.json"));
    }
}

#[gpui::test]
async fn test_git_install_rejects_invalid_theme_resources_without_ui_proxy(
    cx: &mut TestAppContext,
) {
    for resource_directory in ["themes", "icon_themes"] {
        let (_tree, extensions_dir, store, _debounce) = git_extension_store(cx).await;
        let remote = GitExtensionRemote::new("git-extension", None);
        let resources = remote.seed.join(resource_directory);
        std::fs::create_dir(&resources).expect("resource directory");
        std::fs::write(resources.join("broken.json"), "{").expect("malformed resource");
        remote.publish("git-extension", "0.2.0");

        store
            .update(cx, |store, cx| {
                store.install_git_extensions(vec![("git-extension".into(), remote.source())], cx)
            })
            .await;

        let failures = git_failures(&store, cx);
        assert_eq!(failures.len(), 1);
        let failure = failures.first().expect("install failure");
        assert!(
            failure
                .message
                .contains(&format!("{resource_directory}/broken.json"))
        );
        assert!(installed_version(&store, "git-extension", cx).is_none());
        assert!(std::fs::symlink_metadata(extensions_dir.join("installed/git-extension")).is_err());
        assert!(!extensions_dir.join("sources/git-extension").exists());
        assert!(!extensions_dir.join("sources/git-extension.toml").exists());
    }
}

#[gpui::test]
async fn test_git_update_rejects_invalid_theme_resources(cx: &mut TestAppContext) {
    for resource_directory in ["themes", "icon_themes"] {
        let (_tree, extensions_dir, store, _debounce) = git_extension_store(cx).await;
        let proxy = store.read_with(cx, |store, _| store.proxy.clone());
        theme_extension::init(
            proxy,
            Arc::new(ThemeRegistry::new(Box::new(()))),
            cx.executor(),
        );
        let remote = GitExtensionRemote::new("git-extension", None);
        let resources = remote.seed.join(resource_directory);
        std::fs::create_dir(&resources).expect("resource directory");
        let valid_resource = json!({
            "name": "Test family", "author": "Test author",
            "themes": [{"name": "Test theme", "appearance": "dark", "style": {}}],
        })
        .to_string();
        std::fs::write(resources.join("theme.json"), &valid_resource).expect("valid resource");
        remote.publish("git-extension", "0.1.1");
        store
            .update(cx, |store, cx| {
                store.install_git_extension("git-extension".into(), remote.source(), cx)
            })
            .await
            .expect("valid extension installs");
        let original_source =
            installed_source(&store, "git-extension", cx).expect("installed git source");
        let record_path = extensions_dir.join("sources/git-extension.toml");
        let original_record = std::fs::read(&record_path).expect("installed source record");

        std::fs::write(resources.join("theme.json"), "{").expect("malformed replacement");
        remote.publish("git-extension", "0.2.0");
        store
            .update(cx, |store, cx| store.check_for_git_updates(cx))
            .await
            .expect("update check reports resource failure");

        let failures = git_failures(&store, cx);
        assert_eq!(failures.len(), 1);
        let failure = failures.first().expect("update failure");
        assert!(
            failure
                .message
                .contains(&format!("{resource_directory}/theme.json"))
        );
        assert_eq!(
            installed_source(&store, "git-extension", cx),
            Some(original_source)
        );
        assert_eq!(
            std::fs::read(&record_path).expect("source record"),
            original_record
        );
        assert_eq!(
            installed_version(&store, "git-extension", cx).as_deref(),
            Some("0.1.1")
        );
        let installed_resource = extensions_dir
            .join("installed/git-extension")
            .join(resource_directory)
            .join("theme.json");
        assert_eq!(
            std::fs::read_to_string(installed_resource).expect("original installed resource"),
            valid_resource
        );
    }
}

#[gpui::test]
async fn test_git_update_rejects_invalid_snippets(cx: &mut TestAppContext) {
    for replacement in [
        Some("{"),
        Some(r#"{"Bad": {"body": 123}}"#),
        Some(r#"{"Bad": {"body": "${1"}}"#),
        None,
    ] {
        let (_tree, extensions_dir, store, _debounce) = git_extension_store(cx).await;
        let remote = GitExtensionRemote::new("git-extension", None);
        let resources = remote.seed.join("snippets");
        std::fs::create_dir(&resources).expect("snippet directory");
        let valid_snippets = r#"{
            // Match the loader's JSONC support.
            "Hello": {"prefix": "hello", "body": ["Hello ${1:World}", "$0"],},
        }"#;
        for name in ["rust.json", "global.json"] {
            std::fs::write(resources.join(name), valid_snippets).expect("valid snippets");
        }
        let declaration = "snippets = [\"snippets/rust.json\", \"snippets/global.json\"]\n";
        remote.publish_with_manifest("git-extension", "0.1.1", declaration);
        store
            .update(cx, |store, cx| {
                store.install_git_extension("git-extension".into(), remote.source(), cx)
            })
            .await
            .expect("valid snippets install without a snippet proxy");
        let installed_snippets =
            extensions_dir.join("installed/git-extension/snippets/global.json");
        let original_snippets =
            std::fs::read(&installed_snippets).expect("installed snippet bytes");
        let original_source = installed_source(&store, "git-extension", cx);
        let record_path = extensions_dir.join("sources/git-extension.toml");
        let original_record = std::fs::read(&record_path).expect("source record");

        let broken_path = resources.join("global.json");
        match replacement {
            Some(contents) => std::fs::write(&broken_path, contents).expect("broken snippets"),
            None => std::fs::remove_file(&broken_path).expect("remove declared snippets"),
        }
        remote.publish_with_manifest("git-extension", "0.2.0", declaration);
        store
            .update(cx, |store, cx| store.check_for_git_updates(cx))
            .await
            .expect("update reports snippet failure");

        let failures = git_failures(&store, cx);
        let failure = failures.first().expect("git failure notification");
        assert_eq!(failures.len(), 1);
        assert!(failure.message.contains("global.json"), "{failure:?}");
        assert_eq!(
            installed_source(&store, "git-extension", cx),
            original_source
        );
        assert_eq!(
            std::fs::read(&record_path).expect("source record"),
            original_record
        );
        assert_eq!(
            installed_version(&store, "git-extension", cx).as_deref(),
            Some("0.1.1")
        );
        assert_eq!(
            std::fs::read(&installed_snippets).expect("original snippets"),
            original_snippets
        );
    }
}

#[gpui::test]
async fn test_git_install_rejects_missing_declared_snippets(cx: &mut TestAppContext) {
    let (_tree, extensions_dir, store, _debounce) = git_extension_store(cx).await;
    let remote = GitExtensionRemote::new("git-extension", None);
    remote.publish_with_manifest("git-extension", "0.2.0", "snippets = \"missing.json\"\n");
    store
        .update(cx, |store, cx| {
            store.install_git_extensions(vec![("git-extension".into(), remote.source())], cx)
        })
        .await;

    let failures = git_failures(&store, cx);
    assert_eq!(failures.len(), 1);
    assert!(
        failures
            .first()
            .expect("git failure")
            .message
            .contains("missing.json")
    );
    assert!(installed_version(&store, "git-extension", cx).is_none());
    assert!(!extensions_dir.join("sources/git-extension").exists());
    assert!(!extensions_dir.join("sources/git-extension.toml").exists());
}

fn git_extension_wasm_with_trapping_init() -> anyhow::Result<Vec<u8>> {
    git_extension_wasm(true)
}

#[gpui::test]
async fn test_wasm_preflight_allows_initializer_host_calls_without_running_them(
    cx: &mut TestAppContext,
) {
    let (_tree, extensions_dir, store, _debounce) = git_extension_store(cx).await;
    let fixture = local_dev_extension("wasm-extension");
    let (fs, wasm_host) =
        store.read_with(cx, |store, _| (store.fs.clone(), store.wasm_host.clone()));
    let manifest = Arc::new(
        ExtensionManifest::load(fs, &tree_root(&fixture))
            .await
            .expect("WASM fixture manifest"),
    );
    let wasm = git_extension_wasm_with_initializer(WasmFixtureInitializer::NodeBinaryPath)
        .expect("component whose initializer calls node-binary-path");
    let live_work = extensions_dir.join("work/wasm-extension");

    wasm_host
        .validate_extension(wasm.clone(), manifest.clone())
        .await
        .expect("validate before first install");
    assert!(
        !live_work.exists(),
        "preflight must not create the published work directory"
    );

    std::fs::create_dir_all(&live_work).expect("existing extension work directory");
    let sentinel = live_work.join("active-state");
    std::fs::write(&sentinel, "current extension state").expect("existing extension state");
    wasm_host
        .validate_extension(wasm.clone(), manifest.clone())
        .await
        .expect("validate upgrade alongside existing state");
    assert_eq!(
        std::fs::read_to_string(&sentinel).expect("state survives validation"),
        "current extension state"
    );

    let published = wasm_host
        .load_extension(wasm, &manifest, &cx.to_async())
        .await
        .expect("initializer can handle the unavailable Node runtime without a host trap");
    assert_eq!(
        std::fs::canonicalize(published.work_dir.as_ref()).expect("published work directory"),
        std::fs::canonicalize(&live_work).expect("live work directory")
    );
    assert_eq!(
        std::fs::read_to_string(&sentinel).expect("published extension retains existing state"),
        "current extension state"
    );
}

#[gpui::test]
async fn test_wasm_preflight_does_not_execute_constructor_or_core_start(cx: &mut TestAppContext) {
    let (_tree, extensions_dir, store, _debounce) = git_extension_store(cx).await;
    let fixture = local_dev_extension("wasm-extension");
    let (fs, wasm_host) =
        store.read_with(cx, |store, _| (store.fs.clone(), store.wasm_host.clone()));
    let manifest = Arc::new(
        ExtensionManifest::load(fs, &tree_root(&fixture))
            .await
            .expect("WASM fixture manifest"),
    );
    for (initializer, expected_error) in [
        (
            WasmFixtureInitializer::Trap,
            "failed to initialize wasm extension",
        ),
        (
            WasmFixtureInitializer::CoreStartTrap,
            "failed to instantiate wasm extension",
        ),
    ] {
        let wasm = git_extension_wasm_with_initializer(initializer).expect("trapping component");
        wasm_host
            .validate_extension(wasm.clone(), manifest.clone())
            .await
            .expect("preflight validates types without running guest code");
        let error = wasm_host
            .load_extension(wasm, &manifest, &cx.to_async())
            .await
            .err()
            .expect("published load executes the guest trap");
        assert!(format!("{error:#}").contains(expected_error), "{error:#}");
    }
    assert!(extensions_dir.join("work/wasm-extension").exists());
}

#[gpui::test]
async fn test_git_compiled_artifacts_distinguish_optional_and_required_wasm(
    cx: &mut TestAppContext,
) {
    let (_tree, _extensions_dir, store, _debounce) = git_extension_store(cx).await;
    let remote = GitExtensionRemote::new("git-extension", None);
    let (fs, wasm_host) =
        store.read_with(cx, |store, _| (store.fs.clone(), store.wasm_host.clone()));
    let publisher = store.read_with(cx, |store, _| store.publisher.clone());
    let owner = crate::GitCommandOwner::Checkout(Arc::new(
        publisher
            .lock_for_read()
            .await
            .expect("fixture filesystem guard"),
    ));
    let mut manifest = ExtensionManifest::load(fs.clone(), &remote.seed)
        .await
        .expect("extension manifest");
    crate::validate_git_compiled_artifacts(
        &cx.executor(),
        &fs,
        &remote.seed,
        &manifest,
        &wasm_host,
        owner.clone(),
    )
    .await
    .expect("metadata-only extensions need no wasm");
    let wasm_path = remote.seed.join("extension.wasm");
    let cargo_path = remote.seed.join("Cargo.toml");
    for declared_in_manifest in [true, false] {
        if declared_in_manifest {
            manifest.lib.kind = Some(crate::ExtensionLibraryKind::Rust);
        } else {
            std::fs::write(
                &cargo_path,
                "[package]\nname = \"test\"\nversion = \"0.1.0\"\n",
            )
            .expect("Rust source marker");
        }
        let error = crate::validate_git_compiled_artifacts(
            &cx.executor(),
            &fs,
            &remote.seed,
            &manifest,
            &wasm_host,
            owner.clone(),
        )
        .await
        .expect_err("a declared or source-inferred library must exist");
        assert!(format!("{error:#}").contains("extension.wasm"));
        std::fs::write(&wasm_path, b"corrupt wasm").expect("damaged compiled extension");
        let error = crate::validate_git_compiled_artifacts(
            &cx.executor(),
            &fs,
            &remote.seed,
            &manifest,
            &wasm_host,
            owner.clone(),
        )
        .await
        .expect_err("a present compiled library must be valid wasm");
        assert!(format!("{error:#}").contains("extension.wasm"));
        std::fs::write(
            &wasm_path,
            git_extension_wasm_with_trapping_init().expect("trapping component"),
        )
        .expect("structurally valid component");
        crate::validate_git_compiled_artifacts(
            &cx.executor(),
            &fs,
            &remote.seed,
            &manifest,
            &wasm_host,
            owner.clone(),
        )
        .await
        .expect("health checks compile the library without executing its trapping init");
        std::fs::remove_file(&wasm_path).expect("remove compiled fixture");
        manifest.lib.kind = None;
        if !declared_in_manifest {
            std::fs::remove_file(&cargo_path).expect("remove Rust source marker");
        }
    }
}

#[gpui::test]
async fn test_git_compiled_artifacts_validate_unused_declared_grammars(cx: &mut TestAppContext) {
    let (_tree, _extensions_dir, store, _debounce) = git_extension_store(cx).await;
    let remote = GitExtensionRemote::new("git-extension", None);
    let (fs, wasm_host) =
        store.read_with(cx, |store, _| (store.fs.clone(), store.wasm_host.clone()));
    let publisher = store.read_with(cx, |store, _| store.publisher.clone());
    let owner = crate::GitCommandOwner::Checkout(Arc::new(
        publisher
            .lock_for_read()
            .await
            .expect("fixture filesystem guard"),
    ));
    let mut manifest = ExtensionManifest::load(fs.clone(), &remote.seed)
        .await
        .expect("extension manifest");
    manifest
        .grammars
        .insert("html".into(), GrammarManifestEntry::default());
    assert!(manifest.languages.is_empty());
    let grammar_path = remote.seed.join("grammars/html.wasm");
    let error = crate::validate_git_compiled_artifacts(
        &cx.executor(),
        &fs,
        &remote.seed,
        &manifest,
        &wasm_host,
        owner.clone(),
    )
    .await
    .expect_err("declared grammars are required even without language references");
    assert!(format!("{error:#}").contains("html.wasm"));
    std::fs::create_dir_all(remote.seed.join("grammars")).expect("grammar directory");
    for bytes in [b"corrupt grammar".as_slice(), b"\0asm\x01\0\0\0".as_slice()] {
        std::fs::write(&grammar_path, bytes).expect("damaged grammar fixture");
        let error = crate::validate_git_compiled_artifacts(
            &cx.executor(),
            &fs,
            &remote.seed,
            &manifest,
            &wasm_host,
            owner.clone(),
        )
        .await
        .expect_err("grammar binary must load as the declared parser");
        assert!(format!("{error:#}").contains("html.wasm"));
    }
    std::fs::write(
        &grammar_path,
        include_bytes!("../../language/test_data/grammars/html.wasm"),
    )
    .expect("valid precompiled grammar");
    crate::validate_git_compiled_artifacts(
        &cx.executor(),
        &fs,
        &remote.seed,
        &manifest,
        &wasm_host,
        owner.clone(),
    )
    .await
    .expect("valid unused grammar passes without invoking clang");
    manifest
        .grammars
        .insert("unused".into(), GrammarManifestEntry::default());
    let error = crate::validate_git_compiled_artifacts(
        &cx.executor(),
        &fs,
        &remote.seed,
        &manifest,
        &wasm_host,
        owner.clone(),
    )
    .await
    .expect_err("every declared grammar is checked");
    assert!(format!("{error:#}").contains("unused.wasm"));
}

#[gpui::test]
async fn test_git_update_repairs_prebuilt_wasm_at_unchanged_commit_after_reindex(
    cx: &mut TestAppContext,
) {
    let valid_wasm = git_extension_wasm(false).expect("component with successful init");
    for subdirectory in [None, Some("editors/ext")] {
        for removed in [true, false] {
            let (_tree, extensions_dir, store, _debounce) = git_extension_store(cx).await;
            let remote = GitExtensionRemote::new("git-extension", subdirectory);
            let remote_extension = match subdirectory {
                Some(path) => remote.seed.join(path),
                None => remote.seed.clone(),
            };
            std::fs::write(remote_extension.join("extension.wasm"), &valid_wasm)
                .expect("tracked prebuilt component");
            remote.publish("git-extension", "0.1.1");
            store
                .update(cx, |store, cx| {
                    store.install_git_extension("git-extension".into(), remote.source(), cx)
                })
                .await
                .expect("prebuilt component installs without Rust compilation");
            let original_source = installed_source(&store, "git-extension", cx);
            let record_path = extensions_dir.join("sources/git-extension.toml");
            let original_record = std::fs::read(&record_path).expect("installed source record");
            let installed_wasm = extensions_dir.join("installed/git-extension/extension.wasm");
            if removed {
                std::fs::remove_file(&installed_wasm).expect("remove compiled component");
            } else {
                std::fs::write(&installed_wasm, b"corrupt wasm")
                    .expect("damage compiled component");
            }
            store.update(cx, |store, cx| store.reload(None, cx)).await;
            if removed {
                store.read_with(cx, |store, _| {
                    assert!(
                        store
                            .extension_index
                            .extensions
                            .get("git-extension")
                            .expect("reindexed extension")
                            .manifest
                            .lib
                            .kind
                            .is_none(),
                        "reindexing a missing optional prebuilt library loses its inferred kind"
                    );
                });
            }
            let marker = extensions_dir.join("sources/git-extension/untracked-repair-marker");
            std::fs::write(&marker, "previous checkout").expect("repair marker");
            store
                .update(cx, |store, cx| store.check_for_git_updates(cx))
                .await
                .expect("unchanged source repairs damaged artifacts");
            assert!(git_failures(&store, cx).is_empty());
            assert_eq!(
                std::fs::read(&installed_wasm).expect("repaired compiled component"),
                valid_wasm
            );
            assert_eq!(
                installed_source(&store, "git-extension", cx),
                original_source
            );
            assert_eq!(
                std::fs::read(&record_path).expect("source record"),
                original_record
            );
            assert!(!marker.exists(), "repair must replace the damaged checkout");
        }
    }
}

#[gpui::test]
async fn test_git_update_reports_unrepairable_wasm_at_unchanged_commit(cx: &mut TestAppContext) {
    let (_tree, extensions_dir, store, _debounce) = git_extension_store(cx).await;
    let remote = GitExtensionRemote::new("git-extension", None);
    std::fs::write(
        remote.seed.join("extension.wasm"),
        git_extension_wasm(false).expect("valid component"),
    )
    .expect("tracked prebuilt component");
    remote.publish("git-extension", "0.1.1");
    store
        .update(cx, |store, cx| {
            store.install_git_extension("git-extension".into(), remote.source(), cx)
        })
        .await
        .expect("prebuilt component installs");
    let original_source = installed_source(&store, "git-extension", cx);
    let record_path = extensions_dir.join("sources/git-extension.toml");
    let original_record = std::fs::read(&record_path).expect("source record");
    let installed_wasm = extensions_dir.join("installed/git-extension/extension.wasm");
    std::fs::remove_file(&installed_wasm).expect("remove compiled component");
    store.update(cx, |store, cx| store.reload(None, cx)).await;
    std::fs::rename(&remote.bare, remote.bare.with_extension("unavailable"))
        .expect("make fixture remote unavailable");
    store
        .update(cx, |store, cx| store.check_for_git_updates(cx))
        .await
        .expect("failed repair is reported");
    let failures = git_failures(&store, cx);
    assert_eq!(failures.len(), 1);
    let failure = failures.first().expect("repair failure notification");
    assert!(failure.message.contains("extension.wasm"), "{failure:?}");
    assert!(
        failure
            .message
            .contains("repairing invalid git extension artifacts"),
        "{failure:?}"
    );
    assert_eq!(
        installed_source(&store, "git-extension", cx),
        original_source
    );
    assert_eq!(
        std::fs::read(&record_path).expect("unchanged source record"),
        original_record
    );
    assert!(!installed_wasm.exists());
}

fn git_extension_wasm(init_traps: bool) -> anyhow::Result<Vec<u8>> {
    git_extension_wasm_with_initializer(if init_traps {
        WasmFixtureInitializer::Trap
    } else {
        WasmFixtureInitializer::Success
    })
}

enum WasmFixtureInitializer {
    Success,
    Trap,
    CoreStartTrap,
    NodeBinaryPath,
}

fn git_extension_wasm_with_initializer(
    initializer: WasmFixtureInitializer,
) -> anyhow::Result<Vec<u8>> {
    use wasm_encoder::{Encode, Section};

    let mut resolve = wit_parser::Resolve::default();
    let (package, _) = resolve.push_dir(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../extension_api/wit/since_v0.0.1"),
    )?;
    let world = resolve.select_world(&[package], Some("extension"))?;
    let mut module = wit_component::dummy_module(
        &resolve,
        world,
        wit_parser::ManglingAndAbi::Legacy(wit_parser::LiftLowerAbi::Sync),
    );
    if matches!(initializer, WasmFixtureInitializer::Success) {
        let mut function_index = 0;
        let mut init_function = None;
        let mut init_opcode = None;
        for payload in wasmparser::Parser::new(0).parse_all(&module) {
            match payload? {
                wasmparser::Payload::ImportSection(imports) => {
                    for import in imports.into_imports() {
                        if matches!(
                            import?.ty,
                            wasmparser::TypeRef::Func(_) | wasmparser::TypeRef::FuncExact(_)
                        ) {
                            function_index += 1;
                        }
                    }
                }
                wasmparser::Payload::ExportSection(exports) => {
                    for export in exports {
                        let export = export?;
                        if export.kind == wasmparser::ExternalKind::Func
                            && export.name == "init-extension"
                        {
                            init_function = Some(export.index);
                        }
                    }
                }
                wasmparser::Payload::CodeSectionEntry(body) => {
                    if Some(function_index) == init_function {
                        let mut operators = body.get_operators_reader()?;
                        init_opcode = Some(operators.original_position());
                        anyhow::ensure!(
                            matches!(operators.read()?, wasmparser::Operator::Unreachable),
                            "dummy init should trap"
                        );
                        anyhow::ensure!(
                            matches!(operators.read()?, wasmparser::Operator::End),
                            "dummy init should contain only the trap"
                        );
                        break;
                    }
                    function_index += 1;
                }
                _ => {}
            }
        }
        let init_opcode = init_opcode.context("dummy module should export init-extension")?;
        // Init returns nothing, so a same-sized nop makes it succeed without changing section sizes.
        *module
            .get_mut(init_opcode)
            .context("init opcode should be inside module")? = 0x01;
    }
    if matches!(
        initializer,
        WasmFixtureInitializer::NodeBinaryPath | WasmFixtureInitializer::CoreStartTrap
    ) {
        use wasm_encoder::{
            BlockType, CodeSection, Function, Instruction, MemArg, Module, RawSection, StartSection,
        };

        let mut function_index = 0;
        let mut init_function = None;
        let mut realloc_function = None;
        let mut node_function = None;
        let mut code = CodeSection::new();
        for payload in wasmparser::Parser::new(0).parse_all(&module) {
            match payload? {
                wasmparser::Payload::ImportSection(imports) => {
                    for import in imports.into_imports() {
                        let import = import?;
                        if matches!(
                            import.ty,
                            wasmparser::TypeRef::Func(_) | wasmparser::TypeRef::FuncExact(_)
                        ) {
                            if import.name == "node-binary-path" {
                                node_function = Some(function_index);
                            }
                            function_index += 1;
                        }
                    }
                }
                wasmparser::Payload::ExportSection(exports) => {
                    for export in exports {
                        let export = export?;
                        if export.kind == wasmparser::ExternalKind::Func {
                            match export.name {
                                "init-extension" => init_function = Some(export.index),
                                "cabi_realloc" => realloc_function = Some(export.index),
                                _ => {}
                            }
                        }
                    }
                }
                wasmparser::Payload::CodeSectionEntry(body) => {
                    if matches!(initializer, WasmFixtureInitializer::NodeBinaryPath)
                        && Some(function_index) == init_function
                    {
                        let mut function = Function::new([]);
                        // The dummy module has zero pages. Reserve one for the result and string.
                        function.instruction(&Instruction::I32Const(1));
                        function.instruction(&Instruction::MemoryGrow(0));
                        function.instruction(&Instruction::Drop);
                        function.instruction(&Instruction::I32Const(0));
                        function.instruction(&Instruction::Call(
                            node_function.context("dummy module imports node-binary-path")?,
                        ));
                        // An unavailable Node runtime returns a guest-visible Err, not a trap.
                        function.instruction(&Instruction::I32Const(0));
                        function.instruction(&Instruction::I32Load8U(MemArg {
                            offset: 0,
                            align: 0,
                            memory_index: 0,
                        }));
                        function.instruction(&Instruction::I32Const(1));
                        function.instruction(&Instruction::I32Ne);
                        function.instruction(&Instruction::If(BlockType::Empty));
                        function.instruction(&Instruction::Unreachable);
                        function.instruction(&Instruction::End);
                        function.instruction(&Instruction::End);
                        code.function(&function);
                    } else if matches!(initializer, WasmFixtureInitializer::NodeBinaryPath)
                        && Some(function_index) == realloc_function
                    {
                        let mut function = Function::new([]);
                        // The single result string must not overlap its return area at address zero.
                        function.instruction(&Instruction::I32Const(1024));
                        function.instruction(&Instruction::End);
                        code.function(&function);
                    } else {
                        code.raw(body.as_bytes());
                    }
                    function_index += 1;
                }
                _ => {}
            }
        }
        let mut rewritten = Module::new();
        for payload in wasmparser::Parser::new(0).parse_all(&module) {
            let payload = payload?;
            if matches!(payload, wasmparser::Payload::CodeSectionStart { .. }) {
                if matches!(initializer, WasmFixtureInitializer::CoreStartTrap) {
                    rewritten.section(&StartSection {
                        function_index: init_function
                            .context("dummy module exports init-extension")?,
                    });
                }
                rewritten.section(&code);
            } else if let Some((id, range)) = payload.as_section() {
                rewritten.section(&RawSection {
                    id,
                    data: module.get(range).context("dummy module section bounds")?,
                });
            }
        }
        module = rewritten.finish();
    }
    wit_component::embed_component_metadata(
        &mut module,
        &resolve,
        world,
        wit_component::StringEncoding::UTF8,
    )?;
    let mut bytes = wit_component::ComponentEncoder::default()
        .module(&module)?
        .validate(true)
        .encode()?;
    let version = wasm_encoder::CustomSection {
        name: "zed:api-version".into(),
        data: std::borrow::Cow::Borrowed(&[0, 0, 0, 0, 0, 1]),
    };
    bytes.push(version.id());
    version.encode(&mut bytes);
    Ok(bytes)
}

#[gpui::test]
async fn test_git_update_rejects_wasm_preflight_failures(cx: &mut TestAppContext) {
    use wasm_encoder::{Component, CustomSection};

    let mut missing_exports = Component::new();
    missing_exports.section(&CustomSection {
        name: "zed:api-version".into(),
        data: std::borrow::Cow::Borrowed(&[0, 0, 0, 0, 0, 1]),
    });
    for (wasm, expected_error) in [
        (Component::new().finish(), "no zed:api-version section"),
        (
            missing_exports.finish(),
            "failed to validate wasm extension",
        ),
    ] {
        let (_tree, extensions_dir, store, _debounce) = git_extension_store(cx).await;
        let remote = GitExtensionRemote::new("git-extension", None);
        store
            .update(cx, |store, cx| {
                store.install_git_extension("git-extension".into(), remote.source(), cx)
            })
            .await
            .expect("original extension installs");
        let original_source = installed_source(&store, "git-extension", cx);
        let record_path = extensions_dir.join("sources/git-extension.toml");
        let original_record = std::fs::read(&record_path).expect("source record");
        std::fs::write(remote.seed.join("extension.wasm"), wasm).expect("replacement component");
        remote.publish("git-extension", "0.2.0");
        store
            .update(cx, |store, cx| store.check_for_git_updates(cx))
            .await
            .expect("update reports wasm failure");

        let failures = git_failures(&store, cx);
        assert_eq!(failures.len(), 1);
        assert!(
            failures
                .first()
                .expect("git failure")
                .message
                .contains(expected_error),
            "{failures:?}"
        );
        assert_eq!(
            installed_source(&store, "git-extension", cx),
            original_source
        );
        assert_eq!(
            std::fs::read(&record_path).expect("source record"),
            original_record
        );
        assert_eq!(
            installed_version(&store, "git-extension", cx).as_deref(),
            Some("0.1.0")
        );
        assert!(
            !extensions_dir
                .join("installed/git-extension/extension.wasm")
                .exists()
        );
    }
}

#[gpui::test]
async fn test_git_update_that_cannot_be_indexed_keeps_installed_extension(cx: &mut TestAppContext) {
    let (_tree, extensions_dir, store, _debounce) = git_extension_store(cx).await;
    let remote = GitExtensionRemote::new("git-extension", None);
    store
        .update(cx, |store, cx| {
            store.install_git_extension("git-extension".into(), remote.source(), cx)
        })
        .await
        .unwrap();
    let installed_commit =
        installed_source(&store, "git-extension", cx).map(|source| source.commit);

    let language = remote.seed.join("languages").join("broken");
    std::fs::create_dir_all(&language).unwrap();
    std::fs::write(language.join("config.toml"), "name = ").unwrap();
    remote.publish("git-extension", "0.2.0");
    store
        .update(cx, |store, cx| store.check_for_git_updates(cx))
        .await
        .unwrap();

    let failures = git_failures(&store, cx);
    assert_eq!(failures.len(), 1);
    assert!(
        failures[0]
            .message
            .contains("validating language resources in languages/broken"),
        "{}",
        failures[0].message
    );
    let manifest =
        std::fs::read_to_string(extensions_dir.join("installed/git-extension/extension.toml"))
            .unwrap();
    assert!(manifest.contains("version = \"0.1.0\""), "{manifest}");
    assert!(
        !extensions_dir
            .join("sources/git-extension/languages")
            .exists()
    );
    assert_eq!(
        installed_source(&store, "git-extension", cx).map(|source| source.commit),
        installed_commit
    );
}

#[gpui::test]
async fn test_git_update_skips_an_extension_uninstalled_after_snapshot(cx: &mut TestAppContext) {
    let (_tree, _extensions_dir, store, _debounce) = git_extension_store(cx).await;
    let remote = GitExtensionRemote::new("git-extension", None);
    store
        .update(cx, |store, cx| {
            store.install_git_extension("git-extension".into(), remote.source(), cx)
        })
        .await
        .expect("install should succeed");

    let (source_lock, publisher) = store.read_with(cx, |store, _| {
        (store.git_source_lock.clone(), store.publisher.clone())
    });
    let publication_guard = publisher.lock().await;
    let uninstall = store.update(cx, |store, cx| {
        store.uninstall_extension("git-extension".into(), cx)
    });
    cx.run_until_parked();
    assert!(
        source_lock.try_lock().is_none(),
        "uninstall owns the source lock"
    );
    assert!(installed_version(&store, "git-extension", cx).is_some());
    let update = store.update(cx, |store, cx| store.check_for_git_updates(cx));
    cx.run_until_parked();
    drop(publication_guard);
    uninstall.await.expect("uninstall should succeed");
    assert!(installed_version(&store, "git-extension", cx).is_none());
    update.await.expect("update check should succeed");

    assert!(installed_version(&store, "git-extension", cx).is_none());
    assert!(git_failures(&store, cx).is_empty());
    assert!(
        store
            .update(cx, |store, _| store.take_git_extension_failures())
            .is_empty()
    );
}

#[gpui::test]
async fn test_uninstall_clears_git_extension_failures(cx: &mut TestAppContext) {
    for delivered in [false, true] {
        let (_tree, _extensions_dir, store, _debounce) = git_extension_store(cx).await;
        let remote = GitExtensionRemote::new("git-extension", None);
        store
            .update(cx, |store, cx| {
                store.install_git_extension("git-extension".into(), remote.source(), cx)
            })
            .await
            .expect("install should succeed");
        remote.publish("other-extension", "0.2.0");
        store
            .update(cx, |store, cx| store.check_for_git_updates(cx))
            .await
            .expect("update check should report the failed update");
        assert_eq!(git_failures(&store, cx).len(), 1);
        if delivered {
            assert_eq!(
                store
                    .update(cx, |store, _| store.take_git_extension_failures())
                    .len(),
                1
            );
        }

        store
            .update(cx, |store, cx| {
                store.uninstall_extension("git-extension".into(), cx)
            })
            .await
            .expect("uninstall should succeed");

        assert!(git_failures(&store, cx).is_empty());
        store.read_with(cx, |store, _| {
            assert!(store.undelivered_git_extension_failures.is_empty());
        });
        assert!(
            store
                .update(cx, |store, _| store.take_git_extension_failures())
                .is_empty()
        );
    }
}

#[gpui::test]
async fn test_git_update_reports_a_lost_source_record(cx: &mut TestAppContext) {
    for malformed in [false, true] {
        let (_tree, extensions_dir, store, _debounce) = git_extension_store(cx).await;
        let remote = GitExtensionRemote::new("git-extension", None);
        store
            .update(cx, |store, cx| {
                store.install_git_extension("git-extension".into(), remote.source(), cx)
            })
            .await
            .unwrap();
        let record_path = extensions_dir.join("sources/git-extension.toml");
        if malformed {
            std::fs::write(&record_path, "commit = ").unwrap();
        } else {
            std::fs::remove_file(&record_path).unwrap();
        }

        store
            .update(cx, |store, cx| store.check_for_git_updates(cx))
            .await
            .unwrap();

        let failures = git_failures(&store, cx);
        assert_eq!(failures.len(), 1, "malformed: {malformed}");
        assert_eq!(failures[0].url, remote.url);
        assert!(
            failures[0].message.contains("missing or unreadable"),
            "{}",
            failures[0].message
        );
        assert_eq!(installed_source(&store, "git-extension", cx), None);
        let extension_settings = crate::ExtensionSettings {
            auto_install_extensions: [(
                "git-extension".into(),
                settings::AutoInstallExtensionContent::Git {
                    git: remote.url.clone(),
                    reference: None,
                    path: None,
                },
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        };
        let to_install = store.read_with(cx, |store, _| {
            store
                .git_extensions_to_install(&extension_settings)
                .into_iter()
                .map(|(extension_id, _)| extension_id)
                .collect::<Vec<_>>()
        });
        assert_eq!(to_install, [Arc::<str>::from("git-extension")]);
    }
}

#[gpui::test]
async fn test_git_update_failures_are_reported_until_resolved(cx: &mut TestAppContext) {
    let (_tree, _extensions_dir, store, _debounce) = git_extension_store(cx).await;
    let remote = GitExtensionRemote::new("git-extension", None);
    let first_commit = git_in(&remote.seed, &["rev-parse", "HEAD"]);
    store
        .update(cx, |store, cx| {
            store.install_git_extension("git-extension".into(), remote.source(), cx)
        })
        .await
        .unwrap();

    remote.publish("other-extension", "0.2.0");
    store
        .update(cx, |store, cx| store.check_for_git_updates(cx))
        .await
        .unwrap();
    let failures = git_failures(&store, cx);
    assert_eq!(failures.len(), 1);
    assert_eq!(
        installed_source(&store, "git-extension", cx).map(|source| source.commit),
        Some(first_commit)
    );
    assert_eq!(
        store
            .update(cx, |store, _| store.take_git_extension_failures())
            .len(),
        1
    );

    remote.publish("git-extension", "0.3.0");
    store
        .update(cx, |store, cx| store.check_for_git_updates(cx))
        .await
        .unwrap();
    assert!(git_failures(&store, cx).is_empty());
    assert_eq!(
        installed_version(&store, "git-extension", cx).as_deref(),
        Some("0.3.0")
    );
}

fn local_dev_extension(extension_id: &str) -> TempTree {
    TempTree::new(json!({
        "extension.toml": format!(
            "id = \"{extension_id}\"\nname = \"Local Extension\"\nversion = \"9.0.0\"\nschema_version = 1\n"
        ),
    }))
}

#[gpui::test]
async fn test_local_extension_rejects_aliases_into_other_managed_extensions(
    cx: &mut TestAppContext,
) {
    for git_parent in [true, false] {
        let (_tree, extensions_dir, store, _debounce) =
            git_extension_store_with_http(cx, marketplace_serving("parent", "2.0.0")).await;
        let parent_remote = GitExtensionRemote::new("parent", None);
        if git_parent {
            store
                .update(cx, |store, cx| {
                    store.install_git_extension("parent".into(), parent_remote.source(), cx)
                })
                .await
                .expect("install Git parent");
        } else {
            store
                .update(cx, |store, cx| {
                    store.install_latest_extension_task("parent".into(), cx)
                })
                .await
                .expect("install marketplace parent");
        }
        let child_remote = GitExtensionRemote::new("child", None);
        store
            .update(cx, |store, cx| {
                store.install_git_extension("child".into(), child_remote.source(), cx)
            })
            .await
            .expect("install original child");
        let parent_source = installed_source(&store, "parent", cx);
        let child_source = installed_source(&store, "child", cx);
        let parent_manifest_path = extensions_dir.join("installed/parent/extension.toml");
        let parent_manifest = std::fs::read(&parent_manifest_path).expect("parent manifest");
        let child_record_path = extensions_dir.join("sources/child.toml");
        let child_record = std::fs::read(&child_record_path).expect("child source record");
        let nested = extensions_dir.join(if git_parent {
            "sources/parent/nested"
        } else {
            "installed/parent/nested"
        });
        std::fs::create_dir(&nested).expect("nested extension directory");
        std::fs::write(
            nested.join("extension.toml"),
            "id = \"child\"\nname = \"Nested\"\nversion = \"9.0.0\"\nschema_version = 1\n",
        )
        .expect("nested extension manifest");
        let aliases = TempTree::new(json!({}));
        let alias = tree_root(&aliases).join("external-alias");
        store
            .read_with(cx, |store, _| store.fs.clone())
            .create_symlink(&alias, nested.clone())
            .await
            .expect("external alias to managed directory");
        for source in [nested, alias] {
            let error = store
                .update(cx, |store, cx| store.install_dev_extension(source, cx))
                .await
                .expect_err("local source must not depend on another managed extension lifetime");
            assert!(
                format!("{error:#}").contains("another managed extension"),
                "{error:#}"
            );
        }
        assert_eq!(
            std::fs::read(&parent_manifest_path).expect("preserved parent"),
            parent_manifest
        );
        assert_eq!(
            std::fs::read(&child_record_path).expect("preserved child record"),
            child_record
        );
        assert_eq!(installed_source(&store, "parent", cx), parent_source);
        assert_eq!(installed_source(&store, "child", cx), child_source);
        assert_eq!(
            installed_version(&store, "child", cx).as_deref(),
            Some("0.1.0")
        );
        assert!(
            extensions_dir
                .join("installed/child/extension.toml")
                .is_file()
        );
    }
}

#[gpui::test]
async fn test_local_dev_extension_replaces_git_source(cx: &mut TestAppContext) {
    let (_tree, extensions_dir, store, _debounce) = git_extension_store(cx).await;
    let remote = GitExtensionRemote::new("git-extension", None);
    store
        .update(cx, |store, cx| {
            store.install_git_extension("git-extension".into(), remote.source(), cx)
        })
        .await
        .unwrap();

    remote.publish("other-extension", "0.2.0");
    store
        .update(cx, |store, cx| store.check_for_git_updates(cx))
        .await
        .expect("update check should report the failed update");
    assert_eq!(git_failures(&store, cx).len(), 1);

    let local = local_dev_extension("git-extension");
    store
        .update(cx, |store, cx| {
            store.install_dev_extension(tree_root(&local), cx)
        })
        .await
        .unwrap();
    assert!(!extensions_dir.join("sources/git-extension").exists());
    assert!(!extensions_dir.join("sources/git-extension.toml").exists());
    assert_eq!(installed_source(&store, "git-extension", cx), None);
    assert!(git_failures(&store, cx).is_empty());
    store.read_with(cx, |store, _| {
        assert!(store.undelivered_git_extension_failures.is_empty());
    });

    remote.publish("git-extension", "0.2.0");
    store
        .update(cx, |store, cx| store.check_for_git_updates(cx))
        .await
        .unwrap();
    assert_eq!(
        installed_version(&store, "git-extension", cx).as_deref(),
        Some("9.0.0")
    );
    assert_eq!(
        std::fs::canonicalize(extensions_dir.join("installed/git-extension")).unwrap(),
        std::fs::canonicalize(local.path()).unwrap()
    );
}

#[gpui::test]
async fn test_local_dev_staging_failure_keeps_git_install(cx: &mut TestAppContext) {
    let (_tree, extensions_dir, store, _debounce) = git_extension_store(cx).await;
    let remote = GitExtensionRemote::new("git-extension", None);
    store
        .update(cx, |store, cx| {
            store.install_git_extension("git-extension".into(), remote.source(), cx)
        })
        .await
        .expect("initial git install");
    let local = local_dev_extension("git-extension");
    let installed = extensions_dir.join("installed/git-extension");
    let record_path = extensions_dir.join("sources/git-extension.toml");
    let old_target = std::fs::read_link(&installed).expect("existing installed link");
    let old_record = std::fs::read(&record_path).expect("existing source record");
    let old_manifest = std::fs::read(installed.join("extension.toml")).expect("existing manifest");
    let staging_dir = extensions_dir.join("staging");
    std::fs::remove_dir(&staging_dir).expect("staging is empty after initial install");
    std::fs::write(&staging_dir, "occupied").expect("block staging directory creation");
    store
        .update(cx, |store, cx| {
            store.install_dev_extension(tree_root(&local), cx)
        })
        .await
        .expect_err("local link staging must fail before replacing the installed extension");
    assert_eq!(
        std::fs::read_link(&installed).expect("old link remains"),
        old_target
    );
    assert_eq!(
        std::fs::read(&record_path).expect("old record remains"),
        old_record
    );
    assert_eq!(
        std::fs::read(installed.join("extension.toml")).expect("old checkout remains"),
        old_manifest
    );
    assert_eq!(
        installed_version(&store, "git-extension", cx).as_deref(),
        Some("0.1.0")
    );
    assert!(installed_source(&store, "git-extension", cx).is_some());
    assert!(local.path().join("extension.toml").is_file());
    std::fs::remove_file(&staging_dir).expect("remove staging obstruction");
    std::fs::create_dir(&staging_dir).expect("restore staging directory");
}

#[gpui::test]
async fn test_local_dev_alias_preserves_managed_git_checkout(cx: &mut TestAppContext) {
    let (_tree, extensions_dir, store, _debounce) = git_extension_store(cx).await;
    let remote = GitExtensionRemote::new("git-extension", None);
    store
        .update(cx, |store, cx| {
            store.install_git_extension("git-extension".into(), remote.source(), cx)
        })
        .await
        .expect("initial git install");
    let original_source = installed_source(&store, "git-extension", cx);
    let record_path = extensions_dir.join("sources/git-extension.toml");
    let old_record = std::fs::read(&record_path).expect("existing source record");
    let alias_tree = TempTree::new(json!({}));
    let alias = tree_root(&alias_tree).join("source");
    let checkout = extensions_dir.join("sources/git-extension");
    let fs = store.read_with(cx, |store, _| store.fs.clone());
    fs.create_symlink(&alias, checkout.clone())
        .await
        .expect("external alias to managed source");
    store
        .update(cx, |store, cx| {
            store.install_dev_extension(alias.clone(), cx)
        })
        .await
        .expect("relinking a managed source through an alias preserves its checkout");
    assert!(checkout.join("extension.toml").is_file());
    assert_eq!(
        std::fs::read(&record_path).expect("source record remains"),
        old_record
    );
    assert_eq!(
        installed_source(&store, "git-extension", cx),
        original_source
    );
    assert_eq!(
        std::fs::canonicalize(extensions_dir.join("installed/git-extension"))
            .expect("installed link"),
        std::fs::canonicalize(&checkout).expect("managed checkout")
    );
    assert!(!extensions_dir.join("staging/git-source.previous").exists());
}

#[gpui::test]
async fn test_index_ignores_a_git_record_for_another_checkout(cx: &mut TestAppContext) {
    let (_tree, extensions_dir, store, _debounce) = git_extension_store(cx).await;
    let local = local_dev_extension("local-extension");
    store
        .update(cx, |store, cx| {
            store.install_dev_extension(tree_root(&local), cx)
        })
        .await
        .unwrap();

    std::fs::create_dir_all(extensions_dir.join("sources/local-extension")).unwrap();
    std::fs::write(
        extensions_dir.join("sources/local-extension.toml"),
        "url = \"https://example.com/stale\"\ncommit = \"0000000\"\n",
    )
    .unwrap();
    store.update(cx, |store, cx| store.reload(None, cx)).await;

    let entry = store.read_with(cx, |store, _| {
        store.extension_index.extensions["local-extension"].clone()
    });
    assert!(entry.dev);
    assert_eq!(entry.source, None);
}

#[gpui::test]
async fn test_git_update_check_follows_the_ref_git_fetches(cx: &mut TestAppContext) {
    let (_tree, extensions_dir, store, _debounce) = git_extension_store(cx).await;
    let remote = GitExtensionRemote::new("git-extension", None);
    let tagged = git_in(&remote.seed, &["rev-parse", "HEAD"]);
    remote.push_ref("HEAD", "refs/tags/release");
    remote.publish("git-extension", "0.2.0");
    remote.push_ref("HEAD", "refs/heads/release");

    let mut source = remote.source();
    source.reference = Some("release".into());
    store
        .update(cx, |store, cx| {
            store.install_git_extension("git-extension".into(), source, cx)
        })
        .await
        .unwrap();
    assert_eq!(
        installed_source(&store, "git-extension", cx).map(|source| source.commit),
        Some(tagged)
    );
    let marker = extensions_dir.join("sources/git-extension/marker");
    std::fs::write(&marker, "").unwrap();

    store
        .update(cx, |store, cx| store.check_for_git_updates(cx))
        .await
        .unwrap();

    assert!(
        marker.exists(),
        "a ref that resolves to the installed tag should not be fetched again"
    );
}

#[gpui::test]
async fn test_git_update_reports_a_deleted_ref(cx: &mut TestAppContext) {
    let (_tree, extensions_dir, store, _debounce) = git_extension_store(cx).await;
    let remote = GitExtensionRemote::new("git-extension", None);
    remote.push_ref("HEAD", "refs/heads/release");
    let mut source = remote.source();
    source.reference = Some("release".into());
    store
        .update(cx, |store, cx| {
            store.install_git_extension("git-extension".into(), source, cx)
        })
        .await
        .unwrap();

    git_in(
        &remote.seed,
        &[
            "push",
            &remote.bare.to_string_lossy(),
            "--delete",
            "release",
        ],
    );
    store
        .update(cx, |store, cx| store.check_for_git_updates(cx))
        .await
        .unwrap();

    let failures = git_failures(&store, cx);
    assert_eq!(failures.len(), 1);
    assert!(
        failures[0]
            .message
            .contains("the repository no longer advertises release"),
        "{}",
        failures[0].message
    );
    let manifest =
        std::fs::read_to_string(extensions_dir.join("installed/git-extension/extension.toml"))
            .unwrap();
    assert!(manifest.contains("version = \"0.1.0\""), "{manifest}");
}

#[gpui::test]
async fn test_git_update_skips_suppressed_durable_sources(cx: &mut TestAppContext) {
    let (_tree, extensions_dir, store, _debounce) = git_extension_store(cx).await;
    let remote = GitExtensionRemote::new("git-extension", None);
    store
        .update(cx, |store, cx| {
            store.install_git_extension("git-extension".into(), remote.source(), cx)
        })
        .await
        .expect("initial Git install");
    let source = installed_source(&store, "git-extension", cx).expect("installed source");
    assert!(crate::SUPPRESSED_EXTENSIONS.contains("ruff"));
    let suppressed_record = extensions_dir.join("sources/ruff.toml");
    let suppressed_source = crate::InstalledGitSource {
        url: extensions_dir
            .join("missing-remote.git")
            .to_string_lossy()
            .into_owned(),
        ..source.clone()
    };
    let suppressed_contents =
        toml::to_string(&suppressed_source).expect("suppressed source record");
    std::fs::write(&suppressed_record, &suppressed_contents)
        .expect("write durable suppressed source");
    let builds = Arc::new(Mutex::new(Vec::new()));
    let fs = store.read_with(cx, |store, _| {
        store.builder.set_precompile_hook({
            let builds = builds.clone();
            move |directory| {
                let directory = directory.to_path_buf();
                let builds = builds.clone();
                async move {
                    let manifest: toml::Value = toml::from_str(&std::fs::read_to_string(
                        directory.join("extension.toml"),
                    )?)?;
                    let id = manifest
                        .get("id")
                        .and_then(toml::Value::as_str)
                        .expect("build fixture extension id");
                    builds.lock().push(id.to_owned());
                    Ok(())
                }
                .boxed()
            }
        });
        store.fs.clone()
    });
    fs.remove_file(
        &extensions_dir.join("installed/git-extension"),
        RemoveOptions::default(),
    )
    .await
    .expect("remove normal extension link before repair");

    store
        .update(cx, |store, cx| store.check_for_git_updates(cx))
        .await
        .expect("repair normal source and skip suppressed source");

    assert!(git_failures(&store, cx).is_empty());
    assert_eq!(*builds.lock(), vec!["git-extension".to_owned()]);
    assert_eq!(installed_source(&store, "git-extension", cx), Some(source));
    assert!(
        extensions_dir
            .join("installed/git-extension/extension.toml")
            .is_file()
    );
    assert!(installed_version(&store, "ruff", cx).is_none());
    assert!(!extensions_dir.join("installed/ruff").exists());
    assert!(!extensions_dir.join("sources/ruff").exists());
    assert_eq!(
        std::fs::read_to_string(&suppressed_record).expect("untouched suppressed record"),
        suppressed_contents
    );
}

#[gpui::test]
async fn test_git_update_repairs_valid_tracked_theme_edits(cx: &mut TestAppContext) {
    check_git_checkout_content_repair(cx, GitCheckoutEdit::Theme).await;
}

#[gpui::test]
async fn test_git_update_repairs_staged_only_theme_edits(cx: &mut TestAppContext) {
    check_git_checkout_content_repair(cx, GitCheckoutEdit::Index).await;
}

#[gpui::test]
async fn test_pinned_git_update_repairs_changed_head(cx: &mut TestAppContext) {
    check_git_checkout_content_repair(cx, GitCheckoutEdit::Head).await;
}

#[gpui::test]
async fn test_git_update_repairs_valid_tracked_wasm_edits(cx: &mut TestAppContext) {
    check_git_checkout_content_repair(cx, GitCheckoutEdit::Wasm).await;
}

#[gpui::test]
async fn test_git_update_removes_unvalidated_discovered_theme(cx: &mut TestAppContext) {
    check_git_checkout_content_repair(cx, GitCheckoutEdit::AddedTheme).await;
}

enum GitCheckoutEdit {
    Theme,
    Index,
    Head,
    Wasm,
    AddedTheme,
}

const CHECKOUT_THEME: &str = r#"{"name":"Original family","author":"Test","themes":[{"name":"Original theme","appearance":"dark","style":{}}]}"#;

fn wasm_with_test_marker(mut wasm: Vec<u8>) -> Vec<u8> {
    use wasm_encoder::{Encode, Section};
    let marker = wasm_encoder::CustomSection {
        name: "test-marker".into(),
        data: std::borrow::Cow::Borrowed(b"different valid component"),
    };
    wasm.push(marker.id());
    marker.encode(&mut wasm);
    wasm
}

async fn check_git_checkout_content_repair(cx: &mut TestAppContext, edit: GitCheckoutEdit) {
    let (_tree, extensions_dir, store, _debounce) = git_extension_store(cx).await;
    let remote = GitExtensionRemote::new("git-extension", None);
    std::fs::create_dir_all(remote.seed.join("themes")).expect("theme directory");
    std::fs::write(remote.seed.join("themes/theme.json"), CHECKOUT_THEME).expect("tracked theme");
    let wasm = git_extension_wasm(false).expect("valid component");
    std::fs::write(remote.seed.join("extension.wasm"), &wasm).expect("tracked component");
    remote.publish("git-extension", "0.1.1");
    store
        .update(cx, |store, cx| {
            store.install_git_extension("git-extension".into(), remote.source(), cx)
        })
        .await
        .expect("initial Git install");
    let source = installed_source(&store, "git-extension", cx).expect("installed source");
    let checkout = extensions_dir.join("sources/git-extension");
    let theme = checkout.join("themes/theme.json");
    let marker = checkout.join("untracked-repair-marker");
    std::fs::write(&marker, "old checkout").expect("repair marker");
    match edit {
        GitCheckoutEdit::Theme => {
            std::fs::write(&theme, CHECKOUT_THEME.replace("Original", "Modified"))
                .expect("valid changed theme");
        }
        GitCheckoutEdit::Index => {
            std::fs::write(&theme, CHECKOUT_THEME.replace("Original", "Modified"))
                .expect("staged theme");
            git_in(&checkout, &["add", "--", "themes/theme.json"]);
            std::fs::write(&theme, CHECKOUT_THEME)
                .expect("restore working file without changing index");
        }
        GitCheckoutEdit::Head => {
            git_in(
                &checkout,
                &["commit", "--allow-empty", "-m", "local HEAD drift"],
            );
            cx.update(|cx| {
                cx.update_global::<SettingsStore, _>(|settings, cx| {
                    settings.update_user_settings(cx, |content| {
                        content
                            .extension
                            .auto_update_extensions
                            .insert("git-extension".into(), false);
                    });
                });
            });
            remote.publish("git-extension", "0.2.0");
        }
        GitCheckoutEdit::Wasm => {
            std::fs::write(
                checkout.join("extension.wasm"),
                wasm_with_test_marker(wasm.clone()),
            )
            .expect("different valid component");
        }
        GitCheckoutEdit::AddedTheme => {
            std::fs::write(
                checkout.join("themes/added.json"),
                CHECKOUT_THEME.replace("Original", "Added"),
            )
            .expect("new inferred theme");
        }
    }
    store
        .update(cx, |store, cx| store.check_for_git_updates(cx))
        .await
        .expect("repair content drift");
    assert!(git_failures(&store, cx).is_empty());
    assert!(!marker.exists(), "content drift must replace checkout");
    assert_eq!(
        installed_source(&store, "git-extension", cx),
        Some(source.clone())
    );
    assert_eq!(git_in(&checkout, &["rev-parse", "HEAD"]), source.commit);
    assert_eq!(
        std::fs::read_to_string(&theme).expect("repaired theme"),
        CHECKOUT_THEME
    );
    assert_eq!(
        std::fs::read(checkout.join("extension.wasm")).expect("repaired component"),
        wasm
    );
    assert!(!checkout.join("themes/added.json").exists());
}

#[gpui::test]
async fn test_git_checkout_baseline_preserves_validated_build_outputs(cx: &mut TestAppContext) {
    let (_tree, extensions_dir, store, _debounce) = git_extension_store(cx).await;
    let remote = GitExtensionRemote::new("git-extension", None);
    std::fs::create_dir_all(remote.seed.join("themes")).expect("theme directory");
    std::fs::write(remote.seed.join("themes/tracked.json"), CHECKOUT_THEME)
        .expect("tracked source theme");
    std::fs::write(
        remote.seed.join(".gitignore"),
        "extension.wasm\nsnippets.json\nlanguages/\nthemes/generated.json\ntarget/\n",
    )
    .expect("ignored build outputs");
    remote.publish("git-extension", "0.1.1");
    let wasm = git_extension_wasm(false).expect("generated component");
    let generated_theme = CHECKOUT_THEME.replace("Original", "Generated");
    const SNIPPETS: &str = r#"{"Hello":{"prefix":"hello","body":"Hello"}}"#;
    store.read_with(cx, |store, _| {
        store.builder.set_precompile_hook({
            let wasm = wasm.clone();
            let generated_theme = generated_theme.clone();
            move |directory| {
                let directory = directory.to_path_buf();
                let wasm = wasm.clone();
                let generated_theme = generated_theme.clone();
                async move {
                    std::fs::write(directory.join("extension.wasm"), wasm)?;
                    std::fs::write(directory.join("themes/tracked.json"), &generated_theme)?;
                    std::fs::write(directory.join("themes/generated.json"), &generated_theme)?;
                    std::fs::write(directory.join("snippets.json"), SNIPPETS)?;
                    std::fs::create_dir_all(directory.join("languages/plain"))?;
                    std::fs::write(
                        directory.join("languages/plain/config.toml"),
                        "name = \"Generated Plain\"\n",
                    )?;
                    std::fs::create_dir_all(directory.join("target"))?;
                    std::fs::write(
                        directory.join("target/build-cache"),
                        "not an extension resource",
                    )?;
                    Ok(())
                }
                .boxed()
            }
        });
    });
    store
        .update(cx, |store, cx| {
            store.install_git_extension("git-extension".into(), remote.source(), cx)
        })
        .await
        .expect("install validated build outputs");
    let source = installed_source(&store, "git-extension", cx).expect("postbuild source record");
    let checkout = extensions_dir.join("sources/git-extension");
    let source_tree = git_in(&checkout, &["rev-parse", "HEAD^{tree}"]);
    assert_ne!(source.checkout_tree.as_deref(), Some(source_tree.as_str()));
    git_in(
        &checkout,
        &[
            "ls-files",
            "--error-unmatch",
            "--",
            "extension.wasm",
            "themes/generated.json",
            "snippets.json",
            "languages/plain/config.toml",
        ],
    );
    assert!(git_in(&checkout, &["ls-files", "--", "target"]).is_empty());
    let marker = checkout.join("unchanged-marker");
    std::fs::write(&marker, "unchanged validated checkout").expect("checkout marker");
    store
        .update(cx, |store, cx| store.check_for_git_updates(cx))
        .await
        .expect("accept postbuild baseline");
    assert!(git_failures(&store, cx).is_empty());
    assert!(
        marker.exists(),
        "legitimate generated outputs must not trigger repairs"
    );
    std::fs::write(
        checkout.join("snippets.json"),
        SNIPPETS.replace("Hello", "Modified"),
    )
    .expect("valid changed generated resource");
    store
        .update(cx, |store, cx| store.check_for_git_updates(cx))
        .await
        .expect("repair generated resource edit");
    assert!(git_failures(&store, cx).is_empty());
    assert!(!marker.exists());
    assert_eq!(
        std::fs::read_to_string(checkout.join("snippets.json"))
            .expect("repaired generated resource"),
        SNIPPETS
    );
    assert_eq!(installed_source(&store, "git-extension", cx), Some(source));
}

#[gpui::test]
async fn test_git_checkout_without_content_baseline_is_rebuilt(cx: &mut TestAppContext) {
    let (_tree, extensions_dir, store, _debounce) = git_extension_store(cx).await;
    let remote = GitExtensionRemote::new("git-extension", None);
    store
        .update(cx, |store, cx| {
            store.install_git_extension("git-extension".into(), remote.source(), cx)
        })
        .await
        .expect("initial Git install");
    let mut source = installed_source(&store, "git-extension", cx).expect("installed source");
    source.checkout_tree = None;
    let record = extensions_dir.join("sources/git-extension.toml");
    std::fs::write(
        &record,
        toml::to_string(&source).expect("legacy source record"),
    )
    .expect("remove old baseline");
    let marker = extensions_dir.join("sources/git-extension/legacy-marker");
    std::fs::write(&marker, "old checkout").expect("legacy marker");
    store
        .update(cx, |store, cx| store.check_for_git_updates(cx))
        .await
        .expect("rebuild legacy checkout");
    assert!(git_failures(&store, cx).is_empty());
    assert!(!marker.exists());
    let repaired = installed_source(&store, "git-extension", cx).expect("repaired source");
    assert_eq!(repaired.commit, source.commit);
    assert!(repaired.checkout_tree.is_some());
}

#[gpui::test]
async fn test_git_update_rebuilds_a_missing_checkout(cx: &mut TestAppContext) {
    let (_tree, extensions_dir, store, _debounce) = git_extension_store(cx).await;
    let remote = GitExtensionRemote::new("git-extension", None);
    store
        .update(cx, |store, cx| {
            store.install_git_extension("git-extension".into(), remote.source(), cx)
        })
        .await
        .unwrap();
    let installed_commit =
        installed_source(&store, "git-extension", cx).map(|source| source.commit);

    std::fs::remove_dir_all(extensions_dir.join("sources/git-extension")).unwrap();
    store
        .update(cx, |store, cx| store.check_for_git_updates(cx))
        .await
        .unwrap();

    assert!(git_failures(&store, cx).is_empty());
    let manifest =
        std::fs::read_to_string(extensions_dir.join("installed/git-extension/extension.toml"))
            .unwrap();
    assert!(manifest.contains("version = \"0.1.0\""), "{manifest}");
    assert_eq!(
        installed_source(&store, "git-extension", cx).map(|source| source.commit),
        installed_commit
    );
}

#[gpui::test]
async fn test_git_queries_are_validated_with_the_staged_grammar(executor: BackgroundExecutor) {
    let fs = FakeFs::new(executor.clone());
    let root = Path::new(path!("/extension"));
    fs.insert_tree(
        root,
        json!({
            "extension.toml": "id = \"test\"\nname = \"Test\"\nversion = \"1.0.0\"\nschema_version = 1\nlanguages = [\"languages/html\"]\n[grammars.html]\nrepository = \"unused\"\nrev = \"unused\"\n",
            "languages": {"html": {
                "config.toml": "name = \"Test HTML\"\ngrammar = \"html\"\n",
                "highlights.scm": "(tag_name) @tag",
            }},
            "grammars": {},
        }),
    )
    .await;
    let grammar_path = root.join("grammars/html.wasm");
    fs.insert_file(
        &grammar_path,
        include_bytes!("../../language/test_data/grammars/html.wasm").to_vec(),
    )
    .await;
    let filesystem: Arc<dyn Fs> = fs.clone();
    let manifest = ExtensionManifest::load(filesystem.clone(), root)
        .await
        .expect("valid manifest");
    let proxy = Arc::new(ExtensionHostProxy::new());
    crate::validate_git_language_resources(&filesystem, root, &manifest, &proxy)
        .await
        .expect("valid queries need no live grammar registration");
    fs.remove_file(
        &root.join("languages/html/highlights.scm"),
        RemoveOptions::default(),
    )
    .await
    .expect("remove valid query");

    for query_file in QueryFiles::all().query_files() {
        let query_path = root.join("languages/html").join(query_file.file_name());
        fs.insert_file(&query_path, b"(".to_vec()).await;
        let error = crate::validate_git_language_resources(&filesystem, root, &manifest, &proxy)
            .await
            .expect_err("every supported query kind must be parsed");
        assert!(format!("{error:#}").contains("languages/html"));
        fs.remove_file(&query_path, RemoveOptions::default())
            .await
            .expect("remove malformed query");
    }
    let live_grammar = TempTree::new(json!({"grammars": {}}));
    let live_grammar_path = live_grammar.path().join("grammars/html.wasm");
    std::fs::write(
        &live_grammar_path,
        include_bytes!("../../language/test_data/grammars/html-jinja.wasm"),
    )
    .expect("live grammar fixture");
    let registry = Arc::new(LanguageRegistry::test(executor));
    registry.register_wasm_grammars("test".into(), vec![("html".into(), live_grammar_path)]);
    registry
        .validate_extension_language_queries(
            "consumer".into(),
            LanguageConfig {
                name: "HTML-Jinja".into(),
                grammar: Some("html".into()),
                ..Default::default()
            },
            language::LanguageQueries {
                highlights: Some("(jinja) @tag".into()),
                ..Default::default()
            },
        )
        .await
        .expect("live grammar accepts Jinja nodes");
    language_extension::init(LspAccess::Noop, proxy.clone(), registry);
    fs.insert_file(
        &root.join("languages/html/highlights.scm"),
        b"(jinja) @tag".to_vec(),
    )
    .await;
    crate::validate_git_language_resources(&filesystem, root, &manifest, &proxy)
        .await
        .expect_err("staged HTML must reject nodes accepted by the installed HTML-Jinja grammar");
    fs.insert_file(&grammar_path, b"invalid wasm".to_vec())
        .await;
    crate::validate_git_language_resources(&filesystem, root, &manifest, &proxy)
        .await
        .expect_err("invalid grammar must fail before swapping");
}

#[gpui::test]
async fn test_git_update_rejects_invalid_language_queries(cx: &mut TestAppContext) {
    let (_tree, extensions_dir, store, _debounce) = git_extension_store(cx).await;
    let registry = Arc::new(LanguageRegistry::test(cx.executor()));
    registry.register_wasm_grammars(
        "shared-html".into(),
        vec![(
            "html".into(),
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../language/test_data/grammars/html.wasm"),
        )],
    );
    language_extension::init(
        LspAccess::Noop,
        store.read_with(cx, |store, _| store.proxy.clone()),
        registry,
    );
    let remote = GitExtensionRemote::new("git-extension", None);
    let language_dir = remote.seed.join("languages/html");
    std::fs::create_dir_all(&language_dir).expect("language directory");
    std::fs::write(
        language_dir.join("config.toml"),
        "name = \"Test HTML\"\ngrammar = \"html\"\n",
    )
    .expect("language config");
    let query_path = language_dir.join("highlights.scm");
    let valid_query = "(tag_name) @tag";
    std::fs::write(&query_path, valid_query).expect("valid query");
    remote.publish("git-extension", "0.1.1");
    store
        .update(cx, |store, cx| {
            store.install_git_extension("git-extension".into(), remote.source(), cx)
        })
        .await
        .expect("valid language installs");
    let original_source = installed_source(&store, "git-extension", cx);
    let record_path = extensions_dir.join("sources/git-extension.toml");
    let original_record = std::fs::read(&record_path).expect("installed source record");
    std::fs::write(&query_path, "(").expect("malformed query");
    remote.publish("git-extension", "0.2.0");
    store
        .update(cx, |store, cx| store.check_for_git_updates(cx))
        .await
        .expect("failed update is reported");
    let failures = git_failures(&store, cx);
    assert_eq!(failures.len(), 1);
    assert!(
        failures
            .first()
            .expect("failure")
            .message
            .contains("highlights")
    );
    assert_eq!(
        installed_source(&store, "git-extension", cx),
        original_source
    );
    assert_eq!(
        std::fs::read(&record_path).expect("source record"),
        original_record
    );
    assert_eq!(
        std::fs::read_to_string(
            extensions_dir.join("installed/git-extension/languages/html/highlights.scm")
        )
        .expect("installed query"),
        valid_query,
    );
    std::fs::write(&query_path, valid_query).expect("repaired query");
    remote.publish("git-extension", "0.3.0");
    store
        .update(cx, |store, cx| store.check_for_git_updates(cx))
        .await
        .expect("repaired update succeeds");
    assert!(git_failures(&store, cx).is_empty());
    assert_eq!(
        installed_version(&store, "git-extension", cx).as_deref(),
        Some("0.3.0")
    );
}

const VALID_GIT_LANGUAGE_METADATA: [(&str, &str); 2] = [
    (
        settings::SemanticTokenRules::FILE_NAME,
        r#"[
            // Language metadata accepts JSONC.
            {"token_type": "function", "style": ["function",],},
        ]"#,
    ),
    (
        task::TaskTemplates::FILE_NAME,
        r#"[
            // Task metadata uses the same lenient parser as the runtime loader.
            {"label": "Print", "command": "echo", "args": ["hello",],},
        ]"#,
    ),
];

#[gpui::test]
async fn test_git_update_repairs_resources_at_an_unchanged_commit(cx: &mut TestAppContext) {
    let (_tree, extensions_dir, store, _debounce) = git_extension_store(cx).await;
    let registry = Arc::new(LanguageRegistry::test(cx.executor()));
    registry.register_wasm_grammars(
        "shared-html".into(),
        vec![(
            "html".into(),
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../language/test_data/grammars/html.wasm"),
        )],
    );
    language_extension::init(
        LspAccess::Noop,
        store.read_with(cx, |store, _| store.proxy.clone()),
        registry,
    );
    let remote = GitExtensionRemote::new("git-extension", None);
    for (path, contents) in [
        (
            "custom/theme.json",
            r#"{"name":"Test family","author":"Test author","themes":[{"name":"Test theme","appearance":"dark","style":{}}]}"#,
        ),
        (
            "themes/inferred.json",
            r#"{"name":"Inferred family","author":"Test author","themes":[{"name":"Inferred theme","appearance":"dark","style":{}}]}"#,
        ),
        (
            "custom/snippets.json",
            r#"{"Hello":{"prefix":"hello","body":"Hello ${1:World}"}}"#,
        ),
        (
            "languages/html/config.toml",
            "name = \"Test HTML\"\ngrammar = \"html\"\n",
        ),
        ("languages/html/highlights.scm", "(tag_name) @tag"),
        ("languages/plain/config.toml", "name = \"Test Plain\"\n"),
    ] {
        let path = remote.seed.join(path);
        std::fs::create_dir_all(path.parent().expect("resource parent"))
            .expect("resource directory");
        std::fs::write(path, contents).expect("valid resource fixture");
    }
    for (file_name, contents) in VALID_GIT_LANGUAGE_METADATA {
        std::fs::write(
            remote.seed.join("languages/plain").join(file_name),
            contents,
        )
        .expect("language metadata fixture");
    }
    remote.publish_with_manifest(
        "git-extension",
        "0.1.1",
        "themes = [\"custom/theme.json\"]\nsnippets = [\"custom/snippets.json\"]\n",
    );
    store
        .update(cx, |store, cx| {
            store.install_git_extension("git-extension".into(), remote.source(), cx)
        })
        .await
        .expect("valid resource extension installs");
    let original_source = installed_source(&store, "git-extension", cx);
    let record_path = extensions_dir.join("sources/git-extension.toml");
    let original_record = std::fs::read(&record_path).expect("source record");
    for (resource, damaged_contents) in [
        ("custom/theme.json", None),
        ("custom/theme.json", Some("{")),
        ("themes/inferred.json", Some("{")),
        ("custom/snippets.json", None),
        ("custom/snippets.json", Some("{")),
        ("languages/html/highlights.scm", Some("(")),
        ("languages/plain/tasks.json", None),
        ("languages/plain/tasks.json", Some("[")),
        ("languages/plain/semantic_token_rules.json", Some("[")),
    ] {
        let installed_resource = extensions_dir
            .join("installed/git-extension")
            .join(resource);
        let original_contents =
            std::fs::read(&installed_resource).expect("healthy cached resource");
        match damaged_contents {
            Some(contents) => {
                std::fs::write(&installed_resource, contents).expect("damage cached resource")
            }
            None => {
                std::fs::remove_file(&installed_resource).expect("remove cached declared resource")
            }
        }
        if damaged_contents.is_none() {
            store.update(cx, |store, cx| store.reload(None, cx)).await;
        }
        let marker = extensions_dir.join("sources/git-extension/untracked-repair-marker");
        std::fs::write(&marker, "old checkout").expect("repair marker");
        store
            .update(cx, |store, cx| store.check_for_git_updates(cx))
            .await
            .expect("unchanged revision repairs invalid cached resources");
        assert!(git_failures(&store, cx).is_empty(), "{resource}");
        assert_eq!(
            std::fs::read(&installed_resource).expect("repaired resource"),
            original_contents,
            "{resource}"
        );
        assert_eq!(
            installed_source(&store, "git-extension", cx),
            original_source,
            "{resource}"
        );
        assert_eq!(
            std::fs::read(&record_path).expect("source record"),
            original_record,
            "{resource}"
        );
        assert!(
            !marker.exists(),
            "{resource}: repair must replace the checkout"
        );
    }

    let damaged_resource = extensions_dir.join("installed/git-extension/custom/snippets.json");
    std::fs::write(&damaged_resource, "{").expect("damage cached snippets");
    std::fs::rename(&remote.bare, remote.bare.with_extension("unavailable"))
        .expect("make fixture remote unavailable");
    store
        .update(cx, |store, cx| store.check_for_git_updates(cx))
        .await
        .expect("resource repair failure is reported");
    let failures = git_failures(&store, cx);
    assert_eq!(failures.len(), 1);
    let failure = failures.first().expect("resource repair failure");
    assert!(
        failure.message.contains("custom/snippets.json"),
        "{failure:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&damaged_resource).expect("existing damaged resource remains"),
        "{"
    );
    assert_eq!(
        std::fs::read(&record_path).expect("source record"),
        original_record
    );
}

#[gpui::test]
async fn test_pinned_git_extension_checks_health_and_repairs_only_its_installed_commit(
    cx: &mut TestAppContext,
) {
    let (_tree, extensions_dir, store, _debounce) = git_extension_store(cx).await;
    let remote = GitExtensionRemote::new("git-extension", Some("editors/ext"));
    let valid_wasm = git_extension_wasm(false).expect("prebuilt component with successful init");
    std::fs::write(remote.seed.join("editors/ext/extension.wasm"), &valid_wasm)
        .expect("prebuilt extension fixture");
    remote.publish("git-extension", "0.1.1");
    store
        .update(cx, |store, cx| {
            store.install_git_extension("git-extension".into(), remote.source(), cx)
        })
        .await
        .expect("prebuilt extension installs");
    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|settings, cx| {
            settings.update_user_settings(cx, |content| {
                content
                    .extension
                    .auto_update_extensions
                    .insert("git-extension".into(), false);
            });
        });
    });
    let original_source = installed_source(&store, "git-extension", cx).expect("installed source");
    assert_eq!(original_source.reference.as_deref(), Some("master"));
    let record_path = extensions_dir.join("sources/git-extension.toml");
    let original_record = std::fs::read(&record_path).expect("installed source record");
    let unavailable_remote = remote.bare.with_extension("unavailable");
    std::fs::rename(&remote.bare, &unavailable_remote).expect("make fixture remote unavailable");
    let marker = extensions_dir.join("sources/git-extension/untracked-health-marker");
    std::fs::write(&marker, "healthy checkout").expect("health marker");
    store
        .update(cx, |store, cx| store.check_for_git_updates(cx))
        .await
        .expect("healthy pinned extension needs no remote access");
    assert!(git_failures(&store, cx).is_empty());
    assert!(
        marker.exists(),
        "healthy pinned checkout must not be replaced"
    );
    std::fs::rename(&unavailable_remote, &remote.bare).expect("restore fixture remote");
    let newer_commit = remote.publish("git-extension", "0.2.0");
    assert_ne!(newer_commit, original_source.commit);

    let checkout = extensions_dir.join("sources/git-extension");
    let installed_wasm = extensions_dir.join("installed/git-extension/extension.wasm");
    for (damaged_path, is_directory) in [
        (&checkout, true),
        (&installed_wasm, false),
        (&record_path, false),
    ] {
        if is_directory {
            std::fs::remove_dir_all(damaged_path).expect("remove pinned checkout");
        } else {
            std::fs::remove_file(damaged_path).expect("remove pinned artifact or source record");
        }
        store
            .update(cx, |store, cx| store.check_for_git_updates(cx))
            .await
            .expect("broken pinned extension repairs its installed commit");
        assert!(git_failures(&store, cx).is_empty());
        assert_eq!(
            std::fs::read(&installed_wasm).expect("repaired pinned artifact"),
            valid_wasm
        );
        assert_eq!(
            installed_source(&store, "git-extension", cx).as_ref(),
            Some(&original_source)
        );
        assert_eq!(
            std::fs::read(&record_path).expect("pinned source record"),
            original_record
        );
        assert_eq!(
            git_in(&checkout, &["rev-parse", "HEAD"]),
            original_source.commit
        );
        assert_eq!(
            installed_version(&store, "git-extension", cx).as_deref(),
            Some("0.1.1")
        );
    }
    std::fs::rename(&remote.bare, &unavailable_remote)
        .expect("make fixture remote unavailable again");
    std::fs::remove_file(&installed_wasm).expect("remove pinned artifact before failed repair");
    store
        .update(cx, |store, cx| store.check_for_git_updates(cx))
        .await
        .expect("failed pinned repair is reported");
    let failures = git_failures(&store, cx);
    assert_eq!(failures.len(), 1);
    let failure = failures.first().expect("pinned repair failure");
    assert!(failure.message.contains("extension.wasm"), "{failure:?}");
    assert_eq!(
        std::fs::read(&record_path).expect("pinned record survives repair failure"),
        original_record
    );
    assert_eq!(
        installed_source(&store, "git-extension", cx).as_ref(),
        Some(&original_source)
    );
    assert!(!installed_wasm.exists());
}

#[gpui::test]
async fn test_git_update_repairs_legacy_declared_resources(cx: &mut TestAppContext) {
    let (_tree, extensions_dir, store, _debounce) = git_extension_store(cx).await;
    let remote = GitExtensionRemote::new("git-extension", Some("git-extension"));
    let directory = remote.seed.join("git-extension");
    std::fs::remove_file(directory.join("extension.toml")).expect("replace modern manifest");
    std::fs::write(
        directory.join("extension.json"),
        r#"{"name":"Legacy Fixture","version":"0.1.0","themes":{"Fixture":"custom/theme.json"}}"#,
    )
    .expect("legacy manifest");
    std::fs::create_dir_all(directory.join("custom")).expect("declared resource directory");
    let theme = r#"{"name":"Fixture","author":"Test","themes":[]}"#;
    std::fs::write(directory.join("custom/theme.json"), theme).expect("valid declared theme");
    git_in(&remote.seed, &["add", "-A"]);
    git_in(&remote.seed, &["commit", "-m", "legacy fixture"]);
    git_in(
        &remote.seed,
        &["push", &remote.bare.to_string_lossy(), "HEAD:master"],
    );
    store
        .update(cx, |store, cx| {
            store.install_git_extension("git-extension".into(), remote.source(), cx)
        })
        .await
        .expect("legacy extension installs");
    let source = installed_source(&store, "git-extension", cx);
    let installed_theme = extensions_dir.join("installed/git-extension/custom/theme.json");
    std::fs::write(&installed_theme, "{").expect("damage custom declared theme");
    store
        .update(cx, |store, cx| store.check_for_git_updates(cx))
        .await
        .expect("repair legacy theme");
    assert!(git_failures(&store, cx).is_empty());
    assert_eq!(
        std::fs::read_to_string(&installed_theme).expect("restored theme"),
        theme
    );
    assert_eq!(installed_source(&store, "git-extension", cx), source);
}

#[cfg(unix)]
#[gpui::test]
async fn test_git_source_path_symlinks_stay_inside_the_checkout(cx: &mut TestAppContext) {
    let (_tree, extensions_dir, store, _debounce) = git_extension_store(cx).await;
    let remote = GitExtensionRemote::new("git-extension", Some("actual"));
    store
        .update(cx, |store, cx| {
            store.install_git_extension("git-extension".into(), remote.source(), cx)
        })
        .await
        .expect("initial install");
    let original_source = installed_source(&store, "git-extension", cx);
    let external = local_dev_extension("git-extension");
    std::os::unix::fs::symlink(tree_root(&external), remote.seed.join("escape"))
        .expect("external directory symlink");
    std::os::unix::fs::symlink("actual", remote.seed.join("alias"))
        .expect("repository directory symlink");
    git_in(&remote.seed, &["add", "-A"]);
    git_in(&remote.seed, &["commit", "-m", "directory aliases"]);
    git_in(
        &remote.seed,
        &["push", &remote.bare.to_string_lossy(), "HEAD:master"],
    );

    let mut source = remote.source();
    source.path = Some("escape".into());
    let error = store
        .update(cx, |store, cx| {
            store.install_git_extension("git-extension".into(), source, cx)
        })
        .await
        .expect_err("external source path must be rejected before building");
    assert!(format!("{error:#}").contains("resolves outside its repository"));
    assert_eq!(
        installed_source(&store, "git-extension", cx),
        original_source
    );
    assert_eq!(
        installed_version(&store, "git-extension", cx).as_deref(),
        Some("0.1.0")
    );
    assert!(tree_root(&external).join("extension.toml").is_file());

    let mut source = remote.source();
    source.path = Some("alias".into());
    store
        .update(cx, |store, cx| {
            store.install_git_extension("git-extension".into(), source, cx)
        })
        .await
        .expect("a relative alias inside the repository remains valid");
    assert_eq!(
        std::fs::canonicalize(extensions_dir.join("installed/git-extension"))
            .expect("installed alias"),
        std::fs::canonicalize(extensions_dir.join("sources/git-extension/actual"))
            .expect("repository target"),
    );
}

#[gpui::test]
async fn test_configured_git_update_reports_lost_provenance_after_reindex(cx: &mut TestAppContext) {
    for (missing_install, malformed_record, removed_directory) in [
        (false, false, None),
        (true, false, None),
        (false, true, None),
        (false, false, Some("editors/ext")),
        (true, true, Some("")),
    ] {
        let (_tree, extensions_dir, store, _debounce) = git_extension_store(cx).await;
        let remote = GitExtensionRemote::new("git-extension", Some("editors/ext"));
        store
            .update(cx, |store, cx| {
                store.install_git_extension("git-extension".into(), remote.source(), cx)
            })
            .await
            .expect("initial install");
        let original_source =
            installed_source(&store, "git-extension", cx).expect("installed source");
        cx.update(|cx| {
            cx.update_global::<SettingsStore, _>(|settings, cx| {
                settings.update_user_settings(cx, |content| {
                    content
                        .extension
                        .auto_update_extensions
                        .insert("git-extension".into(), false);
                    content.extension.auto_install_extensions.insert(
                        "git-extension".into(),
                        settings::AutoInstallExtensionContent::Git {
                            git: remote.url.clone(),
                            reference: Some("master".into()),
                            path: Some("editors/ext".into()),
                        },
                    );
                });
            });
        });
        let checkout = extensions_dir.join("sources/git-extension");
        let manifest = checkout.join("editors/ext/extension.toml");
        let original_manifest = std::fs::read(&manifest).expect("installed manifest");
        remote.publish("git-extension", "0.2.0");
        let record = extensions_dir.join("sources/git-extension.toml");
        if malformed_record {
            std::fs::write(&record, "[").expect("corrupt source record");
        } else {
            std::fs::remove_file(&record).expect("remove source record");
        }
        if missing_install {
            let fs = store.read_with(cx, |store, _| store.fs.clone());
            fs.remove_file(
                &extensions_dir.join("installed/git-extension"),
                RemoveOptions::default(),
            )
            .await
            .expect("remove installed link");
        }
        if let Some(directory) = removed_directory {
            std::fs::remove_dir_all(checkout.join(directory))
                .expect("remove managed source directory");
        }
        store.update(cx, |store, cx| store.reload(None, cx)).await;
        assert!(installed_source(&store, "git-extension", cx).is_none());
        store
            .update(cx, |store, cx| store.check_for_git_updates(cx))
            .await
            .expect("report lost provenance");
        let failures = git_failures(&store, cx);
        assert_eq!(failures.len(), 1);
        assert!(
            failures
                .first()
                .expect("Git failure")
                .message
                .contains("installed Git commit cannot be recovered")
        );
        if removed_directory.is_none() {
            assert_eq!(
                std::fs::read(&manifest).expect("preserved manifest"),
                original_manifest
            );
        } else {
            assert!(
                !manifest.exists(),
                "lost provenance must not fetch a replacement"
            );
        }
        if removed_directory != Some("") {
            assert_eq!(
                git_in(&checkout, &["rev-parse", "HEAD"]),
                original_source.commit
            );
        } else {
            assert!(
                !checkout.exists(),
                "lost provenance must not recreate the checkout"
            );
        }
        assert_eq!(record.exists(), malformed_record);

        let local = local_dev_extension("git-extension");
        store
            .update(cx, |store, cx| {
                store.install_dev_extension(tree_root(&local), cx)
            })
            .await
            .expect("explicit local replacement");
        std::fs::create_dir_all(&checkout).expect("orphan checkout directory");
        store
            .update(cx, |store, cx| store.check_for_git_updates(cx))
            .await
            .expect("local replacement stays local");
        assert!(git_failures(&store, cx).is_empty());
        assert_eq!(
            installed_version(&store, "git-extension", cx).as_deref(),
            Some("9.0.0")
        );
    }
}

#[gpui::test]
async fn test_pinned_git_update_recovers_after_reindex(cx: &mut TestAppContext) {
    for damaged_resource in ["extension.toml", "languages/plain/config.toml"] {
        let (_tree, extensions_dir, store, _debounce) = git_extension_store(cx).await;
        let remote = GitExtensionRemote::new("git-extension", None);
        std::fs::create_dir_all(remote.seed.join("languages/plain")).expect("language directory");
        std::fs::write(
            remote.seed.join("languages/plain/config.toml"),
            "name = \"Plain Fixture\"\n",
        )
        .expect("language config");
        remote.publish("git-extension", "0.1.1");
        store
            .update(cx, |store, cx| {
                store.install_git_extension("git-extension".into(), remote.source(), cx)
            })
            .await
            .expect("initial install");
        let original_source = installed_source(&store, "git-extension", cx);
        cx.update(|cx| {
            cx.update_global::<SettingsStore, _>(|settings, cx| {
                settings.update_user_settings(cx, |content| {
                    content
                        .extension
                        .auto_update_extensions
                        .insert("git-extension".into(), false);
                });
            });
        });
        remote.publish("git-extension", "0.2.0");
        std::fs::write(
            extensions_dir
                .join("installed/git-extension")
                .join(damaged_resource),
            "[",
        )
        .expect("corrupt indexed resource");
        store.update(cx, |store, cx| store.reload(None, cx)).await;
        assert!(installed_version(&store, "git-extension", cx).is_none());
        store
            .update(cx, |store, cx| store.check_for_git_updates(cx))
            .await
            .expect("repair from sidecar");
        assert!(git_failures(&store, cx).is_empty());
        assert_eq!(
            installed_source(&store, "git-extension", cx),
            original_source
        );
        assert_eq!(
            installed_version(&store, "git-extension", cx).as_deref(),
            Some("0.1.1")
        );
    }
}

#[gpui::test]
async fn test_git_update_repairs_a_missing_installed_link(cx: &mut TestAppContext) {
    let (_tree, extensions_dir, store, _debounce) = git_extension_store(cx).await;
    let remote = GitExtensionRemote::new("git-extension", None);
    store
        .update(cx, |store, cx| {
            store.install_git_extension("git-extension".into(), remote.source(), cx)
        })
        .await
        .expect("initial install");
    let original_source = installed_source(&store, "git-extension", cx);
    let fs = store.read_with(cx, |store, _| store.fs.clone());
    fs.remove_file(
        &extensions_dir.join("installed/git-extension"),
        RemoveOptions::default(),
    )
    .await
    .expect("remove installed link without its checkout");
    store
        .update(cx, |store, cx| store.check_for_git_updates(cx))
        .await
        .expect("repair link");
    assert!(git_failures(&store, cx).is_empty());
    assert_eq!(
        installed_source(&store, "git-extension", cx),
        original_source
    );
    assert_eq!(
        std::fs::canonicalize(extensions_dir.join("installed/git-extension"))
            .expect("repaired link"),
        std::fs::canonicalize(extensions_dir.join("sources/git-extension"))
            .expect("source checkout")
    );
}

#[gpui::test]
async fn test_pinned_git_update_recovers_after_restart(cx: &mut TestAppContext) {
    let (_tree, extensions_dir, store, debounce) = git_extension_store(cx).await;
    let remote = GitExtensionRemote::new("git-extension", None);
    store
        .update(cx, |store, cx| {
            store.install_git_extension("git-extension".into(), remote.source(), cx)
        })
        .await
        .expect("initial install");
    let original_source = installed_source(&store, "git-extension", cx);
    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|settings, cx| {
            settings.update_user_settings(cx, |content| {
                content
                    .extension
                    .auto_update_extensions
                    .insert("git-extension".into(), false);
            });
        });
    });
    remote.publish("git-extension", "0.2.0");
    std::fs::write(
        extensions_dir.join("installed/git-extension/extension.toml"),
        "[",
    )
    .expect("corrupt manifest before shutdown");
    store.update(cx, |store, cx| store.reload(None, cx)).await;
    assert!(installed_version(&store, "git-extension", cx).is_none());
    drop((store, debounce));
    let (restarted, _debounce) =
        extension_store_in(&extensions_dir, FakeHttpClient::with_200_response(), cx).await;
    restarted
        .update(cx, |store, cx| store.check_for_git_updates(cx))
        .await
        .expect("repair after restart");
    assert!(git_failures(&restarted, cx).is_empty());
    assert_eq!(
        installed_source(&restarted, "git-extension", cx),
        original_source
    );
    assert_eq!(
        installed_version(&restarted, "git-extension", cx).as_deref(),
        Some("0.1.0")
    );
}

#[gpui::test]
async fn test_pinned_git_auto_install_requires_the_recorded_commit(cx: &mut TestAppContext) {
    for record_damage in [None, Some("malformed"), Some("missing")] {
        let (_tree, extensions_dir, store, _debounce) = git_extension_store(cx).await;
        let remote = GitExtensionRemote::new("git-extension", None);
        store
            .update(cx, |store, cx| {
                store.install_git_extension("git-extension".into(), remote.source(), cx)
            })
            .await
            .expect("initial install");
        let original_source = installed_source(&store, "git-extension", cx);
        cx.update(|cx| {
            cx.update_global::<SettingsStore, _>(|settings, cx| {
                settings.update_user_settings(cx, |content| {
                    content
                        .extension
                        .auto_update_extensions
                        .insert("git-extension".into(), false);
                });
            });
        });
        remote.publish("git-extension", "0.2.0");
        std::fs::write(
            extensions_dir.join("installed/git-extension/extension.toml"),
            "[",
        )
        .expect("corrupt manifest");
        let record = extensions_dir.join("sources/git-extension.toml");
        match record_damage {
            Some("malformed") => std::fs::write(&record, "[").expect("corrupt source record"),
            Some(_) => std::fs::remove_file(&record).expect("remove source record"),
            None => {}
        }
        store.update(cx, |store, cx| store.reload(None, cx)).await;
        assert!(installed_version(&store, "git-extension", cx).is_none());
        store
            .update(cx, |store, cx| {
                store.install_git_extensions(vec![("git-extension".into(), remote.source())], cx)
            })
            .await;
        if record_damage.is_none() {
            assert!(git_failures(&store, cx).is_empty());
            assert_eq!(
                installed_source(&store, "git-extension", cx),
                original_source
            );
            assert_eq!(
                installed_version(&store, "git-extension", cx).as_deref(),
                Some("0.1.0")
            );
            let replacement = GitExtensionRemote::new("git-extension", None);
            replacement.publish("git-extension", "0.3.0");
            store
                .update(cx, |store, cx| {
                    store.install_git_extensions(
                        vec![("git-extension".into(), replacement.source())],
                        cx,
                    )
                })
                .await;
            assert!(git_failures(&store, cx).is_empty());
            assert_eq!(
                installed_version(&store, "git-extension", cx).as_deref(),
                Some("0.3.0")
            );
        } else {
            assert!(installed_version(&store, "git-extension", cx).is_none());
            let failures = git_failures(&store, cx);
            assert_eq!(failures.len(), 1);
            assert!(
                failures
                    .first()
                    .expect("repair failure")
                    .message
                    .contains("installed commit")
            );
        }
    }
}

#[gpui::test]
async fn test_git_update_repairs_default_snippets_at_an_unchanged_commit(cx: &mut TestAppContext) {
    let (_tree, extensions_dir, store, _debounce) = git_extension_store(cx).await;
    let remote = GitExtensionRemote::new("git-extension", None);
    let valid_snippets = r#"{"Hello":{"prefix":"hello","body":"Hello ${1:World}"}}"#;
    std::fs::write(remote.seed.join("snippets.json"), valid_snippets)
        .expect("default snippets fixture");
    remote.publish("git-extension", "0.1.1");
    store
        .update(cx, |store, cx| {
            store.install_git_extension("git-extension".into(), remote.source(), cx)
        })
        .await
        .expect("inferred default snippets install");
    let original_source = installed_source(&store, "git-extension", cx);
    let installed_snippets = extensions_dir.join("installed/git-extension/snippets.json");
    for damaged_contents in [Some("{"), None] {
        match damaged_contents {
            Some(contents) => {
                std::fs::write(&installed_snippets, contents).expect("damage default snippets")
            }
            None => std::fs::remove_file(&installed_snippets).expect("remove default snippets"),
        }
        store.update(cx, |store, cx| store.reload(None, cx)).await;
        store
            .update(cx, |store, cx| store.check_for_git_updates(cx))
            .await
            .expect("inferred snippets repair at an unchanged revision");
        assert!(git_failures(&store, cx).is_empty());
        assert_eq!(
            std::fs::read_to_string(&installed_snippets).expect("repaired default snippets"),
            valid_snippets
        );
        assert_eq!(
            installed_source(&store, "git-extension", cx),
            original_source
        );
    }
}

#[gpui::test]
async fn test_git_language_metadata_validation_without_grammar(executor: BackgroundExecutor) {
    let fs = FakeFs::new(executor);
    let root = Path::new(path!("/extension"));
    fs.insert_tree(
        root,
        json!({
            "extension.toml": "id = \"test\"\nname = \"Test\"\nversion = \"1.0.0\"\nschema_version = 1\nlanguages = [\"languages/plain\"]\n",
            "languages": {"plain": {
                "config.toml": "name = \"Test Plain\"\n",
            }},
        }),
    )
    .await;
    let filesystem: Arc<dyn Fs> = fs.clone();
    let manifest = ExtensionManifest::load(filesystem.clone(), root)
        .await
        .expect("valid grammarless language manifest");
    let proxy = ExtensionHostProxy::new();
    crate::validate_git_language_resources(&filesystem, root, &manifest, &proxy)
        .await
        .expect("missing optional language metadata is valid");
    let language_dir = root.join("languages/plain");
    for (file_name, contents) in VALID_GIT_LANGUAGE_METADATA {
        fs.insert_file(language_dir.join(file_name), contents.as_bytes().to_vec())
            .await;
    }
    crate::validate_git_language_resources(&filesystem, root, &manifest, &proxy)
        .await
        .expect("comments and trailing commas are valid without a grammar");

    for (file_name, valid_contents) in VALID_GIT_LANGUAGE_METADATA {
        let resource_path = language_dir.join(file_name);
        for invalid_contents in [b"[".as_slice(), b"[123]".as_slice(), b"\xff".as_slice()] {
            fs.insert_file(&resource_path, invalid_contents.to_vec())
                .await;
            let error =
                crate::validate_git_language_resources(&filesystem, root, &manifest, &proxy)
                    .await
                    .expect_err("present malformed or unreadable metadata must fail");
            let message = format!("{error:#}");
            assert!(message.contains(file_name), "{message}");
            assert!(message.contains("languages/plain"), "{message}");
        }
        fs.remove_file(&resource_path, RemoveOptions::default())
            .await
            .expect("remove unreadable metadata");
        fs.create_dir(&resource_path)
            .await
            .expect("directory in place of metadata file");
        let error = crate::validate_git_language_resources(&filesystem, root, &manifest, &proxy)
            .await
            .expect_err("an existing metadata path that cannot be read is not absent");
        assert!(format!("{error:#}").contains(file_name));
        fs.remove_dir(&resource_path, RemoveOptions::default())
            .await
            .expect("remove metadata directory");
        fs.insert_file(&resource_path, valid_contents.as_bytes().to_vec())
            .await;
    }
}

#[gpui::test]
async fn test_git_update_rejects_invalid_language_metadata(cx: &mut TestAppContext) {
    for (file_name, invalid_contents) in [
        (settings::SemanticTokenRules::FILE_NAME, "["),
        (
            settings::SemanticTokenRules::FILE_NAME,
            r#"[{"token_type": 123}]"#,
        ),
        (task::TaskTemplates::FILE_NAME, "["),
        (
            task::TaskTemplates::FILE_NAME,
            r#"[{"label": "Missing command"}]"#,
        ),
    ] {
        let (_tree, extensions_dir, store, _debounce) = git_extension_store(cx).await;
        let remote = GitExtensionRemote::new("git-extension", None);
        let language_dir = remote.seed.join("languages/plain");
        std::fs::create_dir_all(&language_dir).expect("grammarless language directory");
        std::fs::write(language_dir.join("config.toml"), "name = \"Test Plain\"\n")
            .expect("grammarless language config");
        for (metadata_name, contents) in VALID_GIT_LANGUAGE_METADATA {
            std::fs::write(language_dir.join(metadata_name), contents)
                .expect("valid language metadata");
        }
        remote.publish("git-extension", "0.1.1");
        store
            .update(cx, |store, cx| {
                store.install_git_extension("git-extension".into(), remote.source(), cx)
            })
            .await
            .expect("valid JSONC language metadata installs");
        let original_source = installed_source(&store, "git-extension", cx);
        let record_path = extensions_dir.join("sources/git-extension.toml");
        let original_record = std::fs::read(&record_path).expect("installed source record");
        let installed_resource = extensions_dir
            .join("installed/git-extension/languages/plain")
            .join(file_name);
        let original_resource =
            std::fs::read(&installed_resource).expect("installed language metadata");
        let original_manifest =
            std::fs::read(extensions_dir.join("sources/git-extension/extension.toml"))
                .expect("installed checkout manifest");

        std::fs::write(language_dir.join(file_name), invalid_contents)
            .expect("invalid language metadata");
        remote.publish("git-extension", "0.2.0");
        store
            .update(cx, |store, cx| store.check_for_git_updates(cx))
            .await
            .expect("update failure is reported");
        let failures = git_failures(&store, cx);
        assert_eq!(failures.len(), 1);
        let failure = failures.first().expect("git update failure notification");
        assert!(failure.message.contains(file_name), "{failure:?}");
        assert!(failure.message.contains("languages/plain"), "{failure:?}");
        assert_eq!(
            installed_source(&store, "git-extension", cx),
            original_source
        );
        assert_eq!(
            std::fs::read(&record_path).expect("source record remains readable"),
            original_record
        );
        assert_eq!(
            std::fs::read(&installed_resource).expect("previous metadata remains readable"),
            original_resource
        );
        assert_eq!(
            std::fs::read(extensions_dir.join("sources/git-extension/extension.toml"))
                .expect("previous checkout remains readable"),
            original_manifest
        );
        assert_eq!(
            installed_version(&store, "git-extension", cx).as_deref(),
            Some("0.1.1")
        );

        std::fs::write(language_dir.join(file_name), &original_resource).expect("repair metadata");
        remote.publish("git-extension", "0.3.0");
        store
            .update(cx, |store, cx| store.check_for_git_updates(cx))
            .await
            .expect("repaired metadata updates");
        assert!(git_failures(&store, cx).is_empty());
        assert_eq!(
            installed_version(&store, "git-extension", cx).as_deref(),
            Some("0.3.0")
        );
    }
}
