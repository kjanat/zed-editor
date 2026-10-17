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
        assert_eq!(fs.metadata_call_count(), prev_fs_metadata_call_count + 2);
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

    fs.set_remove_dir_error("/extensions/ext-a", "simulated removal failure".to_string());

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
async fn test_headless_install_failure_after_removing_old_files_evicts_extension(
    cx: &mut TestAppContext,
) {
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
    store.read_with(cx, |store, _cx| {
        assert_eq!(
            store
                .loaded_extensions
                .get("foo")
                .map(|extension| extension.version.as_ref()),
            Some("1.0.0")
        );
    });

    fs.set_remove_dir_error("/extensions/foo", "simulated removal failure".to_string());
    let install_result = store
        .update(cx, |store, cx| {
            store.install_extension(extension(), PathBuf::from("/staged/foo"), cx)
        })
        .await;
    assert!(
        install_result.is_err(),
        "an install whose removal step failed should report the error"
    );

    store.read_with(cx, |store, _cx| {
        assert_eq!(
            store
                .loaded_extensions
                .get("foo")
                .map(|extension| extension.version.as_ref()),
            None,
            "a failed install after destructive operations started should evict the extension"
        );
    });

    let missing = store
        .update(cx, |store, cx| store.sync_extensions(vec![extension()], cx))
        .await
        .unwrap();
    assert_eq!(
        missing.len(),
        0,
        "a later sync should reload the evicted extension from the intact installed files"
    );
    store.read_with(cx, |store, _cx| {
        assert_eq!(
            store
                .loaded_extensions
                .get("foo")
                .map(|extension| extension.version.as_ref()),
            Some("1.0.0"),
            "a later sync should repair the evicted extension"
        );
    });
}

#[gpui::test]
async fn test_headless_failed_install_cleanup_is_retried_on_later_syncs(cx: &mut TestAppContext) {
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

    fs.set_remove_dir_error("/extensions/foo", "simulated removal failure".to_string());
    let install_result = store
        .update(cx, |store, cx| {
            store.install_extension(extension(), PathBuf::from("/staged/foo"), cx)
        })
        .await;
    assert!(
        install_result.is_err(),
        "an install whose removal step failed should report the error"
    );

    store
        .update(cx, |store, cx| store.sync_extensions(Vec::new(), cx))
        .await
        .unwrap();
    assert!(
        fs.is_dir(Path::new("/extensions/foo")).await,
        "the cleanup retry should leave the files while removal keeps failing"
    );

    fs.clear_remove_dir_error("/extensions/foo");
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

    fs.set_remove_dir_error("/extensions/ext-a", "simulated removal failure".to_string());
    fs.set_remove_dir_error("/extensions/ext-c", "simulated removal failure".to_string());
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

    fs.clear_remove_dir_error("/extensions/ext-a");
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
    FakeHttpClient::create(move |request| async move {
        if !request
            .uri()
            .path()
            .ends_with(&format!("/extensions/{extension_id}/download"))
        {
            return Ok(Response::builder().status(404).body("not found".into())?);
        }
        let manifest = format!(
            "id = \"{extension_id}\"\nname = \"Marketplace Extension\"\nversion = \"{version}\"\nschema_version = 1\n"
        );
        let mut bytes = Vec::<u8>::new();
        let mut archive = async_tar::Builder::new(&mut bytes);
        let mut header = async_tar::Header::new_gnu();
        header.set_size(manifest.len() as u64);
        header.set_mode(0o644);
        archive
            .append_data(&mut header, "extension.toml", manifest.as_bytes())
            .await?;
        archive.into_inner().await?;
        let mut gzipped_bytes = Vec::new();
        GzipEncoder::new(BufReader::new(bytes.as_slice()))
            .read_to_end(&mut gzipped_bytes)
            .await?;
        Ok(Response::new(gzipped_bytes.into()))
    })
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
    let (_tree, extensions_dir, store, _debounce) = git_extension_store(cx).await;
    let remote = GitExtensionRemote::new("git-extension", None);
    let source = remote.source();
    store
        .update(cx, |store, cx| {
            store.install_git_extension("git-extension".into(), source.clone(), cx)
        })
        .await
        .unwrap();
    let index_path = extensions_dir.join("index.json");
    assert!(index_path.exists());

    let commit = remote.publish("git-extension", "0.2.0");
    let (fs, builder, staging_dir, sources_dir, wasm_host) = store.read_with(cx, |store, _| {
        (
            store.fs.clone(),
            store.builder.clone(),
            store.staging_dir.clone(),
            store.sources_dir.clone(),
            store.wasm_host.clone(),
        )
    });
    crate::build_git_source(
        fs,
        &builder,
        &staging_dir,
        &sources_dir,
        &index_path,
        &extensions_dir.join("installed"),
        wasm_host,
        "git-extension",
        &source,
    )
    .await
    .unwrap();
    assert!(!index_path.exists());
    drop(store);

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
}

#[gpui::test]
async fn test_failed_git_relink_restores_checkout_record_and_link(cx: &mut TestAppContext) {
    let (_tree, extensions_dir, store, _debounce) = git_extension_store(cx).await;
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
    let staged = tempfile::tempdir_in(extensions_dir.join("staging"))
        .expect("staging directory should be writable");
    std::fs::write(
        staged.path().join("extension.toml"),
        old_manifest.replace("0.1.0", "0.2.0"),
    )
    .expect("transaction fixture should be written");
    let backup = extensions_dir.join("staging/git-source.previous");

    crate::GitSourceTransaction {
        staging_dir: &extensions_dir.join("staging"),
        sources_dir: &extensions_dir.join("sources"),
        index_path: &extensions_dir.join("index.json"),
        installed_dir: &extensions_dir.join("installed"),
        extension_id: "git-extension",
    }
    .commit(&fs, staged.path(), old_record.clone(), async {
        assert!(
            std::fs::symlink_metadata(backup.join("installed"))
                .expect("previous installed link should be backed up")
                .file_type()
                .is_symlink()
        );
        assert!(std::fs::symlink_metadata(&installed).is_err());
        // Simulate another filesystem actor occupying the removed link's name.
        fs.atomic_write(installed.clone(), "occupied".into())
            .await?;
        fs.create_symlink(&installed, live.clone()).await
    })
    .await
    .expect_err("relinking onto an occupied name must fail");
    assert_eq!(
        std::fs::read_link(&installed).expect("installed extension link should exist"),
        old_target
    );
    assert_eq!(
        std::fs::read_to_string(installed.join("extension.toml"))
            .expect("extension fixture contents should be readable"),
        old_manifest
    );
    assert_eq!(
        std::fs::read_to_string(record_path)
            .expect("extension fixture contents should be readable"),
        old_record
    );
    assert!(!backup.exists());
    store.update(cx, |store, cx| store.reload(None, cx)).await;
    assert_eq!(
        installed_version(&store, "git-extension", cx).as_deref(),
        Some("0.1.0")
    );
}

#[gpui::test]
async fn test_git_source_commit_defers_failed_cleanup(cx: &mut TestAppContext) {
    let (_tree, extensions_dir, store, _debounce) = git_extension_store(cx).await;
    let remote = GitExtensionRemote::new("git-extension", None);
    store
        .update(cx, |store, cx| {
            store.install_git_extension("git-extension".into(), remote.source(), cx)
        })
        .await
        .expect("initial install");
    let fs = store.read_with(cx, |store, _| store.fs.clone());
    let staging_dir = extensions_dir.join("staging");
    let sources_dir = extensions_dir.join("sources");
    let installed_dir = extensions_dir.join("installed");
    let index_path = extensions_dir.join("index.json");
    let record_path = sources_dir.join("git-extension.toml");
    let record = std::fs::read_to_string(&record_path).expect("source record");
    let staged = tempfile::tempdir_in(&staging_dir).expect("staged checkout");
    let manifest = std::fs::read_to_string(installed_dir.join("git-extension/extension.toml"))
        .expect("installed manifest")
        .replace("0.1.0", "0.2.0");
    std::fs::write(staged.path().join("extension.toml"), &manifest).expect("new manifest");
    crate::GitSourceTransaction {
        staging_dir: &staging_dir,
        sources_dir: &sources_dir,
        index_path: &index_path,
        installed_dir: &installed_dir,
        extension_id: "git-extension",
    }
    .commit(&fs, staged.path(), record.clone(), async {
        fs.create_symlink(
            &installed_dir.join("git-extension"),
            sources_dir.join("git-extension"),
        )
        .await?;
        // Make cache invalidation fail only during post-commit cleanup.
        fs.create_dir(&index_path).await
    })
    .await
    .expect("committed install must succeed despite deferred cleanup");
    let backup = staging_dir.join("git-source.previous");
    assert!(backup.join("committed").is_file());
    assert_eq!(
        std::fs::read_to_string(&record_path).expect("record"),
        record
    );
    assert_eq!(
        std::fs::read_to_string(installed_dir.join("git-extension/extension.toml"))
            .expect("new installed manifest"),
        manifest,
    );
    std::fs::remove_dir(&index_path).expect("remove cleanup obstruction");
    crate::recover_git_source(&fs, &staging_dir, &sources_dir, &index_path, &installed_dir)
        .await
        .expect("cleanup retry");
    assert!(!backup.exists());
    store.update(cx, |store, cx| store.reload(None, cx)).await;
    assert_eq!(
        installed_version(&store, "git-extension", cx).as_deref(),
        Some("0.2.0")
    );
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
async fn test_restart_recovers_git_relink_transaction(cx: &mut TestAppContext) {
    use GitRelinkInterruption::*;
    for interruption in [
        CheckoutReplaced,
        InstalledMovedOut,
        LinkCreated,
        RecordWritten,
        Committed,
        CheckoutRestored,
        InstalledRestored,
    ] {
        let (_tree, extensions_dir, store, debounce) = git_extension_store(cx).await;
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
        let old_target =
            std::fs::read_link(&installed).expect("installed extension link should exist");
        let mut new_source = installed_source(&store, "git-extension", cx)
            .expect("installed extension should have a source record");
        new_source.path = Some("nested".into());
        let new_record = toml::to_string(&new_source).expect("source record should serialize");
        let new_manifest = old_manifest.replace("0.1.0", "0.2.0");
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
        std::fs::rename(&live, backup.join("checkout"))
            .expect("transaction fixture entry should move");
        std::fs::create_dir_all(live.join("nested"))
            .expect("transaction fixture directory should be created");
        std::fs::write(live.join("nested/extension.toml"), &new_manifest)
            .expect("transaction fixture should be written");
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
            std::fs::write(&record_path, &new_record)
                .expect("transaction fixture should be written");
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
    new_manifest: &str,
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
        std::fs::create_dir_all(&live).unwrap();
        std::fs::write(live.join("extension.toml"), new_manifest).unwrap();
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
async fn test_restart_finishes_an_interrupted_git_source_swap(cx: &mut TestAppContext) {
    use GitSourceSwapInterruption::*;
    for interruption in [
        BackupCreated,
        CheckoutMovedOut,
        NewCheckoutMovedIn,
        FirstCheckoutMovedIn,
        RecordWritten,
    ] {
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
        let old_manifest =
            std::fs::read_to_string(extensions_dir.join("sources/git-extension/extension.toml"))
                .unwrap();
        let old_commit = installed_source(&store, "git-extension", cx)
            .unwrap()
            .commit;
        let new_record = old_record.replace(&old_commit, &remote.publish("git-extension", "0.2.0"));
        if interruption == FirstCheckoutMovedIn {
            store
                .update(cx, |store, cx| {
                    store.uninstall_extension("git-extension".into(), cx)
                })
                .await
                .unwrap();
        }
        drop((store, debounce));
        interrupt_git_source_swap(
            &extensions_dir,
            interruption,
            &old_manifest.replace("0.1.0", "0.2.0"),
            &new_record,
        );

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
            std::fs::read_dir(extensions_dir.join("staging"))
                .unwrap()
                .count(),
            0,
            "{interruption:?}"
        );
    }
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
    let old_commit = installed_source(&store, "git-extension", cx)
        .unwrap()
        .commit;
    let commit = remote.publish("git-extension", "0.2.0");
    interrupt_git_source_swap(
        &extensions_dir,
        GitSourceSwapInterruption::NewCheckoutMovedIn,
        &old_manifest.replace("0.1.0", "0.2.0"),
        &old_record.replace(&old_commit, &commit),
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

    let content: settings::ExtensionSettingsContent = serde_json::from_value(json!({
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
async fn test_git_errors_redact_url_credentials(cx: &mut TestAppContext) {
    let (_tree, extensions_dir, store, _debounce) = git_extension_store(cx).await;
    let url = "https://kjanat:ghp_hunter2hunter2hunter2@127.0.0.1:9/zed-ldap.git";

    let directory = extensions_dir.join("work");
    git_in(&directory, &["init"]);
    git_in(&directory, &["remote", "add", "origin", url]);
    let error = crate::run_git(&directory, &["remote", "add", "origin", url])
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
            std::fs::read_to_string(
                extensions_dir.join("installed/git-extension/snippets/global.json")
            )
            .expect("original snippets"),
            valid_snippets
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
async fn test_git_update_rejects_wasm_load_failures(cx: &mut TestAppContext) {
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
            "failed to instantiate wasm extension",
        ),
        (
            git_extension_wasm_with_trapping_init().expect("component with trapping init"),
            "failed to initialize wasm extension",
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
            .contains("indexing the fetched extension"),
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

    let source_lock = store.read_with(cx, |store, _| store.git_source_lock.clone());
    let source_guard = source_lock.lock().await;
    let update = store.update(cx, |store, cx| store.check_for_git_updates(cx));
    cx.run_until_parked();
    store
        .update(cx, |store, cx| {
            store.uninstall_extension("git-extension".into(), cx)
        })
        .await
        .expect("uninstall should succeed");
    assert!(installed_version(&store, "git-extension", cx).is_none());
    drop(source_guard);
    update.await.expect("update check should succeed");

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
