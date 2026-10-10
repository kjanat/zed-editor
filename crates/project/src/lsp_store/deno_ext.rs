use std::{any::Any, path::PathBuf, sync::Arc};

use anyhow::{Context as _, Result, anyhow};
use gpui::{App, Context, Entity, Task};
use language::{Buffer, DiskState};
use lsp::{LanguageServerId, Uri};
use rpc::proto;
use serde::{Deserialize, Serialize};
use settings::Settings as _;
use util::{
    paths::{PathStyle, UrlExt as _},
    rel_path::RelPath,
};
use worktree::WorktreeId;

use crate::{LspStore, ProjectSettings};

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
}

impl VirtualDocumentFile {
    fn new(uri: Uri, worktree_id: WorktreeId) -> Result<Self> {
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
        })
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
        if let Some(buffer) = self.open_virtual_document(&uri, cx) {
            return Task::ready(Ok(buffer));
        }
        let Some(server) = self.language_server_for_id(language_server_id) else {
            return Task::ready(Err(anyhow!(
                "language server {language_server_id} is not running"
            )));
        };
        let worktree_id = self
            .language_server_statuses
            .get(&language_server_id)
            .and_then(|status| status.worktree)
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
        let file = match VirtualDocumentFile::new(uri.clone(), worktree_id) {
            Ok(file) => file,
            Err(error) => return Task::ready(Err(error)),
        };
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
                if let Some(buffer) = lsp_store.open_virtual_document(&uri, cx) {
                    return buffer;
                }
                lsp_store.buffer_store.update(cx, |buffer_store, cx| {
                    buffer_store.create_read_only_buffer(text, Arc::new(file), cx)
                })
            })
        })
    }

    fn open_virtual_document(&self, uri: &Uri, cx: &App) -> Option<Entity<Buffer>> {
        self.buffer_store.read(cx).buffers().find(|buffer| {
            VirtualDocumentFile::from_dyn(buffer.read(cx).file())
                .is_some_and(|file| file.uri == *uri)
        })
    }
}
