use std::{any::Any, path::PathBuf, str::FromStr, sync::Arc};

use anyhow::{Context as _, Result, anyhow};
use gpui::{App, Context, Entity, Task, TaskExt as _};
use language::{
    Buffer, Diagnostic, DiagnosticSourceKind, DiskState, Language, ToPointUtf16 as _, Unclipped,
};
use lsp::{LanguageServer, LanguageServerId, LanguageServerName, Uri};
use rpc::proto;
use serde::{Deserialize, Serialize};
use settings::Settings as _;
use util::{
    ResultExt as _,
    paths::{PathStyle, UrlExt as _},
    rel_path::RelPath,
};
use worktree::WorktreeId;

use crate::{
    LspStore, ProjectPath, ProjectSettings,
    buffer_store::BufferStore,
    lsp_store::{
        DocumentDiagnosticsUpdate, LanguageServerSeed, LanguageServerState, LocalLspStore,
        LspBufferSnapshot, LspStoreEvent,
    },
};

pub const SCHEME: &str = "deno";

/// [Deno LSP integration](https://docs.deno.com/runtime/reference/lsp_integration/#virtualtextdocument)
pub enum VirtualTextDocument {}

impl lsp::request::Request for VirtualTextDocument {
    type Params = VirtualTextDocumentParams;
    type Result = Option<String>;
    const METHOD: &'static str = "deno/virtualTextDocument";
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VirtualTextDocumentParams {
    pub text_document: lsp::TextDocumentIdentifier,
}

#[derive(Clone)]
pub struct VirtualDocumentFile {
    uri: Uri,
    path: Arc<RelPath>,
    full_path: PathBuf,
    worktree_id: WorktreeId,
    server: Option<LanguageServerSeed>,
    roots: Vec<Arc<RelPath>>,
    text_server: Option<LanguageServerId>,
}

impl VirtualDocumentFile {
    fn new(
        uri: Uri,
        worktree_id: WorktreeId,
        server: Option<LanguageServerSeed>,
        roots: Vec<Arc<RelPath>>,
        text_server: Option<LanguageServerId>,
    ) -> Result<Self> {
        let path = uri
            .to_file_path_ext(PathStyle::Unix)
            .map_err(|()| anyhow!("{uri} has no path"))?;
        let path = RelPath::new(path.strip_prefix("/")?, PathStyle::Unix)?.into_arc();
        anyhow::ensure!(path.file_name().is_some(), "{uri} names no file");
        let full_path = PathBuf::from(format!("{SCHEME}:/{}", path.as_unix_str()));
        Ok(Self {
            uri,
            path,
            full_path,
            worktree_id,
            server,
            roots,
            text_server,
        })
    }

    pub fn from_proto(file: proto::File) -> Result<Self> {
        let virtual_document = file
            .virtual_document
            .context("file is not a virtual document")?;
        Self::new(
            Uri::from_str(&virtual_document.uri)?,
            WorktreeId::from_proto(file.worktree_id),
            None,
            Vec::new(),
            None,
        )
    }

    pub fn from_dyn(file: Option<&Arc<dyn language::File>>) -> Option<&Self> {
        file.and_then(|file| {
            let file: &dyn language::File = file.as_ref();
            let file: &dyn Any = file;
            file.downcast_ref()
        })
    }

    pub fn uri(&self) -> &Uri {
        &self.uri
    }
}

impl language::File for VirtualDocumentFile {
    fn as_local(&self) -> Option<&dyn language::LocalFile> {
        None
    }

    fn disk_state(&self) -> DiskState {
        DiskState::Historic { was_deleted: false }
    }

    fn path(&self) -> &Arc<RelPath> {
        &self.path
    }

    fn full_path(&self, _: &App) -> PathBuf {
        self.full_path.clone()
    }

    fn path_style(&self, _: &App) -> PathStyle {
        PathStyle::Unix
    }

    fn file_name<'a>(&'a self, _: &'a App) -> &'a str {
        self.path.file_name().unwrap_or_default()
    }

    fn worktree_id(&self, _: &App) -> WorktreeId {
        self.worktree_id
    }

    fn to_proto(&self, _: &App) -> proto::File {
        proto::File {
            worktree_id: self.worktree_id.to_proto(),
            entry_id: None,
            path: self.path.as_unix_str().to_owned(),
            mtime: None,
            is_deleted: false,
            is_historic: true,
            size: None,
            inode: None,
            device: None,
            virtual_document: Some(proto::VirtualDocument {
                uri: self.uri.to_string(),
            }),
        }
    }

    fn is_private(&self) -> bool {
        false
    }
}

impl LspStore {
    pub(super) fn open_deno_virtual_document(
        &mut self,
        uri: Uri,
        language_server_id: LanguageServerId,
        cx: &mut Context<Self>,
    ) -> Task<Result<Entity<Buffer>>> {
        let Some(server) = self.language_server_for_id(language_server_id) else {
            return Task::ready(Err(anyhow!(
                "language server {language_server_id} is not running"
            )));
        };
        let local = self.as_local();
        let seed = local.and_then(|local| local.server_seed(language_server_id));
        let roots = local
            .zip(seed.as_ref())
            .map(|(local, seed)| local.server_roots(seed))
            .unwrap_or_default();
        let worktree_id = seed
            .as_ref()
            .map(|seed| seed.worktree_id)
            .or_else(|| {
                self.language_server_statuses
                    .get(&language_server_id)
                    .and_then(|status| status.worktree)
            })
            .or_else(|| {
                self.worktree_store
                    .read(cx)
                    .worktrees()
                    .next()
                    .map(|worktree| worktree.read(cx).id())
            });
        let Some(worktree_id) = worktree_id else {
            return Task::ready(Err(anyhow!("no worktree to open {uri} in")));
        };
        let file = match VirtualDocumentFile::new(
            uri.clone(),
            worktree_id,
            seed,
            roots,
            Some(language_server_id),
        ) {
            Ok(file) => file,
            Err(error) => return Task::ready(Err(error)),
        };
        if let Some(buffer) = self.open_virtual_document(&file, cx) {
            return Task::ready(Ok(buffer));
        }
        let request_timeout = ProjectSettings::get_global(cx)
            .global_lsp_settings
            .get_request_timeout();
        cx.spawn(async move |lsp_store, cx| {
            let text = server
                .request::<VirtualTextDocument>(
                    VirtualTextDocumentParams {
                        text_document: lsp::TextDocumentIdentifier::new(uri.clone()),
                    },
                    request_timeout,
                )
                .await
                .into_response()
                .with_context(|| format!("{} failed to load {uri}", server.name()))?
                .with_context(|| format!("{} has no document for {uri}", server.name()))?;
            lsp_store.update(cx, |lsp_store, cx| {
                if let Some(buffer) = lsp_store.open_virtual_document(&file, cx) {
                    return buffer;
                }
                lsp_store.buffer_store.update(cx, |buffer_store, cx| {
                    buffer_store.create_read_only_buffer(text, Arc::new(file), cx)
                })
            })
        })
    }

    fn open_virtual_document(
        &self,
        file: &VirtualDocumentFile,
        cx: &App,
    ) -> Option<Entity<Buffer>> {
        self.buffer_store.read(cx).buffers().find(|buffer| {
            VirtualDocumentFile::from_dyn(buffer.read(cx).file()).is_some_and(|open| {
                open.uri == file.uri
                    && open.worktree_id == file.worktree_id
                    && open.server == file.server
            })
        })
    }

    pub(super) fn merge_virtual_document_diagnostics(
        &mut self,
        source_kind: DiagnosticSourceKind,
        update: DocumentDiagnosticsUpdate<'_, lsp::PublishDiagnosticsParams>,
        merge: impl Fn(&Uri, &Diagnostic, &App) -> bool,
        cx: &mut Context<Self>,
    ) {
        let uri = update.diagnostics.uri.clone();
        let server_id = update.server_id;
        let Some(local) = self.as_local() else {
            return;
        };
        let Some(buffer) = self.buffer_store.read(cx).buffers().find(|buffer| {
            VirtualDocumentFile::from_dyn(buffer.read(cx).file()).is_some_and(|file| {
                file.uri == uri && local.virtual_document_server_id(file) == Some(server_id)
            })
        }) else {
            log::warn!("skipping diagnostics update, no open virtual document for {uri}");
            return;
        };
        let diagnostics = self.lsp_to_document_diagnostics(
            PathBuf::new(),
            source_kind,
            server_id,
            update.diagnostics,
            &update.disk_based_sources,
            update.registration_id.clone(),
        );
        let snapshot = buffer.read(cx).snapshot();
        let reused_diagnostics = buffer
            .read(cx)
            .buffer_diagnostics(Some(server_id))
            .iter()
            .filter(|entry| merge(&uri, &entry.diagnostic, cx))
            .map(|entry| {
                (*entry).clone().map_coordinates(|range| {
                    Unclipped(range.start.to_point_utf16(&snapshot))
                        ..Unclipped(range.end.to_point_utf16(&snapshot))
                })
            })
            .collect::<Vec<_>>();
        if let Some(local) = self.as_local_mut() {
            local
                .update_buffer_diagnostics(
                    &buffer,
                    server_id,
                    Some(update.registration_id),
                    update.result_id,
                    diagnostics.version,
                    diagnostics.diagnostics,
                    reused_diagnostics,
                    cx,
                )
                .with_context(|| format!("updating diagnostics for {uri}"))
                .log_err();
        }
    }
}

impl LocalLspStore {
    pub(super) fn virtual_document_server_id(
        &self,
        file: &VirtualDocumentFile,
    ) -> Option<LanguageServerId> {
        Some(self.language_server_ids.get(file.server.as_ref()?)?.id)
    }

    fn server_roots(&self, seed: &LanguageServerSeed) -> Vec<Arc<RelPath>> {
        self.language_server_ids
            .get(seed)
            .map(|server| server.project_roots.iter().cloned().collect())
            .unwrap_or_default()
    }

    pub(super) fn virtual_document_roots(
        &self,
        file: &VirtualDocumentFile,
    ) -> Option<(LanguageServerName, Vec<ProjectPath>)> {
        let seed = file.server.as_ref()?;
        let roots = if self.language_server_ids.contains_key(seed) {
            self.server_roots(seed)
        } else {
            file.roots.clone()
        };
        let roots = roots
            .into_iter()
            .map(|path| ProjectPath {
                worktree_id: seed.worktree_id,
                path,
            })
            .collect();
        Some((seed.name.clone(), roots))
    }

    pub(super) fn rebind_virtual_documents(
        &self,
        buffer_store: &Entity<BufferStore>,
        server_for_seed: impl Fn(&LanguageServerSeed) -> Option<LanguageServerId>,
        cx: &mut Context<LspStore>,
    ) {
        let rebound = buffer_store
            .read(cx)
            .buffers()
            .filter_map(|buffer| {
                let file = VirtualDocumentFile::from_dyn(buffer.read(cx).file())?;
                let old_seed = file.server.as_ref()?;
                let seed = self.server_seed(server_for_seed(old_seed)?)?;
                if &seed == old_seed {
                    return None;
                }
                let file = VirtualDocumentFile {
                    worktree_id: seed.worktree_id,
                    roots: self.server_roots(&seed),
                    server: Some(seed),
                    ..file.clone()
                };
                Some((buffer, file))
            })
            .collect::<Vec<_>>();
        for (buffer, file) in rebound {
            buffer.update(cx, |buffer, cx| buffer.file_updated(Arc::new(file), cx));
        }
    }

    fn refetch_virtual_document(
        &mut self,
        buffer: &Entity<Buffer>,
        server: Arc<LanguageServer>,
        cx: &mut Context<LspStore>,
    ) {
        let buffer_id = buffer.read(cx).remote_id();
        let server_id = server.server_id();
        if self.virtual_document_refetches.get(&buffer_id) == Some(&server_id) {
            return;
        }
        let Some(uri) =
            VirtualDocumentFile::from_dyn(buffer.read(cx).file()).map(|file| file.uri.clone())
        else {
            return;
        };
        self.virtual_document_refetches.insert(buffer_id, server_id);
        let request_timeout = ProjectSettings::get_global(cx)
            .global_lsp_settings
            .get_request_timeout();
        let buffer = buffer.downgrade();
        cx.spawn(async move |lsp_store, cx| {
            let text = server
                .request::<VirtualTextDocument>(
                    VirtualTextDocumentParams {
                        text_document: lsp::TextDocumentIdentifier::new(uri.clone()),
                    },
                    request_timeout,
                )
                .await
                .into_response()
                .with_context(|| format!("{} failed to load {uri}", server.name()))
                .and_then(|text| {
                    text.with_context(|| format!("{} has no document for {uri}", server.name()))
                });
            lsp_store.update(cx, |lsp_store, cx| {
                let Some(local) = lsp_store.as_local_mut() else {
                    return Ok(());
                };
                if local.virtual_document_refetches.get(&buffer_id) == Some(&server_id) {
                    local.virtual_document_refetches.remove(&buffer_id);
                }
                let Some(buffer) = buffer.upgrade() else {
                    return Ok(());
                };
                let Some(file) = VirtualDocumentFile::from_dyn(buffer.read(cx).file()).cloned()
                else {
                    return Ok(());
                };
                if local.virtual_document_server_id(&file) != Some(server_id) {
                    return Ok(());
                }
                let text = match text {
                    Ok(text) => text,
                    Err(error) => {
                        cx.emit(LspStoreEvent::Notification(format!(
                            "{error:#}. Restart {} to load it again.",
                            server.name()
                        )));
                        return Err(error);
                    }
                };
                let file = VirtualDocumentFile {
                    text_server: Some(server_id),
                    ..file
                };
                buffer.update(cx, |buffer, cx| {
                    if buffer.text() != text {
                        buffer.set_text(text, cx);
                        if let Some(entry) = buffer.peek_undo_stack() {
                            buffer.forget_transaction(entry.transaction_id());
                        }
                    }
                    buffer.file_updated(Arc::new(file), cx);
                });
                if local.registered_buffers.contains_key(&buffer_id) {
                    local.register_virtual_document(&buffer, cx);
                }
                anyhow::Ok(())
            })?
        })
        .detach_and_log_err(cx);
    }

    fn start_virtual_document_server(
        &mut self,
        buffer: &Entity<Buffer>,
        file: &VirtualDocumentFile,
        language: &Arc<Language>,
        cx: &mut Context<LspStore>,
    ) -> Option<LanguageServerId> {
        let seed = file.server.as_ref()?;
        if self.all_language_servers_stopped
            || !self.runtime.starts_servers()
            || self.stopped_language_servers.contains(&seed.name)
        {
            return None;
        }
        let worktree = self
            .worktree_store
            .read(cx)
            .worktree_for_id(seed.worktree_id, cx)?;
        let mut server_ids = Vec::new();
        for root in &file.roots {
            server_ids.extend(self.start_language_servers_for_path(
                &worktree,
                ProjectPath {
                    worktree_id: seed.worktree_id,
                    path: root.clone(),
                },
                language,
                |node| node.name().as_ref() == Some(&seed.name),
                cx,
            ));
        }
        let servers = server_ids
            .into_iter()
            .filter_map(|server_id| Some((server_id, self.server_seed(server_id)?)))
            .collect::<Vec<_>>();
        let (server_id, server_seed) = servers
            .iter()
            .find(|(_, server_seed)| server_seed == seed)
            .or_else(|| servers.first())
            .cloned()?;
        if &server_seed != seed {
            let file = VirtualDocumentFile {
                worktree_id: server_seed.worktree_id,
                roots: self.server_roots(&server_seed),
                server: Some(server_seed),
                ..file.clone()
            };
            buffer.update(cx, |buffer, cx| buffer.file_updated(Arc::new(file), cx));
        }
        Some(server_id)
    }

    pub(super) fn register_virtual_document(
        &mut self,
        buffer_handle: &Entity<Buffer>,
        cx: &mut Context<LspStore>,
    ) {
        let buffer = buffer_handle.read(cx);
        let Some(file) = VirtualDocumentFile::from_dyn(buffer.file()).cloned() else {
            return;
        };
        let Some(language) = buffer.language().cloned() else {
            return;
        };
        let buffer_id = buffer.remote_id();
        let snapshot = buffer.text_snapshot();
        let Some(server_id) = self
            .virtual_document_server_id(&file)
            .or_else(|| self.start_virtual_document_server(buffer_handle, &file, &language, cx))
        else {
            return;
        };
        let Some(LanguageServerState::Running {
            server, adapter, ..
        }) = self.language_servers.get(&server_id)
        else {
            return;
        };
        if file.text_server != Some(server_id) {
            let server = server.clone();
            self.refetch_virtual_document(buffer_handle, server, cx);
            return;
        }
        let language_name = language.name();
        let uri = file.uri.clone();
        let mut registered = false;
        self.buffer_snapshots
            .entry(buffer_id)
            .or_default()
            .entry(server_id)
            .or_insert_with(|| {
                registered = true;
                server.register_buffer(
                    uri.clone(),
                    adapter.language_id(&language_name),
                    0,
                    snapshot.text_with_line_endings(),
                );
                vec![LspBufferSnapshot {
                    version: 0,
                    snapshot: snapshot.clone(),
                }]
            });
        self.buffers_opened_in_servers
            .entry(buffer_id)
            .or_default()
            .insert(server_id);
        if registered {
            cx.emit(LspStoreEvent::LanguageServerUpdate {
                language_server_id: server_id,
                name: None,
                message: proto::update_language_server::Variant::RegisteredForBuffer(
                    proto::RegisteredForBuffer {
                        buffer_abs_path: uri.to_string(),
                        buffer_id: buffer_id.to_proto(),
                    },
                ),
            });
        }
    }
}
