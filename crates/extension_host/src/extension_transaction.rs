use anyhow::{Context as _, Result};
use fs::{Fs, RemoveOptions, RenameOptions};
#[cfg(test)]
use futures::lock::MutexGuard;
use futures::{
    channel::oneshot,
    lock::{Mutex, OwnedMutexGuard},
};
use gpui::BackgroundExecutor;
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use util::ResultExt as _;

use super::{
    InstalledGitSource, git_source_dir, git_source_extension_path, git_source_record_path,
    is_single_path_component,
};

pub(super) const GIT_SOURCE_BACKUP_ID: &str = "extension_id";
pub(super) const GIT_SOURCE_BACKUP_CHECKOUT: &str = "checkout";
pub(super) const GIT_SOURCE_BACKUP_RECORD: &str = "record.toml";
pub(super) const GIT_SOURCE_BACKUP_INSTALLED: &str = "installed";
pub(super) const GIT_SOURCE_BACKUP_STATE: &str = "state.toml";
pub(super) const GIT_SOURCE_BACKUP_COMMITTED: &str = "committed";

#[derive(Serialize, Deserialize)]
pub(super) struct GitSourceBackupState {
    pub(super) checkout_existed: bool,
    pub(super) installed_existed: bool,
    pub(super) record: Option<String>,
}

pub(super) enum GitSourceDisposition {
    Retain,
    Retire,
}

pub(super) enum PreparedExtension {
    Git {
        checkout: tempfile::TempDir,
        source: InstalledGitSource,
    },
    Local {
        staged: tempfile::TempDir,
        git_source: GitSourceDisposition,
    },
    Marketplace {
        staged: StagedDirectory,
    },
    Remove,
}

pub(super) struct StagedDirectory {
    fs: Arc<dyn Fs>,
    path: PathBuf,
    executor: BackgroundExecutor,
    needs_cleanup: bool,
}

impl StagedDirectory {
    pub(super) async fn new(
        fs: Arc<dyn Fs>,
        staging_root: &Path,
        executor: BackgroundExecutor,
    ) -> Result<Self> {
        fs.create_dir(staging_root).await?;
        let path = staging_root.join(format!("prepared-{}", uuid::Uuid::new_v4()));
        anyhow::ensure!(
            fs.metadata(&path).await?.is_none(),
            "extension staging path already exists: {}",
            path.display()
        );
        let staged = Self {
            fs,
            path,
            executor,
            needs_cleanup: true,
        };
        staged.fs.create_dir(&staged.path).await?;
        Ok(staged)
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    pub(super) async fn cleanup(mut self) -> Result<()> {
        remove_entry(&self.fs, &self.path).await?;
        self.needs_cleanup = false;
        Ok(())
    }
}

impl Drop for StagedDirectory {
    fn drop(&mut self) {
        if self.needs_cleanup {
            let fs = self.fs.clone();
            let path = self.path.clone();
            self.executor
                .spawn(async move {
                    remove_entry(&fs, &path)
                        .await
                        .with_context(|| {
                            format!("cleaning up extension staging directory {}", path.display())
                        })
                        .log_err();
                })
                .detach();
        }
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TransactionPhase {
    BeforeBackup,
    BeforePublish,
    BeforeRecord,
    BeforeCommit,
    AfterCommit,
    BeforeCleanup,
}

#[cfg(test)]
pub(super) struct PhaseInfo {
    pub(super) phase: TransactionPhase,
    pub(super) extension_id: String,
    pub(super) installed: PathBuf,
    pub(super) checkout: PathBuf,
    pub(super) record: PathBuf,
    pub(super) backup: PathBuf,
    pub(super) index: PathBuf,
    pub(super) candidate: Option<PathBuf>,
}

#[cfg(test)]
type TransactionHook = Arc<
    dyn Fn(PhaseInfo) -> futures::future::BoxFuture<'static, Result<()>> + Send + Sync + 'static,
>;

#[cfg(test)]
type IndexSaveHook =
    Arc<dyn Fn() -> futures::future::BoxFuture<'static, Result<()>> + Send + Sync + 'static>;

#[derive(Clone)]
pub(super) struct ExtensionPublisher {
    fs: Arc<dyn Fs>,
    staging_dir: PathBuf,
    sources_dir: PathBuf,
    index_path: PathBuf,
    installed_dir: PathBuf,
    gate: Arc<Mutex<()>>,
    executor: BackgroundExecutor,
    #[cfg(test)]
    hook: Arc<parking_lot::Mutex<Option<TransactionHook>>>,
    #[cfg(test)]
    index_save_hook: Arc<parking_lot::Mutex<Option<IndexSaveHook>>>,
}

impl ExtensionPublisher {
    pub(super) fn new(
        fs: Arc<dyn Fs>,
        staging_dir: PathBuf,
        sources_dir: PathBuf,
        index_path: PathBuf,
        installed_dir: PathBuf,
        executor: BackgroundExecutor,
    ) -> Self {
        Self {
            fs,
            staging_dir,
            sources_dir,
            index_path,
            installed_dir,
            gate: Arc::new(Mutex::new(())),
            executor,
            #[cfg(test)]
            hook: Arc::default(),
            #[cfg(test)]
            index_save_hook: Arc::default(),
        }
    }

    #[cfg(test)]
    pub(super) async fn lock(&self) -> MutexGuard<'_, ()> {
        self.gate.lock().await
    }

    pub(super) async fn recover(&self) -> Result<()> {
        let publisher = self.clone();
        let guard = self.gate.clone().lock_owned().await;
        self.complete_mutation(async move {
            let _guard = guard;
            publisher.recover_locked().await
        })
        .await
    }

    pub(super) async fn recover_locked(&self) -> Result<()> {
        recover_git_source(
            &self.fs,
            &self.staging_dir,
            &self.sources_dir,
            &self.index_path,
            &self.installed_dir,
        )
        .await
    }

    pub(super) async fn lock_for_read(&self) -> Result<OwnedMutexGuard<()>> {
        let publisher = self.clone();
        let guard = self.gate.clone().lock_owned().await;
        self.complete_mutation(async move {
            recover_git_source_inner(
                &publisher.fs,
                &publisher.staging_dir,
                &publisher.sources_dir,
                &publisher.index_path,
                &publisher.installed_dir,
                RecoveryMode::Read,
            )
            .await?;
            Ok(guard)
        })
        .await
    }

    pub(super) async fn complete_mutation<T: Send + 'static>(
        &self,
        mutation: impl Future<Output = Result<T>> + Send + 'static,
    ) -> Result<T> {
        let (sender, receiver) = oneshot::channel();
        // Filesystem workers can finish a syscall after their awaiting future is dropped.
        // Keep the gate and staging owners alive until every mutation has settled.
        self.executor
            .spawn(async move {
                let result = mutation.await;
                if let Err(result) = sender.send(result) {
                    result
                        .context(
                            "finishing an extension transaction after its caller was cancelled",
                        )
                        .log_err();
                }
            })
            .detach();
        receiver
            .await
            .context("extension transaction worker exited")?
    }

    #[cfg(test)]
    pub(super) fn set_hook(
        &self,
        hook: impl Fn(PhaseInfo) -> futures::future::BoxFuture<'static, Result<()>>
        + Send
        + Sync
        + 'static,
    ) {
        *self.hook.lock() = Some(Arc::new(hook));
    }

    #[cfg(test)]
    pub(super) fn clear_hook(&self) {
        *self.hook.lock() = None;
    }

    #[cfg(test)]
    pub(super) fn set_index_save_hook(
        &self,
        hook: impl Fn() -> futures::future::BoxFuture<'static, Result<()>> + Send + Sync + 'static,
    ) {
        *self.index_save_hook.lock() = Some(Arc::new(hook));
    }

    #[cfg(test)]
    pub(super) fn clear_index_save_hook(&self) {
        *self.index_save_hook.lock() = None;
    }

    #[cfg(test)]
    pub(super) async fn before_index_save(&self) -> Result<()> {
        let hook = self.index_save_hook.lock().clone();
        if let Some(hook) = hook {
            hook().await?;
        }
        Ok(())
    }

    #[cfg(test)]
    async fn run_hook(
        &self,
        phase: TransactionPhase,
        extension_id: &str,
        candidate: &PreparedExtension,
    ) -> Result<()> {
        let hook = self.hook.lock().clone();
        if let Some(hook) = hook {
            hook(PhaseInfo {
                phase,
                extension_id: extension_id.to_string(),
                installed: self.installed_dir.join(extension_id),
                checkout: git_source_dir(&self.sources_dir, extension_id),
                record: git_source_record_path(&self.sources_dir, extension_id),
                backup: git_source_backup_dir(&self.staging_dir),
                index: self.index_path.clone(),
                candidate: match candidate {
                    PreparedExtension::Git { checkout, .. } => Some(checkout.path().to_path_buf()),
                    PreparedExtension::Local { staged, .. } => {
                        Some(staged.path().join(extension_id))
                    }
                    PreparedExtension::Marketplace { staged } => {
                        Some(staged.path().join(extension_id))
                    }
                    PreparedExtension::Remove => None,
                },
            })
            .await?;
        }
        Ok(())
    }

    pub(super) async fn publish(
        &self,
        extension_id: &str,
        candidate: PreparedExtension,
    ) -> Result<()> {
        let publisher = self.clone();
        let extension_id = extension_id.to_string();
        let guard = self.gate.clone().lock_owned().await;
        self.complete_mutation(async move {
            let _guard = guard;
            let result = publisher.publish_inner(&extension_id, &candidate).await;
            if let PreparedExtension::Marketplace { staged } = candidate {
                staged.cleanup().await.log_err();
            }
            result
        })
        .await
    }

    async fn publish_inner(&self, extension_id: &str, candidate: &PreparedExtension) -> Result<()> {
        anyhow::ensure!(
            is_single_path_component(extension_id),
            "extension id {extension_id:?} must be a single path component"
        );
        if let PreparedExtension::Git { source, .. } = candidate {
            super::validate_git_install(extension_id, &source.source())?;
        }
        self.fs.create_dir(&self.staging_dir).await?;
        self.recover_locked().await?;
        let backup = git_source_backup_dir(&self.staging_dir);
        let checkout = git_source_dir(&self.sources_dir, extension_id);
        let installed = self.installed_dir.join(extension_id);
        let record_path = git_source_record_path(&self.sources_dir, extension_id);
        let checkout_metadata = self.fs.metadata(&checkout).await?;
        let record_metadata = self.fs.metadata(&record_path).await?;
        anyhow::ensure!(
            checkout_metadata
                .as_ref()
                .is_none_or(|metadata| metadata.is_dir),
            "extension checkout collides with a source record: {}",
            checkout.display()
        );
        anyhow::ensure!(
            record_metadata
                .as_ref()
                .is_none_or(|metadata| !metadata.is_dir),
            "extension source record collides with a checkout: {}",
            record_path.display()
        );
        let state = GitSourceBackupState {
            checkout_existed: checkout_metadata.is_some(),
            installed_existed: self.fs.metadata(&installed).await?.is_some(),
            record: if record_metadata.is_some() {
                Some(self.fs.load(&record_path).await?)
            } else {
                None
            },
        };
        self.fs
            .remove_file(&self.index_path, remove_options(false))
            .await?;
        let retain_git_source = matches!(
            candidate,
            PreparedExtension::Local {
                git_source: GitSourceDisposition::Retain,
                ..
            }
        );
        let result = async {
            self.fs.create_dir(&backup).await?;
            self.fs
                .atomic_write(backup.join(GIT_SOURCE_BACKUP_ID), extension_id.to_string())
                .await?;
            self.fs
                .atomic_write(
                    backup.join(GIT_SOURCE_BACKUP_STATE),
                    toml::to_string(&state)?,
                )
                .await?;
            #[cfg(test)]
            self.run_hook(TransactionPhase::BeforeBackup, extension_id, candidate)
                .await?;

            if state.checkout_existed && !retain_git_source {
                self.fs
                    .rename(
                        &checkout,
                        &backup.join(GIT_SOURCE_BACKUP_CHECKOUT),
                        RenameOptions::default(),
                    )
                    .await?;
            }
            if let PreparedExtension::Git {
                checkout: prepared_checkout,
                ..
            } = candidate
            {
                self.fs
                    .rename(
                        prepared_checkout.path(),
                        &checkout,
                        RenameOptions {
                            create_parents: true,
                            ..RenameOptions::default()
                        },
                    )
                    .await?;
            }
            if state.installed_existed {
                self.fs
                    .rename(
                        &installed,
                        &backup.join(GIT_SOURCE_BACKUP_INSTALLED),
                        RenameOptions::default(),
                    )
                    .await?;
            }
            #[cfg(test)]
            self.run_hook(TransactionPhase::BeforePublish, extension_id, candidate)
                .await?;

            match candidate {
                PreparedExtension::Git { source, .. } => {
                    self.fs.create_dir(&self.installed_dir).await?;
                    self.fs
                        .create_symlink(
                            &installed,
                            git_source_extension_path(
                                &self.sources_dir,
                                extension_id,
                                source.path.as_deref(),
                            ),
                        )
                        .await?;
                }
                PreparedExtension::Local { staged, .. } => {
                    self.publish_entry(&staged.path().join(extension_id), &installed)
                        .await?;
                }
                PreparedExtension::Marketplace { staged } => {
                    self.publish_entry(&staged.path().join(extension_id), &installed)
                        .await?;
                }
                PreparedExtension::Remove => {}
            }
            #[cfg(test)]
            self.run_hook(TransactionPhase::BeforeRecord, extension_id, candidate)
                .await?;

            match candidate {
                PreparedExtension::Git { source, .. } => {
                    self.fs
                        .atomic_write(record_path.clone(), toml::to_string(source)?)
                        .await?;
                }
                PreparedExtension::Local {
                    git_source: GitSourceDisposition::Retain,
                    ..
                } => {}
                PreparedExtension::Local {
                    git_source: GitSourceDisposition::Retire,
                    ..
                }
                | PreparedExtension::Marketplace { .. }
                | PreparedExtension::Remove => {
                    remove_record(&self.fs, &record_path).await?;
                }
            }
            #[cfg(test)]
            self.run_hook(TransactionPhase::BeforeCommit, extension_id, candidate)
                .await?;

            // The source record alone cannot distinguish a repair from an interrupted swap.
            self.fs
                .atomic_write(backup.join(GIT_SOURCE_BACKUP_COMMITTED), String::new())
                .await
        }
        .await;

        if let Err(error) = result {
            if self
                .fs
                .metadata(&backup.join(GIT_SOURCE_BACKUP_COMMITTED))
                .await?
                .is_some()
            {
                log::error!(
                    "extension {extension_id} committed but its final write reported an error: {error:#}"
                );
                self.recover_locked()
                    .await
                    .context("cleaning up an extension replacement after its commit write failed")
                    .log_err();
                return Ok(());
            }
            self.recover_locked()
                .await
                .with_context(|| format!("recovering failed extension replacement: {error:#}"))?;
            return Err(error);
        }

        #[cfg(test)]
        for phase in [
            TransactionPhase::AfterCommit,
            TransactionPhase::BeforeCleanup,
        ] {
            if let Err(error) = self.run_hook(phase, extension_id, candidate).await {
                log::error!("cleaning up committed extension {extension_id}: {error:#}");
                return Ok(());
            }
        }
        self.recover_locked()
            .await
            .context("cleaning up a committed extension replacement")
            .log_err();
        Ok(())
    }

    async fn publish_entry(&self, prepared: &Path, installed: &Path) -> Result<()> {
        self.fs
            .rename(
                prepared,
                installed,
                RenameOptions {
                    create_parents: true,
                    ..RenameOptions::default()
                },
            )
            .await
    }
}

pub(super) fn git_source_backup_dir(staging_dir: &Path) -> PathBuf {
    staging_dir.join("git-source.previous")
}

pub(super) async fn recover_git_source(
    fs: &Arc<dyn Fs>,
    staging_dir: &Path,
    sources_dir: &Path,
    index_path: &Path,
    installed_dir: &Path,
) -> Result<()> {
    recover_git_source_inner(
        fs,
        staging_dir,
        sources_dir,
        index_path,
        installed_dir,
        RecoveryMode::Write,
    )
    .await
}

#[derive(Clone, Copy)]
enum RecoveryMode {
    Read,
    Write,
}

impl RecoveryMode {
    fn cleanup_result(self, result: Result<()>) -> Result<()> {
        match self {
            Self::Read => {
                result
                    .context("cleaning up a completed extension transaction")
                    .log_err();
                Ok(())
            }
            Self::Write => result,
        }
    }
}

async fn recover_git_source_inner(
    fs: &Arc<dyn Fs>,
    staging_dir: &Path,
    sources_dir: &Path,
    index_path: &Path,
    installed_dir: &Path,
    mode: RecoveryMode,
) -> Result<()> {
    let discarded = staging_dir.join("git-source.discarded");
    mode.cleanup_result(remove_entry(fs, &discarded).await)?;
    let backup = git_source_backup_dir(staging_dir);
    let identifier = backup.join(GIT_SOURCE_BACKUP_ID);
    if fs.metadata(&identifier).await?.is_none() {
        // The identifier is written before any live entry moves.
        return mode.cleanup_result(remove_entry(fs, &backup).await);
    }
    let extension_id = fs.load(&identifier).await?;
    anyhow::ensure!(
        is_single_path_component(&extension_id),
        "git source backup names extension {extension_id:?}"
    );
    let state_path = backup.join(GIT_SOURCE_BACKUP_STATE);
    if fs.metadata(&state_path).await?.is_some() {
        let state: GitSourceBackupState = toml::from_str(&fs.load(&state_path).await?)?;
        let committed = fs
            .metadata(&backup.join(GIT_SOURCE_BACKUP_COMMITTED))
            .await?
            .is_some();
        let invalidate = fs.remove_file(index_path, remove_options(false)).await;
        if committed {
            mode.cleanup_result(invalidate)?;
        } else {
            invalidate?;
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
                None => remove_record(fs, &record_path).await?,
            }
        }
    } else {
        let checkout = git_source_dir(sources_dir, &extension_id);
        let previous = backup.join(GIT_SOURCE_BACKUP_CHECKOUT);
        let intended_path = backup.join(GIT_SOURCE_BACKUP_RECORD);
        let intended = if fs.metadata(&intended_path).await?.is_some() {
            Some(fs.load(&intended_path).await?)
        } else {
            None
        };
        let committed = match &intended {
            Some(intended) => {
                fs.is_dir(&checkout).await
                    && fs
                        .load(&git_source_record_path(sources_dir, &extension_id))
                        .await
                        .is_ok_and(|record| &record == intended)
            }
            None => false,
        };
        let invalidate = fs.remove_file(index_path, remove_options(false)).await;
        if committed {
            mode.cleanup_result(invalidate)?;
        } else {
            invalidate?;
            let has_previous = fs.is_dir(&previous).await;
            if has_previous || intended.is_some() {
                remove_entry(fs, &checkout).await?;
            }
            if has_previous {
                fs.rename(&previous, &checkout, RenameOptions::default())
                    .await?;
            }
        }
    }
    // Detach the completed journal before deletion so interruption cannot resurrect it.
    let cleanup = async {
        fs.rename(&backup, &discarded, RenameOptions::default())
            .await?;
        remove_entry(fs, &discarded).await
    }
    .await;
    mode.cleanup_result(cleanup)
}

pub(super) async fn restore_git_source_entry(
    fs: &Arc<dyn Fs>,
    live: &Path,
    previous: &Path,
    existed: bool,
) -> Result<()> {
    let has_previous = fs.metadata(previous).await?.is_some();
    if has_previous || !existed {
        remove_entry(fs, live).await?;
        if has_previous {
            fs.rename(previous, live, RenameOptions::default()).await?;
        }
    }
    Ok(())
}

async fn remove_entry(fs: &Arc<dyn Fs>, path: &Path) -> Result<()> {
    if let Some(metadata) = fs.metadata(path).await? {
        if metadata.is_dir && !metadata.is_symlink {
            fs.remove_dir(path, remove_options(true)).await?;
        } else {
            fs.remove_file(path, remove_options(false)).await?;
        }
    }
    Ok(())
}

async fn remove_record(fs: &Arc<dyn Fs>, path: &Path) -> Result<()> {
    if fs.metadata(path).await?.is_some() {
        fs.remove_file(path, remove_options(false)).await?;
    }
    Ok(())
}

fn remove_options(recursive: bool) -> RemoveOptions {
    RemoveOptions {
        recursive,
        ignore_if_not_exists: true,
    }
}
