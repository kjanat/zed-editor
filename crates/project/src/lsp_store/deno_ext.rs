use std::{any::Any, path::PathBuf, str::FromStr, sync::Arc};

use anyhow::{Context as _, Result, anyhow};
use gpui::{App, Context, Entity, Task};
use language::{Buffer, Diagnostic, DiagnosticSourceKind, DiskState, ToPointUtf16 as _, Unclipped};
use lsp::{LanguageServerId, LanguageServerName, Uri};
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
    LspStore, ProjectSettings,
    lsp_store::{
        DocumentDiagnosticsUpdate, LanguageServerState, LocalLspStore, LspBufferSnapshot,
        LspStoreEvent,
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

pub struct VirtualDocumentFile {
    uri: Uri,
    path: Arc<RelPath>,
    full_path: PathBuf,
    worktree_id: WorktreeId,
    server_name: LanguageServerName,
}

impl VirtualDocumentFile {
    fn new(uri: Uri, worktree_id: WorktreeId, server_name: LanguageServerName) -> Result<Self> {
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
            server_name,
        })
    }

    pub fn from_proto(file: proto::File) -> Result<Self> {
        let virtual_document = file
            .virtual_document
            .context("file is not a virtual document")?;
        Self::new(
            Uri::from_str(&virtual_document.uri)?,
            WorktreeId::from_proto(file.worktree_id),
            LanguageServerName(virtual_document.server_name.into()),
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
                server_name: self.server_name.to_string(),
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
        let worktree_id = self
            .as_local()
            .and_then(|local| {
                local
                    .language_server_ids
                    .iter()
                    .find(|(_, server)| server.id == language_server_id)
                    .map(|(seed, _)| seed.worktree_id)
            })
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
        let file = match VirtualDocumentFile::new(uri.clone(), worktree_id, server.name()) {
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
                    && open.server_name == file.server_name
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
                file.uri == uri && local.virtual_document_server_ids(file).contains(&server_id)
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
    pub(super) fn virtual_document_server_ids(
        &self,
        file: &VirtualDocumentFile,
    ) -> Vec<LanguageServerId> {
        self.language_server_ids
            .iter()
            .filter(|(seed, _)| {
                seed.worktree_id == file.worktree_id && seed.name == file.server_name
            })
            .map(|(_, server)| server.id)
            .collect()
    }

    pub(super) fn register_virtual_document(
        &mut self,
        buffer_handle: &Entity<Buffer>,
        cx: &mut Context<LspStore>,
    ) {
        let buffer = buffer_handle.read(cx);
        let Some(file) = VirtualDocumentFile::from_dyn(buffer.file()) else {
            return;
        };
        let Some(language_name) = buffer.language().map(|language| language.name()) else {
            return;
        };
        let buffer_id = buffer.remote_id();
        let uri = file.uri.clone();
        let snapshot = buffer.text_snapshot();
        for server_id in self.virtual_document_server_ids(file) {
            let Some(LanguageServerState::Running {
                server, adapter, ..
            }) = self.language_servers.get(&server_id)
            else {
                continue;
            };
            let (server, adapter) = (server.clone(), adapter.clone());
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
}
