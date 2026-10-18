mod capability_granter;
pub mod extension_settings;
mod extension_transaction;
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
use fs::{Fs, RemoveOptions};
use futures::future::{Shared, join_all};
use futures::{
    AsyncReadExt as _, Future, FutureExt as _, StreamExt as _,
    channel::{
        mpsc::{UnboundedReceiver, UnboundedSender, unbounded},
        oneshot,
    },
    io::BufReader,
    lock::OwnedMutexGuard,
    select_biased,
    stream::FuturesUnordered,
};
use gpui::{
    App, AppContext as _, AsyncApp, BackgroundExecutor, Context, Entity, EntityId, EventEmitter,
    Global, Subscription, Task, TaskExt, UpdateGlobal as _, WeakEntity, actions,
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
    paths::{PathStyle, RemotePathBuf, SanitizedPath, normalize_lexically},
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
#[cfg(test)]
use extension_transaction::GitSourceBackupState;
use extension_transaction::{
    ExtensionPublisher, GitSourceDisposition, PreparedExtension, StagedDirectory,
};

pub const RELOAD_DEBOUNCE_DURATION: Duration = Duration::from_millis(200);
const FS_WATCH_LATENCY: Duration = Duration::from_millis(100);
const EXTENSION_PREPARATION_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const EXTENSION_BUILD_TIMEOUT: Duration = Duration::from_secs(10 * 60);
pub(crate) const REMOTE_SYNC_RETRY_DELAY: Duration = Duration::from_secs(1);
pub(crate) const MAX_REMOTE_SYNC_RETRY_DELAY: Duration = Duration::from_secs(60);
pub(crate) const MAX_REMOTE_SYNC_ATTEMPTS: usize = 10;
pub(crate) const REMOTE_SYNC_TIMEOUT: Duration = Duration::from_secs(60 * 60);
pub(crate) const GIT_COMMAND_TIMEOUT: Duration = Duration::from_secs(120);

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
    publisher: ExtensionPublisher,
    git_extension_failures: BTreeMap<Arc<str>, GitExtensionFailure>,
    undelivered_git_extension_failures: BTreeSet<Arc<str>>,
    pub outstanding_operations: BTreeMap<Arc<str>, ExtensionOperation>,
    #[cfg(test)]
    auto_install_prepared_observer: Option<(Arc<str>, oneshot::Sender<()>)>,
    pub index_path: PathBuf,
    pub modified_extensions: HashSet<Arc<str>>,
    pub wasm_host: Arc<WasmHost>,
    pub wasm_extensions: Vec<(Arc<ExtensionManifest>, WasmExtension)>,
    pub tasks: Vec<Task<()>>,
    pub(crate) remote_clients: HashMap<EntityId, RemoteClientState>,
    pub(crate) initial_index_load: Shared<Task<()>>,
    pub(crate) initial_maintenance: Shared<Task<()>>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkout_tree: Option<String>,
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

async fn with_extension_timeout<T>(
    executor: &BackgroundExecutor,
    timeout: Duration,
    operation: impl Future<Output = Result<T>>,
) -> Result<T> {
    let timer = executor.timer(timeout).fuse();
    let operation = operation.fuse();
    futures::pin_mut!(timer, operation);
    select_biased! {
        result = operation => result,
        _ = timer => anyhow::bail!("extension operation timed out after {timeout:?}"),
    }
}

#[derive(Clone)]
enum GitCommandOwner {
    Scratch(Arc<tempfile::TempDir>),
    Checkout(Arc<OwnedMutexGuard<()>>),
}

async fn run_git(
    executor: &BackgroundExecutor,
    directory: &Path,
    args: &[&str],
    owner: GitCommandOwner,
) -> Result<String> {
    let (mut sender, receiver) = oneshot::channel();
    executor.spawn({
        let executor = executor.clone();
        let directory = directory.to_path_buf();
        let arguments = args.iter().map(|argument| (*argument).to_owned()).collect::<Vec<_>>();
        async move {
            let result = async {
                let cancellation = async {
                    let timeout = executor.timer(GIT_COMMAND_TIMEOUT).fuse();
                    let cancelled = sender.cancellation().fuse();
                    futures::pin_mut!(timeout, cancelled);
                    select_biased! {
                        _ = cancelled => anyhow!("Git command cancelled"),
                        _ = timeout => anyhow!("extension operation timed out after {GIT_COMMAND_TIMEOUT:?}"),
                    }
                }.fuse();
                futures::pin_mut!(cancellation);
                #[cfg(test)]
                {
                    let arguments = arguments.iter().map(String::as_str).collect::<Vec<_>>();
                    let hook = extension_store_test::git_command_test_hook(&directory, &arguments).fuse();
                    futures::pin_mut!(hook);
                    select_biased! {
                        error = cancellation => return Err(error),
                        _ = hook => {}
                    }
                }
                let mut command = util::command::new_std_command("git");
                command.args(&arguments).current_dir(&directory).env("GIT_TERMINAL_PROMPT", "0");
                let mut child = util::process::Child::spawn_contained(
                    command,
                    std::process::Stdio::null(),
                    std::process::Stdio::piped(),
                    std::process::Stdio::piped(),
                )?;
                child.output_until_cancelled(cancellation).await
            }.await;
            // The receiver may have been dropped. Keep staging or the publication
            // gate until every child has exited and released its captured pipes.
            match owner {
                GitCommandOwner::Scratch(directory) => drop(directory),
                GitCommandOwner::Checkout(guard) => drop(guard),
            }
            if let Err(result) = sender.send(result) {
                result.context("finishing a cancelled Git command").log_err();
            }
        }
    }).detach();
    let output = receiver
        .await
        .context("Git command worker exited")?
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
    executor: &BackgroundExecutor,
    fs: Arc<dyn Fs>,
    builder: &Arc<ExtensionBuilder>,
    staging_dir: &Path,
    sources_dir: &Path,
    publisher: &ExtensionPublisher,
    wasm_host: Arc<WasmHost>,
    extension_id: &str,
    source: &GitExtensionSource,
    checkout_commit: Option<&str>,
) -> Result<PathBuf> {
    publisher.recover().await?;
    let prepared = prepare_git_source(
        executor,
        fs.clone(),
        staging_dir,
        extension_id,
        source,
        checkout_commit,
    )
    .await?;
    publish_prepared_git_source(
        executor,
        fs,
        builder,
        sources_dir,
        publisher,
        wasm_host,
        extension_id,
        prepared,
    )
    .await
}

struct PreparedGitSource {
    checkout: tempfile::TempDir,
    manifest: ExtensionManifest,
    source: InstalledGitSource,
}

async fn prepare_git_source(
    executor: &BackgroundExecutor,
    fs: Arc<dyn Fs>,
    staging_dir: &Path,
    extension_id: &str,
    source: &GitExtensionSource,
    checkout_commit: Option<&str>,
) -> Result<PreparedGitSource> {
    validate_git_install(extension_id, source)?;
    fs.create_dir(staging_dir).await?;
    let checkout = Arc::new(tempfile::tempdir_in(staging_dir)?);
    let directory = checkout.path();
    let owner = GitCommandOwner::Scratch(checkout.clone());
    run_git(executor, directory, &["init"], owner.clone()).await?;
    run_git(
        executor,
        directory,
        &["remote", "add", "--", "origin", &source.url],
        owner.clone(),
    )
    .await?;
    // Repairs of pinned extensions preserve both the installed commit and configured source.
    let reference = checkout_commit
        .or(source.reference.as_deref())
        .unwrap_or("HEAD");
    run_git(
        executor,
        directory,
        &["fetch", "--depth", "1", "--", "origin", reference],
        owner.clone(),
    )
    .await?;
    run_git(
        executor,
        directory,
        &["checkout", "--force", "--detach", "FETCH_HEAD"],
        owner.clone(),
    )
    .await?;
    let commit = run_git(executor, directory, &["rev-parse", "HEAD"], owner).await?;

    let staged_extension = git_source_subdirectory(directory.to_path_buf(), source.path.as_deref());
    let checkout_root = fs.canonicalize(directory).await?;
    let extension_root = fs.canonicalize(&staged_extension).await?;
    anyhow::ensure!(
        extension_root.starts_with(&checkout_root),
        "the extension path resolves outside its repository"
    );
    let manifest = ExtensionManifest::load(fs.clone(), &staged_extension).await?;
    anyhow::ensure!(
        manifest.id.as_ref() == extension_id,
        "the repository declares extension id {}, expected {extension_id}",
        manifest.id
    );
    let manifest = manifest_with_defaults(&fs, &staged_extension, manifest).await?;
    let checkout = Arc::try_unwrap(checkout)
        .map_err(|_| anyhow!("Git preparation still owns its checkout after command cleanup"))?;
    Ok(PreparedGitSource {
        checkout,
        manifest,
        source: InstalledGitSource {
            url: source.url.clone(),
            reference: source.reference.clone(),
            path: source.path.clone(),
            commit,
            checkout_tree: None,
        },
    })
}

async fn publish_prepared_git_source(
    executor: &BackgroundExecutor,
    fs: Arc<dyn Fs>,
    builder: &Arc<ExtensionBuilder>,
    sources_dir: &Path,
    publisher: &ExtensionPublisher,
    wasm_host: Arc<WasmHost>,
    extension_id: &str,
    prepared: PreparedGitSource,
) -> Result<PathBuf> {
    let (sender, receiver) = oneshot::channel();
    let builder = builder.clone();
    // A cancelled installer must not remove its checkout while compiler children
    // still use it. The worker owns staging until the builder has reaped them.
    executor
        .spawn({
            let executor = executor.clone();
            let fs = fs.clone();
            async move {
                let mut prepared = prepared;
                let directory = git_source_subdirectory(
                    prepared.checkout.path().to_path_buf(),
                    prepared.source.path.as_deref(),
                );
                let result = builder
                    .compile_extension_with_timeout(
                        &directory,
                        &mut prepared.manifest,
                        CompileExtensionOptions::dev(),
                        fs,
                        executor,
                        EXTENSION_BUILD_TIMEOUT,
                    )
                    .await
                    .context("building Git extension")
                    .map(|()| prepared);
                if let Err(result) = sender.send(result) {
                    result
                        .context("finishing a cancelled Git extension build")
                        .log_err();
                }
            }
        })
        .detach();
    let PreparedGitSource {
        checkout,
        manifest,
        mut source,
    } = receiver
        .await
        .context("Git extension build worker stopped")??;
    let staged_extension =
        git_source_subdirectory(checkout.path().to_path_buf(), source.path.as_deref());
    {
        let _filesystem_guard = publisher.lock_for_read().await?;
        validate_prepared_extension(&fs, &staged_extension, sources_dir, manifest, &wasm_host)
            .await?;
    }

    let checkout = Arc::new(checkout);
    source.checkout_tree = Some(
        capture_git_checkout_tree(executor, &fs, &checkout, &staged_extension, &source).await?,
    );
    let checkout = Arc::try_unwrap(checkout)
        .map_err(|_| anyhow!("Git checkout is still in use after recording its contents"))?;
    let extension_path =
        git_source_extension_path(sources_dir, extension_id, source.path.as_deref());
    publisher
        .publish(extension_id, PreparedExtension::Git { checkout, source })
        .await?;
    Ok(extension_path)
}

async fn capture_git_checkout_tree(
    executor: &BackgroundExecutor,
    fs: &Arc<dyn Fs>,
    checkout: &Arc<tempfile::TempDir>,
    extension_dir: &Path,
    source: &InstalledGitSource,
) -> Result<String> {
    let owner = GitCommandOwner::Scratch(checkout.clone());
    let head = run_git(
        executor,
        checkout.path(),
        &["rev-parse", "HEAD"],
        owner.clone(),
    )
    .await?;
    anyhow::ensure!(
        head == source.commit,
        "the extension build changed its Git commit"
    );
    // Build outputs may replace tracked files. Record the validated result rather
    // than comparing future health checks against the pristine source tree.
    run_git(
        executor,
        checkout.path(),
        &["add", "--update", "--", "."],
        owner.clone(),
    )
    .await?;
    let manifest = ExtensionManifest::load(fs.clone(), extension_dir).await?;
    let manifest = manifest_with_defaults(fs, extension_dir, manifest).await?;
    let resources = git_runtime_resource_paths(fs, extension_dir, &manifest).await?;
    if !resources.is_empty() {
        let mut args = vec!["--literal-pathspecs", "add", "--force", "--"];
        args.extend(resources.iter().map(String::as_str));
        run_git(executor, extension_dir, &args, owner.clone()).await?;
    }
    run_git(executor, checkout.path(), &["write-tree"], owner).await
}

async fn git_runtime_resource_paths(
    fs: &Arc<dyn Fs>,
    extension_dir: &Path,
    manifest: &ExtensionManifest,
) -> Result<BTreeSet<String>> {
    let mut resources = BTreeSet::new();
    for path in manifest.themes.iter().chain(&manifest.icon_themes) {
        resources.insert(path.as_std_path().to_string_lossy().into_owned());
    }
    for path in manifest
        .snippets
        .iter()
        .flat_map(|snippets| snippets.paths())
    {
        resources.insert(path.to_string_lossy().into_owned());
    }
    for language_path in &manifest.languages {
        let language_dir = extension_dir.join(language_path.as_std_path());
        for file_name in [
            "config.toml",
            SemanticTokenRules::FILE_NAME,
            TaskTemplates::FILE_NAME,
        ] {
            if fs.is_file(&language_dir.join(file_name)).await {
                resources.insert(
                    language_path
                        .as_std_path()
                        .join(file_name)
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
        let queries = discover_query_files(fs.clone(), &language_dir).await?;
        for query in queries.query_files() {
            resources.insert(
                language_path
                    .as_std_path()
                    .join(query.file_name())
                    .to_string_lossy()
                    .into_owned(),
            );
        }
    }
    if fs.is_file(&extension_dir.join("extension.wasm")).await {
        resources.insert("extension.wasm".into());
    }
    for grammar in manifest.grammars.keys() {
        resources.insert(format!("grammars/{grammar}.wasm"));
    }
    Ok(resources)
}

async fn validate_git_checkout_tree(
    executor: &BackgroundExecutor,
    fs: &Arc<dyn Fs>,
    checkout: &Path,
    manifest: &ExtensionManifest,
    source: &InstalledGitSource,
    owner: GitCommandOwner,
) -> Result<()> {
    let tree = source
        .checkout_tree
        .as_deref()
        .context("the installed checkout has no content baseline")?;
    anyhow::ensure!(
        is_commit_sha(tree),
        "the installed checkout has an invalid content baseline"
    );
    let head = run_git(executor, checkout, &["rev-parse", "HEAD"], owner.clone()).await?;
    anyhow::ensure!(
        head == source.commit,
        "the checkout no longer uses the installed Git commit"
    );
    for cached in [false, true] {
        let mut args = vec!["diff", "--quiet", "--no-ext-diff", "--no-textconv"];
        if cached {
            args.push("--cached");
        }
        args.extend([tree, "--", "."]);
        run_git(executor, checkout, &args, owner.clone())
            .await
            .context("the checkout differs from its validated contents")?;
    }
    let extension_dir = git_source_subdirectory(checkout.to_path_buf(), source.path.as_deref());
    let resources = git_runtime_resource_paths(fs, &extension_dir, manifest).await?;
    if !resources.is_empty() {
        let mut args = vec!["--literal-pathspecs", "ls-files", "--others", "--"];
        args.extend(resources.iter().map(String::as_str));
        let untracked = run_git(executor, &extension_dir, &args, owner).await?;
        anyhow::ensure!(
            untracked.is_empty(),
            "unvalidated extension resources: {untracked}"
        );
    }
    Ok(())
}

struct PreparedMarketplaceExtension {
    staged: StagedDirectory,
    manifest: ExtensionManifest,
    external_grammars: BTreeSet<Arc<str>>,
    affected_grammars: BTreeSet<Arc<str>>,
}

struct PreparedGitExtension {
    prepared: PreparedGitSource,
    external_grammars: BTreeSet<Arc<str>>,
    affected_grammars: BTreeSet<Arc<str>>,
}

enum PreparedAutoExtension {
    Marketplace(PreparedMarketplaceExtension),
    Git(PreparedGitExtension),
}

impl PreparedAutoExtension {
    fn external_grammars(&self) -> &BTreeSet<Arc<str>> {
        match self {
            Self::Marketplace(prepared) => &prepared.external_grammars,
            Self::Git(prepared) => &prepared.external_grammars,
        }
    }

    fn affected_grammars(&self) -> &BTreeSet<Arc<str>> {
        match self {
            Self::Marketplace(prepared) => &prepared.affected_grammars,
            Self::Git(prepared) => &prepared.affected_grammars,
        }
    }
}

async fn external_grammar_dependencies(
    fs: &Arc<dyn Fs>,
    directory: &Path,
    manifest: &ExtensionManifest,
    proxy: &ExtensionHostProxy,
) -> Result<BTreeSet<Arc<str>>> {
    let mut external_grammars = BTreeSet::new();
    for language_path in &manifest.languages {
        let config = fs
            .load(
                &directory
                    .join(language_path)
                    .join(LanguageConfig::FILE_NAME),
            )
            .await
            .and_then(|contents| LanguageConfig::from_toml(&contents).map_err(Into::into))
            .with_context(|| format!("validating language resources in {language_path}"))?;
        if let Some(grammar) = config.grammar
            && !manifest.grammars.contains_key(&grammar)
            && !proxy.is_native_grammar(&grammar)
        {
            external_grammars.insert(grammar);
        }
    }
    Ok(external_grammars)
}

async fn manifest_with_defaults(
    fs: &Arc<dyn Fs>,
    directory: &Path,
    mut manifest: ExtensionManifest,
) -> Result<ExtensionManifest> {
    let declared = manifest.clone();
    extension::extension_builder::populate_defaults(&mut manifest, directory, fs.clone()).await?;
    // Legacy defaults clear declarations; retain them so missing explicit resources still fail.
    if manifest.schema_version.is_v0() {
        for path in declared.languages {
            if !manifest.languages.contains(&path) {
                manifest.languages.push(path);
            }
        }
        for path in declared.themes {
            if !manifest.themes.contains(&path) {
                manifest.themes.push(path);
            }
        }
        for (name, grammar) in declared.grammars {
            manifest.grammars.entry(name).or_insert(grammar);
        }
    }
    Ok(manifest)
}

async fn validate_prepared_extension(
    fs: &Arc<dyn Fs>,
    directory: &Path,
    sources_dir: &Path,
    manifest: ExtensionManifest,
    wasm_host: &Arc<WasmHost>,
) -> Result<()> {
    let manifest = manifest_with_defaults(fs, directory, manifest).await?;
    validate_git_theme_resources(fs, directory, &manifest).await?;
    validate_git_snippet_resources(fs, directory, &manifest).await?;
    validate_git_language_resources(fs, directory, &manifest, &wasm_host.proxy).await?;
    validate_grammar_artifacts(fs, directory, &manifest).await?;
    let mut staged_index = ExtensionIndex::default();
    ExtensionStore::add_extension_to_index(
        fs.clone(),
        directory.to_path_buf(),
        sources_dir,
        &mut staged_index,
        wasm_host.proxy.clone(),
    )
    .await
    .context("indexing the prepared extension")?;

    let entry = staged_index
        .extensions
        .get(&manifest.id)
        .context("the prepared extension is missing from the staged index")?;
    validate_git_theme_resources(fs, directory, &entry.manifest).await?;
    validate_git_snippet_resources(fs, directory, &entry.manifest).await?;
    validate_git_language_resources(fs, directory, &entry.manifest, &wasm_host.proxy).await?;
    if manifest.lib.kind.is_some() || entry.manifest.lib.kind.is_some() {
        let wasm_bytes = fs
            .load_bytes(&directory.join("extension.wasm"))
            .await
            .context("reading the prepared wasm extension")?;
        wasm_host
            .validate_extension(wasm_bytes, entry.manifest.clone())
            .await
            .context("validating the prepared wasm extension")?;
    }
    Ok(())
}

async fn validate_git_compiled_artifacts(
    executor: &BackgroundExecutor,
    fs: &Arc<dyn Fs>,
    extension_dir: &Path,
    manifest: &ExtensionManifest,
    wasm_host: &WasmHost,
    owner: GitCommandOwner,
) -> Result<()> {
    let wasm_path = extension_dir.join("extension.wasm");
    let requires_wasm = manifest.lib.kind.is_some()
        || fs.metadata(&extension_dir.join("Cargo.toml")).await?.is_some()
        || fs.metadata(&wasm_path).await?.is_some()
        // Reindexing a missing prebuilt library loses its inferred lib.kind.
        || !run_git(executor, extension_dir, &["ls-tree", "--name-only", "HEAD", "--", "extension.wasm"], owner)
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
    validate_grammar_artifacts(fs, extension_dir, manifest).await
}

async fn validate_grammar_artifacts(
    fs: &Arc<dyn Fs>,
    extension_dir: &Path,
    manifest: &ExtensionManifest,
) -> Result<()> {
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

async fn remote_commit(
    executor: &BackgroundExecutor,
    fs: &Arc<dyn Fs>,
    staging_dir: &Path,
    source: &GitExtensionSource,
) -> Result<Option<String>> {
    fs.create_dir(staging_dir).await?;
    // Relative repository URLs must resolve from the same staging-child layout
    // as prepare_git_source, even after its checkout moves into sources/.
    let directory = Arc::new(tempfile::tempdir_in(staging_dir)?);
    let owner = GitCommandOwner::Scratch(directory.clone());
    run_git(executor, directory.path(), &["init"], owner.clone()).await?;
    let reference = source.reference.as_deref().unwrap_or("HEAD");
    let peeled = format!("{reference}^{{}}");
    let output = run_git(
        executor,
        directory.path(),
        &["ls-remote", "--", &source.url, reference, &peeled],
        owner,
    )
    .await?;
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
        let publisher = ExtensionPublisher::new(
            fs.clone(),
            staging_dir.clone(),
            sources_dir.clone(),
            index_path.clone(),
            installed_dir.clone(),
            cx.background_executor().clone(),
        );

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
            publisher,
            git_extension_failures: BTreeMap::default(),
            undelivered_git_extension_failures: BTreeSet::default(),
            index_path,
            builder: Arc::new(ExtensionBuilder::new(builder_client, build_dir)),
            outstanding_operations: Default::default(),
            #[cfg(test)]
            auto_install_prepared_observer: None,
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
            initial_maintenance: Task::ready(()).shared(),
        };

        cx.foreground_executor()
            .block_on(async { this.publisher.lock_for_read().await })
            .context("recovering an interrupted extension transaction")
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

        let publisher = this.publisher.clone();
        let fs = this.fs.clone();
        let index_path = this.index_path.clone();
        let load_initial_extensions = cx
            .spawn(async move |this, cx| {
                let Some(filesystem_guard) = publisher.lock_for_read().await.log_err() else {
                    return;
                };
                // A publication may invalidate the cache before this task acquires the gate.
                let (extension_index, _filesystem_guard) =
                    if extension_index_needs_rebuild || !fs.is_file(&index_path).await {
                        let Some(rebuild) = this
                            .update(cx, |this, cx| {
                                this.rebuild_extension_index(filesystem_guard, cx)
                            })
                            .log_err()
                        else {
                            return;
                        };
                        let Some(rebuilt) = rebuild.await.log_err() else {
                            return;
                        };
                        rebuilt
                    } else {
                        (extension_index, filesystem_guard)
                    };
                if let Some(load) = this
                    .update(cx, |this, cx| this.extensions_updated(extension_index, cx))
                    .log_err()
                {
                    load.await;
                }
            })
            .shared();
        let initial_index_load = load_initial_extensions.clone();
        this.initial_index_load = initial_index_load.clone();

        this.initial_maintenance = cx
            .spawn(async move |this, cx| {
                initial_index_load.await;
                this.update(cx, |this, cx| this.auto_install_extensions(cx))
                    .ok();
                if let Some(update) = this
                    .update(cx, |this, cx| this.check_for_updates_task(cx))
                    .log_err()
                {
                    update.await.log_err();
                }
            })
            .shared();

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
                                let publisher = this.read_with(cx, |this, _| this.publisher.clone())?;
                                let Some(filesystem_guard) = publisher.lock_for_read().await.log_err() else {
                                    this.update(cx, |this, _| {
                                        for sender in this.reload_complete_senders.drain(..) {
                                            sender.send(()).ok();
                                        }
                                    })?;
                                    index_changed = false;
                                    continue;
                                };
                                let (index, _filesystem_guard) = this
                                    .update(cx, |this, cx| this.rebuild_extension_index(filesystem_guard, cx))?
                                    .await?;
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

        self.install_auto_extensions(extensions_to_install, git_extensions_to_install, cx)
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
        self.install_auto_extensions(extension_ids, Vec::new(), cx)
    }

    pub fn install_auto_extensions(
        &mut self,
        marketplace_extensions: Vec<Arc<str>>,
        git_extensions: Vec<(Arc<str>, GitExtensionSource)>,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        cx.spawn(async move |this, cx| {
            let mut downloads = FuturesUnordered::new();
            let extensions = marketplace_extensions
                .into_iter()
                .map(|extension_id| (extension_id, None))
                .chain(
                    git_extensions
                        .into_iter()
                        .map(|(id, source)| (id, Some(source))),
                );
            for (extension_id, source) in extensions {
                let git_url = source.as_ref().map(|source| source.url.clone());
                let operation = match Self::begin_operation(
                    &this,
                    &extension_id,
                    ExtensionOperation::Install,
                    cx,
                ) {
                    Ok(operation) => operation,
                    Err(error) => {
                        Self::record_auto_install_result(
                            &this,
                            &extension_id,
                            git_url.as_deref(),
                            &Err(error),
                            cx,
                        )
                        .log_err();
                        continue;
                    }
                };
                let download = this.update(cx, |this, cx| {
                    let prepare = if let Some(source) = source {
                        this.prepare_git_extension(extension_id.clone(), source, cx)
                            .map(|result| result.map(PreparedAutoExtension::Git))
                            .boxed()
                    } else {
                        let url = this.latest_extension_url(&extension_id, cx)?;
                        this.prepare_marketplace_extension(extension_id.clone(), url, cx)
                            .map(|result| result.map(PreparedAutoExtension::Marketplace))
                            .boxed()
                    };
                    let executor = cx.background_executor().clone();
                    #[cfg(test)]
                    let observer = if this
                        .auto_install_prepared_observer
                        .as_ref()
                        .is_some_and(|(id, _)| id == &extension_id)
                    {
                        this.auto_install_prepared_observer
                            .take()
                            .map(|(_, observer)| observer)
                    } else {
                        None
                    };
                    anyhow::Ok(cx.background_spawn(async move {
                        let result = with_extension_timeout(
                            &executor,
                            EXTENSION_PREPARATION_TIMEOUT,
                            prepare,
                        )
                        .await
                        .context("preparing extension candidate");
                        #[cfg(test)]
                        if result.is_ok()
                            && let Some(observer) = observer
                        {
                            observer.send(()).expect("candidate preparation observer");
                        }
                        result
                    }))
                });
                match download {
                    Ok(Ok(download)) => {
                        downloads.push(async move {
                            (extension_id, operation, git_url, download.await)
                        });
                    }
                    Ok(Err(error)) | Err(error) => {
                        Self::record_auto_install_result(
                            &this,
                            &extension_id,
                            git_url.as_deref(),
                            &Err(error),
                            cx,
                        )
                        .log_err();
                    }
                }
            }
            let mut pending: Vec<(_, _, _, PreparedAutoExtension)> = Vec::new();
            loop {
                let ready = pending.iter().position(|(extension_id, _, _, prepared)| {
                    // An unresolved manifest can introduce or remove any shared grammar.
                    // Preparation deadlines bound this wait; candidates using only
                    // native or their own grammars can proceed immediately.
                    if !downloads.is_empty() {
                        return prepared.external_grammars().is_empty();
                    }
                    !pending.iter().any(|(other_id, _, _, provider)| {
                        other_id != extension_id
                            && prepared
                                .external_grammars()
                                .iter()
                                .any(|name| provider.affected_grammars().contains(name))
                    })
                });
                let Some(ready) = ready else {
                    if let Some((extension_id, operation, git_url, result)) = downloads.next().await
                    {
                        match result {
                            Ok(prepared) => {
                                pending.push((extension_id, operation, git_url, prepared));
                            }
                            Err(error) => {
                                Self::record_auto_install_result(
                                    &this,
                                    &extension_id,
                                    git_url.as_deref(),
                                    &Err(error),
                                    cx,
                                )
                                .log_err();
                            }
                        }
                        continue;
                    }
                    for (extension_id, _, git_url, prepared) in &pending {
                        let error = anyhow!(
                            "cyclic grammar dependencies {:?}",
                            prepared.external_grammars()
                        );
                        Self::record_auto_install_result(
                            &this,
                            extension_id,
                            git_url.as_deref(),
                            &Err(error),
                            cx,
                        )
                        .log_err();
                    }
                    break;
                };
                let (extension_id, _operation, git_url, prepared) = pending.remove(ready);
                let result = match prepared {
                    PreparedAutoExtension::Marketplace(prepared) => {
                        Self::install_prepared_marketplace_extension(
                            &this,
                            &extension_id,
                            prepared,
                            ExtensionOperation::Install,
                            cx,
                        )
                        .await
                    }
                    PreparedAutoExtension::Git(prepared) => {
                        Self::install_prepared_git_extension(
                            &this,
                            &extension_id,
                            prepared.prepared,
                            cx,
                        )
                        .await
                    }
                };
                Self::record_auto_install_result(
                    &this,
                    &extension_id,
                    git_url.as_deref(),
                    &result,
                    cx,
                )
                .log_err();
            }
        })
    }

    fn record_auto_install_result(
        this: &WeakEntity<Self>,
        extension_id: &Arc<str>,
        git_url: Option<&str>,
        result: &Result<()>,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        this.update(cx, |this, cx| {
            if let Some(url) = git_url {
                this.record_git_extension_result(extension_id, url, result, cx);
            } else if let Err(error) = result {
                log::error!("failed to install extension {extension_id}: {error:#}");
            }
        })
    }

    pub fn install_git_extensions(
        &mut self,
        extensions: Vec<(Arc<str>, GitExtensionSource)>,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        self.install_auto_extensions(Vec::new(), extensions, cx)
    }

    pub fn git_extensions_to_install(
        &self,
        extension_settings: &ExtensionSettings,
    ) -> Vec<(Arc<str>, GitExtensionSource)> {
        extension_settings
            .git_sources()
            .filter(|(extension_id, _)| !SUPPRESSED_EXTENSIONS.contains(extension_id.as_ref()))
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
        self.check_for_updates_task(cx).detach_and_log_err(cx);
    }

    fn check_for_updates_task(&mut self, cx: &mut Context<Self>) -> Task<Result<()>> {
        let task = self.fetch_extensions_with_update_available(cx);
        cx.spawn(async move |this, cx| Self::upgrade_extensions(this, task.await?, cx).await)
            .detach();
        self.check_for_git_updates(cx)
    }

    pub fn check_for_git_updates(&mut self, cx: &mut Context<Self>) -> Task<Result<()>> {
        let mut extensions = self
            .extension_index
            .extensions
            .iter()
            .filter_map(|(extension_id, entry)| entry.source.as_ref().map(|_| extension_id.clone()))
            .collect::<BTreeSet<_>>();
        extensions.extend(
            ExtensionSettings::get_global(cx)
                .git_sources()
                .filter(|(extension_id, _)| is_single_path_component(extension_id))
                .map(|(extension_id, _)| extension_id.clone()),
        );
        let fs = self.fs.clone();
        let builder = self.builder.clone();
        let staging_dir = self.staging_dir.clone();
        let sources_dir = self.sources_dir.clone();
        let installed_dir = self.installed_dir.clone();
        let publisher = self.publisher.clone();
        let proxy = self.proxy.clone();
        let wasm_host = self.wasm_host.clone();
        let git_source_lock = self.git_source_lock.clone();

        cx.spawn(async move |this, cx| {
            {
                let _source_guard = git_source_lock.lock().await;
                let _filesystem_guard = publisher.lock_for_read().await?;
                if fs.metadata(&sources_dir).await?.is_some() {
                    let mut records = fs.read_dir(&sources_dir).await?;
                    while let Some(record) = records.next().await {
                        let record = record?;
                        if record.extension() == Some(std::ffi::OsStr::new("toml"))
                            && let Some(extension_id) = record.file_stem().and_then(|id| id.to_str())
                            && is_single_path_component(extension_id)
                            && fs.is_file(&record).await
                        {
                            extensions.insert(extension_id.into());
                        }
                    }
                }
            }
            for extension_id in extensions.into_iter().filter(|id| !SUPPRESSED_EXTENSIONS.contains(id.as_ref())) {
                let _source_guard = git_source_lock.lock().await;
                publisher.recover().await?;
                let recorded_source = load_git_source_record(&fs, &sources_dir, &extension_id).await;
                let Some((indexed_source, auto_update, needs_reload)) = this.read_with(cx, |this, cx| {
                    let extension_settings = ExtensionSettings::get_global(cx);
                    let configured_source = extension_settings
                        .git_sources()
                        .find(|(id, _)| *id == &extension_id)
                        .map(|(_, source)| source);
                    let source = match this.extension_index.extensions.get(&extension_id) {
                        Some(entry) => entry.source.as_ref().map(|indexed| recorded_source.as_ref().unwrap_or(indexed)),
                        None => recorded_source.as_ref(),
                    };
                    source
                        .filter(|installed| {
                            configured_source
                                .as_ref()
                                .is_none_or(|source| installed.matches(source))
                        })
                        .map(|source| {
                            (
                                source.clone(),
                                extension_settings.should_auto_update(&extension_id),
                                this.extension_index.extensions.get(&extension_id)
                                    .and_then(|entry| entry.source.as_ref()) != Some(source),
                            )
                        })
                })?
                else {
                    let owns_checkout = if recorded_source.is_none() {
                        let _filesystem_guard = publisher.lock_for_read().await?;
                        let checkout = git_source_dir(&sources_dir, &extension_id);
                        let checkout_exists = fs.metadata(&checkout).await?.is_some_and(|metadata| metadata.is_dir);
                        let installed = installed_dir.join(extension_id.as_ref());
                        match fs.metadata(&installed).await? {
                            None => {
                                checkout_exists || (
                                    !this.read_with(cx, |this, _| this.extension_index.extensions.contains_key(&extension_id))?
                                    && fs.metadata(&git_source_record_path(&sources_dir, &extension_id)).await?.is_some_and(|metadata| !metadata.is_dir)
                                )
                            }
                            Some(metadata) if metadata.is_symlink => {
                                match fs.canonicalize(&installed).await {
                                    Ok(target) => checkout_exists && target.starts_with(fs.canonicalize(&checkout).await?),
                                    Err(error) if error.downcast_ref::<std::io::Error>()
                                        .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) => {
                                        let target = installed_dir.join(fs.read_link(&installed).await?);
                                        let target = normalize_lexically(SanitizedPath::new(&target).as_path())?;
                                        let checkout = normalize_lexically(SanitizedPath::new(&checkout).as_path())?;
                                        target.starts_with(checkout)
                                    }
                                    Err(error) => return Err(error),
                                }
                            }
                            Some(_) => false,
                        }
                    } else {
                        false
                    };
                    if owns_checkout {
                        this.update(cx, |this, cx| {
                            let configured_source = ExtensionSettings::get_global(cx)
                                .git_sources().find(|(id, _)| *id == &extension_id)
                                .map(|(_, source)| source);
                            let url = configured_source.map(|source| source.url).unwrap_or_default();
                            this.record_git_extension_result(
                                &extension_id, &url,
                                &Err(anyhow!("the source record is missing or unreadable; the installed Git commit cannot be recovered")), cx,
                            );
                        })?;
                    }
                    continue;
                };
                let indexed_url = indexed_source.url.clone();
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
                        let publisher = publisher.clone();
                        let proxy = proxy.clone();
                        let wasm_host = wasm_host.clone();
                        let extension_id = extension_id.clone();
                        let executor = cx.background_executor().clone();
                        async move {
                            let installed =
                                load_git_source_record(&fs, &sources_dir, &extension_id).await;
                            let record_missing = installed.is_none();
                            let installed = match installed {
                                Some(installed) => installed,
                                None if !auto_update => indexed_source,
                                None => return GitUpdate::RecordMissing,
                            };
                            let source = installed.source();
                            let result = async {
                                let checkout_intact = async {
                                    let filesystem_guard = Arc::new(publisher.lock_for_read().await?);
                                    anyhow::ensure!(
                                        !record_missing,
                                        "the installed source record is missing or unreadable"
                                    );
                                    let extension_path = git_source_extension_path(
                                        &sources_dir,
                                        &extension_id,
                                        source.path.as_deref(),
                                    );
                                    let installed_path = installed_dir.join(extension_id.as_ref());
                                    anyhow::ensure!(
                                        fs.metadata(&installed_path).await?.is_some_and(|metadata| metadata.is_symlink),
                                        "the installed Git extension link is missing"
                                    );
                                    anyhow::ensure!(
                                        fs.canonicalize(&installed_path).await? == fs.canonicalize(&extension_path).await?,
                                        "the installed Git extension link points to another checkout"
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
                                    let manifest = manifest_with_defaults(
                                        &fs, &extension_path, (*entry.manifest).clone(),
                                    ).await?;
                                    validate_git_theme_resources(&fs, &extension_path, &manifest)
                                        .await?;
                                    validate_git_snippet_resources(&fs, &extension_path, &manifest)
                                        .await?;
                                    validate_git_language_resources(
                                        &fs,
                                        &extension_path,
                                        &manifest,
                                        &proxy,
                                    )
                                    .await?;
                                    validate_git_compiled_artifacts(
                                        &executor,
                                        &fs,
                                        &extension_path,
                                        &manifest,
                                        &wasm_host,
                                        GitCommandOwner::Checkout(filesystem_guard.clone()),
                                    )
                                    .await?;
                                    validate_git_checkout_tree(
                                        &executor,
                                        &fs,
                                        &git_source_dir(&sources_dir, &extension_id),
                                        &manifest,
                                        &installed,
                                        GitCommandOwner::Checkout(filesystem_guard),
                                    ).await?;
                                    Ok(())
                                }
                                .await;
                                if checkout_intact.is_ok() {
                                    if !auto_update {
                                        return anyhow::Ok(needs_reload);
                                    }
                                    match remote_commit(&executor, &fs, &staging_dir, &source).await? {
                                        Some(remote_commit)
                                            if remote_commit == installed.commit =>
                                        {
                                            return anyhow::Ok(needs_reload);
                                        }
                                        Some(_) => {}
                                        None => {
                                            let reference =
                                                source.reference.as_deref().unwrap_or("HEAD");
                                            anyhow::ensure!(
                                                is_commit_sha(reference),
                                                "the repository no longer advertises {reference}"
                                            );
                                            return Ok(needs_reload);
                                        }
                                    }
                                }
                                build_git_source(
                                    &executor,
                                    fs,
                                    &builder,
                                    &staging_dir,
                                    &sources_dir,
                                    &publisher,
                                    wasm_host,
                                    &extension_id,
                                    &source,
                                    (!auto_update).then_some(installed.commit.as_str()),
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

    fn prepare_git_extension(
        &self,
        extension_id: Arc<str>,
        source: GitExtensionSource,
        cx: &Context<Self>,
    ) -> Task<Result<PreparedGitExtension>> {
        let fs = self.fs.clone();
        let staging_dir = self.staging_dir.clone();
        let proxy = self.proxy.clone();
        let sources_dir = self.sources_dir.clone();
        let publisher = self.publisher.clone();
        let source_lock = self.git_source_lock.clone();
        let auto_update = ExtensionSettings::get_global(cx).should_auto_update(&extension_id);
        let executor = cx.background_executor().clone();
        let indexed_source = self
            .extension_index
            .extensions
            .get(&extension_id)
            .and_then(|entry| entry.source.clone());
        let mut affected_grammars = self
            .extension_manifest_for_id(&extension_id)
            .map(|manifest| manifest.grammars.keys().cloned().collect::<BTreeSet<_>>())
            .unwrap_or_default();
        cx.background_spawn(async move {
            validate_git_install(&extension_id, &source)?;
            let checkout_commit = if !auto_update
                && indexed_source.as_ref().is_none_or(|installed| installed.matches(&source))
            {
                let _source_guard = source_lock.lock().await;
                let _filesystem_guard = publisher.lock_for_read().await?;
                let record_path = git_source_record_path(&sources_dir, &extension_id);
                let record_exists = fs.metadata(&record_path).await?.is_some();
                let recorded_source = if record_exists {
                    let result = fs.load(&record_path).await.and_then(|contents| {
                        toml::from_str::<InstalledGitSource>(&contents).map_err(Into::into)
                    });
                    match result {
                        Ok(record) => Some(record),
                        Err(error) if indexed_source.is_none() => {
                            return Err(error).context("recovering the installed commit of a pinned Git extension");
                        }
                        Err(error) => {
                            log::warn!("using the cached source of pinned extension {extension_id}: {}", redact_command(&format!("{error:#}")));
                            indexed_source.clone()
                        }
                    }
                } else {
                    indexed_source.clone()
                };
                anyhow::ensure!(
                    recorded_source.is_some()
                        || fs.metadata(&git_source_dir(&sources_dir, &extension_id)).await?.is_none(),
                    "cannot recover the installed commit of a pinned Git extension without its source record"
                );
                recorded_source
                    .filter(|installed| installed.matches(&source))
                    .map(|installed| installed.commit)
            } else {
                None
            };
            let prepared = prepare_git_source(
                &executor, fs.clone(), &staging_dir, &extension_id, &source, checkout_commit.as_deref(),
            ).await?;
            affected_grammars.extend(prepared.manifest.grammars.keys().cloned());
            let directory = git_source_subdirectory(
                prepared.checkout.path().to_path_buf(),
                prepared.source.path.as_deref(),
            );
            let external_grammars =
                external_grammar_dependencies(&fs, &directory, &prepared.manifest, &proxy).await?;
            Ok(PreparedGitExtension {
                prepared,
                external_grammars,
                affected_grammars,
            })
        })
    }

    async fn install_prepared_git_extension(
        this: &WeakEntity<Self>,
        extension_id: &Arc<str>,
        prepared: PreparedGitSource,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        let (fs, builder, sources_dir, publisher, wasm_host, source_lock) =
            this.read_with(cx, |this, _| {
                (
                    this.fs.clone(),
                    this.builder.clone(),
                    this.sources_dir.clone(),
                    this.publisher.clone(),
                    this.wasm_host.clone(),
                    this.git_source_lock.clone(),
                )
            })?;
        let _source_guard = source_lock.lock().await;
        let result = cx
            .background_spawn({
                let extension_id = extension_id.clone();
                let executor = cx.background_executor().clone();
                async move {
                    publisher.recover().await?;
                    publish_prepared_git_source(
                        &executor,
                        fs,
                        &builder,
                        &sources_dir,
                        &publisher,
                        wasm_host,
                        &extension_id,
                        prepared,
                    )
                    .await
                }
            })
            .await;
        if let Err(error) = result {
            this.update(cx, |this, cx| this.reload(None, cx))?.await;
            return Err(error);
        }
        Self::finish_extension_install(this, extension_id, ExtensionOperation::Install, cx).await
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
        let publisher = self.publisher.clone();
        let wasm_host = self.wasm_host.clone();
        let git_source_lock = self.git_source_lock.clone();

        cx.spawn(async move |this, cx| {
            let _source_guard = git_source_lock.lock().await;
            let _operation =
                Self::begin_operation(&this, &extension_id, ExtensionOperation::Install, cx)?;
            let result = cx
                .background_spawn({
                    let fs = fs.clone();
                    let extension_id = extension_id.clone();
                    let executor = cx.background_executor().clone();
                    async move {
                        build_git_source(
                            &executor,
                            fs,
                            &builder,
                            &staging_dir,
                            &sources_dir,
                            &publisher,
                            wasm_host,
                            &extension_id,
                            &source,
                            None,
                        )
                        .await
                    }
                })
                .await;
            if let Err(error) = result {
                this.update(cx, |this, cx| this.reload(None, cx))?.await;
                return Err(error);
            }
            Self::finish_extension_install(&this, &extension_id, ExtensionOperation::Install, cx)
                .await
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
        cx.spawn(async move |this, cx| {
            let _operation = Self::begin_operation(&this, &extension_id, operation, cx)?;
            let prepared = this
                .update(cx, |this, cx| {
                    this.prepare_marketplace_extension(extension_id.clone(), url, cx)
                })?
                .await?;
            Self::install_prepared_marketplace_extension(
                &this,
                &extension_id,
                prepared,
                operation,
                cx,
            )
            .await
        })
    }

    fn prepare_marketplace_extension(
        &self,
        extension_id: Arc<str>,
        url: Url,
        cx: &Context<Self>,
    ) -> Task<Result<PreparedMarketplaceExtension>> {
        if !is_single_path_component(&extension_id) {
            return Task::ready(Err(anyhow!("extension id must be a single path component")));
        }
        let staging_dir = self.staging_dir.clone();
        let http_client = self.http_client.clone();
        let fs = self.fs.clone();
        let executor = cx.background_executor().clone();
        let proxy = self.proxy.clone();
        let mut affected_grammars = self
            .extension_index
            .extensions
            .get(&extension_id)
            .map(|entry| {
                entry
                    .manifest
                    .grammars
                    .keys()
                    .cloned()
                    .collect::<BTreeSet<_>>()
            })
            .unwrap_or_default();
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

            let staged = StagedDirectory::new(fs.clone(), &staging_dir, executor)
                .await
                .context("staging the downloaded extension")?;
            let directory = staged.path().join(extension_id.as_ref());
            fs.create_dir(&directory).await?;
            let decompressed =
                std::pin::pin!(GzipDecoder::new(BufReader::new(tar_gz_bytes.as_slice())));
            let reader: std::pin::Pin<&mut (dyn futures::AsyncRead + Send)> = decompressed;
            fs.extract_tar_file(&directory, Archive::new(reader))
                .await
                .context("unpacking the downloaded extension")?;
            let manifest = ExtensionManifest::load(fs.clone(), &directory).await?;
            anyhow::ensure!(
                manifest.id == extension_id,
                "downloaded extension declares id {}, expected {extension_id}",
                manifest.id
            );
            let manifest = manifest_with_defaults(&fs, &directory, manifest).await?;
            // Consumers also wait for replacements which remove an installed grammar.
            affected_grammars.extend(manifest.grammars.keys().cloned());
            let external_grammars =
                external_grammar_dependencies(&fs, &directory, &manifest, &proxy).await?;
            Ok(PreparedMarketplaceExtension {
                staged,
                manifest,
                external_grammars,
                affected_grammars,
            })
        })
    }

    async fn install_prepared_marketplace_extension(
        this: &WeakEntity<Self>,
        extension_id: &Arc<str>,
        prepared: PreparedMarketplaceExtension,
        operation: ExtensionOperation,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        let (fs, sources_dir, wasm_host, publisher, source_lock) =
            this.read_with(cx, |this, _| {
                (
                    this.fs.clone(),
                    this.sources_dir.clone(),
                    this.wasm_host.clone(),
                    this.publisher.clone(),
                    this.git_source_lock.clone(),
                )
            })?;
        let _source_guard = source_lock.lock().await;
        let directory = prepared.staged.path().join(extension_id.as_ref());
        cx.background_spawn({
            let directory = directory.clone();
            let publisher = publisher.clone();
            async move {
                let _filesystem_guard = publisher.lock_for_read().await?;
                validate_prepared_extension(
                    &fs,
                    &directory,
                    &sources_dir,
                    prepared.manifest,
                    &wasm_host,
                )
                .await
            }
        })
        .await?;
        let result = publisher
            .publish(
                extension_id,
                PreparedExtension::Marketplace {
                    staged: prepared.staged,
                },
            )
            .await;
        if let Err(error) = result {
            this.update(cx, |this, cx| this.reload(None, cx))?.await;
            return Err(error);
        }
        this.update(cx, |this, cx| {
            this.clear_git_extension_failure(extension_id, cx)
        })?;
        Self::finish_extension_install(this, extension_id, operation, cx).await
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

        let url = match self.latest_extension_url(&extension_id, cx) {
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

    fn latest_extension_url(&self, extension_id: &str, cx: &App) -> Result<Url> {
        let schema_versions = schema_version_range();
        let wasm_api_versions = wasm_api_version_range(ReleaseChannel::global(cx));

        self.http_client.build_zed_api_url(
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
        let work_dir = self.wasm_host.work_dir.join(extension_id.as_ref());
        let publisher = self.publisher.clone();
        let git_source_lock = self.git_source_lock.clone();
        let fs = self.fs.clone();

        let extension_manifest = self.extension_manifest_for_id(&extension_id).cloned();
        match self.outstanding_operations.entry(extension_id.clone()) {
            btree_map::Entry::Occupied(_) => {
                return Task::ready(Err(anyhow!(
                    "extension {extension_id} already has an operation in progress"
                )));
            }
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

            let _source_guard = git_source_lock.lock().await;
            let result = publisher
                .publish(&extension_id, PreparedExtension::Remove)
                .await;
            if let Err(error) = result {
                extension_store
                    .update(cx, |store, cx| store.reload(None, cx))?
                    .await;
                return Err(error);
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
        self.install_dev_extension_with_operation(
            extension_source_path,
            ExtensionOperation::Install,
            cx,
        )
    }

    fn install_dev_extension_with_operation(
        &mut self,
        extension_source_path: PathBuf,
        operation: ExtensionOperation,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let extensions_dir = self.extensions_dir();
        let sources_dir = self.sources_dir.clone();
        let staging_dir = self.staging_dir.clone();
        let publisher = self.publisher.clone();
        let wasm_host = self.wasm_host.clone();
        let git_source_lock = self.git_source_lock.clone();
        let fs = self.fs.clone();
        let builder = self.builder.clone();

        cx.spawn(async move |this, cx| {
            let _source_guard = git_source_lock.lock().await;
            fs.create_dir(&staging_dir).await?;
            publisher.recover().await?;
            let mut extension_manifest =
                ExtensionManifest::load(fs.clone(), &extension_source_path).await?;
            let extension_id = extension_manifest.id.clone();
            anyhow::ensure!(
                is_single_path_component(&extension_id),
                "extension id must be a single path component"
            );
            let resolved_source = fs.canonicalize(&extension_source_path).await?;
            let output_path = extensions_dir.join(extension_id.as_ref());
            if let Some(metadata) = fs.metadata(&output_path).await?
                && !metadata.is_symlink
            {
                anyhow::ensure!(
                    !resolved_source.starts_with(fs.canonicalize(&output_path).await?),
                    "cannot replace an installed directory with a link into itself"
                );
            }
            let checkout = git_source_dir(&sources_dir, &extension_id);
            let replacement = if fs.metadata(&checkout).await?.is_some()
                && resolved_source.starts_with(fs.canonicalize(&checkout).await?)
            {
                GitSourceDisposition::Retain
            } else {
                GitSourceDisposition::Retire
            };
            if matches!(replacement, GitSourceDisposition::Retire) {
                for managed_root in [&sources_dir, &extensions_dir] {
                    if fs.metadata(managed_root).await?.is_some() {
                        anyhow::ensure!(
                            !resolved_source.starts_with(fs.canonicalize(managed_root).await?),
                            "cannot link a local extension into another managed extension"
                        );
                    }
                }
            }

            let _operation = Self::begin_operation(&this, &extension_id, operation, cx)?;

            if matches!(replacement, GitSourceDisposition::Retain) {
                let installed = load_git_source_record(&fs, &sources_dir, &extension_id)
                    .await
                    .context("managed Git source record is missing or unreadable")?;
                let recorded_path = git_source_extension_path(
                    &sources_dir,
                    &extension_id,
                    installed.path.as_deref(),
                );
                anyhow::ensure!(
                    resolved_source == fs.canonicalize(&recorded_path).await?,
                    "local target differs from the managed Git extension directory"
                );
                let result = cx
                    .background_spawn({
                        let fs = fs.clone();
                        let extension_id = extension_id.clone();
                        let executor = cx.background_executor().clone();
                        async move {
                            build_git_source(
                                &executor,
                                fs,
                                &builder,
                                &staging_dir,
                                &sources_dir,
                                &publisher,
                                wasm_host,
                                &extension_id,
                                &installed.source(),
                                Some(&installed.commit),
                            )
                            .await
                        }
                    })
                    .await;
                if let Err(error) = result {
                    this.update(cx, |this, cx| this.reload(None, cx))?.await;
                    return Err(error);
                }
                Self::finish_extension_install(&this, &extension_id, operation, cx).await?;
                this.update(cx, |this, cx| {
                    this.clear_git_extension_failure(&extension_id, cx)
                })?;
                return Ok(());
            }

            cx.background_spawn({
                let extension_source_path = extension_source_path.clone();
                let fs = fs.clone();
                let publisher = publisher.clone();
                let sources_dir = sources_dir.clone();
                let executor = cx.background_executor().clone();
                async move {
                    let filesystem_guard = publisher.lock_for_read().await?;
                    publisher
                        .complete_mutation(async move {
                            let _filesystem_guard = filesystem_guard;
                            builder
                                .compile_extension_with_timeout(
                                    &extension_source_path,
                                    &mut extension_manifest,
                                    CompileExtensionOptions::dev(),
                                    fs.clone(),
                                    executor,
                                    EXTENSION_BUILD_TIMEOUT,
                                )
                                .await?;
                            validate_prepared_extension(
                                &fs,
                                &extension_source_path,
                                &sources_dir,
                                extension_manifest,
                                &wasm_host,
                            )
                            .await
                        })
                        .await
                }
            })
            .await
            .inspect_err(|error| {
                util::log_err(error);
            })?;

            let staged = tempfile::tempdir_in(&staging_dir)?;
            let staged_link = staged.path().join(extension_id.as_ref());
            fs.create_symlink(&staged_link, resolved_source).await?;
            let result = publisher
                .publish(
                    &extension_id,
                    PreparedExtension::Local {
                        staged,
                        git_source: replacement,
                    },
                )
                .await;
            if let Err(error) = result {
                this.update(cx, |this, cx| this.reload(None, cx))?.await;
                return Err(error);
            }
            Self::finish_extension_install(&this, &extension_id, operation, cx).await?;
            this.update(cx, |this, cx| {
                this.clear_git_extension_failure(&extension_id, cx);
            })?;
            Ok(())
        })
    }

    async fn finish_extension_install(
        this: &WeakEntity<Self>,
        extension_id: &Arc<str>,
        operation: ExtensionOperation,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        this.update(cx, |this, cx| this.reload(Some(extension_id.clone()), cx))?
            .await;
        if let ExtensionOperation::Install = operation {
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
        }

        Ok(())
    }

    pub fn rebuild_dev_extension(&mut self, extension_id: Arc<str>, cx: &mut Context<Self>) {
        let path = self.installed_dir.join(extension_id.as_ref());
        self.install_dev_extension_with_operation(path, ExtensionOperation::Upgrade, cx)
            .detach_and_log_err(cx);
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

    fn rebuild_extension_index(
        &self,
        filesystem_guard: OwnedMutexGuard<()>,
        cx: &mut Context<Self>,
    ) -> Task<Result<(ExtensionIndex, OwnedMutexGuard<()>)>> {
        let fs = self.fs.clone();
        let work_dir = self.wasm_host.work_dir.clone();
        let extensions_dir = self.installed_dir.clone();
        let sources_dir = self.sources_dir.clone();
        let index_path = self.index_path.clone();
        let proxy = self.proxy.clone();
        let publisher = self.publisher.clone();
        cx.background_spawn(async move {
            #[cfg(test)]
            let hook_publisher = publisher.clone();
            publisher
                .complete_mutation(async move {
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
                        #[cfg(test)]
                        hook_publisher.before_index_save().await?;
                        fs.save(&index_path, &index_json.as_str().into(), Default::default())
                            .await
                            .context("failed to save extension index")
                            .log_err();
                    }

                    log::info!("rebuilt extension index in {:?}", start_time.elapsed());
                    Ok((index, filesystem_guard))
                })
                .await
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
