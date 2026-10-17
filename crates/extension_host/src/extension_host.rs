mod capability_granter;
pub mod extension_settings;
pub mod headless_host;
mod language_collisions;
pub mod wasm_host;

#[cfg(test)]
mod extension_store_test;

use anyhow::{Context as _, Result, anyhow, bail};
use async_compression::futures::bufread::GzipDecoder;
use async_tar::Archive;
use client::{Client, proto, telemetry::Telemetry};
use cloud_api_types::{ExtensionMetadata, ExtensionProvides, GetExtensionsResponse};
use collections::{BTreeMap, BTreeSet, FxHashSet, HashMap, HashSet, btree_map};
pub use extension::ExtensionManifest;
use extension::extension_builder::{CompileExtensionOptions, ExtensionBuilder};
use extension::{
    ExtensionContextServerProxy, ExtensionDebugAdapterProviderProxy, ExtensionEvents,
    ExtensionGrammarProxy, ExtensionHostProxy, ExtensionLanguageProxy,
    ExtensionLanguageServerProxy, ExtensionSnippetProxy, ExtensionThemeProxy,
};
use fs::{Fs, RemoveOptions, RenameOptions};
use futures::future::{Shared, join_all};
use futures::{
    AsyncReadExt as _, Future, FutureExt as _, StreamExt as _,
    channel::{
        mpsc::{UnboundedReceiver, UnboundedSender, unbounded},
        oneshot,
    },
    io::BufReader,
    select_biased,
};
use gpui::{
    App, AppContext as _, AsyncApp, Context, Entity, EntityId, EventEmitter, Global, Subscription,
    Task, TaskExt, UpdateGlobal as _, WeakEntity, actions,
};
use http_client::{AsyncBody, HttpClient, HttpClientWithUrl};
use language::{
    LanguageConfig, LanguageMatcher, LanguageName, LanguageQueries, LoadedLanguage, QueryFile,
    QueryFileContents, QueryFiles, Rope,
};
use node_runtime::NodeRuntime;
use project::{ContextProviderWithTasks, Project};
use release_channel::ReleaseChannel;
use remote::{ConnectionState, RemoteClient, RemoteClientEvent};
use semver::Version;
use serde::{Deserialize, Serialize};
use settings::{SemanticTokenRules, Settings, SettingsStore};
use std::ops::RangeInclusive;
use std::str::FromStr;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::{
    borrow::Cow,
    cmp::Ordering,
    mem,
    path::{self, Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use task::TaskTemplates;
use url::Url;
use util::{
    PathExt, ResultExt,
    paths::{PathStyle, RemotePathBuf},
    redact::{redact_arguments, redact_command},
};
use wasm_host::{
    WasmExtension, WasmHost,
    wit::{is_supported_wasm_api_version, wasm_api_version_range},
};

pub use extension::{
    ExtensionLibraryKind, GrammarManifestEntry, OldExtensionManifest, SchemaVersion,
};
pub use extension_settings::{ExtensionSettings, GitExtensionSource};
use language_collisions::LanguageCollisions;
pub use language_collisions::{LanguageCollision, LanguageCollisionKey};

use crate::headless_host::hash_directory_contents;

pub const RELOAD_DEBOUNCE_DURATION: Duration = Duration::from_millis(200);
const FS_WATCH_LATENCY: Duration = Duration::from_millis(100);
pub(crate) const REMOTE_SYNC_RETRY_DELAY: Duration = Duration::from_secs(1);
pub(crate) const MAX_REMOTE_SYNC_RETRY_DELAY: Duration = Duration::from_secs(60);
pub(crate) const MAX_REMOTE_SYNC_ATTEMPTS: usize = 10;
pub(crate) const REMOTE_SYNC_TIMEOUT: Duration = Duration::from_secs(60 * 60);

pub(crate) fn remote_sync_retry_delay(attempts: usize) -> Duration {
    let exponential = REMOTE_SYNC_RETRY_DELAY * 2u32.saturating_pow(attempts.min(30) as u32);
    exponential.min(MAX_REMOTE_SYNC_RETRY_DELAY)
}

async fn with_remote_sync_timeout<T>(
    cx: &AsyncApp,
    timeout: Duration,
    description: &str,
    future: impl Future<Output = Result<T>>,
) -> Result<T> {
    let timer = cx.background_executor().timer(timeout).fuse();
    let future = future.fuse();
    futures::pin_mut!(timer, future);
    select_biased! {
        result = future => result,
        _ = timer => anyhow::bail!("timed out after {timeout:?} while {description}"),
    }
}

/// Extensions that should no longer be loaded or downloaded.
///
/// These snippets should no longer be downloaded or loaded, because their
/// functionality has been integrated into the core editor.
static SUPPRESSED_EXTENSIONS: LazyLock<FxHashSet<&str>> = LazyLock::new(|| {
    FxHashSet::from_iter([
        "snippets",
        "ruff",
        "ty",
        "basedpyright",
        "basher",
        // ACP
        "opencode",
        "mistral-vibe",
        "auggie",
        "stakpak",
        "codebuddy",
        "autohand-acp",
        "corust-agent",
        "factory-droid",
        "qqcode",
    ])
});

/// Returns the [`SchemaVersion`] range that is compatible with this version of Zed.
pub fn schema_version_range() -> RangeInclusive<SchemaVersion> {
    SchemaVersion::ZERO..=SchemaVersion::CURRENT
}

/// Returns whether the given extension version is compatible with this version of Zed.
pub fn is_version_compatible(
    release_channel: ReleaseChannel,
    extension_version: &ExtensionMetadata,
) -> bool {
    let schema_version = extension_version.manifest.schema_version.unwrap_or(0);
    if SchemaVersion::CURRENT.0 < schema_version {
        return false;
    }

    if let Some(wasm_api_version) = extension_version
        .manifest
        .wasm_api_version
        .as_ref()
        .and_then(|wasm_api_version| Version::from_str(wasm_api_version).ok())
        && !is_supported_wasm_api_version(release_channel, wasm_api_version)
    {
        return false;
    }

    true
}

pub struct ExtensionStore {
    pub proxy: Arc<ExtensionHostProxy>,
    pub builder: Arc<ExtensionBuilder>,
    pub extension_index: ExtensionIndex,
    language_collisions: LanguageCollisions,
    registered_languages: BTreeMap<LanguageName, Arc<str>>,
    pub fs: Arc<dyn Fs>,
    pub http_client: Arc<HttpClientWithUrl>,
    pub telemetry: Option<Arc<Telemetry>>,
    pub reload_tx: UnboundedSender<Option<Arc<str>>>,
    pub reload_complete_senders: Vec<oneshot::Sender<()>>,
    pub installed_dir: PathBuf,
    pub staging_dir: PathBuf,
    pub sources_dir: PathBuf,
    git_source_lock: Arc<futures::lock::Mutex<()>>,
    git_extension_failures: BTreeMap<Arc<str>, GitExtensionFailure>,
    undelivered_git_extension_failures: BTreeSet<Arc<str>>,
    pub outstanding_operations: BTreeMap<Arc<str>, ExtensionOperation>,
    pub index_path: PathBuf,
    pub modified_extensions: HashSet<Arc<str>>,
    pub wasm_host: Arc<WasmHost>,
    pub wasm_extensions: Vec<(Arc<ExtensionManifest>, WasmExtension)>,
    pub tasks: Vec<Task<()>>,
    pub(crate) remote_clients: HashMap<EntityId, RemoteClientState>,
    pub(crate) initial_index_load: Shared<Task<()>>,
}

pub(crate) struct RemoteClientState {
    dirty_tx: UnboundedSender<RemoteSyncSignal>,
    _task: Task<()>,
    _subscriptions: Subscription,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RemoteSyncSignal {
    IndexChanged,
    Reconnected,
}

#[derive(Clone, Copy)]
pub enum ExtensionOperation {
    Upgrade,
    Install,
    Remove,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExtensionStatus {
    NotInstalled,
    Installing,
    Upgrading,
    Installed(Arc<str>),
    Removing,
}

#[derive(Clone)]
pub enum Event {
    ExtensionsUpdated,
    StartedReloading,
    ExtensionInstalled(Arc<str>),
    ExtensionUninstalled(Arc<str>),
    ExtensionFailedToLoad(Arc<str>),
}

impl EventEmitter<Event> for ExtensionStore {}

struct GlobalExtensionStore(Entity<ExtensionStore>);

impl Global for GlobalExtensionStore {}

#[derive(Debug, Deserialize, Serialize, Default, PartialEq, Eq)]
pub struct ExtensionIndex {
    pub extensions: BTreeMap<Arc<str>, ExtensionIndexEntry>,
    pub themes: BTreeMap<Arc<str>, ExtensionIndexThemeEntry>,
    #[serde(default)]
    pub icon_themes: BTreeMap<Arc<str>, ExtensionIndexIconThemeEntry>,
    pub languages: BTreeMap<LanguageName, ExtensionIndexLanguageEntry>,
    // Required on disk so older indexes that discarded shadowed providers get rebuilt.
    pub language_providers: BTreeMap<LanguageName, BTreeSet<Arc<str>>>,
}

impl ExtensionIndex {
    fn extensions_to_sync_to_remote(&self) -> RemoteSyncExtensions {
        let mut extensions = RemoteSyncExtensions::default();

        for (id, entry) in &self.extensions {
            if entry.manifest.remote_load().is_some() {
                extensions.insert_extension_and_language_dependencies(self, id);
            }
        }

        extensions
    }
}

#[derive(Default)]
struct RemoteSyncExtensions(HashMap<Arc<str>, ExtensionIndexEntry>);

impl RemoteSyncExtensions {
    fn insert_extension_and_language_dependencies(
        &mut self,
        index: &ExtensionIndex,
        id: &Arc<str>,
    ) {
        if self.0.contains_key(id) {
            return;
        }

        let Some(entry) = index.extensions.get(id) else {
            return;
        };

        self.0.insert(id.clone(), entry.clone());

        let Some(remote_load) = entry.manifest.remote_load() else {
            return;
        };

        for language in remote_load.language_dependencies() {
            if let Some(language_entry) = index.languages.get(&language) {
                self.insert_extension_and_language_dependencies(index, &language_entry.extension);
            }
        }
    }

    fn into_entries(self) -> impl Iterator<Item = (Arc<str>, ExtensionIndexEntry)> {
        self.0.into_iter()
    }

    fn contains(&self, id: &str) -> bool {
        self.0.contains_key(id)
    }
}

#[derive(Clone, PartialEq, Eq, Debug, Deserialize, Serialize)]
pub struct ExtensionIndexEntry {
    pub manifest: Arc<ExtensionManifest>,
    pub dev: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<InstalledGitSource>,
}

#[derive(Clone, PartialEq, Eq, Debug, Deserialize, Serialize)]
pub struct InstalledGitSource {
    pub url: String,
    pub reference: Option<String>,
    pub path: Option<String>,
    pub commit: String,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct GitExtensionFailure {
    pub extension_id: Arc<str>,
    pub url: String,
    pub message: String,
}

enum GitUpdate {
    RecordMissing,
    Checked { url: String, result: Result<bool> },
}

impl InstalledGitSource {
    fn matches(&self, source: &GitExtensionSource) -> bool {
        self.url == source.url && self.reference == source.reference && self.path == source.path
    }

    fn source(&self) -> GitExtensionSource {
        GitExtensionSource {
            url: self.url.clone(),
            reference: self.reference.clone(),
            path: self.path.clone(),
        }
    }
}

fn is_single_path_component(extension_id: &str) -> bool {
    let mut components = Path::new(extension_id).components();
    matches!(
        (components.next(), components.next()),
        (Some(path::Component::Normal(name)), None) if name == std::ffi::OsStr::new(extension_id)
    )
}

fn validate_git_install(extension_id: &str, source: &GitExtensionSource) -> Result<()> {
    anyhow::ensure!(
        is_single_path_component(extension_id),
        "extension id {extension_id:?} must be a single path component"
    );
    if let Some(path) = &source.path {
        anyhow::ensure!(
            Path::new(path).components().all(|component| matches!(
                component,
                path::Component::Normal(_) | path::Component::CurDir
            )),
            "path {path:?} of extension {extension_id} must stay inside its repository"
        );
    }
    Ok(())
}

fn git_source_dir(sources_dir: &Path, extension_id: &str) -> PathBuf {
    sources_dir.join(extension_id)
}

fn git_source_record_path(sources_dir: &Path, extension_id: &str) -> PathBuf {
    sources_dir.join(format!("{extension_id}.toml"))
}

fn git_source_extension_path(
    sources_dir: &Path,
    extension_id: &str,
    path: Option<&str>,
) -> PathBuf {
    git_source_subdirectory(git_source_dir(sources_dir, extension_id), path)
}

fn git_source_subdirectory(mut directory: PathBuf, path: Option<&str>) -> PathBuf {
    if let Some(path) = path {
        directory.extend(Path::new(path).components());
    }
    directory
}

const GIT_SOURCE_BACKUP_ID: &str = "extension_id";
const GIT_SOURCE_BACKUP_CHECKOUT: &str = "checkout";
const GIT_SOURCE_BACKUP_RECORD: &str = "record.toml";
const GIT_SOURCE_BACKUP_INSTALLED: &str = "installed";
const GIT_SOURCE_BACKUP_STATE: &str = "state.toml";
const GIT_SOURCE_BACKUP_COMMITTED: &str = "committed";

#[derive(Serialize, Deserialize)]
struct GitSourceBackupState {
    checkout_existed: bool,
    installed_existed: bool,
    record: Option<String>,
}

fn git_source_backup_dir(staging_dir: &Path) -> PathBuf {
    staging_dir.join("git-source.previous")
}

async fn recover_git_source(
    fs: &Arc<dyn Fs>,
    staging_dir: &Path,
    sources_dir: &Path,
    index_path: &Path,
    installed_dir: &Path,
) -> Result<()> {
    let backup = git_source_backup_dir(staging_dir);
    let identifier = backup.join(GIT_SOURCE_BACKUP_ID);
    if fs.metadata(&identifier).await?.is_none() {
        return Ok(());
    }
    let extension_id = fs.load(&identifier).await?;
    anyhow::ensure!(
        is_single_path_component(&extension_id),
        "git source backup names extension {extension_id:?}"
    );
    fs.remove_file(
        index_path,
        RemoveOptions {
            recursive: false,
            ignore_if_not_exists: true,
        },
    )
    .await?;
    if fs
        .metadata(&backup.join(GIT_SOURCE_BACKUP_STATE))
        .await?
        .is_some()
    {
        let state: GitSourceBackupState =
            toml::from_str(&fs.load(&backup.join(GIT_SOURCE_BACKUP_STATE)).await?)?;
        if fs
            .metadata(&backup.join(GIT_SOURCE_BACKUP_COMMITTED))
            .await?
            .is_none()
        {
            restore_git_source_entry(
                fs,
                &git_source_dir(sources_dir, &extension_id),
                &backup.join(GIT_SOURCE_BACKUP_CHECKOUT),
                state.checkout_existed,
            )
            .await?;
            restore_git_source_entry(
                fs,
                &installed_dir.join(&extension_id),
                &backup.join(GIT_SOURCE_BACKUP_INSTALLED),
                state.installed_existed,
            )
            .await?;
            let record_path = git_source_record_path(sources_dir, &extension_id);
            match state.record {
                Some(record) => fs.atomic_write(record_path, record).await?,
                None => {
                    fs.remove_file(
                        &record_path,
                        RemoveOptions {
                            recursive: false,
                            ignore_if_not_exists: true,
                        },
                    )
                    .await?
                }
            }
        }
    } else {
        let live = git_source_dir(sources_dir, &extension_id);
        let previous = backup.join(GIT_SOURCE_BACKUP_CHECKOUT);
        let intended = fs.load(&backup.join(GIT_SOURCE_BACKUP_RECORD)).await.ok();
        let committed = match &intended {
            Some(intended) => {
                fs.is_dir(&live).await
                    && fs
                        .load(&git_source_record_path(sources_dir, &extension_id))
                        .await
                        .is_ok_and(|record| &record == intended)
            }
            None => false,
        };
        if !committed {
            let has_previous = fs.is_dir(&previous).await;
            if has_previous || intended.is_some() {
                fs.remove_dir(
                    &live,
                    RemoveOptions {
                        recursive: true,
                        ignore_if_not_exists: true,
                    },
                )
                .await?;
            }
            if has_previous {
                fs.rename(&previous, &live, RenameOptions::default())
                    .await?;
            }
        }
    }
    let discarded = tempfile::tempdir_in(staging_dir)?;
    fs.rename(
        &backup,
        &discarded.path().join(extension_id),
        RenameOptions::default(),
    )
    .await
}

async fn restore_git_source_entry(
    fs: &Arc<dyn Fs>,
    live: &Path,
    previous: &Path,
    existed: bool,
) -> Result<()> {
    let has_previous = fs.metadata(previous).await?.is_some();
    if has_previous || !existed {
        if let Some(metadata) = fs.metadata(live).await? {
            let options = RemoveOptions {
                recursive: !metadata.is_symlink,
                ignore_if_not_exists: true,
            };
            if metadata.is_dir && !metadata.is_symlink {
                fs.remove_dir(live, options).await?;
            } else {
                fs.remove_file(live, options).await?;
            }
        }
        if has_previous {
            fs.rename(previous, live, RenameOptions::default()).await?;
        }
    }
    Ok(())
}

async fn run_git(directory: &Path, args: &[&str]) -> Result<String> {
    let output = util::command::new_command("git")
        .args(args)
        .current_dir(directory)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .await
        .with_context(|| format!("running git {}", redact_arguments(args).join(" ")))?;
    anyhow::ensure!(
        output.status.success(),
        "git {} failed: {}",
        redact_arguments(args).join(" "),
        redact_command(String::from_utf8_lossy(&output.stderr).trim())
    );
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

async fn build_git_source(
    fs: Arc<dyn Fs>,
    builder: &ExtensionBuilder,
    staging_dir: &Path,
    sources_dir: &Path,
    index_path: &Path,
    installed_dir: &Path,
    wasm_host: Arc<WasmHost>,
    extension_id: &str,
    source: &GitExtensionSource,
) -> Result<PathBuf> {
    fs.create_dir(staging_dir).await?;
    recover_git_source(&fs, staging_dir, sources_dir, index_path, installed_dir).await?;
    let backup = git_source_backup_dir(staging_dir);
    fs.remove_dir(
        &backup,
        RemoveOptions {
            recursive: true,
            ignore_if_not_exists: true,
        },
    )
    .await?;
    let checkout = tempfile::tempdir_in(staging_dir)?;
    let directory = checkout.path();
    run_git(directory, &["init"]).await?;
    run_git(directory, &["remote", "add", "origin", &source.url]).await?;
    let reference = source.reference.as_deref().unwrap_or("HEAD");
    run_git(directory, &["fetch", "--depth", "1", "origin", reference]).await?;
    run_git(
        directory,
        &["checkout", "--force", "--detach", "FETCH_HEAD"],
    )
    .await?;
    let commit = run_git(directory, &["rev-parse", "HEAD"]).await?;

    let staged_extension = git_source_subdirectory(directory.to_path_buf(), source.path.as_deref());
    let mut manifest = ExtensionManifest::load(fs.clone(), &staged_extension).await?;
    anyhow::ensure!(
        manifest.id.as_ref() == extension_id,
        "the repository declares extension id {}, expected {extension_id}",
        manifest.id
    );
    builder
        .compile_extension(
            &staged_extension,
            &mut manifest,
            CompileExtensionOptions::dev(),
            fs.clone(),
        )
        .await?;
    validate_git_theme_resources(&fs, &staged_extension, &manifest).await?;
    validate_git_snippet_resources(&fs, &staged_extension, &manifest).await?;
    let wasm_path = staged_extension.join("extension.wasm");
    let mut staged_index = ExtensionIndex::default();
    ExtensionStore::add_extension_to_index(
        fs.clone(),
        staged_extension.clone(),
        sources_dir,
        &mut staged_index,
        wasm_host.proxy.clone(),
    )
    .await
    .context("indexing the fetched extension")?;

    let entry = staged_index
        .extensions
        .get(extension_id)
        .context("the fetched extension is missing from the staged index")?;
    validate_git_language_resources(&fs, &staged_extension, &entry.manifest, &wasm_host.proxy)
        .await?;
    if entry.manifest.lib.kind.is_some() {
        let wasm_bytes = fs
            .load_bytes(&wasm_path)
            .await
            .context("reading the fetched wasm extension")?;
        wasm_host
            .validate_extension(wasm_bytes, entry.manifest.clone())
            .await
            .context("validating the fetched wasm extension")?;
    }

    let record = toml::to_string(&InstalledGitSource {
        url: source.url.clone(),
        reference: source.reference.clone(),
        path: source.path.clone(),
        commit,
    })?;
    let extension_path =
        git_source_extension_path(sources_dir, extension_id, source.path.as_deref());
    GitSourceTransaction {
        staging_dir,
        sources_dir,
        index_path,
        installed_dir,
        extension_id,
    }
    .commit(
        &fs,
        directory,
        record,
        fs.create_symlink(&installed_dir.join(extension_id), extension_path.clone()),
    )
    .await?;
    Ok(extension_path)
}

struct GitSourceTransaction<'a> {
    staging_dir: &'a Path,
    sources_dir: &'a Path,
    index_path: &'a Path,
    installed_dir: &'a Path,
    extension_id: &'a str,
}

impl GitSourceTransaction<'_> {
    async fn commit(
        &self,
        fs: &Arc<dyn Fs>,
        directory: &Path,
        record: String,
        install_link: impl Future<Output = Result<()>>,
    ) -> Result<()> {
        let staging_dir = self.staging_dir;
        let sources_dir = self.sources_dir;
        let index_path = self.index_path;
        let installed_dir = self.installed_dir;
        let extension_id = self.extension_id;
        let backup = git_source_backup_dir(staging_dir);
        fs.remove_file(
            index_path,
            RemoveOptions {
                recursive: false,
                ignore_if_not_exists: true,
            },
        )
        .await?;

        let live = git_source_dir(sources_dir, extension_id);
        let installed = installed_dir.join(extension_id);
        let record_path = git_source_record_path(sources_dir, extension_id);
        let state = GitSourceBackupState {
            checkout_existed: fs.metadata(&live).await?.is_some(),
            installed_existed: fs.metadata(&installed).await?.is_some(),
            record: if fs.metadata(&record_path).await?.is_some() {
                Some(fs.load(&record_path).await?)
            } else {
                None
            },
        };
        let committed = async {
            fs.create_dir(&backup).await?;
            fs.atomic_write(backup.join(GIT_SOURCE_BACKUP_ID), extension_id.to_string())
                .await?;
            fs.atomic_write(
                backup.join(GIT_SOURCE_BACKUP_STATE),
                toml::to_string(&state)?,
            )
            .await?;
            if state.checkout_existed {
                fs.rename(
                    &live,
                    &backup.join(GIT_SOURCE_BACKUP_CHECKOUT),
                    RenameOptions::default(),
                )
                .await?;
            }
            fs.rename(
                directory,
                &live,
                RenameOptions {
                    create_parents: true,
                    ..RenameOptions::default()
                },
            )
            .await?;
            if state.installed_existed {
                fs.rename(
                    &installed,
                    &backup.join(GIT_SOURCE_BACKUP_INSTALLED),
                    RenameOptions::default(),
                )
                .await?;
            }
            install_link.await?;
            fs.atomic_write(record_path, record).await?;
            // Repairs can keep the same source record; only this marker commits the link too.
            fs.atomic_write(backup.join(GIT_SOURCE_BACKUP_COMMITTED), String::new())
                .await
        }
        .await;
        match committed {
            Ok(()) => {
                // The install is committed; retry cleanup on the next install or startup.
                recover_git_source(fs, staging_dir, sources_dir, index_path, installed_dir)
                    .await
                    .context("cleaning up a committed git extension install")
                    .log_err();
            }
            Err(error) => {
                recover_git_source(fs, staging_dir, sources_dir, index_path, installed_dir)
                    .await
                    .with_context(|| {
                        format!("recovering failed git extension install: {error:#}")
                    })?;
                return Err(error);
            }
        }
        Ok(())
    }
}

async fn validate_git_compiled_artifacts(
    fs: &Arc<dyn Fs>,
    extension_dir: &Path,
    manifest: &ExtensionManifest,
    wasm_host: &WasmHost,
) -> Result<()> {
    let wasm_path = extension_dir.join("extension.wasm");
    let requires_wasm = manifest.lib.kind.is_some()
        || fs.metadata(&extension_dir.join("Cargo.toml")).await?.is_some()
        || fs.metadata(&wasm_path).await?.is_some()
        // Reindexing a missing prebuilt library loses its inferred lib.kind.
        || !run_git(extension_dir, &["ls-tree", "--name-only", "HEAD", "--", "extension.wasm"])
            .await?
            .is_empty();
    if requires_wasm {
        let bytes = fs
            .load_bytes(&wasm_path)
            .await
            .context("reading extension.wasm")?;
        wasm_host
            .validate_compiled_extension(&manifest.id, &bytes)
            .context("validating extension.wasm")?;
    }
    for grammar_name in manifest.grammars.keys() {
        let grammar_path = extension_dir
            .join("grammars")
            .join(format!("{grammar_name}.wasm"));
        let bytes = fs
            .load_bytes(&grammar_path)
            .await
            .with_context(|| format!("reading grammar {grammar_name}.wasm"))?;
        language::with_parser(|parser| {
            let mut store = parser.take_wasm_store().context("missing wasm store")?;
            let grammar = store.load_language(grammar_name, &bytes);
            parser.set_wasm_store(store)?;
            grammar.map(drop).map_err(anyhow::Error::from)
        })
        .with_context(|| format!("validating grammar {grammar_name}.wasm"))?;
    }
    Ok(())
}

async fn validate_git_language_resources(
    fs: &Arc<dyn Fs>,
    extension_dir: &Path,
    manifest: &ExtensionManifest,
    proxy: &ExtensionHostProxy,
) -> Result<()> {
    for language_path in &manifest.languages {
        let language_dir = extension_dir.join(language_path);
        let result = async {
            let config = fs
                .load(&language_dir.join(LanguageConfig::FILE_NAME))
                .await?;
            let config = LanguageConfig::from_toml(&config)?;
            let rules_path = language_dir.join(SemanticTokenRules::FILE_NAME);
            if fs
                .metadata(&rules_path)
                .await
                .with_context(|| format!("checking {}", SemanticTokenRules::FILE_NAME))?
                .is_some()
            {
                let contents = fs
                    .load(&rules_path)
                    .await
                    .with_context(|| format!("reading {}", SemanticTokenRules::FILE_NAME))?;
                SemanticTokenRules::parse(&contents)
                    .with_context(|| format!("validating {}", SemanticTokenRules::FILE_NAME))?;
            }
            let tasks_path = language_dir.join(TaskTemplates::FILE_NAME);
            if fs
                .metadata(&tasks_path)
                .await
                .with_context(|| format!("checking {}", TaskTemplates::FILE_NAME))?
                .is_some()
            {
                let contents = fs
                    .load(&tasks_path)
                    .await
                    .with_context(|| format!("reading {}", TaskTemplates::FILE_NAME))?;
                serde_json_lenient::from_str::<TaskTemplates>(&contents)
                    .with_context(|| format!("validating {}", TaskTemplates::FILE_NAME))?;
            }
            let Some(grammar_name) = config.grammar.clone() else {
                return Ok(());
            };
            let query_files = discover_query_files(fs.clone(), &language_dir).await?;
            let mut files = Vec::new();
            for query_file in query_files.query_files() {
                let contents = fs.load(&language_dir.join(query_file.file_name())).await?;
                files.push(QueryFileContents::new(query_file, Cow::Owned(contents)));
            }
            let queries = LanguageQueries::from_files(files);
            if manifest.grammars.contains_key(&grammar_name) {
                let grammar_path = extension_dir
                    .join("grammars")
                    .join(format!("{grammar_name}.wasm"));
                let bytes = fs.load_bytes(&grammar_path).await?;
                let grammar = language::with_parser(|parser| {
                    let mut store = parser.take_wasm_store().context("missing wasm store")?;
                    let grammar = store.load_language(&grammar_name, &bytes);
                    parser.set_wasm_store(store)?;
                    grammar.map_err(anyhow::Error::from)
                })?;
                language::Language::new(config, Some(grammar)).with_queries(queries)?;
            } else {
                proxy
                    .validate_language_queries(manifest.id.clone(), config, queries)
                    .await?;
            }
            anyhow::Ok(())
        }
        .await;
        result.with_context(|| format!("validating language resources in {language_path}"))?;
    }
    Ok(())
}

async fn validate_git_theme_resources(
    fs: &Arc<dyn Fs>,
    extension_dir: &Path,
    manifest: &ExtensionManifest,
) -> Result<()> {
    // Indexing tolerates broken resources and absent UI proxies; a replacement must not.
    for theme_path in &manifest.themes {
        let path = extension_dir.join(theme_path.as_std_path());
        let contents = fs
            .load_bytes(&path)
            .await
            .with_context(|| format!("reading theme {theme_path}"))?;
        theme_settings::deserialize_user_theme(&contents)
            .with_context(|| format!("validating theme {theme_path}"))?;
    }
    for icon_theme_path in &manifest.icon_themes {
        let path = extension_dir.join(icon_theme_path.as_std_path());
        let contents = fs
            .load_bytes(&path)
            .await
            .with_context(|| format!("reading icon theme {icon_theme_path}"))?;
        theme::deserialize_icon_theme(&contents)
            .with_context(|| format!("validating icon theme {icon_theme_path}"))?;
    }
    Ok(())
}

async fn validate_git_snippet_resources(
    fs: &Arc<dyn Fs>,
    extension_dir: &Path,
    manifest: &ExtensionManifest,
) -> Result<()> {
    for snippet_path in manifest
        .snippets
        .iter()
        .flat_map(|snippets| snippets.paths())
    {
        let path = extension_dir.join(snippet_path);
        let contents = fs
            .load(&path)
            .await
            .with_context(|| format!("reading snippets {}", snippet_path.display()))?;
        let snippets =
            serde_json_lenient::from_str::<snippet_provider::format::VsSnippetsFile>(&contents)
                .with_context(|| format!("parsing snippets {}", snippet_path.display()))?;
        for snippet in snippet_provider::file_to_snippets(snippets, &path) {
            snippet?;
        }
    }
    Ok(())
}

async fn remote_commit(directory: &Path, source: &GitExtensionSource) -> Result<Option<String>> {
    let reference = source.reference.as_deref().unwrap_or("HEAD");
    let peeled = format!("{reference}^{{}}");
    let output = run_git(directory, &["ls-remote", &source.url, reference, &peeled]).await?;
    let refs = output
        .lines()
        .filter_map(|line| {
            let (commit, name) = line.split_once('\t')?;
            Some((name, commit))
        })
        .collect::<HashMap<_, _>>();
    Ok(resolve_advertised_ref(&refs, reference).map(str::to_string))
}

fn is_commit_sha(reference: &str) -> bool {
    matches!(reference.len(), 40 | 64) && reference.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn resolve_advertised_ref<'a>(refs: &HashMap<&str, &'a str>, reference: &str) -> Option<&'a str> {
    [
        reference.to_string(),
        format!("refs/{reference}"),
        format!("refs/tags/{reference}"),
        format!("refs/heads/{reference}"),
        format!("refs/remotes/{reference}"),
        format!("refs/remotes/{reference}/HEAD"),
    ]
    .iter()
    .find_map(|name| {
        refs.get(format!("{name}^{{}}").as_str())
            .or_else(|| refs.get(name.as_str()))
            .copied()
    })
}

async fn remove_git_source(fs: &Arc<dyn Fs>, sources_dir: &Path, extension_id: &str) -> Result<()> {
    let options = RemoveOptions {
        recursive: true,
        ignore_if_not_exists: true,
    };
    fs.remove_dir(&git_source_dir(sources_dir, extension_id), options)
        .await?;
    fs.remove_file(&git_source_record_path(sources_dir, extension_id), options)
        .await
}

async fn git_source_for_dev_extension(
    fs: &Arc<dyn Fs>,
    extension_dir: &Path,
    sources_dir: &Path,
    extension_id: &str,
) -> Option<InstalledGitSource> {
    let record = load_git_source_record(fs, sources_dir, extension_id).await?;
    let linked = fs.canonicalize(extension_dir).await.log_err()?;
    let checkout = git_source_extension_path(sources_dir, extension_id, record.path.as_deref());
    let checkout = fs.canonicalize(&checkout).await.ok()?;
    (linked == checkout).then_some(record)
}

async fn load_git_source_record(
    fs: &Arc<dyn Fs>,
    sources_dir: &Path,
    extension_id: &str,
) -> Option<InstalledGitSource> {
    let record_path = git_source_record_path(sources_dir, extension_id);
    if !fs.is_file(&record_path).await {
        return None;
    }
    fs.load(&record_path)
        .await
        .and_then(|record| Ok(toml::from_str(&record)?))
        .with_context(|| format!("reading git source record {record_path:?}"))
        .log_err()
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Deserialize, Serialize)]
pub struct ExtensionIndexThemeEntry {
    pub extension: Arc<str>,
    pub path: PathBuf,
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Deserialize, Serialize)]
pub struct ExtensionIndexIconThemeEntry {
    pub extension: Arc<str>,
    pub path: PathBuf,
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Deserialize, Serialize)]
pub struct ExtensionIndexLanguageEntry {
    pub extension: Arc<str>,
    pub path: PathBuf,
    pub matcher: Arc<LanguageMatcher>,
    pub hidden: bool,
    pub grammar: Option<Arc<str>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query_files: Option<QueryFiles>,
}

actions!(
    zed,
    [
        /// Reloads all installed extensions.
        ReloadExtensions
    ]
);

pub fn init(
    extension_host_proxy: Arc<ExtensionHostProxy>,
    fs: Arc<dyn Fs>,
    client: Arc<Client>,
    node_runtime: NodeRuntime,
    cx: &mut App,
) {
    let store = cx.new(move |cx| {
        ExtensionStore::new(
            paths::extensions_dir().clone(),
            None,
            extension_host_proxy,
            fs,
            client.http_client(),
            client.http_client(),
            Some(client.telemetry().clone()),
            node_runtime,
            cx,
        )
    });

    cx.on_action(|_: &ReloadExtensions, cx| {
        let store = cx.global::<GlobalExtensionStore>().0.clone();
        store.update(cx, |store, cx| drop(store.reload_all(cx)));
    });

    cx.set_global(GlobalExtensionStore(store));

    cx.observe_new::<Project>(|project, _window, cx| {
        let Some(client) = project.remote_client() else {
            return;
        };
        if let Some(store) = ExtensionStore::try_global(cx) {
            store.update(cx, |store, cx| store.register_remote_client(client, cx));
        }
    })
    .detach();
}

impl ExtensionStore {
    pub fn active_language_collision_warnings(
        &self,
    ) -> &BTreeMap<LanguageCollisionKey, LanguageCollision> {
        self.language_collisions.active_warnings()
    }

    pub fn take_language_collision_warnings(&mut self) -> Vec<LanguageCollision> {
        self.language_collisions.take_warnings()
    }

    pub fn active_git_extension_failures(&self) -> &BTreeMap<Arc<str>, GitExtensionFailure> {
        &self.git_extension_failures
    }

    pub fn take_git_extension_failures(&mut self) -> Vec<GitExtensionFailure> {
        mem::take(&mut self.undelivered_git_extension_failures)
            .into_iter()
            .filter_map(|extension_id| self.git_extension_failures.get(&extension_id).cloned())
            .collect()
    }

    fn clear_git_extension_failure(&mut self, extension_id: &str, cx: &mut Context<Self>) {
        self.undelivered_git_extension_failures.remove(extension_id);
        if self.git_extension_failures.remove(extension_id).is_some() {
            cx.notify();
        }
    }

    fn record_git_extension_result(
        &mut self,
        extension_id: &Arc<str>,
        url: &str,
        result: &Result<()>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(()) => {
                self.clear_git_extension_failure(extension_id, cx);
            }
            Err(error) => {
                let url = redact_command(url);
                let message = redact_command(&format!("{error:#}"));
                log::error!("failed to install extension {extension_id} from {url}: {message}");
                self.git_extension_failures.insert(
                    extension_id.clone(),
                    GitExtensionFailure {
                        extension_id: extension_id.clone(),
                        url,
                        message,
                    },
                );
                self.undelivered_git_extension_failures
                    .insert(extension_id.clone());
                cx.notify();
            }
        }
    }

    pub fn try_global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalExtensionStore>()
            .map(|store| store.0.clone())
    }

    pub fn global(cx: &App) -> Entity<Self> {
        cx.global::<GlobalExtensionStore>().0.clone()
    }

    pub fn new(
        extensions_dir: PathBuf,
        build_dir: Option<PathBuf>,
        extension_host_proxy: Arc<ExtensionHostProxy>,
        fs: Arc<dyn Fs>,
        http_client: Arc<HttpClientWithUrl>,
        builder_client: Arc<dyn HttpClient>,
        telemetry: Option<Arc<Telemetry>>,
        node_runtime: NodeRuntime,
        cx: &mut Context<Self>,
    ) -> Self {
        let work_dir = extensions_dir.join("work");
        let build_dir = build_dir.unwrap_or_else(|| extensions_dir.join("build"));
        let installed_dir = extensions_dir.join("installed");
        let staging_dir = extensions_dir.join("staging");
        let sources_dir = extensions_dir.join("sources");
        let index_path = extensions_dir.join("index.json");

        let (reload_tx, mut reload_rx) = unbounded();
        let mut this = Self {
            proxy: extension_host_proxy.clone(),
            extension_index: Default::default(),
            language_collisions: Default::default(),
            registered_languages: Default::default(),
            installed_dir,
            staging_dir,
            sources_dir,
            git_source_lock: Arc::default(),
            git_extension_failures: BTreeMap::default(),
            undelivered_git_extension_failures: BTreeSet::default(),
            index_path,
            builder: Arc::new(ExtensionBuilder::new(builder_client, build_dir)),
            outstanding_operations: Default::default(),
            modified_extensions: Default::default(),
            reload_complete_senders: Vec::new(),
            wasm_host: WasmHost::new(
                fs.clone(),
                http_client.clone(),
                node_runtime,
                extension_host_proxy,
                work_dir,
                cx,
            ),
            wasm_extensions: Vec::new(),
            fs,
            http_client,
            telemetry,
            reload_tx,
            tasks: Vec::new(),

            remote_clients: HashMap::default(),
            initial_index_load: Task::ready(()).shared(),
        };

        cx.foreground_executor()
            .block_on(recover_git_source(
                &this.fs,
                &this.staging_dir,
                &this.sources_dir,
                &this.index_path,
                &this.installed_dir,
            ))
            .context("recovering an interrupted git extension install")
            .log_err();

        // The extensions store maintains an index file, which contains a complete
        // list of the installed extensions and the resources that they provide.
        // This index is loaded synchronously on startup.
        let (index_content, index_metadata, extensions_metadata) =
            cx.foreground_executor().block_on(async {
                futures::join!(
                    this.fs.load(&this.index_path),
                    this.fs.metadata(&this.index_path),
                    this.fs.metadata(&this.installed_dir),
                )
            });

        // Normally, there is no need to rebuild the index. But if the index file
        // is invalid or is out-of-date according to the filesystem mtimes, then
        // it must be asynchronously rebuilt.
        let mut extension_index = ExtensionIndex::default();
        let mut extension_index_needs_rebuild = true;
        if let Ok(index_content) = index_content
            && let Some(index) = serde_json::from_str(&index_content).log_err()
        {
            extension_index = index;
            if let (Ok(Some(index_metadata)), Ok(Some(extensions_metadata))) =
                (index_metadata, extensions_metadata)
                && index_metadata
                    .mtime
                    .bad_is_greater_than(extensions_metadata.mtime)
            {
                extension_index_needs_rebuild = false;
            }
        }

        // Immediately load all of the extensions in the initial manifest. If the
        // index needs to be rebuild, then enqueue
        let load_initial_extensions = this.extensions_updated(extension_index, cx);
        let mut reload_future = None;
        if extension_index_needs_rebuild {
            reload_future = Some(this.reload(None, cx));
        }

        let initial_index_load = cx
            .spawn(async move |_, _| {
                if let Some(future) = reload_future {
                    future.await;
                }
            })
            .shared();
        this.initial_index_load = initial_index_load.clone();

        cx.spawn(async move |this, cx| {
            initial_index_load.await;
            this.update(cx, |this, cx| this.auto_install_extensions(cx))
                .ok();
            this.update(cx, |this, cx| this.check_for_updates(cx)).ok();
        })
        .detach();

        // Perform all extension loading in a single task to ensure that we
        // never attempt to simultaneously load/unload extensions from multiple
        // parallel tasks.
        this.tasks.push(cx.spawn(async move |this, cx| {
            async move {
                load_initial_extensions.await;

                let mut index_changed = false;
                let mut debounce_timer = cx.background_spawn(futures::future::pending()).fuse();

                loop {
                    select_biased! {
                        _ = debounce_timer => {
                            if index_changed {
                                let index = this
                                    .update(cx, |this, cx| this.rebuild_extension_index(cx))?
                                    .await;
                                this.update(cx, |this, cx| this.extensions_updated(index, cx))?
                                    .await;
                                index_changed = false;
                            }
                        }
                        extension_id = reload_rx.next() => {
                            let Some(extension_id) = extension_id else { break; };
                            this.update(cx, |this, _cx| {
                                this.modified_extensions.extend(extension_id);
                            })?;
                            index_changed = true;
                            debounce_timer = cx.background_executor().timer(RELOAD_DEBOUNCE_DURATION).fuse()
                        }
                    }
                }

                anyhow::Ok(())
            }
            .map(drop)
            .await;
        }));

        // Watch the installed extensions directory for changes. Whenever changes are
        // detected, rebuild the extension index, and load/unload any extensions that
        // have been added, removed, or modified.
        this.tasks.push(cx.background_spawn({
            let fs = this.fs.clone();
            let reload_tx = this.reload_tx.clone();
            let installed_dir = this.installed_dir.clone();
            async move {
                fs.create_dir(&installed_dir).await.log_err();
                let (mut paths, _) = fs.watch(&installed_dir, FS_WATCH_LATENCY).await;
                while let Some(events) = paths.next().await {
                    for event in events {
                        let Ok(event_path) = event.path.strip_prefix(&installed_dir) else {
                            continue;
                        };

                        if let Some(path::Component::Normal(extension_dir_name)) =
                            event_path.components().next()
                            && let Some(extension_id) = extension_dir_name.to_str()
                        {
                            reload_tx.unbounded_send(Some(extension_id.into())).ok();
                        }
                    }
                }
            }
        }));

        this
    }

    pub fn reload(
        &mut self,
        modified_extension: Option<Arc<str>>,
        cx: &mut Context<Self>,
    ) -> impl Future<Output = ()> + use<> {
        let (tx, rx) = oneshot::channel();
        self.reload_complete_senders.push(tx);
        self.reload_tx
            .unbounded_send(modified_extension)
            .expect("reload task exited");
        cx.emit(Event::StartedReloading);

        async move {
            rx.await.ok();
        }
    }

    pub fn reload_all(&mut self, cx: &mut Context<Self>) -> impl Future<Output = ()> + use<> {
        self.modified_extensions
            .extend(self.extension_index.extensions.keys().cloned());
        self.reload(None, cx)
    }

    fn extensions_dir(&self) -> PathBuf {
        self.installed_dir.clone()
    }

    pub fn outstanding_operations(&self) -> &BTreeMap<Arc<str>, ExtensionOperation> {
        &self.outstanding_operations
    }

    pub fn installed_extensions(&self) -> &BTreeMap<Arc<str>, ExtensionIndexEntry> {
        &self.extension_index.extensions
    }

    /// Returns the status of the extension, where outstanding operations take
    /// precedence over the installed state.
    pub fn extension_status(&self, extension_id: &str) -> ExtensionStatus {
        match self.outstanding_operations.get(extension_id) {
            Some(ExtensionOperation::Install) => ExtensionStatus::Installing,
            Some(ExtensionOperation::Remove) => ExtensionStatus::Removing,
            Some(ExtensionOperation::Upgrade) => ExtensionStatus::Upgrading,
            None => match self.extension_index.extensions.get(extension_id) {
                Some(extension) => ExtensionStatus::Installed(extension.manifest.version.clone()),
                None => ExtensionStatus::NotInstalled,
            },
        }
    }

    pub fn is_dev_extension(&self, extension_id: &str) -> bool {
        self.extension_index
            .extensions
            .get(extension_id)
            .is_some_and(|extension| extension.dev)
    }

    pub fn dev_extensions(&self) -> impl Iterator<Item = &Arc<ExtensionManifest>> {
        self.extension_index
            .extensions
            .values()
            .filter_map(|extension| extension.dev.then_some(&extension.manifest))
    }

    pub fn extension_manifest_for_id(&self, extension_id: &str) -> Option<&Arc<ExtensionManifest>> {
        self.extension_index
            .extensions
            .get(extension_id)
            .map(|extension| &extension.manifest)
    }

    /// Returns the names of themes provided by extensions.
    pub fn extension_themes<'a>(
        &'a self,
        extension_id: &'a str,
    ) -> impl Iterator<Item = &'a Arc<str>> {
        self.extension_index
            .themes
            .iter()
            .filter_map(|(name, theme)| theme.extension.as_ref().eq(extension_id).then_some(name))
    }

    /// Returns the path to the theme file within an extension, if there is an
    /// extension that provides the theme.
    pub fn path_to_extension_theme(&self, theme_name: &str) -> Option<PathBuf> {
        let entry = self.extension_index.themes.get(theme_name)?;

        Some(
            self.extensions_dir()
                .join(entry.extension.as_ref())
                .join(&entry.path),
        )
    }

    /// Returns the names of icon themes provided by extensions.
    pub fn extension_icon_themes<'a>(
        &'a self,
        extension_id: &'a str,
    ) -> impl Iterator<Item = &'a Arc<str>> {
        self.extension_index
            .icon_themes
            .iter()
            .filter_map(|(name, icon_theme)| {
                icon_theme
                    .extension
                    .as_ref()
                    .eq(extension_id)
                    .then_some(name)
            })
    }

    /// Returns the path to the icon theme file within an extension, if there is
    /// an extension that provides the icon theme.
    pub fn path_to_extension_icon_theme(
        &self,
        icon_theme_name: &str,
    ) -> Option<(PathBuf, PathBuf)> {
        let entry = self.extension_index.icon_themes.get(icon_theme_name)?;

        let icon_theme_path = self
            .extensions_dir()
            .join(entry.extension.as_ref())
            .join(&entry.path);
        let icons_root_path = self.extensions_dir().join(entry.extension.as_ref());

        Some((icon_theme_path, icons_root_path))
    }

    pub fn fetch_extensions(
        &self,
        search: Option<&str>,
        provides_filter: Option<&BTreeSet<ExtensionProvides>>,
        cx: &mut Context<Self>,
    ) -> Task<Result<Vec<ExtensionMetadata>>> {
        let version = SchemaVersion::CURRENT.to_string();
        let mut query = vec![("max_schema_version", version.as_str())];
        if let Some(search) = search {
            query.push(("filter", search));
        }

        let provides_filter = provides_filter.map(|provides_filter| {
            provides_filter
                .iter()
                .map(|provides| provides.to_string())
                .collect::<Vec<_>>()
                .join(",")
        });
        if let Some(provides_filter) = provides_filter.as_deref() {
            query.push(("provides", provides_filter));
        }

        self.fetch_extensions_from_api("/extensions", &query, cx)
    }

    pub fn fetch_extensions_with_update_available(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Task<Result<Vec<ExtensionMetadata>>> {
        let schema_versions = schema_version_range();
        let wasm_api_versions = wasm_api_version_range(ReleaseChannel::global(cx));
        let extension_settings = ExtensionSettings::get_global(cx);
        let extension_ids = self
            .extension_index
            .extensions
            .iter()
            .filter(|(id, entry)| !entry.dev && extension_settings.should_auto_update(id))
            .map(|(id, _)| id.as_ref())
            .collect::<Vec<_>>()
            .join(",");
        let task = self.fetch_extensions_from_api(
            "/extensions/updates",
            &[
                ("min_schema_version", &schema_versions.start().to_string()),
                ("max_schema_version", &schema_versions.end().to_string()),
                (
                    "min_wasm_api_version",
                    &wasm_api_versions.start().to_string(),
                ),
                ("max_wasm_api_version", &wasm_api_versions.end().to_string()),
                ("ids", &extension_ids),
            ],
            cx,
        );
        cx.spawn(async move |this, cx| {
            let extensions = task.await?;
            this.update(cx, |this, _cx| {
                extensions
                    .into_iter()
                    .filter(|extension| {
                        this.extension_index
                            .extensions
                            .get(&extension.id)
                            .is_none_or(|installed_extension| {
                                installed_extension.manifest.version != extension.manifest.version
                            })
                    })
                    .collect()
            })
        })
    }

    pub fn fetch_extension_versions(
        &self,
        extension_id: &str,
        cx: &mut Context<Self>,
    ) -> Task<Result<Vec<ExtensionMetadata>>> {
        self.fetch_extensions_from_api(&format!("/extensions/{extension_id}"), &[], cx)
    }

    /// Installs any extensions that should be included with Zed by default.
    ///
    /// This can be used to make certain functionality provided by extensions
    /// available out-of-the-box.
    pub fn auto_install_extensions(&mut self, cx: &mut Context<Self>) {
        if cfg!(test) {
            return;
        }

        let extension_settings = ExtensionSettings::get_global(cx);

        let git_extensions_to_install = self.git_extensions_to_install(extension_settings);
        let extensions_to_install = self.marketplace_extensions_to_install(extension_settings);

        self.install_marketplace_extensions(extensions_to_install, cx)
            .detach();
        self.install_git_extensions(git_extensions_to_install, cx)
            .detach();
    }

    pub fn marketplace_extensions_to_install(
        &self,
        extension_settings: &ExtensionSettings,
    ) -> Vec<Arc<str>> {
        extension_settings
            .marketplace_extensions()
            .filter(|extension_id| extension_settings.should_auto_install(extension_id))
            .filter(|extension_id| {
                let installed_from_marketplace = self
                    .extension_index
                    .extensions
                    .get(extension_id.as_ref())
                    .is_some_and(|entry| entry.source.is_none());
                !installed_from_marketplace
                    && !SUPPRESSED_EXTENSIONS.contains(extension_id.as_ref())
            })
            .cloned()
            .collect()
    }

    pub fn install_marketplace_extensions(
        &mut self,
        extension_ids: Vec<Arc<str>>,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        let installs = extension_ids
            .into_iter()
            .map(|extension_id| {
                let install = self.install_latest_extension_task(extension_id.clone(), cx);
                (extension_id, install)
            })
            .collect::<Vec<_>>();
        let fs = self.fs.clone();
        let sources_dir = self.sources_dir.clone();
        let git_source_lock = self.git_source_lock.clone();

        cx.spawn(async move |this, cx| {
            join_all(installs.into_iter().map(|(extension_id, install)| {
                let this = this.clone();
                let fs = fs.clone();
                let sources_dir = sources_dir.clone();
                let git_source_lock = git_source_lock.clone();
                let mut cx = cx.clone();
                async move {
                    if let Err(error) = install.await {
                        log::error!("failed to install extension {extension_id}: {error:#}");
                        return;
                    }
                    let _source_guard = git_source_lock.lock().await;
                    let installed_from_marketplace = this
                        .read_with(&cx, |this, _| {
                            this.extension_index
                                .extensions
                                .get(extension_id.as_ref())
                                .is_some_and(|entry| !entry.dev)
                        })
                        .unwrap_or(false);
                    if installed_from_marketplace
                        && remove_git_source(&fs, &sources_dir, &extension_id)
                            .await
                            .with_context(|| {
                                format!("removing the git source of extension {extension_id}")
                            })
                            .log_err()
                            .is_some()
                    {
                        this.update(&mut cx, |this, cx| {
                            this.clear_git_extension_failure(&extension_id, cx);
                        })
                        .log_err();
                    }
                }
            }))
            .await;
        })
    }

    pub fn install_git_extensions(
        &mut self,
        extensions: Vec<(Arc<str>, GitExtensionSource)>,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        cx.spawn(async move |this, cx| {
            for (extension_id, source) in extensions {
                let url = source.url.clone();
                let Ok(install) = this.update(cx, |this, cx| {
                    this.install_git_extension(extension_id.clone(), source, cx)
                }) else {
                    break;
                };
                let result = install.await;
                if this
                    .update(cx, |this, cx| {
                        this.record_git_extension_result(&extension_id, &url, &result, cx)
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
    }

    pub fn git_extensions_to_install(
        &self,
        extension_settings: &ExtensionSettings,
    ) -> Vec<(Arc<str>, GitExtensionSource)> {
        extension_settings
            .git_sources()
            .filter(|(extension_id, source)| {
                self.extension_index
                    .extensions
                    .get(extension_id.as_ref())
                    .and_then(|entry| entry.source.as_ref())
                    .is_none_or(|installed| !installed.matches(source))
            })
            .map(|(extension_id, source)| (extension_id.clone(), source))
            .collect()
    }

    pub fn check_for_updates(&mut self, cx: &mut Context<Self>) {
        let task = self.fetch_extensions_with_update_available(cx);
        cx.spawn(async move |this, cx| Self::upgrade_extensions(this, task.await?, cx).await)
            .detach();
        self.check_for_git_updates(cx).detach_and_log_err(cx);
    }

    pub fn check_for_git_updates(&mut self, cx: &mut Context<Self>) -> Task<Result<()>> {
        let extension_settings = ExtensionSettings::get_global(cx);
        let extensions = self
            .extension_index
            .extensions
            .iter()
            .filter(|(extension_id, _)| extension_settings.should_auto_update(extension_id))
            .filter_map(|(extension_id, entry)| entry.source.as_ref().map(|_| extension_id.clone()))
            .collect::<Vec<_>>();
        let fs = self.fs.clone();
        let builder = self.builder.clone();
        let staging_dir = self.staging_dir.clone();
        let sources_dir = self.sources_dir.clone();
        let installed_dir = self.installed_dir.clone();
        let index_path = self.index_path.clone();
        let proxy = self.proxy.clone();
        let wasm_host = self.wasm_host.clone();
        let git_source_lock = self.git_source_lock.clone();

        cx.spawn(async move |this, cx| {
            for extension_id in extensions {
                let _source_guard = git_source_lock.lock().await;
                let Some(indexed_url) = this.read_with(cx, |this, cx| {
                    let configured_source = ExtensionSettings::get_global(cx)
                        .git_sources()
                        .find(|(id, _)| *id == &extension_id)
                        .map(|(_, source)| source);
                    this.extension_index
                        .extensions
                        .get(&extension_id)
                        .and_then(|entry| entry.source.as_ref())
                        .filter(|installed| {
                            configured_source
                                .as_ref()
                                .is_none_or(|source| installed.matches(source))
                        })
                        .map(|source| source.url.clone())
                })?
                else {
                    continue;
                };
                let _operation = match Self::begin_operation(
                    &this,
                    &extension_id,
                    ExtensionOperation::Upgrade,
                    cx,
                ) {
                    Ok(operation) => operation,
                    Err(error) => {
                        log::error!("skipping git update of extension {extension_id}: {error:#}");
                        continue;
                    }
                };
                let update = cx
                    .background_spawn({
                        let fs = fs.clone();
                        let builder = builder.clone();
                        let staging_dir = staging_dir.clone();
                        let sources_dir = sources_dir.clone();
                        let installed_dir = installed_dir.clone();
                        let index_path = index_path.clone();
                        let proxy = proxy.clone();
                        let wasm_host = wasm_host.clone();
                        let extension_id = extension_id.clone();
                        async move {
                            let Some(installed) =
                                load_git_source_record(&fs, &sources_dir, &extension_id).await
                            else {
                                return GitUpdate::RecordMissing;
                            };
                            let source = installed.source();
                            let result = async {
                                let checkout_intact = async {
                                    let extension_path = git_source_extension_path(
                                        &sources_dir,
                                        &extension_id,
                                        source.path.as_deref(),
                                    );
                                    let mut index = ExtensionIndex::default();
                                    ExtensionStore::add_extension_to_index(
                                        fs.clone(),
                                        extension_path.clone(),
                                        &sources_dir,
                                        &mut index,
                                        proxy.clone(),
                                    )
                                    .await?;
                                    let entry = index.extensions.get(&extension_id).context(
                                        "the checkout no longer declares the installed extension",
                                    )?;
                                    validate_git_compiled_artifacts(
                                        &fs,
                                        &extension_path,
                                        &entry.manifest,
                                        &wasm_host,
                                    )
                                    .await
                                }
                                .await;
                                if checkout_intact.is_ok() {
                                    match remote_commit(&sources_dir, &source).await? {
                                        Some(remote_commit)
                                            if remote_commit == installed.commit =>
                                        {
                                            return anyhow::Ok(false);
                                        }
                                        Some(_) => {}
                                        None => {
                                            let reference =
                                                source.reference.as_deref().unwrap_or("HEAD");
                                            anyhow::ensure!(
                                                is_commit_sha(reference),
                                                "the repository no longer advertises {reference}"
                                            );
                                            return Ok(false);
                                        }
                                    }
                                }
                                build_git_source(
                                    fs,
                                    &builder,
                                    &staging_dir,
                                    &sources_dir,
                                    &index_path,
                                    &installed_dir,
                                    wasm_host,
                                    &extension_id,
                                    &source,
                                )
                                .await
                                .with_context(|| {
                                    match checkout_intact {
                                        Ok(()) => "updating git extension".to_string(),
                                        Err(error) => format!(
                                            "repairing invalid git extension artifacts: {error:#}"
                                        ),
                                    }
                                })?;
                                Ok(true)
                            }
                            .await;
                            GitUpdate::Checked {
                                url: source.url,
                                result,
                            }
                        }
                    })
                    .await;
                let (url, result) = match update {
                    GitUpdate::Checked { url, result } => (url, result),
                    GitUpdate::RecordMissing => {
                        let error = anyhow!(
                            "the source record of extension {extension_id} is missing or unreadable"
                        );
                        this.update(cx, |this, cx| {
                            this.record_git_extension_result(
                                &extension_id,
                                &indexed_url,
                                &Err(error),
                                cx,
                            )
                        })?;
                        this.update(cx, |this, cx| this.reload(Some(extension_id.clone()), cx))?
                            .await;
                        this.update(cx, |this, cx| this.auto_install_extensions(cx))?;
                        continue;
                    }
                };
                let updated = matches!(result, Ok(true));
                this.update(cx, |this, cx| {
                    this.record_git_extension_result(&extension_id, &url, &result.map(|_| ()), cx)
                })?;
                if updated {
                    this.update(cx, |this, cx| this.reload(Some(extension_id.clone()), cx))?
                        .await;
                }
            }
            Ok(())
        })
    }

    pub fn install_git_extension(
        &mut self,
        extension_id: Arc<str>,
        source: GitExtensionSource,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        if let Err(error) = validate_git_install(&extension_id, &source) {
            return Task::ready(Err(error));
        }
        let fs = self.fs.clone();
        let builder = self.builder.clone();
        let staging_dir = self.staging_dir.clone();
        let sources_dir = self.sources_dir.clone();
        let index_path = self.index_path.clone();
        let wasm_host = self.wasm_host.clone();
        let extensions_dir = self.extensions_dir();
        let git_source_lock = self.git_source_lock.clone();

        cx.spawn(async move |this, cx| {
            let _source_guard = git_source_lock.lock().await;
            let _operation =
                Self::begin_operation(&this, &extension_id, ExtensionOperation::Install, cx)?;
            let result = cx
                .background_spawn({
                    let fs = fs.clone();
                    let extension_id = extension_id.clone();
                    let extensions_dir = extensions_dir.clone();
                    async move {
                        build_git_source(
                            fs,
                            &builder,
                            &staging_dir,
                            &sources_dir,
                            &index_path,
                            &extensions_dir,
                            wasm_host,
                            &extension_id,
                            &source,
                        )
                        .await
                    }
                })
                .await;
            if let Err(error) = result {
                this.update(cx, |this, cx| this.reload(None, cx))?.await;
                return Err(error);
            }
            Self::finish_dev_extension_install(&this, &extension_id, cx).await
        })
    }

    fn begin_operation(
        this: &WeakEntity<Self>,
        extension_id: &Arc<str>,
        operation: ExtensionOperation,
        cx: &mut AsyncApp,
    ) -> Result<util::Deferred<impl FnOnce() + use<>>> {
        let claimed = this.update(cx, |this, cx| {
            match this.outstanding_operations.entry(extension_id.clone()) {
                btree_map::Entry::Occupied(_) => return false,
                btree_map::Entry::Vacant(entry) => entry.insert(operation),
            };
            cx.notify();
            true
        })?;
        anyhow::ensure!(
            claimed,
            "extension {extension_id} has another operation in progress"
        );
        Ok(cx.on_drop(this, {
            let extension_id = extension_id.clone();
            move |this, cx| {
                this.outstanding_operations.remove(extension_id.as_ref());
                cx.notify();
            }
        }))
    }

    async fn upgrade_extensions(
        this: WeakEntity<Self>,
        extensions: Vec<ExtensionMetadata>,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        for extension in extensions {
            let task = this.update(cx, |this, cx| {
                if let Some(installed_extension) =
                    this.extension_index.extensions.get(&extension.id)
                {
                    let installed_version =
                        Version::from_str(&installed_extension.manifest.version).ok()?;
                    let latest_version = Version::from_str(&extension.manifest.version).ok()?;

                    if installed_version >= latest_version {
                        return None;
                    }
                }

                Some(this.upgrade_extension(extension.id, extension.manifest.version, cx))
            })?;

            if let Some(task) = task {
                task.await.log_err();
            }
        }
        anyhow::Ok(())
    }

    fn fetch_extensions_from_api(
        &self,
        path: &str,
        query: &[(&str, &str)],
        cx: &mut Context<ExtensionStore>,
    ) -> Task<Result<Vec<ExtensionMetadata>>> {
        let url = self.http_client.build_zed_api_url(path, query);
        let http_client = self.http_client.clone();
        cx.spawn(async move |_, _| {
            let mut response = http_client
                .get(url?.as_ref(), AsyncBody::empty(), true)
                .await?;

            let mut body = Vec::new();
            response
                .body_mut()
                .read_to_end(&mut body)
                .await
                .context("error reading extensions")?;

            if response.status().is_client_error() {
                let text = String::from_utf8_lossy(body.as_slice());
                bail!(
                    "status error {}, response: {text:?}",
                    response.status().as_u16()
                );
            }

            let mut response: GetExtensionsResponse = serde_json::from_slice(&body)?;

            response
                .data
                .retain(|extension| !SUPPRESSED_EXTENSIONS.contains(extension.id.as_ref()));

            Ok(response.data)
        })
    }

    pub fn install_extension(
        &mut self,
        extension_id: Arc<str>,
        version: Arc<str>,
        cx: &mut Context<Self>,
    ) {
        self.install_or_upgrade_extension(extension_id, version, ExtensionOperation::Install, cx)
            .detach_and_log_err(cx);
    }

    fn install_or_upgrade_extension_at_endpoint(
        &mut self,
        extension_id: Arc<str>,
        url: Url,
        operation: ExtensionOperation,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let extension_dir = self.installed_dir.join(extension_id.as_ref());
        let staging_dir = self.staging_dir.clone();
        let http_client = self.http_client.clone();
        let fs = self.fs.clone();

        match self.outstanding_operations.entry(extension_id.clone()) {
            btree_map::Entry::Occupied(_) => return Task::ready(Ok(())),
            btree_map::Entry::Vacant(e) => e.insert(operation),
        };
        cx.notify();

        cx.spawn(async move |this, cx| {
            let _finish = cx.on_drop(&this, {
                let extension_id = extension_id.clone();
                move |this, cx| {
                    this.outstanding_operations.remove(extension_id.as_ref());
                    cx.notify();
                }
            });

            cx.background_spawn(async move {
                let mut response = http_client
                    .get(url.as_ref(), Default::default(), true)
                    .await
                    .context("downloading extension")?;

                let content_length = response
                    .headers()
                    .get(http_client::http::header::CONTENT_LENGTH)
                    .and_then(|value| value.to_str().ok()?.parse::<usize>().ok());

                let mut body = BufReader::new(response.body_mut());
                let mut tar_gz_bytes = Vec::new();
                body.read_to_end(&mut tar_gz_bytes).await?;

                if let Some(content_length) = content_length {
                    let actual_len = tar_gz_bytes.len();
                    if content_length != actual_len {
                        bail!(
                            "downloaded extension size {actual_len} \
                        does not match content length {content_length}"
                        );
                    }
                }

                let decompressed_bytes = GzipDecoder::new(BufReader::new(tar_gz_bytes.as_slice()));
                let archive = Archive::new(decompressed_bytes);

                let remove_dir = || {
                    fs.remove_dir(
                        &extension_dir,
                        RemoveOptions {
                            recursive: true,
                            ignore_if_not_exists: true,
                        },
                    )
                };

                let temp_dir = fs
                    .create_dir(&staging_dir)
                    .await
                    .and_then(|()| tempfile::tempdir_in(&staging_dir).map_err(Into::into));

                match temp_dir {
                    Ok(temp_dir) => {
                        archive.unpack(temp_dir.path()).await?;
                        remove_dir().await?;
                        fs.rename(
                            temp_dir.path(),
                            &extension_dir,
                            RenameOptions {
                                overwrite: true,
                                ignore_if_exists: true,
                                create_parents: true,
                            },
                        )
                        .await
                    }
                    Err(_) => {
                        remove_dir().await?;
                        archive.unpack(extension_dir).await.map_err(Into::into)
                    }
                }
            })
            .await?;

            this.update(cx, |this, cx| this.reload(Some(extension_id.clone()), cx))?
                .await;

            if let ExtensionOperation::Install = operation {
                this.update(cx, |this, cx| {
                    cx.emit(Event::ExtensionInstalled(extension_id.clone()));
                    if let Some(events) = ExtensionEvents::try_global(cx)
                        && let Some(manifest) = this.extension_manifest_for_id(&extension_id)
                    {
                        events.update(cx, |this, cx| {
                            this.emit(extension::Event::ExtensionInstalled(manifest.clone()), cx)
                        });
                    }
                })
                .ok();
            }

            anyhow::Ok(())
        })
    }

    pub fn install_latest_extension(&mut self, extension_id: Arc<str>, cx: &mut Context<Self>) {
        self.install_latest_extension_task(extension_id, cx)
            .detach_and_log_err(cx);
    }

    fn install_latest_extension_task(
        &mut self,
        extension_id: Arc<str>,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        log::info!("installing extension {extension_id} latest version");

        let schema_versions = schema_version_range();
        let wasm_api_versions = wasm_api_version_range(ReleaseChannel::global(cx));

        let url = match self.http_client.build_zed_api_url(
            &format!("/extensions/{extension_id}/download"),
            &[
                ("min_schema_version", &schema_versions.start().to_string()),
                ("max_schema_version", &schema_versions.end().to_string()),
                (
                    "min_wasm_api_version",
                    &wasm_api_versions.start().to_string(),
                ),
                ("max_wasm_api_version", &wasm_api_versions.end().to_string()),
            ],
        ) {
            Ok(url) => url,
            Err(error) => return Task::ready(Err(error)),
        };

        self.install_or_upgrade_extension_at_endpoint(
            extension_id,
            url,
            ExtensionOperation::Install,
            cx,
        )
    }

    pub fn upgrade_extension(
        &mut self,
        extension_id: Arc<str>,
        version: Arc<str>,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.install_or_upgrade_extension(extension_id, version, ExtensionOperation::Upgrade, cx)
    }

    fn install_or_upgrade_extension(
        &mut self,
        extension_id: Arc<str>,
        version: Arc<str>,
        operation: ExtensionOperation,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        log::info!("installing extension {extension_id} {version}");
        let Some(url) = self
            .http_client
            .build_zed_api_url(
                &format!("/extensions/{extension_id}/{version}/download"),
                &[],
            )
            .log_err()
        else {
            return Task::ready(Ok(()));
        };

        self.install_or_upgrade_extension_at_endpoint(extension_id, url, operation, cx)
    }

    pub fn uninstall_extension(
        &mut self,
        extension_id: Arc<str>,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let extension_dir = self.installed_dir.join(extension_id.as_ref());
        let work_dir = self.wasm_host.work_dir.join(extension_id.as_ref());
        let sources_dir = self.sources_dir.clone();
        let fs = self.fs.clone();

        let extension_manifest = self.extension_manifest_for_id(&extension_id).cloned();
        let has_git_source = self
            .extension_index
            .extensions
            .get(&extension_id)
            .is_some_and(|entry| entry.source.is_some());

        match self.outstanding_operations.entry(extension_id.clone()) {
            btree_map::Entry::Occupied(_) => return Task::ready(Ok(())),
            btree_map::Entry::Vacant(e) => e.insert(ExtensionOperation::Remove),
        };

        cx.spawn(async move |extension_store, cx| {
            let _finish = cx.on_drop(&extension_store, {
                let extension_id = extension_id.clone();
                move |this, cx| {
                    this.outstanding_operations.remove(extension_id.as_ref());
                    cx.notify();
                }
            });

            fs.remove_dir(
                &extension_dir,
                RemoveOptions {
                    recursive: true,
                    ignore_if_not_exists: true,
                },
            )
            .await
            .with_context(|| format!("Removing extension dir {extension_dir:?}"))?;
            if has_git_source {
                remove_git_source(&fs, &sources_dir, &extension_id).await?;
            }

            extension_store.update(cx, |extension_store, cx| {
                extension_store.clear_git_extension_failure(&extension_id, cx);
            })?;

            extension_store
                .update(cx, |extension_store, cx| extension_store.reload(None, cx))?
                .await;

            // There's a race between wasm extension fully stopping and the directory removal.
            // On Windows, it's impossible to remove a directory that has a process running in it.
            for i in 0..3 {
                cx.background_executor()
                    .timer(Duration::from_millis(i * 100))
                    .await;
                let removal_result = fs
                    .remove_dir(
                        &work_dir,
                        RemoveOptions {
                            recursive: true,
                            ignore_if_not_exists: true,
                        },
                    )
                    .await;
                match removal_result {
                    Ok(()) => break,
                    Err(e) => {
                        if i == 2 {
                            log::error!("Failed to remove extension work dir {work_dir:?} : {e}");
                        }
                    }
                }
            }

            extension_store.update(cx, |_, cx| {
                cx.emit(Event::ExtensionUninstalled(extension_id.clone()));
                if let Some(events) = ExtensionEvents::try_global(cx)
                    && let Some(manifest) = extension_manifest
                {
                    events.update(cx, |this, cx| {
                        this.emit(extension::Event::ExtensionUninstalled(manifest.clone()), cx)
                    });
                }
            })?;

            anyhow::Ok(())
        })
    }

    pub fn install_dev_extension(
        &mut self,
        extension_source_path: PathBuf,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let extensions_dir = self.extensions_dir();
        let sources_dir = self.sources_dir.clone();
        let fs = self.fs.clone();
        let builder = self.builder.clone();

        cx.spawn(async move |this, cx| {
            let mut extension_manifest =
                ExtensionManifest::load(fs.clone(), &extension_source_path).await?;
            let extension_id = extension_manifest.id.clone();

            Self::uninstall_unless_dev(&this, &extension_id, cx).await;

            if !this.update(cx, |this, cx| {
                match this.outstanding_operations.entry(extension_id.clone()) {
                    btree_map::Entry::Occupied(_) => return false,
                    btree_map::Entry::Vacant(e) => e.insert(ExtensionOperation::Install),
                };
                cx.notify();
                true
            })? {
                return Ok(());
            }

            let _finish = cx.on_drop(&this, {
                let extension_id = extension_id.clone();
                move |this, cx| {
                    this.outstanding_operations.remove(extension_id.as_ref());
                    cx.notify();
                }
            });

            cx.background_spawn({
                let extension_source_path = extension_source_path.clone();
                let fs = fs.clone();
                async move {
                    builder
                        .compile_extension(
                            &extension_source_path,
                            &mut extension_manifest,
                            CompileExtensionOptions::dev(),
                            fs,
                        )
                        .await
                }
            })
            .await
            .inspect_err(|error| {
                util::log_err(error);
            })?;

            if is_single_path_component(&extension_id)
                && !extension_source_path.starts_with(git_source_dir(&sources_dir, &extension_id))
            {
                remove_git_source(&fs, &sources_dir, &extension_id).await?;
            }

            Self::link_dev_extension(
                &this,
                &fs,
                &extensions_dir,
                &extension_id,
                extension_source_path,
                cx,
            )
            .await?;
            this.update(cx, |this, cx| {
                this.clear_git_extension_failure(&extension_id, cx);
            })?;
            Ok(())
        })
    }

    async fn uninstall_unless_dev(
        this: &WeakEntity<Self>,
        extension_id: &Arc<str>,
        cx: &mut AsyncApp,
    ) {
        if let Some(uninstall_task) = this
            .update(cx, |this, cx| {
                this.extension_index
                    .extensions
                    .get(extension_id.as_ref())
                    .is_some_and(|index_entry| !index_entry.dev)
                    .then(|| this.uninstall_extension(extension_id.clone(), cx))
            })
            .ok()
            .flatten()
        {
            uninstall_task.await.log_err();
        }
    }

    async fn link_dev_extension(
        this: &WeakEntity<Self>,
        fs: &Arc<dyn Fs>,
        extensions_dir: &Path,
        extension_id: &Arc<str>,
        extension_source_path: PathBuf,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        let output_path = &extensions_dir.join(extension_id.as_ref());
        if let Some(metadata) = fs.metadata(output_path).await? {
            if metadata.is_symlink {
                fs.remove_file(
                    output_path,
                    RemoveOptions {
                        recursive: false,
                        ignore_if_not_exists: true,
                    },
                )
                .await?;
            } else {
                bail!("extension {extension_id} is still installed");
            }
        }

        fs.create_symlink(output_path, extension_source_path)
            .await?;

        Self::finish_dev_extension_install(this, extension_id, cx).await
    }

    async fn finish_dev_extension_install(
        this: &WeakEntity<Self>,
        extension_id: &Arc<str>,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        this.update(cx, |this, cx| this.reload(None, cx))?.await;
        this.update(cx, |this, cx| {
            cx.emit(Event::ExtensionInstalled(extension_id.clone()));
            if let Some(events) = ExtensionEvents::try_global(cx)
                && let Some(manifest) = this.extension_manifest_for_id(extension_id)
            {
                events.update(cx, |this, cx| {
                    this.emit(extension::Event::ExtensionInstalled(manifest.clone()), cx)
                });
            }
        })?;

        Ok(())
    }

    pub fn rebuild_dev_extension(&mut self, extension_id: Arc<str>, cx: &mut Context<Self>) {
        let path = self.installed_dir.join(extension_id.as_ref());
        let builder = self.builder.clone();
        let fs = self.fs.clone();

        match self.outstanding_operations.entry(extension_id.clone()) {
            btree_map::Entry::Occupied(_) => return,
            btree_map::Entry::Vacant(e) => e.insert(ExtensionOperation::Upgrade),
        };

        cx.notify();
        let compile = cx.background_spawn(async move {
            let mut manifest = ExtensionManifest::load(fs.clone(), &path).await?;
            builder
                .compile_extension(&path, &mut manifest, CompileExtensionOptions::dev(), fs)
                .await
        });

        cx.spawn(async move |this, cx| {
            let result = compile.await;

            this.update(cx, |this, cx| {
                this.outstanding_operations.remove(&extension_id);
                cx.notify();
            })?;

            if result.is_ok() {
                this.update(cx, |this, cx| this.reload(Some(extension_id), cx))?
                    .await;
            }

            result
        })
        .detach_and_log_err(cx)
    }

    /// Updates the set of installed extensions.
    ///
    /// First, this unloads any themes, languages, or grammars that are
    /// no longer in the manifest, or whose files have changed on disk.
    /// Then it loads any themes, languages, or grammars that are newly
    /// added to the manifest, or whose files have changed on disk.
    #[ztracing::instrument(skip_all)]
    fn extensions_updated(
        &mut self,
        mut new_index: ExtensionIndex,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        let old_index = &self.extension_index;

        let suppressed_extensions_to_remove = new_index
            .extensions
            .extract_if(.., |extension_id, _| {
                SUPPRESSED_EXTENSIONS.contains(extension_id.as_ref())
            })
            .collect::<Vec<_>>();

        // Determine which extensions need to be loaded and unloaded, based
        // on the changes to the manifest and the extensions that we know have been
        // modified.
        let mut extensions_to_unload = Vec::default();
        let mut extensions_to_load = Vec::default();
        {
            let mut old_keys = old_index.extensions.iter().peekable();
            let mut new_keys = new_index.extensions.iter().peekable();
            loop {
                match (old_keys.peek(), new_keys.peek()) {
                    (None, None) => break,
                    (None, Some(_)) => {
                        extensions_to_load.push(new_keys.next().unwrap().0.clone());
                    }
                    (Some(_), None) => {
                        extensions_to_unload.push(old_keys.next().unwrap().0.clone());
                    }
                    (Some((old_key, _)), Some((new_key, _))) => match old_key.cmp(new_key) {
                        Ordering::Equal => {
                            let (old_key, old_value) = old_keys.next().unwrap();
                            let (new_key, new_value) = new_keys.next().unwrap();
                            if old_value != new_value || self.modified_extensions.contains(old_key)
                            {
                                extensions_to_unload.push(old_key.clone());
                                extensions_to_load.push(new_key.clone());
                            }
                        }
                        Ordering::Less => {
                            extensions_to_unload.push(old_keys.next().unwrap().0.clone());
                        }
                        Ordering::Greater => {
                            extensions_to_load.push(new_keys.next().unwrap().0.clone());
                        }
                    },
                }
            }
            self.modified_extensions.clear();
        }

        let trigger_suppressed_extension_removal =
            move |this: &mut ExtensionStore, cx: &mut Context<ExtensionStore>| {
                for (id, _) in suppressed_extensions_to_remove {
                    this.uninstall_extension(id, cx).detach_and_log_err(cx);
                }
            };

        if extensions_to_load.is_empty() && extensions_to_unload.is_empty() {
            self.language_collisions
                .update(&new_index, &self.registered_languages, &self.proxy);
            self.extension_index.language_providers = new_index.language_providers;
            cx.notify();
            self.reload_complete_senders.clear();
            trigger_suppressed_extension_removal(self, cx);
            return Task::ready(());
        }

        let reload_count = extensions_to_unload
            .iter()
            .filter(|id| extensions_to_load.contains(id))
            .count();

        log::info!(
            "extensions updated. loading {}, reloading {}, unloading {}",
            extensions_to_load.len() - reload_count,
            reload_count,
            extensions_to_unload.len() - reload_count
        );

        let old_remote_sync_extensions = old_index.extensions_to_sync_to_remote();
        let new_remote_sync_extensions = new_index.extensions_to_sync_to_remote();
        let remote_sync_changed = extensions_to_unload
            .iter()
            .any(|id| old_remote_sync_extensions.contains(id.as_ref()))
            || extensions_to_load
                .iter()
                .any(|id| new_remote_sync_extensions.contains(id.as_ref()));

        let extension_ids = extensions_to_load
            .iter()
            .filter_map(|id| {
                Some((
                    id.clone(),
                    new_index.extensions.get(id)?.manifest.version.clone(),
                ))
            })
            .collect::<Vec<_>>();

        telemetry::event!("Extensions Loaded", id_and_versions = extension_ids);

        let themes_to_remove = old_index
            .themes
            .iter()
            .filter_map(|(name, entry)| {
                if extensions_to_unload.contains(&entry.extension) {
                    Some(name.clone().into())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        let icon_themes_to_remove = old_index
            .icon_themes
            .iter()
            .filter_map(|(name, entry)| {
                if extensions_to_unload.contains(&entry.extension) {
                    Some(name.clone().into())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        let mut languages_to_remove = Vec::new();
        let mut languages_to_readd = Vec::new();
        for (name, entry) in &old_index.languages {
            if !extensions_to_unload.contains(&entry.extension) {
                continue;
            }
            match new_index.languages.get(name) {
                Some(new_entry) if !extensions_to_load.contains(&new_entry.extension) => {
                    languages_to_readd.push((name.clone(), new_entry.clone()));
                }
                _ => languages_to_remove.push(name.clone()),
            }
        }
        let mut server_removal_tasks = Vec::with_capacity(extensions_to_unload.len());
        for extension_id in &extensions_to_unload {
            let Some(extension) = old_index.extensions.get(extension_id) else {
                continue;
            };
            for (language_server_name, config) in &extension.manifest.language_servers {
                for language in config.languages() {
                    server_removal_tasks.push(self.proxy.remove_language_server(
                        &language,
                        language_server_name,
                        cx,
                    ));
                }
            }

            for server_id in extension.manifest.context_servers.keys() {
                self.proxy.unregister_context_server(server_id.clone(), cx);
            }
            for adapter in extension.manifest.debug_adapters.keys() {
                self.proxy.unregister_debug_adapter(adapter.clone());
            }
            for locator in extension.manifest.debug_locators.keys() {
                self.proxy.unregister_debug_locator(locator.clone());
            }
        }

        self.wasm_extensions
            .retain(|(extension, _)| !extensions_to_unload.contains(&extension.id));
        self.proxy.remove_user_themes(themes_to_remove);
        self.proxy.remove_icon_themes(icon_themes_to_remove);
        self.proxy
            .remove_languages(&languages_to_remove, &extensions_to_unload);
        for name in &languages_to_remove {
            self.registered_languages.remove(name);
        }

        // Remove semantic token rules for languages being unloaded.
        let semantic_token_rules_to_remove = languages_to_remove
            .iter()
            .filter(|language| !self.proxy.is_language_registered(language))
            .chain(languages_to_readd.iter().map(|(name, _)| name))
            .collect::<Vec<_>>();
        if !semantic_token_rules_to_remove.is_empty() {
            SettingsStore::update_global(cx, |store, _| {
                for language in semantic_token_rules_to_remove {
                    store.remove_language_semantic_token_rules(language.as_ref());
                }
            });
        }

        let mut grammars_to_add = Vec::new();
        let mut themes_to_add = Vec::new();
        let mut icon_themes_to_add = Vec::new();
        let mut snippets_to_add = Vec::new();
        for extension_id in &extensions_to_load {
            let Some(extension) = new_index.extensions.get(extension_id) else {
                continue;
            };

            let grammars = extension
                .manifest
                .grammars
                .keys()
                .map(|grammar_name| {
                    let mut grammar_path = self.installed_dir.clone();
                    grammar_path.extend([extension_id.as_ref(), "grammars"]);
                    grammar_path.push(grammar_name.as_ref());
                    grammar_path.set_extension("wasm");
                    (grammar_name.clone(), grammar_path)
                })
                .collect();
            grammars_to_add.push((extension_id.clone(), grammars));
            themes_to_add.extend(extension.manifest.themes.iter().map(|theme_path| {
                let mut path = self.installed_dir.clone();
                path.extend([Path::new(extension_id.as_ref()), theme_path.as_std_path()]);
                path
            }));
            icon_themes_to_add.extend(extension.manifest.icon_themes.iter().map(
                |icon_theme_path| {
                    let mut path = self.installed_dir.clone();
                    path.extend([
                        Path::new(extension_id.as_ref()),
                        icon_theme_path.as_std_path(),
                    ]);

                    let mut icons_root_path = self.installed_dir.clone();
                    icons_root_path.extend([Path::new(extension_id.as_ref())]);

                    (path, icons_root_path)
                },
            ));
            snippets_to_add.extend(extension.manifest.snippets.iter().flat_map(|snippets| {
                snippets.paths().map(|snippets_path| {
                    let mut path = self.installed_dir.clone();
                    path.extend([Path::new(extension_id.as_ref()), snippets_path.as_path()]);
                    path
                })
            }));
        }

        for (extension_id, grammars) in grammars_to_add {
            self.proxy.register_grammars(extension_id, grammars);
        }
        let languages_to_add = new_index
            .languages
            .iter()
            .filter(|(_, entry)| extensions_to_load.contains(&entry.extension))
            .map(|(name, entry)| (name.clone(), entry.clone()))
            .chain(languages_to_readd)
            .collect::<Vec<_>>();
        let mut semantic_token_rules_paths: Vec<(LanguageName, PathBuf)> = Vec::new();
        for (language_name, language) in languages_to_add {
            let mut language_path = self.installed_dir.clone();
            language_path.extend([
                Path::new(language.extension.as_ref()),
                language.path.as_path(),
            ]);
            let rules_path = language_path.join(SemanticTokenRules::FILE_NAME);

            let registered = self.proxy.register_language(
                language.extension.clone(),
                language_name.clone(),
                language.grammar.clone(),
                language.matcher.clone(),
                language.hidden,
                Arc::new({
                    let fs = self.fs.clone();
                    let query_files = language.query_files;
                    move || {
                        let fs = fs.clone();
                        let language_path = language_path.clone();
                        async move { load_plugin_language(fs, &language_path, query_files).await }
                            .boxed()
                    }
                }),
            );
            if !registered {
                continue;
            }

            self.registered_languages
                .insert(language_name.clone(), language.extension.clone());

            semantic_token_rules_paths.push((language_name, rules_path));
        }

        let fs = self.fs.clone();
        let wasm_host = self.wasm_host.clone();
        let root_dir = self.installed_dir.clone();
        let proxy = self.proxy.clone();
        let extension_entries = extensions_to_load
            .iter()
            .filter_map(|name| new_index.extensions.get(name).cloned())
            .collect::<Vec<_>>();
        self.extension_index = new_index;
        self.language_collisions.update(
            &self.extension_index,
            &self.registered_languages,
            &self.proxy,
        );
        cx.notify();
        cx.emit(Event::ExtensionsUpdated);
        if remote_sync_changed {
            self.sync_remote_clients();
        }

        cx.spawn(async move |this, cx| {
            let semantic_token_rules_to_add = cx
                .background_spawn({
                    let fs = fs.clone();
                    async move {
                        let _ = join_all(server_removal_tasks).await;
                        for theme_path in themes_to_add {
                            proxy
                                .load_user_theme(theme_path, fs.clone())
                                .await
                                .log_err();
                        }

                        for (icon_theme_path, icons_root_path) in icon_themes_to_add {
                            proxy
                                .load_icon_theme(icon_theme_path, icons_root_path, fs.clone())
                                .await
                                .log_err();
                        }

                        for snippets_path in &snippets_to_add {
                            match fs
                                .load(snippets_path)
                                .await
                                .with_context(|| format!("Loading snippets from {snippets_path:?}"))
                            {
                                Ok(snippets_contents) => {
                                    proxy
                                        .register_snippet(snippets_path, &snippets_contents)
                                        .log_err();
                                }
                                Err(e) => log::error!("Cannot load snippets: {e:#}"),
                            }
                        }

                        // Load semantic token rules if present in the language directory.
                        let mut semantic_token_rules_to_add = Vec::new();
                        for (language_name, rules_path) in semantic_token_rules_paths {
                            if !fs.is_file(&rules_path).await {
                                continue;
                            }
                            let rules = fs
                                .load(&rules_path)
                                .await
                                .and_then(|content| SemanticTokenRules::parse(&content));
                            if let Some(rules) = rules.log_err() {
                                semantic_token_rules_to_add.push((language_name, rules));
                            }
                        }
                        semantic_token_rules_to_add
                    }
                })
                .await;

            // Register semantic token rules for newly loaded extension languages.
            if !semantic_token_rules_to_add.is_empty() {
                this.update(cx, |_, cx| {
                    SettingsStore::update_global(cx, |store, _| {
                        for (language_name, rules) in semantic_token_rules_to_add {
                            store.set_language_semantic_token_rules(language_name.0.clone(), rules);
                        }
                    })
                })
                .ok();
            }

            let mut wasm_extensions = Vec::new();
            for extension in extension_entries {
                if extension.manifest.lib.kind.is_none() {
                    continue;
                };

                let extension_path = root_dir.join(extension.manifest.id.as_ref());
                let wasm_extension = WasmExtension::load(
                    &extension_path,
                    &extension.manifest,
                    wasm_host.clone(),
                    cx,
                )
                .await
                .with_context(|| format!("Loading extension from {extension_path:?}"));

                match wasm_extension {
                    Ok(wasm_extension) => {
                        wasm_extensions.push((extension.manifest.clone(), wasm_extension))
                    }
                    Err(e) => {
                        log::error!(
                            "Failed to load extension: {}, {:#}",
                            extension.manifest.id,
                            e
                        );
                        this.update(cx, |_, cx| {
                            cx.emit(Event::ExtensionFailedToLoad(extension.manifest.id.clone()))
                        })
                        .ok();
                    }
                }
            }

            this.update(cx, |this, cx| {
                this.reload_complete_senders.clear();

                for (manifest, wasm_extension) in &wasm_extensions {
                    let extension = Arc::new(wasm_extension.clone());

                    for (language_server_id, language_server_config) in &manifest.language_servers {
                        for language in language_server_config.languages() {
                            this.proxy.register_language_server(
                                extension.clone(),
                                language_server_id.clone(),
                                language.clone(),
                            );
                        }
                    }

                    for id in manifest.context_servers.keys() {
                        this.proxy
                            .register_context_server(extension.clone(), id.clone(), cx);
                    }

                    for (debug_adapter, meta) in &manifest.debug_adapters {
                        let mut path = root_dir.clone();
                        path.push(Path::new(manifest.id.as_ref()));
                        if let Some(schema_path) = &meta.schema_path {
                            path.push(schema_path);
                        } else {
                            path.push("debug_adapter_schemas");
                            path.push(Path::new(debug_adapter.as_ref()).with_extension("json"));
                        }

                        this.proxy.register_debug_adapter(
                            extension.clone(),
                            debug_adapter.clone(),
                            &path,
                        );
                    }

                    for debug_adapter in manifest.debug_locators.keys() {
                        this.proxy
                            .register_debug_locator(extension.clone(), debug_adapter.clone());
                    }
                }

                this.wasm_extensions.extend(wasm_extensions);
                this.proxy.set_extensions_loaded();
                this.proxy.reload_current_theme(cx);
                this.proxy.reload_current_icon_theme(cx);
                trigger_suppressed_extension_removal(this, cx);

                if let Some(events) = ExtensionEvents::try_global(cx) {
                    events.update(cx, |this, cx| {
                        this.emit(extension::Event::ExtensionsInstalledChanged, cx)
                    });
                }
            })
            .ok();
        })
    }

    fn rebuild_extension_index(&self, cx: &mut Context<Self>) -> Task<ExtensionIndex> {
        let fs = self.fs.clone();
        let work_dir = self.wasm_host.work_dir.clone();
        let extensions_dir = self.installed_dir.clone();
        let sources_dir = self.sources_dir.clone();
        let index_path = self.index_path.clone();
        let proxy = self.proxy.clone();
        cx.background_spawn(async move {
            let start_time = Instant::now();
            let mut index = ExtensionIndex::default();

            fs.create_dir(&work_dir).await.log_err();
            fs.create_dir(&extensions_dir).await.log_err();

            let extension_paths = fs.read_dir(&extensions_dir).await;
            if let Ok(mut extension_paths) = extension_paths {
                while let Some(extension_dir) = extension_paths.next().await {
                    let Ok(extension_dir) = extension_dir else {
                        continue;
                    };

                    if extension_dir
                        .file_name()
                        .is_some_and(|file_name| file_name == ".DS_Store")
                    {
                        continue;
                    }

                    Self::add_extension_to_index(
                        fs.clone(),
                        extension_dir,
                        &sources_dir,
                        &mut index,
                        proxy.clone(),
                    )
                    .await
                    .log_err();
                }
            }

            if let Ok(index_json) = serde_json::to_string_pretty(&index) {
                fs.save(&index_path, &index_json.as_str().into(), Default::default())
                    .await
                    .context("failed to save extension index")
                    .log_err();
            }

            log::info!("rebuilt extension index in {:?}", start_time.elapsed());
            index
        })
    }

    async fn add_extension_to_index(
        fs: Arc<dyn Fs>,
        extension_dir: PathBuf,
        sources_dir: &Path,
        index: &mut ExtensionIndex,
        proxy: Arc<ExtensionHostProxy>,
    ) -> Result<()> {
        let mut extension_manifest = ExtensionManifest::load(fs.clone(), &extension_dir).await?;
        let extension_id = extension_manifest.id.clone();

        // TODO: distinguish dev extensions more explicitly, by the absence
        // of a checksum file that we'll create when downloading normal extensions.
        let is_dev = fs
            .metadata(&extension_dir)
            .await?
            .with_context(|| format!("missing extension directory {extension_dir:?}"))?
            .is_symlink;

        let language_dir = extension_dir.join("languages");
        if let Ok(mut language_paths) = fs.read_dir(&language_dir).await {
            while let Some(language_path) = language_paths.next().await {
                let language_path = language_path
                    .with_context(|| format!("reading entries in language dir {language_dir:?}"))?;
                let Ok(relative_path) = language_path.strip_prefix(&extension_dir) else {
                    continue;
                };
                let Ok(Some(fs_metadata)) = fs.metadata(&language_path).await else {
                    continue;
                };
                if !fs_metadata.is_dir {
                    continue;
                }
                let config = {
                    let fs = fs.clone();
                    let language_config_path = language_path.join(LanguageConfig::FILE_NAME);
                    async move {
                        let config = fs.load(&language_config_path).await.with_context(|| {
                            format!("loading language config from {language_config_path:?}")
                        })?;
                        LanguageConfig::from_toml(&config).map_err(anyhow::Error::from)
                    }
                };
                let query_files = async {
                    Ok(discover_query_files(fs.clone(), &language_path)
                        .await
                        .log_err())
                };
                let (config, query_files) = futures::try_join!(config, query_files)?;

                let relative_path = relative_path.to_rel_path_buf()?;
                if !extension_manifest.languages.contains(&relative_path) {
                    extension_manifest.languages.push(relative_path.clone());
                }

                index.languages.insert(
                    config.name.clone(),
                    ExtensionIndexLanguageEntry {
                        extension: extension_id.clone(),
                        path: relative_path.as_std_path().to_path_buf(),
                        matcher: config.matcher,
                        hidden: config.hidden,
                        grammar: config.grammar,
                        query_files,
                    },
                );
                index
                    .language_providers
                    .entry(config.name)
                    .or_default()
                    .insert(extension_id.clone());
            }
        }

        if let Ok(mut theme_paths) = fs.read_dir(&extension_dir.join("themes")).await {
            while let Some(theme_path) = theme_paths.next().await {
                let theme_path = theme_path?;
                let Ok(relative_path) = theme_path.strip_prefix(&extension_dir) else {
                    continue;
                };

                let Some(theme_families) = proxy
                    .list_theme_names(theme_path.clone(), fs.clone())
                    .await
                    .log_err()
                else {
                    continue;
                };

                let relative_path = relative_path.to_rel_path_buf()?;
                if !extension_manifest.themes.contains(&relative_path) {
                    extension_manifest.themes.push(relative_path.clone());
                }

                for theme_name in theme_families {
                    index.themes.insert(
                        theme_name.into(),
                        ExtensionIndexThemeEntry {
                            extension: extension_id.clone(),
                            path: relative_path.as_std_path().to_path_buf(),
                        },
                    );
                }
            }
        }

        if let Ok(mut icon_theme_paths) = fs.read_dir(&extension_dir.join("icon_themes")).await {
            while let Some(icon_theme_path) = icon_theme_paths.next().await {
                let icon_theme_path = icon_theme_path?;
                let Ok(relative_path) = icon_theme_path.strip_prefix(&extension_dir) else {
                    continue;
                };

                let Some(icon_theme_families) = proxy
                    .list_icon_theme_names(icon_theme_path.clone(), fs.clone())
                    .await
                    .log_err()
                else {
                    continue;
                };

                let relative_path = relative_path.to_rel_path_buf()?;
                if !extension_manifest.icon_themes.contains(&relative_path) {
                    extension_manifest.icon_themes.push(relative_path.clone());
                }

                for icon_theme_name in icon_theme_families {
                    index.icon_themes.insert(
                        icon_theme_name.into(),
                        ExtensionIndexIconThemeEntry {
                            extension: extension_id.clone(),
                            path: relative_path.as_std_path().to_path_buf(),
                        },
                    );
                }
            }
        }

        let extension_wasm_path = extension_dir.join("extension.wasm");
        if fs.is_file(&extension_wasm_path).await {
            extension_manifest
                .lib
                .kind
                .get_or_insert(ExtensionLibraryKind::Rust);
        }

        let source = if is_dev {
            git_source_for_dev_extension(&fs, &extension_dir, sources_dir, &extension_id).await
        } else {
            None
        };
        index.extensions.insert(
            extension_id.clone(),
            ExtensionIndexEntry {
                dev: is_dev,
                manifest: Arc::new(extension_manifest),
                source,
            },
        );

        Ok(())
    }

    fn prepare_remote_extension(
        &mut self,
        extension_id: Arc<str>,
        is_dev: bool,
        tmp_dir: PathBuf,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let src_dir = self.extensions_dir().join(extension_id.as_ref());
        let Some(loaded_extension) = self.extension_index.extensions.get(&extension_id).cloned()
        else {
            return Task::ready(Err(anyhow!("extension no longer installed")));
        };
        let fs = self.fs.clone();
        cx.background_spawn(async move {
            const EXTENSION_TOML: &str = "extension.toml";
            const EXTENSION_WASM: &str = "extension.wasm";
            const CONFIG_TOML: &str = LanguageConfig::FILE_NAME;

            if is_dev {
                let manifest_toml = toml::to_string(&loaded_extension.manifest)?;
                fs.save(
                    &tmp_dir.join(EXTENSION_TOML),
                    &Rope::from(manifest_toml),
                    language::LineEnding::Unix,
                )
                .await?;
            } else {
                fs.copy_file(
                    &src_dir.join(EXTENSION_TOML),
                    &tmp_dir.join(EXTENSION_TOML),
                    fs::CopyOptions::default(),
                )
                .await?
            }

            if fs.is_file(&src_dir.join(EXTENSION_WASM)).await {
                fs.copy_file(
                    &src_dir.join(EXTENSION_WASM),
                    &tmp_dir.join(EXTENSION_WASM),
                    fs::CopyOptions::default(),
                )
                .await?
            }

            for language_path in loaded_extension.manifest.languages.iter() {
                if fs
                    .is_file(&src_dir.join(language_path).join(CONFIG_TOML))
                    .await
                {
                    fs.create_dir(&tmp_dir.join(language_path)).await?;
                    fs.copy_file(
                        &src_dir.join(language_path).join(CONFIG_TOML),
                        &tmp_dir.join(language_path).join(CONFIG_TOML),
                        fs::CopyOptions::default(),
                    )
                    .await?
                }
            }

            for (adapter_name, meta) in loaded_extension.manifest.debug_adapters.iter() {
                let schema_path = extension::build_debug_adapter_schema_path(adapter_name, meta)?;

                if fs.is_file(&src_dir.join(&schema_path)).await {
                    if let Some(parent) = schema_path.parent() {
                        fs.create_dir(&tmp_dir.join(parent)).await?
                    }
                    fs.copy_file(
                        &src_dir.join(&schema_path),
                        &tmp_dir.join(&schema_path),
                        fs::CopyOptions::default(),
                    )
                    .await?
                }
            }

            Ok(())
        })
    }

    fn sync_remote_clients(&mut self) {
        for state in self.remote_clients.values() {
            state
                .dirty_tx
                .unbounded_send(RemoteSyncSignal::IndexChanged)
                .ok();
        }
    }

    async fn reconcile_remote_client(
        this: WeakEntity<Self>,
        client: WeakEntity<RemoteClient>,
        mut dirty_rx: UnboundedReceiver<RemoteSyncSignal>,
        cx: &mut AsyncApp,
    ) {
        let mut failed_attempts = 0_usize;
        loop {
            while let Ok(signal) = dirty_rx.try_recv() {
                if signal == RemoteSyncSignal::Reconnected {
                    failed_attempts = 0;
                }
            }

            let Ok(connection_state) =
                client.read_with(cx, |client, _cx| client.connection_state())
            else {
                return;
            };
            if connection_state == ConnectionState::Disconnected
                || connection_state == ConnectionState::Reconnecting
            {
                failed_attempts = 0;
                if dirty_rx.next().await.is_none() {
                    return;
                }
                continue;
            }

            match Self::sync_extensions_to_remote(&this, client.clone(), cx).await {
                Ok(()) => {
                    failed_attempts = 0;
                    if dirty_rx.next().await.is_none() {
                        return;
                    }
                }
                Err(error) => {
                    failed_attempts += 1;
                    if failed_attempts >= MAX_REMOTE_SYNC_ATTEMPTS {
                        log::error!(
                            "Failed to sync extensions to a remote client {failed_attempts} times, waiting for an extension or connection change before retrying: {error:#}"
                        );
                        match dirty_rx.next().await {
                            None => return,
                            Some(RemoteSyncSignal::Reconnected) => failed_attempts = 0,
                            Some(RemoteSyncSignal::IndexChanged) => {}
                        }
                        continue;
                    }
                    let delay = remote_sync_retry_delay(failed_attempts - 1);
                    log::error!(
                        "Failed to sync extensions to a remote client (attempt {failed_attempts}), will retry in {delay:?}: {error:#}"
                    );
                    let timer = cx.background_executor().timer(delay).fuse();
                    futures::pin_mut!(timer);
                    loop {
                        select_biased! {
                            signal = dirty_rx.next() => {
                                match signal {
                                    None => return,
                                    Some(RemoteSyncSignal::Reconnected) => {
                                        failed_attempts = 0;
                                        break;
                                    }
                                    Some(RemoteSyncSignal::IndexChanged) => {}
                                }
                            }
                            _ = timer => break,
                        }
                    }
                }
            }
        }
    }

    async fn sync_extensions_to_remote(
        this: &WeakEntity<Self>,
        client: WeakEntity<RemoteClient>,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        let entries = this.update(cx, |this, _cx| {
            this.extension_index
                .extensions_to_sync_to_remote()
                .into_entries()
                .collect::<Vec<_>>()
        })?;
        let mut prepared_dev_payloads = HashMap::default();
        let mut extensions = Vec::new();
        for (id, entry) in entries {
            let mut content_fingerprint = None;
            if entry.dev {
                match Self::prepare_dev_extension_payload(this, &id, cx).await {
                    Ok((payload_dir, fingerprint)) => {
                        content_fingerprint = Some(fingerprint);
                        prepared_dev_payloads.insert(id.to_string(), payload_dir);
                    }
                    Err(error) => {
                        log::warn!(
                            "failed to prepare dev extension {id} for a remote sync: {error:#}"
                        );
                    }
                }
            }
            extensions.push(proto::Extension {
                id: id.to_string(),
                version: entry.manifest.version.to_string(),
                dev: entry.dev,
                content_fingerprint,
            });
        }

        let request = client.update(cx, |client, _cx| {
            client
                .proto_client()
                .request(proto::SyncExtensions { extensions })
        })?;
        let response = with_remote_sync_timeout(
            cx,
            REMOTE_SYNC_TIMEOUT,
            "requesting the remote extension list",
            request,
        )
        .await?;
        let path_style = client.read_with(cx, |client, _| client.path_style())?;

        let mut failed_installs = Vec::new();
        for missing_extension in response.missing_extensions.into_iter() {
            let prepared_payload = prepared_dev_payloads.remove(&missing_extension.id);
            if let Err(error) = Self::install_extension_on_remote(
                this,
                &client,
                &missing_extension,
                &response.tmp_dir,
                path_style,
                prepared_payload,
                cx,
            )
            .await
            {
                log::error!(
                    "Failed to install extension {} on the remote: {error:#}",
                    missing_extension.id
                );
                failed_installs.push(missing_extension.id);
            }
        }
        if !prepared_dev_payloads.is_empty() {
            cx.background_executor()
                .spawn(async move { drop(prepared_dev_payloads) })
                .detach();
        }

        anyhow::ensure!(
            failed_installs.is_empty(),
            "failed to install extensions on the remote: {failed_installs:?}"
        );
        anyhow::Ok(())
    }

    async fn prepare_dev_extension_payload(
        this: &WeakEntity<Self>,
        id: &Arc<str>,
        cx: &mut AsyncApp,
    ) -> Result<(tempfile::TempDir, u64)> {
        let payload_dir = cx
            .background_executor()
            .spawn(async move { tempfile::tempdir() })
            .await?;
        this.update(cx, |this, cx| {
            this.prepare_remote_extension(id.clone(), true, payload_dir.path().to_owned(), cx)
        })?
        .await?;
        let fs = this.read_with(cx, |this, _cx| this.fs.clone())?;
        let fingerprint = cx
            .background_executor()
            .spawn({
                let path = payload_dir.path().to_owned();
                async move { hash_directory_contents(&fs, &path).await }
            })
            .await?;
        Ok((payload_dir, fingerprint))
    }

    async fn install_extension_on_remote(
        this: &WeakEntity<Self>,
        client: &WeakEntity<RemoteClient>,
        missing_extension: &proto::Extension,
        remote_tmp_dir: &str,
        path_style: PathStyle,
        prepared_payload: Option<tempfile::TempDir>,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        let already_prepared = prepared_payload.is_some();
        let tmp_dir = match prepared_payload {
            Some(payload_dir) => payload_dir,
            None => {
                cx.background_executor()
                    .spawn(async move { tempfile::tempdir() })
                    .await?
            }
        };
        let result = Self::upload_extension_to_remote(
            this,
            client,
            missing_extension,
            remote_tmp_dir,
            path_style,
            tmp_dir.path().to_owned(),
            already_prepared,
            cx,
        )
        .await;
        cx.background_executor()
            .spawn(async move { drop(tmp_dir) })
            .detach();
        result
    }

    async fn upload_extension_to_remote(
        this: &WeakEntity<Self>,
        client: &WeakEntity<RemoteClient>,
        missing_extension: &proto::Extension,
        remote_tmp_dir: &str,
        path_style: PathStyle,
        local_dir: PathBuf,
        already_prepared: bool,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        static UPLOAD_NONCE: AtomicU64 = AtomicU64::new(0);

        if !already_prepared {
            this.update(cx, |this, cx| {
                this.prepare_remote_extension(
                    missing_extension.id.clone().into(),
                    missing_extension.dev,
                    local_dir.clone(),
                    cx,
                )
            })?
            .await?;
        }
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
        let upload_name = format!(
            "{}-{}-{}-{}",
            missing_extension.id,
            std::process::id(),
            UPLOAD_NONCE.fetch_add(1, AtomicOrdering::Relaxed),
            timestamp,
        );
        let dest_dir = RemotePathBuf::new(
            path_style
                .join(remote_tmp_dir, &upload_name)
                .with_context(|| {
                    format!(
                        "failed to construct destination path: {remote_tmp_dir:?}, {upload_name:?}"
                    )
                })?,
            path_style,
        );
        log::info!(
            "Uploading extension {} to {:?}",
            missing_extension.id,
            dest_dir
        );

        let upload = client.update(cx, |client, cx| {
            client.upload_directory(local_dir, dest_dir.clone(), cx)
        })?;
        with_remote_sync_timeout(cx, REMOTE_SYNC_TIMEOUT, "uploading an extension", upload).await?;

        log::info!("Finished uploading extension {}", missing_extension.id);

        let install = client.update(cx, |client, _cx| {
            client.proto_client().request(proto::InstallExtension {
                tmp_dir: dest_dir.to_proto(),
                extension: Some(missing_extension.clone()),
            })
        })?;
        with_remote_sync_timeout(cx, REMOTE_SYNC_TIMEOUT, "installing an extension", install)
            .await?;
        Ok(())
    }

    pub fn register_remote_client(&mut self, client: Entity<RemoteClient>, cx: &mut Context<Self>) {
        let entity_id = client.entity_id();
        if self.remote_clients.contains_key(&entity_id) {
            return;
        }

        let (dirty_tx, dirty_rx) = unbounded();

        let event_subscription = cx.subscribe(&client, |store, client, event, _cx| match event {
            RemoteClientEvent::Reconnected => {
                if let Some(state) = store.remote_clients.get(&client.entity_id()) {
                    state
                        .dirty_tx
                        .unbounded_send(RemoteSyncSignal::Reconnected)
                        .ok();
                }
            }
            RemoteClientEvent::Disconnected { .. } => {}
        });
        let release_subscription = cx.observe_release(&client, move |store, _client, _cx| {
            store.remote_clients.remove(&entity_id);
        });

        let task = cx.spawn({
            let client = client.downgrade();
            let initial_index_load = self.initial_index_load.clone();
            async move |this, cx| {
                initial_index_load.await;
                Self::reconcile_remote_client(this, client, dirty_rx, cx).await;
            }
        });

        self.remote_clients.insert(
            entity_id,
            RemoteClientState {
                dirty_tx,
                _task: task,
                _subscriptions: Subscription::join(event_subscription, release_subscription),
            },
        );
    }
}

async fn load_plugin_language(
    fs: Arc<dyn Fs>,
    language_path: &Path,
    query_files: Option<QueryFiles>,
) -> Result<LoadedLanguage> {
    let config = {
        let fs = fs.clone();
        let config_path = language_path.join(LanguageConfig::FILE_NAME);
        async move {
            let contents = fs.load(&config_path).await?;
            LanguageConfig::from_toml(&contents).map_err(anyhow::Error::from)
        }
    };
    let context_provider = {
        let fs = fs.clone();
        let tasks_path = language_path.join(TaskTemplates::FILE_NAME);
        async move {
            fs.load(&tasks_path).await.ok().and_then(|contents| {
                serde_json_lenient::from_str(&contents)
                    .log_err()
                    .map(|definitions| {
                        Arc::new(ContextProviderWithTasks::new(definitions)) as Arc<_>
                    })
            })
        }
    };
    let (config, queries, context_provider) = futures::try_join!(
        config,
        async move { Ok(load_plugin_queries(fs, &language_path, query_files).await) },
        async move { Ok(context_provider.await) }
    )?;

    Ok(LoadedLanguage {
        config,
        queries,
        context_provider,
        toolchain_provider: None,
        manifest_name: None,
    })
}

async fn discover_query_files(fs: Arc<dyn Fs>, root_path: &Path) -> Result<QueryFiles> {
    let mut paths = fs.read_dir(root_path).await?;
    let mut query_files = QueryFiles::empty();
    while let Some(path) = paths.next().await {
        let path = path?;
        let Some(query_file) = path
            .file_name()
            .and_then(|file_name| file_name.to_str())
            .and_then(|file_name| file_name.parse::<QueryFile>().ok())
        else {
            continue;
        };
        query_files.insert(query_file.into());
    }
    Ok(query_files)
}

async fn load_plugin_queries(
    fs: Arc<dyn Fs>,
    root_path: &Path,
    query_files: Option<QueryFiles>,
) -> LanguageQueries {
    let query_files = query_files.unwrap_or_else(QueryFiles::all);
    let files = join_all(query_files.query_files().map(|query_file| {
        let fs = fs.clone();
        let path = root_path.join(query_file.file_name());
        async move {
            fs.load(&path)
                .await
                .ok()
                .map(|contents| QueryFileContents::new(query_file, Cow::Owned(contents)))
        }
    }))
    .await;
    LanguageQueries::from_files(files.into_iter().flatten())
}
