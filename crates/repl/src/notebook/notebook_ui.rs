#![allow(unused, dead_code)]
use std::future::Future;
use std::{cell::RefCell, path::PathBuf, sync::Arc};

use anyhow::{Context as _, Result};
use client::proto::ViewId;
use collections::HashMap;
use editor::DisplayPoint;
use feature_flags::{FeatureFlagAppExt as _, NotebookFeatureFlag};
use futures::FutureExt;
use futures::future::Shared;
use gpui::{
    AnyElement, App, Entity, EventEmitter, FocusHandle, Focusable, KeyContext, ListScrollEvent,
    ListState, Point, Task, TaskExt, actions, list, prelude::*,
};
use jupyter_protocol::JupyterKernelspec;
use language::{Language, LanguageRegistry};
use log;
use project::{Project, ProjectEntryId, ProjectPath};
use settings::Settings as _;
use ui::{CommonAnimationExt, KeyBinding, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::item::{ItemEvent, SaveOptions, TabContentParams};
use workspace::searchable::SearchableItemHandle;
use workspace::{Item, ItemHandle, Pane, ProjectItem, ToolbarItemLocation};

use super::{Cell, CellEvent, CellPosition, CellRevision, MarkdownCellEvent, RenderableCell};

use nbformat::v4::CellId;
use nbformat::v4::Metadata as NotebookMetadata;
use serde_json;
use uuid::Uuid;

use crate::components::{KernelPickerDelegate, KernelSelector};
use crate::kernels::{
    Kernel, KernelSession, KernelSpecification, KernelStatus, LocalKernelSpecification,
    NativeRunningKernel, RemoteRunningKernel, SshRunningKernel, WslRunningKernel,
};
use crate::notebook::MovementDirection;
use crate::repl_store::ReplStore;

use picker::Picker;
use runtimelib::{ExecuteRequest, JupyterMessage, JupyterMessageContent};
use ui::PopoverMenuHandle;
use zed_actions::editor::{MoveDown, MoveUp};
use zed_actions::notebook::{
    AddCodeBlock, AddMarkdownBlock, ClearOutputs, DeleteCell, EnterCommandMode, EnterEditMode,
    InterruptKernel, MoveCellDown, MoveCellUp, NotebookMoveDown, NotebookMoveUp, OpenNotebook,
    RestartKernel, Run, RunAll, RunAndAdvance,
};

/// Whether the notebook is in command mode (navigating cells) or edit mode (editing a cell).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum NotebookMode {
    Command,
    Edit,
}

#[derive(PartialEq, Eq)]
enum SelectionMode {
    SelectOnly,
    SelectAndMove,
}

pub(crate) const MAX_TEXT_BLOCK_WIDTH: f32 = 9999.0;
pub(crate) const SMALL_SPACING_SIZE: f32 = 8.0;
pub(crate) const MEDIUM_SPACING_SIZE: f32 = 12.0;
pub(crate) const LARGE_SPACING_SIZE: f32 = 16.0;
pub(crate) const GUTTER_WIDTH: f32 = 19.0;
pub(crate) const CODE_BLOCK_INSET: f32 = MEDIUM_SPACING_SIZE;
pub(crate) const CONTROL_SIZE: f32 = 20.0;

const NOTEBOOK_EXTENSION: &str = "ipynb";

fn cells_on_disk(cells: &[nbformat::v4::Cell]) -> HashMap<CellId, serde_json::Value> {
    cells
        .iter()
        .filter_map(|cell| Some((cell.id().clone(), serde_json::to_value(cell).log_err()?)))
        .collect()
}

fn serialize_cell(cell: &Cell, cx: &App) -> String {
    serde_json::to_string(&cell.to_nbformat_cell(cx))
        .log_err()
        .unwrap_or_default()
}

fn parse_notebook_text(text: &str) -> Result<nbformat::v4::Notebook> {
    // Like opening one, an empty file is an empty notebook.
    if text.trim().is_empty() {
        return Ok(nbformat::v4::Notebook {
            nbformat: 4,
            nbformat_minor: 5,
            cells: vec![],
            metadata: serde_json::from_str("{}")?,
        });
    }
    let mut json: serde_json::Value = serde_json::from_str(text)?;
    if let Some(cells) = json.get_mut("cells").and_then(|c| c.as_array_mut()) {
        // Entries that aren't objects are left for nbformat to reject.
        for cell in cells.iter_mut().filter_map(|cell| cell.as_object_mut()) {
            cell.entry("id")
                .or_insert_with(|| serde_json::Value::String(Uuid::new_v4().to_string()));
        }
    }
    let text = serde_json::to_string(&json)?;

    match nbformat::parse_notebook(&text) {
        Ok(nbformat::Notebook::V4(notebook)) => Ok(notebook),
        Ok(nbformat::Notebook::Legacy(legacy_notebook)) => {
            Ok(nbformat::upgrade_legacy_notebook(legacy_notebook)?)
        }
        Ok(nbformat::Notebook::V3(v3_notebook)) => Ok(nbformat::upgrade_v3_notebook(v3_notebook)?),
        Err(error) => anyhow::bail!("Failed to parse notebook: {error:?}"),
    }
}

pub fn init(cx: &mut App) {
    if cx.has_flag::<NotebookFeatureFlag>() || std::env::var("LOCAL_NOTEBOOK_DEV").is_ok() {
        workspace::register_project_item::<NotebookEditor>(cx);
    }

    cx.observe_flag::<NotebookFeatureFlag, _>({
        move |flag, cx| {
            if *flag {
                workspace::register_project_item::<NotebookEditor>(cx);
            } else {
                // todo: there is no way to unregister a project item, so if the feature flag
                // gets turned off they need to restart Zed.
            }
        }
    })
    .detach();
}

pub struct NotebookEditor {
    languages: Arc<LanguageRegistry>,
    project: Entity<Project>,
    worktree_id: project::WorktreeId,
    focus_handle: FocusHandle,
    notebook_item: Entity<NotebookItem>,
    notebook_language: Shared<Task<Option<Arc<Language>>>>,
    remote_id: Option<ViewId>,
    cell_list: ListState,
    notebook_mode: NotebookMode,
    selected_cell_index: usize,
    cell_order: Vec<CellId>,
    cell_map: HashMap<CellId, Cell>,
    kernel: Kernel,
    kernel_specification: Option<KernelSpecification>,
    execution_requests: HashMap<String, CellId>,
    kernel_picker_handle: PopoverMenuHandle<Picker<KernelPickerDelegate>>,
    /// The backing file was reloaded from disk while the notebook had unsaved
    /// edits, so those edits no longer describe the file they would replace.
    external_change_pending: bool,
    /// The notebook as last loaded or saved.
    saved: SavedNotebook,
    /// The cells as they are in the file, so a reload can tell which ones it changed.
    cells_on_disk: HashMap<CellId, serde_json::Value>,
    /// Each cell's comparison with `saved`, kept until the cell's revision changes
    /// so serializing large outputs doesn't happen on every check.
    cell_comparisons: RefCell<HashMap<CellId, (CellRevision, bool)>>,
}

/// What a save writes and a reload replaces: the cells with their outputs and
/// execution counts, their order, and the selected kernel. The rest of the
/// metadata is left out because the kernel updates it on its own.
#[derive(Default)]
struct SavedNotebook {
    cell_order: Vec<CellId>,
    cells: HashMap<CellId, String>,
    kernelspec: Option<String>,
}

enum SaveDestination {
    CurrentPath(Option<client::proto::File>),
    NewPath(ProjectPath, Option<language::DiskState>),
}

impl NotebookEditor {
    pub fn new(
        project: Entity<Project>,
        notebook_item: Entity<NotebookItem>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        let buffer = notebook_item.read(cx).buffer.clone();
        cx.observe(&buffer, |_, _, cx| {
            cx.emit(());
            cx.notify();
        })
        .detach();
        cx.subscribe_in(&buffer, window, |this, _, event, window, cx| {
            if matches!(event, language::BufferEvent::Reloaded) {
                this.backing_buffer_reloaded(window, cx);
            }
        })
        .detach();

        let languages = project.read(cx).languages().clone();
        let language_name = notebook_item.read(cx).language_name();
        let worktree_id = notebook_item.read(cx).project_path.worktree_id;

        let notebook_language = notebook_item.read(cx).notebook_language();
        let notebook_language = cx
            .spawn_in(window, async move |_, _| notebook_language.await)
            .shared();

        let mut cell_order = vec![]; // Vec<CellId>
        let mut cell_map = HashMap::default(); // HashMap<CellId, Cell>

        let cell_count = notebook_item.read(cx).notebook.cells.len();
        for index in 0..cell_count {
            let cell = notebook_item.read(cx).notebook.cells[index].clone();
            let cell_id = cell.id();
            cell_order.push(cell_id.clone());
            let cell_entity = Cell::load(&cell, &languages, notebook_language.clone(), window, cx);

            Self::subscribe_to_cell(&cell_id, &cell_entity, window, cx);

            cell_map.insert(cell_id.clone(), cell_entity);
        }

        let notebook_handle = cx.entity().downgrade();
        let cell_count = cell_order.len();

        let this = cx.entity();
        let cell_list = ListState::new(cell_count, gpui::ListAlignment::Top, px(1000.));

        let mut editor = Self {
            project,
            languages: languages.clone(),
            worktree_id,
            focus_handle,
            notebook_item: notebook_item.clone(),
            notebook_language,
            remote_id: None,
            cell_list,
            notebook_mode: NotebookMode::Command,
            selected_cell_index: 0,
            cell_order: cell_order.clone(),
            cell_map: cell_map.clone(),
            kernel: Kernel::Shutdown,
            kernel_specification: None,
            execution_requests: HashMap::default(),
            kernel_picker_handle: PopoverMenuHandle::default(),
            external_change_pending: false,
            saved: SavedNotebook::default(),
            cell_comparisons: RefCell::default(),
            cells_on_disk: cells_on_disk(&notebook_item.read(cx).notebook.cells),
        };
        editor.launch_kernel(window, cx);
        // Launching a kernel records its kernelspec, which is not a user edit.
        editor.mark_saved(editor.snapshot(cx));
        editor.refresh_language(cx);
        editor.refresh_kernelspecs(cx);

        cx.subscribe(&notebook_item, |this, _item, _event, cx| {
            this.refresh_language(cx);
        })
        .detach();

        editor
    }

    fn refresh_kernelspecs(&mut self, cx: &mut Context<Self>) {
        let store = ReplStore::global(cx);
        let project = self.project.clone();
        let worktree_id = self.worktree_id;

        let refresh_task = store.update(cx, |store, cx| {
            store.refresh_python_kernelspecs(worktree_id, &project, cx)
        });

        cx.background_spawn(refresh_task).detach_and_log_err(cx);
    }

    fn refresh_language(&mut self, cx: &mut Context<Self>) {
        let notebook_language = self.notebook_item.read(cx).notebook_language();
        let task = cx.spawn(async move |this, cx| {
            let language = notebook_language.await;
            if let Some(this) = this.upgrade() {
                this.update(cx, |this, cx| {
                    for cell in this.cell_map.values() {
                        if let Cell::Code(code_cell) = cell {
                            code_cell.update(cx, |cell, cx| {
                                cell.set_language(language.clone(), cx);
                            });
                        }
                    }
                });
            }
            language
        });
        self.notebook_language = task.shared();
    }

    fn subscribe_to_cell(
        cell_id: &CellId,
        cell: &Cell,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match cell {
            Cell::Code(code_cell) => {
                let cell_id_for_focus = cell_id.clone();
                cx.subscribe_in(
                    code_cell,
                    window,
                    move |this, _cell, event, window, cx| match event {
                        CellEvent::Run(cell_id) => this.execute_cell(cell_id.clone(), window, cx),
                        CellEvent::FocusedIn(_) => this.select_cell_by_id(&cell_id_for_focus, cx),
                    },
                )
                .detach();

                let cell_id_for_editor = cell_id.clone();
                let editor = code_cell.read(cx).editor().clone();
                cx.subscribe(&editor, move |this, _editor, event, cx| {
                    if let editor::EditorEvent::Focused = event {
                        this.select_cell_by_id(&cell_id_for_editor, cx);
                    }
                })
                .detach();
            }
            Cell::Markdown(markdown_cell) => {
                cx.subscribe(
                    markdown_cell,
                    move |_this, cell, event: &MarkdownCellEvent, cx| match event {
                        MarkdownCellEvent::FinishedEditing | MarkdownCellEvent::Run(_) => {
                            cell.update(cx, |cell, cx| {
                                cell.reparse_markdown(cx);
                            });
                        }
                    },
                )
                .detach();

                let cell_id_for_editor = cell_id.clone();
                let editor = markdown_cell.read(cx).editor().clone();
                cx.subscribe(&editor, move |this, _editor, event, cx| {
                    if let editor::EditorEvent::Focused = event {
                        this.select_cell_by_id(&cell_id_for_editor, cx);
                    }
                })
                .detach();
            }
            Cell::Raw(_) => {}
        }
    }

    fn snapshot(&self, cx: &App) -> SavedNotebook {
        SavedNotebook {
            cell_order: self.cell_order.clone(),
            cells: self
                .cell_map
                .iter()
                .map(|(cell_id, cell)| (cell_id.clone(), serialize_cell(cell, cx)))
                .collect(),
            kernelspec: self.serialize_kernelspec(cx),
        }
    }

    fn mark_saved(&mut self, saved: SavedNotebook) {
        self.saved = saved;
        self.cell_comparisons.borrow_mut().clear();
    }

    fn serialize_kernelspec(&self, cx: &App) -> Option<String> {
        let metadata = &self.notebook_item.read(cx).notebook.metadata;
        serde_json::to_string(&metadata.kernelspec).log_err()
    }

    /// Whether the notebook differs from what was last loaded or saved, in
    /// anything a save writes: sources, outputs, execution counts, cell order
    /// or the selected kernel.
    fn is_modified(&self, cx: &App) -> bool {
        self.cell_order != self.saved.cell_order
            || self.serialize_kernelspec(cx) != self.saved.kernelspec
            || self
                .cell_map
                .iter()
                .any(|(cell_id, cell)| self.is_cell_modified(cell_id, cell, cx))
    }

    fn is_cell_modified(&self, cell_id: &CellId, cell: &Cell, cx: &App) -> bool {
        let revision = cell.revision(cx);
        if let Some((cached_revision, modified)) = self.cell_comparisons.borrow().get(cell_id)
            && *cached_revision == revision
        {
            return *modified;
        }
        let modified = self.saved.cells.get(cell_id) != Some(&serialize_cell(cell, cx));
        self.cell_comparisons
            .borrow_mut()
            .insert(cell_id.clone(), (revision, modified));
        modified
    }

    /// Whether a reload would lose anything.
    fn has_unsaved_changes(&self, cx: &App) -> bool {
        self.has_executing_cells(cx) || self.is_modified(cx)
    }

    /// A running cell's results would land in whatever cell has its ID, so its
    /// cell must not be replaced by a reload.
    fn has_executing_cells(&self, cx: &App) -> bool {
        self.cell_map.values().any(|cell| match cell {
            Cell::Code(cell) => cell.read(cx).is_executing(),
            _ => false,
        })
    }

    /// Handles the backing JSON buffer being reloaded from disk, which happens
    /// automatically while it is clean even if the cells hold unsaved edits.
    fn backing_buffer_reloaded(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.has_unsaved_changes(cx) {
            // Rebuilding would discard the user's edits, and saving them would
            // silently replace the external version, so require a decision.
            self.external_change_pending = true;
        } else {
            let text = self.notebook_item.read(cx).buffer.read(cx).text();
            match parse_notebook_text(&text) {
                Ok(notebook) => {
                    self.replace_cells(notebook, window, cx);
                    self.external_change_pending = false;
                }
                Err(error) => {
                    log::error!("failed to parse externally changed notebook: {error:#}");
                    self.external_change_pending = true;
                }
            }
        }
        cx.emit(());
        cx.notify();
    }

    fn replace_cells(
        &mut self,
        notebook: nbformat::v4::Notebook,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let cells = notebook.cells.clone();
        self.notebook_item
            .update(cx, |item, _| item.notebook = notebook);
        // The cells take their language from the notebook's metadata, which the
        // reload may have changed.
        self.refresh_language(cx);

        let focused_cell_id = self.cell_map.iter().find_map(|(cell_id, cell)| {
            let editor = cell.editor(cx)?;
            editor
                .focus_handle(cx)
                .contains_focused(window, cx)
                .then(|| cell_id.clone())
        });
        let mut previous_cells = std::mem::take(&mut self.cell_map);
        let incoming_cells_on_disk = cells_on_disk(&cells);
        let mut cell_order = vec![];
        let mut cell_map = HashMap::default();

        for cell in cells.iter() {
            let cell_id = cell.id().clone();
            cell_order.push(cell_id.clone());
            // Cells the file didn't change keep their editors, and with them their
            // focus, cursor and editing state, as long as they still show the file.
            let unchanged_on_disk = incoming_cells_on_disk
                .get(&cell_id)
                .is_some_and(|incoming| self.cells_on_disk.get(&cell_id) == Some(incoming));
            let reusable = unchanged_on_disk
                && previous_cells.get(&cell_id).is_some_and(|existing| {
                    !self.is_cell_modified(&cell_id, existing, cx)
                        && !matches!(existing, Cell::Code(code_cell) if code_cell.read(cx).is_executing())
                });
            if reusable && let Some(existing) = previous_cells.remove(&cell_id) {
                cell_map.insert(cell_id, existing);
                continue;
            }

            let cell_entity = Cell::load(
                cell,
                &self.languages,
                self.notebook_language.clone(),
                window,
                cx,
            );
            Self::subscribe_to_cell(&cell_id, &cell_entity, window, cx);
            if focused_cell_id.as_ref() == Some(&cell_id) {
                if let Cell::Markdown(markdown_cell) = &cell_entity {
                    markdown_cell.update(cx, |cell, _| cell.set_editing(true));
                }
                if let Some(editor) = cell_entity.editor(cx).cloned() {
                    window.focus(&editor.focus_handle(cx), cx);
                }
            }
            cell_map.insert(cell_id, cell_entity);
        }

        self.cell_order = cell_order;
        self.cell_map = cell_map;
        self.cells_on_disk = incoming_cells_on_disk;
        // Results of requests sent for the replaced cells must not reach the
        // new cells that share their IDs.
        self.execution_requests.clear();
        self.selected_cell_index = self
            .selected_cell_index
            .min(self.cell_order.len().saturating_sub(1));
        self.cell_list = ListState::new(self.cell_order.len(), gpui::ListAlignment::Top, px(1000.));
        if !self.cell_order.is_empty() {
            self.cell_list
                .scroll_to_reveal_item(self.selected_cell_index);
        }
        self.mark_saved(self.snapshot(cx));
        cx.notify();
    }

    pub fn to_notebook(&self, cx: &App) -> nbformat::v4::Notebook {
        let cells: Vec<nbformat::v4::Cell> = self
            .cell_order
            .iter()
            .filter_map(|cell_id| {
                self.cell_map
                    .get(cell_id)
                    .map(|cell| cell.to_nbformat_cell(cx))
            })
            .collect();

        let metadata = self.notebook_item.read(cx).notebook.metadata.clone();

        nbformat::v4::Notebook {
            metadata,
            nbformat: 4,
            nbformat_minor: 5,
            cells,
        }
    }

    fn save_impl(
        &mut self,
        destination: SaveDestination,
        project: Entity<Project>,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let notebook = self.to_notebook(cx);
        // Taken before the write so that changes made while it runs stay unsaved.
        let saved = self.snapshot(cx);
        let written_cells = cells_on_disk(&notebook.cells);
        let buffer = self.notebook_item.read(cx).buffer.clone();

        cx.spawn(async move |this, cx| {
            let json =
                serde_json::to_string_pretty(&notebook).context("Failed to serialize notebook")?;
            buffer.update(cx, |buffer, cx| buffer.set_text(json, cx));

            match destination {
                SaveDestination::CurrentPath(overwrite_file) => {
                    project
                        .update(cx, |project, cx| {
                            let overwrite_files = overwrite_file
                                .into_iter()
                                .map(|file| (buffer.clone(), file))
                                .collect();
                            project.save_buffers_with_overwrite_files(
                                [buffer].into_iter().collect(),
                                overwrite_files,
                                cx,
                            )
                        })
                        .await
                }
                SaveDestination::NewPath(new_path, expected) => {
                    project
                        .update(cx, |project, cx| {
                            project.save_buffer_as_with_disk_state(
                                buffer,
                                new_path.clone(),
                                expected,
                                cx,
                            )
                        })
                        .await?;

                    // The buffer now lives at the new path, so the notebook has
                    // to follow it or the next save writes to the old file.
                    let entry_id = project.read_with(cx, |project, cx| {
                        project.entry_for_path(&new_path, cx).map(|entry| entry.id)
                    });
                    this.update(cx, |this, cx| {
                        this.notebook_item.update(cx, |notebook_item, _| {
                            notebook_item.project_path = new_path;
                            if let Some(entry_id) = entry_id {
                                notebook_item.id = entry_id;
                            }
                        })
                    })
                }
            }?;
            this.update(cx, |this, cx| {
                this.external_change_pending = false;
                this.mark_saved(saved);
                this.cells_on_disk = written_cells;
                cx.notify();
            })
        })
    }

    fn launch_kernel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let spec = self.kernel_specification.clone().or_else(|| {
            ReplStore::global(cx)
                .read(cx)
                .active_kernelspec(self.worktree_id, None, cx)
        });

        let spec = spec.unwrap_or_else(|| {
            KernelSpecification::Jupyter(LocalKernelSpecification {
                name: "python3".to_string(),
                path: PathBuf::from("python3"),
                kernelspec: JupyterKernelspec {
                    argv: vec![
                        "python3".to_string(),
                        "-m".to_string(),
                        "ipykernel_launcher".to_string(),
                        "-f".to_string(),
                        "{connection_file}".to_string(),
                    ],
                    display_name: "Python 3".to_string(),
                    language: "python".to_string(),
                    interrupt_mode: None,
                    metadata: None,
                    env: None,
                },
            })
        });

        self.launch_kernel_with_spec(spec, window, cx);
    }

    fn launch_kernel_with_spec(
        &mut self,
        spec: KernelSpecification,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let entity_id = cx.entity_id();
        let working_directory = self
            .project
            .read(cx)
            .worktree_for_id(self.worktree_id, cx)
            .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
            .unwrap_or_else(std::env::temp_dir);
        let fs = self.project.read(cx).fs().clone();
        let view = cx.entity();

        self.kernel_specification = Some(spec.clone());

        self.notebook_item.update(cx, |item, cx| {
            let kernel_name = spec.name().to_string();
            let language = spec.language().to_string();

            let display_name = match &spec {
                KernelSpecification::Jupyter(s) => s.kernelspec.display_name.clone(),
                KernelSpecification::PythonEnv(s) => s.kernelspec.display_name.clone(),
                KernelSpecification::JupyterServer(s) => s.kernelspec.display_name.clone(),
                KernelSpecification::SshRemote(s) => s.kernelspec.display_name.clone(),
                KernelSpecification::WslRemote(s) => s.kernelspec.display_name.clone(),
            };

            let kernelspec_json = serde_json::json!({
                "display_name": display_name,
                "name": kernel_name,
                "language": language
            });

            if let Ok(k) = serde_json::from_value(kernelspec_json) {
                item.notebook.metadata.kernelspec = Some(k);
                cx.emit(());
            }
        });

        let kernel_task = match spec {
            KernelSpecification::Jupyter(local_spec) => NativeRunningKernel::new(
                local_spec,
                entity_id,
                working_directory,
                fs,
                view,
                window,
                cx,
            ),
            KernelSpecification::PythonEnv(env_spec) => NativeRunningKernel::new(
                env_spec.as_local_spec(),
                entity_id,
                working_directory,
                fs,
                view,
                window,
                cx,
            ),
            KernelSpecification::JupyterServer(remote_spec) => {
                RemoteRunningKernel::new(remote_spec, working_directory, view, window, cx)
            }

            KernelSpecification::SshRemote(spec) => {
                let project = self.project.clone();
                SshRunningKernel::new(spec, working_directory, project, view, window, cx)
            }
            KernelSpecification::WslRemote(spec) => {
                WslRunningKernel::new(spec, entity_id, working_directory, fs, view, window, cx)
            }
        };

        let pending_kernel = cx
            .spawn(async move |this, cx| {
                let kernel = kernel_task.await;

                match kernel {
                    Ok(kernel) => {
                        this.update(cx, |editor, cx| {
                            editor.kernel = Kernel::RunningKernel(kernel);
                            cx.notify();
                        })
                        .ok();
                    }
                    Err(err) => {
                        log::error!("Kernel failed to start: {:?}", err);
                        this.update(cx, |editor, cx| {
                            editor.kernel = Kernel::ErroredLaunch(err.to_string());
                            cx.notify();
                        })
                        .ok();
                    }
                }
            })
            .shared();

        self.kernel = Kernel::StartingKernel(pending_kernel);
        cx.notify();
    }

    // Note: Python environments are only detected as kernels if ipykernel is installed.
    // Users need to run `pip install ipykernel` (or `uv pip install ipykernel`) in their
    // virtual environment for it to appear in the kernel selector.
    // This happens because we have an ipykernel check inside the function python_env_kernel_specification in mod.rs L:121

    fn change_kernel(
        &mut self,
        spec: KernelSpecification,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Kernel::RunningKernel(kernel) = &mut self.kernel {
            kernel.force_shutdown(window, cx).detach();
        }

        self.execution_requests.clear();

        self.launch_kernel_with_spec(spec, window, cx);
    }

    fn restart_kernel(&mut self, _: &RestartKernel, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(spec) = self.kernel_specification.clone() {
            if let Kernel::RunningKernel(kernel) = &mut self.kernel {
                kernel.force_shutdown(window, cx).detach();
            }

            self.kernel = Kernel::Restarting;
            cx.notify();

            self.launch_kernel_with_spec(spec, window, cx);
        }
    }

    fn interrupt_kernel(
        &mut self,
        _: &InterruptKernel,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Kernel::RunningKernel(kernel) = &self.kernel {
            let interrupt_request = runtimelib::InterruptRequest {};
            let message: JupyterMessage = interrupt_request.into();
            kernel.request_tx().try_send(message).ok();
            cx.notify();
        }
    }

    fn execute_cell(&mut self, cell_id: CellId, window: &mut Window, cx: &mut Context<Self>) {
        let code = if let Some(Cell::Code(cell)) = self.cell_map.get(&cell_id) {
            let editor = cell.read(cx).editor().clone();
            let buffer = editor.read(cx).buffer().read(cx);
            buffer
                .as_singleton()
                .map(|b| b.read(cx).text())
                .unwrap_or_default()
        } else {
            return;
        };

        let request = ExecuteRequest {
            code,
            ..Default::default()
        };
        let message: JupyterMessage = request.into();
        let msg_id = message.header.msg_id.clone();

        let send_result = match &mut self.kernel {
            Kernel::RunningKernel(kernel) => kernel
                .request_tx()
                .try_send(message)
                .map_err(|err| format!("failed to send execute request to kernel (the kernel process may have died): {err}")),
            Kernel::StartingKernel(_) => Err("the kernel is still starting".to_string()),
            Kernel::ErroredLaunch(error) => Err(format!("the kernel failed to launch: {error}")),
            Kernel::ShuttingDown | Kernel::Shutdown => Err("the kernel is shut down".to_string()),
            Kernel::Restarting => Err("the kernel is restarting".to_string()),
        };

        if let Some(Cell::Code(cell)) = self.cell_map.get(&cell_id) {
            cell.update(cx, |cell, cx| {
                if cell.has_outputs() {
                    cell.clear_outputs();
                }
                if let Err(error) = &send_result {
                    cell.show_kernel_error(error, window, cx);
                } else {
                    cell.start_execution();
                }
                cx.notify();
            });
        }

        if let Err(error) = send_result {
            log::error!("notebook: cannot execute cell: {error}");
        } else {
            self.execution_requests.insert(msg_id, cell_id.clone());
        }
    }

    fn get_selected_cell(&self) -> Option<&Cell> {
        self.cell_order
            .get(self.selected_cell_index)
            .and_then(|cell_id| self.cell_map.get(cell_id))
    }

    fn has_outputs(&self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.cell_map.values().any(|cell| {
            if let Cell::Code(code_cell) = cell {
                code_cell.read(cx).has_outputs()
            } else {
                false
            }
        })
    }

    fn clear_outputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for cell in self.cell_map.values() {
            if let Cell::Code(code_cell) = cell {
                code_cell.update(cx, |cell, cx| {
                    cell.clear_outputs();
                    cx.notify();
                });
            }
        }
        cx.notify();
    }

    fn run_cells(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for cell_id in self.cell_order.clone() {
            self.execute_cell(cell_id, window, cx);
        }
    }

    fn run_current_cell(&mut self, _: &Run, window: &mut Window, cx: &mut Context<Self>) {
        let Some(cell_id) = self.cell_order.get(self.selected_cell_index).cloned() else {
            return;
        };
        let Some(cell) = self.cell_map.get(&cell_id) else {
            return;
        };
        match cell {
            Cell::Code(_) => {
                self.execute_cell(cell_id, window, cx);
            }
            Cell::Markdown(markdown_cell) => {
                // for markdown, finish editing and move to next cell
                let is_editing = markdown_cell.read(cx).is_editing();
                if is_editing {
                    markdown_cell.update(cx, |cell, cx| {
                        cell.run(cx);
                    });
                    self.enter_command_mode(window, cx);
                }
            }
            Cell::Raw(_) => {}
        }
    }

    fn run_and_advance(&mut self, _: &RunAndAdvance, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(cell_id) = self.cell_order.get(self.selected_cell_index).cloned() {
            if let Some(cell) = self.cell_map.get(&cell_id) {
                match cell {
                    Cell::Code(_) => {
                        self.execute_cell(cell_id, window, cx);
                    }
                    Cell::Markdown(markdown_cell) => {
                        if markdown_cell.read(cx).is_editing() {
                            markdown_cell.update(cx, |cell, cx| {
                                cell.run(cx);
                            });
                        }
                    }
                    Cell::Raw(_) => {}
                }
            }
        }

        let is_last_cell = self.selected_cell_index == self.cell_count().saturating_sub(1);
        if is_last_cell {
            self.add_code_block(window, cx);
            self.enter_command_mode(window, cx);
        } else {
            self.advance_in_command_mode(window, cx);
        }
    }

    fn enter_edit_mode(&mut self, _: &EnterEditMode, window: &mut Window, cx: &mut Context<Self>) {
        self.notebook_mode = NotebookMode::Edit;
        if let Some(cell_id) = self.cell_order.get(self.selected_cell_index) {
            if let Some(cell) = self.cell_map.get(cell_id) {
                match cell {
                    Cell::Code(code_cell) => {
                        let editor = code_cell.read(cx).editor().clone();
                        window.focus(&editor.focus_handle(cx), cx);
                    }
                    Cell::Markdown(markdown_cell) => {
                        markdown_cell.update(cx, |cell, cx| {
                            cell.set_editing(true);
                            cx.notify();
                        });
                        let editor = markdown_cell.read(cx).editor().clone();
                        window.focus(&editor.focus_handle(cx), cx);
                    }
                    Cell::Raw(_) => {}
                }
            }
        }
        cx.notify();
    }

    fn enter_command_mode(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.notebook_mode = NotebookMode::Command;
        self.focus_handle.focus(window, cx);
        cx.notify();
    }

    fn handle_enter_command_mode(
        &mut self,
        _: &EnterCommandMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.enter_command_mode(window, cx);
    }

    /// Advances to the next cell while staying in command mode (used by RunAndAdvance and shift-enter).
    fn advance_in_command_mode(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let count = self.cell_count();
        if count == 0 {
            return;
        }
        if self.selected_cell_index < count - 1 {
            self.selected_cell_index += 1;
            self.cell_list
                .scroll_to_reveal_item(self.selected_cell_index);
        }
        self.notebook_mode = NotebookMode::Command;
        self.focus_handle.focus(window, cx);
        cx.notify();
    }

    // Discussion can be done on this default implementation
    /// Moves focus to the next cell editor (used when already in edit mode).
    fn move_to_next_cell(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.cell_order.is_empty() && self.selected_cell_index < self.cell_order.len() - 1 {
            self.selected_cell_index += 1;
            // focus the new cell's editor
            if let Some(cell_id) = self.cell_order.get(self.selected_cell_index) {
                if let Some(cell) = self.cell_map.get(cell_id) {
                    match cell {
                        Cell::Code(code_cell) => {
                            let editor = code_cell.read(cx).editor();
                            window.focus(&editor.focus_handle(cx), cx);
                        }
                        Cell::Markdown(markdown_cell) => {
                            // Don't auto-enter edit mode for next markdown cell
                            // Just select it
                        }
                        Cell::Raw(_) => {}
                    }
                }
            }
            cx.notify();
        } else {
            // in the end, could optionally create a new cell
            // For now, just stay on the current cell
        }
    }

    fn open_notebook(&mut self, _: &OpenNotebook, _window: &mut Window, _cx: &mut Context<Self>) {
        println!("Open notebook triggered");
    }

    fn move_cell_up(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        println!("Move cell up triggered");
        if self.selected_cell_index > 0 {
            self.cell_order
                .swap(self.selected_cell_index, self.selected_cell_index - 1);
            self.selected_cell_index -= 1;
            cx.notify();
        }
    }

    fn move_cell_down(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        println!("Move cell down triggered");
        if !self.cell_order.is_empty() && self.selected_cell_index < self.cell_order.len() - 1 {
            self.cell_order
                .swap(self.selected_cell_index, self.selected_cell_index + 1);
            self.selected_cell_index += 1;
            cx.notify();
        }
    }

    fn delete_cell(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.cell_order.is_empty() {
            return;
        }
        let index = self.selected_cell_index.min(self.cell_order.len() - 1);
        let cell_id = self.cell_order.remove(index);
        self.cell_map.remove(&cell_id);
        self.cell_list.splice(index..index + 1, 0);

        if self.cell_order.is_empty() {
            self.selected_cell_index = 0;
        } else {
            self.selected_cell_index = index.min(self.cell_order.len() - 1);
            self.cell_list
                .scroll_to_reveal_item(self.selected_cell_index);
        }
        self.notebook_mode = NotebookMode::Command;
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    fn insert_cell_at_current_position(&mut self, cell_id: CellId, cell: Cell) {
        let insert_index = if self.cell_order.is_empty() {
            0
        } else {
            self.selected_cell_index + 1
        };
        self.cell_order.insert(insert_index, cell_id.clone());
        self.cell_map.insert(cell_id, cell);
        self.selected_cell_index = insert_index;
        self.cell_list.splice(insert_index..insert_index, 1);
        self.cell_list.scroll_to_reveal_item(insert_index);
    }

    fn add_markdown_block(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let new_cell_id: CellId = Uuid::new_v4().into();
        let languages = self.languages.clone();
        let metadata: nbformat::v4::CellMetadata =
            serde_json::from_str("{}").expect("empty object should parse");

        let markdown_cell = cx.new(|cx| {
            super::MarkdownCell::new(
                new_cell_id.clone(),
                metadata,
                String::new(),
                languages,
                window,
                cx,
            )
        });

        let cell = Cell::Markdown(markdown_cell.clone());
        Self::subscribe_to_cell(&new_cell_id, &cell, window, cx);
        self.insert_cell_at_current_position(new_cell_id, cell);
        markdown_cell.update(cx, |cell, cx| {
            cell.set_editing(true);
            cx.notify();
        });
        let editor = markdown_cell.read(cx).editor().clone();
        window.focus(&editor.focus_handle(cx), cx);
        self.notebook_mode = NotebookMode::Edit;
        cx.notify();
    }

    fn add_code_block(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let new_cell_id: CellId = Uuid::new_v4().into();
        let notebook_language = self.notebook_language.clone();
        let metadata: nbformat::v4::CellMetadata =
            serde_json::from_str("{}").expect("empty object should parse");

        let code_cell = cx.new(|cx| {
            super::CodeCell::new(
                super::CellSource::None,
                new_cell_id.clone(),
                metadata,
                String::new(),
                notebook_language,
                window,
                cx,
            )
        });

        let cell = Cell::Code(code_cell.clone());
        Self::subscribe_to_cell(&new_cell_id, &cell, window, cx);
        self.insert_cell_at_current_position(new_cell_id, cell);
        let editor = code_cell.read(cx).editor().clone();
        window.focus(&editor.focus_handle(cx), cx);
        self.notebook_mode = NotebookMode::Edit;
        cx.notify();
    }

    fn cell_count(&self) -> usize {
        self.cell_map.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_cell_index
    }

    fn select_cell_by_id(&mut self, cell_id: &CellId, cx: &mut Context<Self>) {
        if let Some(index) = self.cell_order.iter().position(|id| id == cell_id) {
            self.selected_cell_index = index;
            self.notebook_mode = NotebookMode::Edit;
            cx.notify();
        }
    }

    pub fn set_selected_index(
        &mut self,
        index: usize,
        jump_to_index: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // let previous_index = self.selected_cell_index;
        self.selected_cell_index = index;
        let current_index = self.selected_cell_index;

        // in the future we may have some `on_cell_change` event that we want to fire here

        if jump_to_index {
            self.jump_to_cell(current_index, window, cx);
        }
    }

    fn select_next(
        &mut self,
        _: &menu::SelectNext,
        selection_mode: SelectionMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let count = self.cell_count();
        if count > 0 {
            let index = self.selected_index();
            let ix = if index == count - 1 {
                count - 1
            } else {
                index + 1
            };
            self.set_selected_index(ix, true, window, cx);

            if selection_mode == SelectionMode::SelectAndMove
                && let Some(cell) = self.get_selected_cell()
            {
                cell.move_to(MovementDirection::Start, window, cx);
            }

            cx.notify();
        }
    }

    fn select_previous(
        &mut self,
        _: &menu::SelectPrevious,
        selection_mode: SelectionMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let count = self.cell_count();
        if count > 0 {
            let index = self.selected_index();
            let ix = if index == 0 { 0 } else { index - 1 };
            self.set_selected_index(ix, true, window, cx);

            if selection_mode == SelectionMode::SelectAndMove
                && let Some(cell) = self.get_selected_cell()
            {
                cell.move_to(MovementDirection::End, window, cx);
            }

            cx.notify();
        }
    }

    pub fn select_first(
        &mut self,
        _: &menu::SelectFirst,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let count = self.cell_count();
        if count > 0 {
            self.set_selected_index(0, true, window, cx);
            cx.notify();
        }
    }

    pub fn select_last(
        &mut self,
        _: &menu::SelectLast,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let count = self.cell_count();
        if count > 0 {
            self.set_selected_index(count - 1, true, window, cx);
            cx.notify();
        }
    }

    fn jump_to_cell(&mut self, index: usize, _window: &mut Window, _cx: &mut Context<Self>) {
        self.cell_list.scroll_to_reveal_item(index);
    }

    fn button_group(window: &mut Window, cx: &mut Context<Self>) -> Div {
        v_flex()
            .gap(DynamicSpacing::Base04.rems(cx))
            .items_center()
            .w(px(CONTROL_SIZE + 4.0))
            .overflow_hidden()
            .rounded(px(5.))
            .bg(cx.theme().colors().title_bar_background)
            .p_px()
            .border_1()
            .border_color(cx.theme().colors().border)
    }

    fn render_notebook_control(
        id: impl Into<SharedString>,
        icon: IconName,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> IconButton {
        let id: ElementId = ElementId::Name(id.into());
        IconButton::new(id, icon).width(px(CONTROL_SIZE))
    }

    fn render_notebook_controls(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let has_outputs = self.has_outputs(window, cx);

        v_flex()
            .max_w(px(CONTROL_SIZE + 4.0))
            .items_center()
            .gap(DynamicSpacing::Base16.rems(cx))
            .justify_between()
            .flex_none()
            .h_full()
            .py(DynamicSpacing::Base12.px(cx))
            .child(
                v_flex()
                    .gap(DynamicSpacing::Base08.rems(cx))
                    .child(
                        Self::button_group(window, cx)
                            .child(
                                Self::render_notebook_control(
                                    "run-all-cells",
                                    IconName::PlayFilled,
                                    window,
                                    cx,
                                )
                                .tooltip(move |window, cx| {
                                    Tooltip::for_action("Execute all cells", &RunAll, cx)
                                })
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(Box::new(RunAll), cx);
                                }),
                            )
                            .child(
                                Self::render_notebook_control(
                                    "clear-all-outputs",
                                    IconName::ListX,
                                    window,
                                    cx,
                                )
                                .disabled(!has_outputs)
                                .tooltip(move |window, cx| {
                                    Tooltip::for_action("Clear all outputs", &ClearOutputs, cx)
                                })
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(Box::new(ClearOutputs), cx);
                                }),
                            ),
                    )
                    .child(
                        Self::button_group(window, cx)
                            .child(
                                Self::render_notebook_control(
                                    "move-cell-up",
                                    IconName::ArrowUp,
                                    window,
                                    cx,
                                )
                                .tooltip(move |window, cx| {
                                    Tooltip::for_action("Move cell up", &MoveCellUp, cx)
                                })
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(Box::new(MoveCellUp), cx);
                                }),
                            )
                            .child(
                                Self::render_notebook_control(
                                    "move-cell-down",
                                    IconName::ArrowDown,
                                    window,
                                    cx,
                                )
                                .tooltip(move |window, cx| {
                                    Tooltip::for_action("Move cell down", &MoveCellDown, cx)
                                })
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(Box::new(MoveCellDown), cx);
                                }),
                            ),
                    )
                    .child(
                        Self::button_group(window, cx)
                            .child(
                                Self::render_notebook_control(
                                    "new-markdown-cell",
                                    IconName::Plus,
                                    window,
                                    cx,
                                )
                                .tooltip(move |window, cx| {
                                    Tooltip::for_action("Add markdown block", &AddMarkdownBlock, cx)
                                })
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(Box::new(AddMarkdownBlock), cx);
                                }),
                            )
                            .child(
                                Self::render_notebook_control(
                                    "new-code-cell",
                                    IconName::Code,
                                    window,
                                    cx,
                                )
                                .tooltip(move |window, cx| {
                                    Tooltip::for_action("Add code block", &AddCodeBlock, cx)
                                })
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(Box::new(AddCodeBlock), cx);
                                }),
                            ),
                    )
                    .child(
                        Self::button_group(window, cx).child(
                            Self::render_notebook_control(
                                "delete-cell",
                                IconName::Trash,
                                window,
                                cx,
                            )
                            .disabled(self.cell_order.is_empty())
                            .tooltip(move |window, cx| {
                                Tooltip::for_action("Delete cell", &DeleteCell, cx)
                            })
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(DeleteCell), cx);
                            }),
                        ),
                    ),
            )
            .child(
                v_flex()
                    .gap(DynamicSpacing::Base08.rems(cx))
                    .items_center()
                    .child(
                        Self::render_notebook_control("more-menu", IconName::Ellipsis, window, cx)
                            .tooltip(move |window, cx| (Tooltip::text("More options"))(window, cx)),
                    )
                    .child(Self::button_group(window, cx).child({
                        let kernel_status = self.kernel.status();
                        let (icon, icon_color) = match &kernel_status {
                            KernelStatus::Idle => (IconName::ReplNeutral, Color::Success),
                            KernelStatus::Busy => (IconName::ReplNeutral, Color::Warning),
                            KernelStatus::Starting => (IconName::ReplNeutral, Color::Muted),
                            KernelStatus::Error => (IconName::ReplNeutral, Color::Error),
                            KernelStatus::ShuttingDown => (IconName::ReplNeutral, Color::Muted),
                            KernelStatus::Shutdown => (IconName::ReplNeutral, Color::Disabled),
                            KernelStatus::Restarting => (IconName::ReplNeutral, Color::Warning),
                        };
                        let kernel_name = self
                            .kernel_specification
                            .as_ref()
                            .map(|spec| spec.name().to_string())
                            .unwrap_or_else(|| "Select Kernel".to_string());
                        IconButton::new("repl", icon)
                            .icon_color(icon_color)
                            .tooltip(move |window, cx| {
                                Tooltip::text(format!(
                                    "{} ({}). Click to change kernel.",
                                    kernel_name,
                                    kernel_status.to_string()
                                ))(window, cx)
                            })
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.kernel_picker_handle.toggle(window, cx);
                            }))
                    })),
            )
    }

    fn render_kernel_status_bar(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let kernel_status = self.kernel.status();
        let kernel_name = self
            .kernel_specification
            .as_ref()
            .map(|spec| spec.name().to_string())
            .unwrap_or_else(|| "Select Kernel".to_string());

        let (status_icon, status_color) = match &kernel_status {
            KernelStatus::Idle => (IconName::Circle, Color::Success),
            KernelStatus::Busy => (IconName::ArrowCircle, Color::Warning),
            KernelStatus::Starting => (IconName::ArrowCircle, Color::Muted),
            KernelStatus::Error => (IconName::XCircle, Color::Error),
            KernelStatus::ShuttingDown => (IconName::ArrowCircle, Color::Muted),
            KernelStatus::Shutdown => (IconName::Circle, Color::Muted),
            KernelStatus::Restarting => (IconName::ArrowCircle, Color::Warning),
        };

        let is_spinning = matches!(
            kernel_status,
            KernelStatus::Busy
                | KernelStatus::Starting
                | KernelStatus::ShuttingDown
                | KernelStatus::Restarting
        );

        let status_icon_element = if is_spinning {
            Icon::new(status_icon)
                .size(IconSize::Small)
                .color(status_color)
                .with_rotate_animation(2)
                .into_any_element()
        } else {
            Icon::new(status_icon)
                .size(IconSize::Small)
                .color(status_color)
                .into_any_element()
        };

        let worktree_id = self.worktree_id;
        let kernel_picker_handle = self.kernel_picker_handle.clone();
        let view = cx.entity().downgrade();

        h_flex()
            .w_full()
            .px_3()
            .py_1()
            .gap_2()
            .items_center()
            .justify_between()
            .bg(cx.theme().colors().status_bar_background)
            .child(
                KernelSelector::new(
                    Box::new(move |spec: KernelSpecification, window, cx| {
                        if let Some(view) = view.upgrade() {
                            view.update(cx, |this, cx| {
                                this.change_kernel(spec, window, cx);
                            });
                        }
                    }),
                    worktree_id,
                    Button::new("kernel-selector", kernel_name.clone())
                        .label_size(LabelSize::Small)
                        .start_icon(
                            Icon::new(status_icon)
                                .size(IconSize::Small)
                                .color(status_color),
                        ),
                    Tooltip::text(format!(
                        "Kernel: {} ({}). Click to change.",
                        kernel_name,
                        kernel_status.to_string()
                    )),
                )
                .with_handle(kernel_picker_handle),
            )
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        IconButton::new("restart-kernel", IconName::RotateCw)
                            .icon_size(IconSize::Small)
                            .tooltip(|window, cx| {
                                Tooltip::for_action("Restart Kernel", &RestartKernel, cx)
                            })
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.restart_kernel(&RestartKernel, window, cx);
                            })),
                    )
                    .child(
                        IconButton::new("interrupt-kernel", IconName::Stop)
                            .icon_size(IconSize::Small)
                            .disabled(!matches!(kernel_status, KernelStatus::Busy))
                            .tooltip(|window, cx| {
                                Tooltip::for_action("Interrupt Kernel", &InterruptKernel, cx)
                            })
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.interrupt_kernel(&InterruptKernel, window, cx);
                            })),
                    ),
            )
    }

    fn cell_list(&self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity();
        list(self.cell_list.clone(), move |index, window, cx| {
            view.update(cx, |this, cx| {
                let cell_id = &this.cell_order[index];
                let cell = this.cell_map.get(cell_id).unwrap();
                this.render_cell(index, cell, window, cx).into_any_element()
            })
        })
        .size_full()
    }

    fn render_empty_state(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_3()
            .child(Label::new("This notebook is empty.").color(Color::Muted))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("empty-state-add-code", "Add code cell")
                            .start_icon(Icon::new(IconName::Code))
                            .key_binding(KeyBinding::for_action_in(
                                &AddCodeBlock,
                                &self.focus_handle,
                                cx,
                            ))
                            .on_click(
                                cx.listener(|this, _, window, cx| this.add_code_block(window, cx)),
                            ),
                    )
                    .child(
                        Button::new("empty-state-add-markdown", "Add markdown cell")
                            .style(ButtonStyle::Subtle)
                            .start_icon(Icon::new(IconName::FileMarkdown))
                            .key_binding(KeyBinding::for_action_in(
                                &AddMarkdownBlock,
                                &self.focus_handle,
                                cx,
                            ))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.add_markdown_block(window, cx)
                            })),
                    ),
            )
    }

    fn cell_position(&self, index: usize) -> CellPosition {
        match index {
            0 => CellPosition::First,
            index if index == self.cell_count() - 1 => CellPosition::Last,
            _ => CellPosition::Middle,
        }
    }

    fn render_cell(
        &self,
        index: usize,
        cell: &Cell,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let cell_position = self.cell_position(index);

        let is_selected = index == self.selected_cell_index;

        match cell {
            Cell::Code(cell) => {
                cell.update(cx, |cell, _cx| {
                    cell.set_selected(is_selected)
                        .set_cell_position(cell_position);
                });
                cell.clone().into_any_element()
            }
            Cell::Markdown(cell) => {
                cell.update(cx, |cell, _cx| {
                    cell.set_selected(is_selected)
                        .set_cell_position(cell_position);
                });
                cell.clone().into_any_element()
            }
            Cell::Raw(cell) => {
                cell.update(cx, |cell, _cx| {
                    cell.set_selected(is_selected)
                        .set_cell_position(cell_position);
                });
                cell.clone().into_any_element()
            }
        }
    }
}

impl Render for NotebookEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut key_context = KeyContext::new_with_defaults();
        key_context.add("NotebookEditor");
        key_context.set(
            "notebook_mode",
            match self.notebook_mode {
                NotebookMode::Command => "command",
                NotebookMode::Edit => "edit",
            },
        );

        v_flex()
            .size_full()
            .key_context(key_context)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &OpenNotebook, window, cx| {
                this.open_notebook(&OpenNotebook, window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &ClearOutputs, window, cx| this.clear_outputs(window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &Run, window, cx| this.run_current_cell(&Run, window, cx)),
            )
            .on_action(
                cx.listener(|this, action, window, cx| this.run_and_advance(action, window, cx)),
            )
            .on_action(cx.listener(|this, _: &RunAll, window, cx| this.run_cells(window, cx)))
            .on_action(
                cx.listener(|this, _: &MoveCellUp, window, cx| this.move_cell_up(window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &MoveCellDown, window, cx| this.move_cell_down(window, cx)),
            )
            .on_action(cx.listener(|this, _: &AddMarkdownBlock, window, cx| {
                this.add_markdown_block(window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &AddCodeBlock, window, cx| this.add_code_block(window, cx)),
            )
            .on_action(cx.listener(|this, _: &DeleteCell, window, cx| this.delete_cell(window, cx)))
            .on_action(
                cx.listener(|this, action, window, cx| this.enter_edit_mode(action, window, cx)),
            )
            .on_action(cx.listener(|this, action, window, cx| {
                this.handle_enter_command_mode(action, window, cx)
            }))
            .on_action(cx.listener(|this, action, window, cx| {
                this.select_next(action, SelectionMode::SelectOnly, window, cx)
            }))
            .on_action(cx.listener(|this, action, window, cx| {
                this.select_previous(action, SelectionMode::SelectOnly, window, cx)
            }))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_last))
            .on_action(cx.listener(|this, _: &MoveDown, window, cx| {
                this.select_next(
                    &Default::default(),
                    SelectionMode::SelectAndMove,
                    window,
                    cx,
                );
            }))
            .on_action(cx.listener(|this, _: &MoveUp, window, cx| {
                this.select_previous(
                    &Default::default(),
                    SelectionMode::SelectAndMove,
                    window,
                    cx,
                );
            }))
            .on_action(cx.listener(|this, _: &NotebookMoveDown, window, cx| {
                let Some(cell) = this.get_selected_cell() else {
                    return;
                };

                let Some(editor) = cell.editor(cx).cloned() else {
                    return;
                };

                let is_at_last_line = editor.update(cx, |editor, cx| {
                    let display_snapshot = editor.display_snapshot(cx);
                    let selections = editor.selections.all_display(&display_snapshot);
                    if let Some(selection) = selections.last() {
                        let head = selection.head();
                        let cursor_row = head.row();
                        let max_row = display_snapshot.max_point().row();

                        cursor_row >= max_row
                    } else {
                        false
                    }
                });

                if is_at_last_line {
                    this.select_next(
                        &Default::default(),
                        SelectionMode::SelectAndMove,
                        window,
                        cx,
                    );
                } else {
                    editor.update(cx, |editor, cx| {
                        editor.move_down(&Default::default(), window, cx);
                    });
                }
            }))
            .on_action(cx.listener(|this, _: &NotebookMoveUp, window, cx| {
                let Some(cell) = this.get_selected_cell() else {
                    return;
                };

                let Some(editor) = cell.editor(cx).cloned() else {
                    return;
                };

                let is_at_first_line = editor.update(cx, |editor, cx| {
                    let display_snapshot = editor.display_snapshot(cx);
                    let selections = editor.selections.all_display(&display_snapshot);
                    if let Some(selection) = selections.first() {
                        let head = selection.head();
                        let cursor_row = head.row();

                        cursor_row.0 == 0
                    } else {
                        false
                    }
                });

                if is_at_first_line {
                    this.select_previous(
                        &Default::default(),
                        SelectionMode::SelectAndMove,
                        window,
                        cx,
                    );
                } else {
                    editor.update(cx, |editor, cx| {
                        editor.move_up(&Default::default(), window, cx);
                    });
                }
            }))
            .on_action(
                cx.listener(|this, action, window, cx| this.restart_kernel(action, window, cx)),
            )
            .on_action(
                cx.listener(|this, action, window, cx| this.interrupt_kernel(action, window, cx)),
            )
            .child(
                h_flex()
                    .flex_1()
                    .w_full()
                    .h_full()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .h_full()
                            .child(if self.cell_order.is_empty() {
                                self.render_empty_state(cx).into_any_element()
                            } else {
                                self.cell_list(window, cx).into_any_element()
                            }),
                    )
                    .child(self.render_notebook_controls(window, cx)),
            )
            .child(self.render_kernel_status_bar(window, cx))
    }
}

impl Focusable for NotebookEditor {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

// Intended to be a NotebookBuffer
pub struct NotebookItem {
    buffer: Entity<language::Buffer>,
    project_path: ProjectPath,
    languages: Arc<LanguageRegistry>,
    // Raw notebook data
    notebook: nbformat::v4::Notebook,
    // Store our version of the notebook in memory (cell_order, cell_map)
    id: ProjectEntryId,
}

impl project::ProjectItem for NotebookItem {
    fn try_open(
        project: &Entity<Project>,
        path: &ProjectPath,
        cx: &mut App,
    ) -> Option<Task<anyhow::Result<Entity<Self>>>> {
        let path = path.clone();
        let project = project.clone();
        let languages = project.read(cx).languages().clone();

        // For single-file worktrees the relative path is empty, so fall back
        // to the absolute path to detect notebooks opened directly.
        let abs_path = project.read(cx).absolute_path(&path, cx);
        let is_notebook = path.path.extension().unwrap_or_default() == NOTEBOOK_EXTENSION
            || abs_path
                .as_ref()
                .and_then(|abs_path| abs_path.extension())
                .is_some_and(|extension| extension == NOTEBOOK_EXTENSION);

        if is_notebook {
            Some(cx.spawn(async move |cx| {
                // todo: watch for changes to the file
                let buffer = project
                    .update(cx, |project, cx| project.open_buffer(path.clone(), cx))
                    .await?;
                let file_content = buffer.read_with(cx, |buffer, _| buffer.text());

                let notebook = if file_content.trim().is_empty() {
                    nbformat::v4::Notebook {
                        nbformat: 4,
                        nbformat_minor: 5,
                        cells: vec![],
                        metadata: serde_json::from_str("{}").unwrap(),
                    }
                } else {
                    let notebook = match nbformat::parse_notebook(&file_content) {
                        Ok(nb) => nb,
                        Err(_) => {
                            // Pre-process to ensure IDs exist
                            let mut json: serde_json::Value = serde_json::from_str(&file_content)?;
                            if let Some(cells) =
                                json.get_mut("cells").and_then(|c| c.as_array_mut())
                            {
                                for cell in cells.iter_mut().filter_map(|cell| cell.as_object_mut())
                                {
                                    cell.entry("id").or_insert_with(|| {
                                        serde_json::Value::String(Uuid::new_v4().to_string())
                                    });
                                }
                            }
                            let file_content = serde_json::to_string(&json)?;
                            nbformat::parse_notebook(&file_content)?
                        }
                    };

                    match notebook {
                        nbformat::Notebook::V4(notebook) => notebook,
                        // 4.1 - 4.4 are converted to 4.5
                        nbformat::Notebook::Legacy(legacy_notebook) => {
                            // TODO: Decide if we want to mutate the notebook by including Cell IDs
                            // and any other conversions

                            nbformat::upgrade_legacy_notebook(legacy_notebook)?
                        }
                        nbformat::Notebook::V3(v3_notebook) => {
                            nbformat::upgrade_v3_notebook(v3_notebook)?
                        }
                    }
                };

                let id = project
                    .update(cx, |project, cx| {
                        project.entry_for_path(&path, cx).map(|entry| entry.id)
                    })
                    .context("Entry not found")?;

                Ok(cx.new(|_| NotebookItem {
                    buffer,
                    project_path: path,
                    languages,
                    notebook,
                    id,
                }))
            }))
        } else {
            None
        }
    }

    fn entry_id(&self, _: &App) -> Option<ProjectEntryId> {
        Some(self.id)
    }

    fn project_path(&self, _: &App) -> Option<ProjectPath> {
        Some(self.project_path.clone())
    }

    fn is_dirty(&self) -> bool {
        // TODO: Track if notebook metadata or structure has changed
        false
    }
}

impl NotebookItem {
    pub fn language_name(&self) -> Option<String> {
        self.notebook
            .metadata
            .language_info
            .as_ref()
            .map(|l| l.name.clone())
            .or(self
                .notebook
                .metadata
                .kernelspec
                .as_ref()
                .and_then(|spec| spec.language.clone()))
    }

    pub fn notebook_language(&self) -> impl Future<Output = Option<Arc<Language>>> + use<> {
        let language_name = self.language_name();
        let languages = self.languages.clone();

        async move {
            if let Some(language_name) = language_name {
                languages.language_for_name(&language_name).await.ok()
            } else {
                None
            }
        }
    }
}

impl EventEmitter<()> for NotebookItem {}

impl EventEmitter<()> for NotebookEditor {}

// pub struct NotebookControls {
//     pane_focused: bool,
//     active_item: Option<Box<dyn ItemHandle>>,
//     // subscription: Option<Subscription>,
// }

// impl NotebookControls {
//     pub fn new() -> Self {
//         Self {
//             pane_focused: false,
//             active_item: Default::default(),
//             // subscription: Default::default(),
//         }
//     }
// }

// impl EventEmitter<ToolbarItemEvent> for NotebookControls {}

// impl Render for NotebookControls {
//     fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
//         div().child("notebook controls")
//     }
// }

// impl ToolbarItemView for NotebookControls {
//     fn set_active_pane_item(
//         &mut self,
//         active_pane_item: Option<&dyn workspace::ItemHandle>,
//         window: &mut Window, cx: &mut Context<Self>,
//     ) -> workspace::ToolbarItemLocation {
//         cx.notify();
//         self.active_item = None;

//         let Some(item) = active_pane_item else {
//             return ToolbarItemLocation::Hidden;
//         };

//         ToolbarItemLocation::PrimaryLeft
//     }

//     fn pane_focus_update(&mut self, pane_focused: bool, _window: &mut Window, _cx: &mut Context<Self>) {
//         self.pane_focused = pane_focused;
//     }
// }

impl Item for NotebookEditor {
    fn to_item_events(_: &(), emit: &mut dyn FnMut(ItemEvent)) {
        emit(ItemEvent::UpdateTab);
    }

    type Event = ();

    fn can_split(&self) -> bool {
        true
    }

    fn clone_on_split(
        &self,
        _workspace_id: Option<workspace::WorkspaceId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Option<Entity<Self>>>
    where
        Self: Sized,
    {
        Task::ready(Some(cx.new(|cx| {
            Self::new(self.project.clone(), self.notebook_item.clone(), window, cx)
        })))
    }

    fn buffer_kind(&self, _: &App) -> workspace::item::ItemBufferKind {
        workspace::item::ItemBufferKind::Singleton
    }

    fn for_each_project_item(
        &self,
        cx: &App,
        f: &mut dyn FnMut(gpui::EntityId, &dyn project::ProjectItem),
    ) {
        f(self.notebook_item.entity_id(), self.notebook_item.read(cx))
    }

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        self.notebook_item
            .read(cx)
            .project_path
            .path
            .file_name()
            .map(|s| s.to_string())
            .unwrap_or_default()
            .into()
    }

    fn tab_content(&self, params: TabContentParams, window: &Window, cx: &App) -> AnyElement {
        Label::new(self.tab_content_text(params.detail.unwrap_or(0), cx))
            .single_line()
            .color(params.text_color())
            .when(params.preview, |this| this.italic())
            .into_any_element()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(IconName::Book.into())
    }

    fn show_toolbar(&self) -> bool {
        false
    }

    // TODO
    fn pixel_position_of_cursor(&self, _: &App) -> Option<Point<Pixels>> {
        None
    }

    // TODO
    fn as_searchable(&self, _: &Entity<Self>, _: &App) -> Option<Box<dyn SearchableItemHandle>> {
        None
    }

    fn set_nav_history(
        &mut self,
        _: workspace::ItemNavHistory,
        _window: &mut Window,
        _: &mut Context<Self>,
    ) {
        // TODO
    }

    fn can_save(&self, _cx: &App) -> bool {
        true
    }

    fn save(
        &mut self,
        options: SaveOptions,
        project: Entity<Project>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        // The backing buffer already holds the external version, so its identity
        // check would pass; only an explicit overwrite may replace that version.
        if self.external_change_pending && !options.overwrite {
            return Task::ready(Err(anyhow::anyhow!(
                "The notebook changed on disk since you started editing it"
            )));
        }
        let overwrite_file = options
            .overwrite
            .then(|| {
                self.notebook_item
                    .read(cx)
                    .buffer
                    .read(cx)
                    .file()
                    .map(|file| file.to_proto(cx))
            })
            .flatten();
        self.save_impl(SaveDestination::CurrentPath(overwrite_file), project, cx)
    }

    fn save_as(
        &mut self,
        project: Entity<Project>,
        path: ProjectPath,
        expected: Option<language::DiskState>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.save_impl(SaveDestination::NewPath(path, expected), project, cx)
    }

    fn reload(
        &mut self,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let buffer = self.notebook_item.read(cx).buffer.clone();

        cx.spawn_in(window, async move |this, cx| {
            project
                .update(cx, |project, cx| {
                    project.reload_buffers([buffer.clone()].into_iter().collect(), true, cx)
                })
                .await?;

            let file_content = buffer.read_with(cx, |buffer, _| buffer.text());
            let notebook = parse_notebook_text(&file_content)?;

            this.update_in(cx, |this, window, cx| {
                this.replace_cells(notebook, window, cx);
                this.external_change_pending = false;
                cx.emit(());
            })?;

            Ok(())
        })
    }

    fn has_conflict(&self, cx: &App) -> bool {
        self.external_change_pending || self.notebook_item.read(cx).buffer.read(cx).has_conflict()
    }

    fn is_dirty(&self, cx: &App) -> bool {
        self.has_conflict(cx) || self.is_modified(cx)
    }
}

impl ProjectItem for NotebookEditor {
    type Item = NotebookItem;

    fn for_project_item(
        project: Entity<Project>,
        _pane: Option<&Pane>,
        item: Entity<Self::Item>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new(project, item, window, cx)
    }
}

impl KernelSession for NotebookEditor {
    fn route(&mut self, message: &JupyterMessage, window: &mut Window, cx: &mut Context<Self>) {
        // Handle kernel status updates (these are broadcast to all)
        if let JupyterMessageContent::Status(status) = &message.content {
            self.kernel.set_execution_state(&status.execution_state);
            cx.notify();
        }

        if let JupyterMessageContent::KernelInfoReply(reply) = &message.content {
            self.kernel.set_kernel_info(reply);

            if let Ok(language_info) = serde_json::from_value::<nbformat::v4::LanguageInfo>(
                serde_json::to_value(&reply.language_info).unwrap(),
            ) {
                self.notebook_item.update(cx, |item, cx| {
                    item.notebook.metadata.language_info = Some(language_info);
                    cx.emit(());
                });
            }
            cx.notify();
        }

        // Handle cell-specific messages
        if let Some(parent_header) = &message.parent_header {
            if let Some(cell_id) = self.execution_requests.get(&parent_header.msg_id) {
                if let Some(Cell::Code(cell)) = self.cell_map.get(cell_id) {
                    cell.update(cx, |cell, cx| {
                        cell.handle_message(message, window, cx);
                    });
                }
            }
        }
    }

    fn kernel_errored(&mut self, error_message: String, cx: &mut Context<Self>) {
        self.kernel = Kernel::ErroredLaunch(error_message);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::super::RunnableCell as _;
    use super::*;
    use gpui::{TestAppContext, VisualTestContext};
    use project::{FakeFs, Project, ProjectItem as _};
    use serde_json::json;
    use settings::SettingsStore;
    use util::path;
    use util::rel_path::rel_path;

    const NOTEBOOK_WITH_ONE_CODE_CELL: &str = r#"{
        "metadata": {
            "kernelspec": {
                "display_name": "Python 3",
                "language": "python",
                "name": "python3"
            },
            "language_info": {
                "name": "python"
            }
        },
        "nbformat": 4,
        "nbformat_minor": 5,
        "cells": [
            {
                "cell_type": "code",
                "id": "cell-one",
                "metadata": {},
                "execution_count": null,
                "outputs": [],
                "source": ["print('hello')"]
            }
        ]
    }"#;

    #[gpui::test]
    async fn test_automatic_reload_reconciles_notebook_edits(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/notebooks"),
            json!({ "test.ipynb": NOTEBOOK_WITH_ONE_CODE_CELL }),
        )
        .await;
        let project = Project::test(fs.clone(), [path!("/notebooks").as_ref()], cx).await;
        cx.update(|cx| ReplStore::init(fs.clone(), cx));
        let project_path = project.read_with(cx, |project, cx| ProjectPath {
            worktree_id: project.worktrees(cx).next().unwrap().read(cx).id(),
            path: rel_path("test.ipynb").into(),
        });
        let notebook_item = cx
            .update(|cx| {
                NotebookItem::try_open(&project, &project_path, cx)
                    .expect("ipynb files should be openable as notebooks")
            })
            .await
            .expect("notebook should parse");
        let cx = cx.add_empty_window();
        let notebook_editor = cx.update(|window, cx| {
            cx.new(|cx| NotebookEditor::new(project.clone(), notebook_item, window, cx))
        });

        let path = std::path::Path::new(path!("/notebooks/test.ipynb"));
        let external = NOTEBOOK_WITH_ONE_CODE_CELL.replace("print('hello')", "print('external')");
        let notebook_json = |cx: &mut VisualTestContext| {
            notebook_editor.read_with(cx, |editor, cx| {
                serde_json::to_string(&editor.to_notebook(cx)).expect("notebook JSON")
            })
        };
        let edit_first_cell = |text: &str, cx: &mut VisualTestContext| {
            let cell_editor = notebook_editor.read_with(cx, |editor, cx| {
                let cell_id = editor.cell_order.first().expect("notebook has a cell");
                let Some(Cell::Code(cell)) = editor.cell_map.get(cell_id) else {
                    panic!("expected a code cell");
                };
                cell.read(cx).editor().clone()
            });
            cell_editor.update_in(cx, |editor, window, cx| editor.set_text(text, window, cx));
        };
        let file_contents =
            || String::from_utf8(fs.read_file_sync(path).expect("notebook file")).expect("UTF-8");

        // A clean notebook follows the external version.
        fs.insert_file(path, external.clone().into_bytes()).await;
        cx.run_until_parked();
        notebook_editor.read_with(cx, |editor, cx| {
            assert!(!editor.has_conflict(cx));
            assert!(!editor.is_dirty(cx));
        });
        assert!(notebook_json(cx).contains("print('external')"));

        // Unsaved cell edits survive, and only an explicit overwrite replaces the file.
        edit_first_cell("print('mine')", cx);
        fs.insert_file(path, NOTEBOOK_WITH_ONE_CODE_CELL.as_bytes().to_vec())
            .await;
        cx.run_until_parked();
        notebook_editor.read_with(cx, |editor, cx| {
            assert!(editor.has_conflict(cx));
            assert!(editor.is_dirty(cx));
        });
        assert!(notebook_json(cx).contains("print('mine')"));
        notebook_editor
            .update_in(cx, |editor, window, cx| {
                editor.save(SaveOptions::default(), project.clone(), window, cx)
            })
            .await
            .expect_err("an ordinary save must not replace the external version");
        assert_eq!(file_contents(), NOTEBOOK_WITH_ONE_CODE_CELL);
        notebook_editor
            .update_in(cx, |editor, window, cx| {
                editor.save(
                    SaveOptions {
                        overwrite: true,
                        ..Default::default()
                    },
                    project.clone(),
                    window,
                    cx,
                )
            })
            .await
            .expect("a confirmed overwrite replaces the external version");
        assert!(file_contents().contains("print('mine')"));
        notebook_editor.read_with(cx, |editor, cx| {
            assert!(!editor.has_conflict(cx));
            assert!(!editor.is_dirty(cx));
        });

        // Structural edits survive too, and discarding them loads the external version.
        notebook_editor.update_in(cx, |editor, window, cx| editor.add_code_block(window, cx));
        fs.insert_file(path, external.clone().into_bytes()).await;
        cx.run_until_parked();
        notebook_editor.read_with(cx, |editor, cx| {
            assert!(editor.has_conflict(cx));
            assert_eq!(editor.cell_order.len(), 2);
        });
        notebook_editor
            .update_in(cx, |editor, window, cx| {
                editor.reload(project.clone(), window, cx)
            })
            .await
            .expect("discard notebook edits");
        notebook_editor.read_with(cx, |editor, cx| {
            assert!(!editor.has_conflict(cx));
            assert!(!editor.is_dirty(cx));
            assert_eq!(editor.cell_order.len(), 1);
        });
        assert!(notebook_json(cx).contains("print('external')"));

        // An emptied file reloads as an empty notebook.
        fs.insert_file(path, Vec::new()).await;
        cx.run_until_parked();
        notebook_editor.read_with(cx, |editor, cx| {
            assert!(!editor.has_conflict(cx));
            assert!(editor.cell_order.is_empty());
        });
    }

    #[gpui::test]
    async fn test_reloaded_cells_keep_outputs_wiring_and_selection(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/notebooks"),
            json!({ "test.ipynb": NOTEBOOK_WITH_ONE_CODE_CELL }),
        )
        .await;
        let project = Project::test(fs.clone(), [path!("/notebooks").as_ref()], cx).await;
        cx.update(|cx| ReplStore::init(fs.clone(), cx));
        let project_path = project.read_with(cx, |project, cx| ProjectPath {
            worktree_id: project.worktrees(cx).next().unwrap().read(cx).id(),
            path: rel_path("test.ipynb").into(),
        });
        let notebook_item = cx
            .update(|cx| {
                NotebookItem::try_open(&project, &project_path, cx)
                    .expect("ipynb files should be openable as notebooks")
            })
            .await
            .expect("notebook should parse");
        let cx = cx.add_empty_window();
        let notebook_editor = cx.update(|window, cx| {
            cx.new(|cx| NotebookEditor::new(project.clone(), notebook_item, window, cx))
        });
        let path = std::path::Path::new(path!("/notebooks/test.ipynb"));
        let code_cell = |index: usize, cx: &mut VisualTestContext| {
            notebook_editor.read_with(cx, |editor, _| {
                let cell_id = &editor.cell_order[index];
                let Some(Cell::Code(cell)) = editor.cell_map.get(cell_id) else {
                    panic!("expected a code cell");
                };
                cell.clone()
            })
        };

        // Kernel status for a running cell changes nothing that would be saved.
        let cell_id = notebook_editor.read_with(cx, |editor, _| editor.cell_order[0].clone());
        let request: JupyterMessage = ExecuteRequest::new("print('hello')".to_string()).into();
        notebook_editor.update_in(cx, |editor, window, cx| {
            editor
                .execution_requests
                .insert(request.header.msg_id.clone(), cell_id);
            let status = JupyterMessage::new(jupyter_protocol::Status::idle(), Some(&request));
            editor.route(&status, window, cx);
            assert!(!editor.has_unsaved_changes(cx));
            editor.execution_requests.clear();

            // Neither does clearing outputs that aren't there.
            editor.clear_outputs(window, cx);
            assert!(!editor.has_unsaved_changes(cx));
        });

        // Execution results are unsaved state even though no source changed.
        code_cell(0, cx).update(cx, |cell, _| {
            cell.set_execution_count(3);
        });
        let external = NOTEBOOK_WITH_ONE_CODE_CELL.replace("print('hello')", "print('external')");
        fs.insert_file(path, external.into_bytes()).await;
        cx.run_until_parked();
        notebook_editor.read_with(cx, |editor, cx| {
            assert!(editor.has_conflict(cx));
            assert_eq!(code_cell_execution_count(editor, cx), Some(3));
        });
        notebook_editor
            .update_in(cx, |editor, window, cx| {
                editor.reload(project.clone(), window, cx)
            })
            .await
            .expect("discard execution results");

        // A running cell keeps the notebook from being replaced.
        code_cell(0, cx).update(cx, |cell, _| cell.start_execution());
        let running =
            NOTEBOOK_WITH_ONE_CODE_CELL.replace("print('hello')", "print('while running')");
        fs.insert_file(path, running.into_bytes()).await;
        cx.run_until_parked();
        notebook_editor.read_with(cx, |editor, cx| assert!(editor.has_conflict(cx)));
        code_cell(0, cx).update(cx, |cell, _| cell.finish_execution());
        notebook_editor
            .update_in(cx, |editor, window, cx| {
                editor.reload(project.clone(), window, cx)
            })
            .await
            .expect("discard after execution");

        // A newly selected kernel is kept as well.
        notebook_editor.update(cx, |editor, cx| {
            editor.notebook_item.update(cx, |item, _| {
                item.notebook.metadata.kernelspec = serde_json::from_value(json!({
                    "display_name": "Other",
                    "name": "other",
                    "language": "python"
                }))
                .ok();
            });
        });
        let kernel_change =
            NOTEBOOK_WITH_ONE_CODE_CELL.replace("print('hello')", "print('kernel change')");
        fs.insert_file(path, kernel_change.into_bytes()).await;
        cx.run_until_parked();
        notebook_editor.read_with(cx, |editor, cx| assert!(editor.has_conflict(cx)));
        notebook_editor
            .update_in(cx, |editor, window, cx| {
                editor.reload(project.clone(), window, cx)
            })
            .await
            .expect("discard the kernel selection");

        // Rich outputs are part of what is saved, so rerunning a cell into the
        // same output is no change, while clearing it is kept.
        let cell_id = notebook_editor.read_with(cx, |editor, _| editor.cell_order[0].clone());
        notebook_editor.update_in(cx, |editor, window, cx| {
            editor
                .execution_requests
                .insert(request.header.msg_id.clone(), cell_id);
            let markdown = || {
                JupyterMessage::new(
                    jupyter_protocol::DisplayData::from(vec![
                        jupyter_protocol::MediaType::Markdown("**rich**".to_string()),
                    ]),
                    Some(&request),
                )
            };
            editor.route(&markdown(), window, cx);
            assert!(editor.has_unsaved_changes(cx));
            // New outputs are unsaved work for closing the tab as well.
            assert!(editor.is_dirty(cx));
            // Outputs that can't be displayed are still written back.
            editor.route(
                &JupyterMessage::new(
                    jupyter_protocol::DisplayData::from(vec![jupyter_protocol::MediaType::Svg(
                        "<svg/>".to_string(),
                    )]),
                    Some(&request),
                ),
                window,
                cx,
            );
            let serialized = serde_json::to_string(&editor.to_notebook(cx)).unwrap();
            assert!(serialized.contains("**rich**"), "{serialized}");
            assert!(serialized.contains("<svg/>"), "{serialized}");
            editor.mark_saved(editor.snapshot(cx));
            assert!(!editor.has_unsaved_changes(cx));
            assert!(!editor.is_dirty(cx));

            editor.clear_outputs(window, cx);
            assert!(editor.has_unsaved_changes(cx));
            editor.route(&markdown(), window, cx);
            assert!(editor.has_unsaved_changes(cx));
            editor.route(
                &JupyterMessage::new(
                    jupyter_protocol::DisplayData::from(vec![jupyter_protocol::MediaType::Svg(
                        "<svg/>".to_string(),
                    )]),
                    Some(&request),
                ),
                window,
                cx,
            );
            assert!(!editor.has_unsaved_changes(cx));

            editor.execution_requests.clear();
            editor.clear_outputs(window, cx);
        });
        let after_clear =
            NOTEBOOK_WITH_ONE_CODE_CELL.replace("print('hello')", "print('after clear')");
        fs.insert_file(path, after_clear.into_bytes()).await;
        cx.run_until_parked();
        notebook_editor.read_with(cx, |editor, cx| assert!(editor.has_conflict(cx)));
        notebook_editor
            .update_in(cx, |editor, window, cx| {
                editor.reload(project.clone(), window, cx)
            })
            .await
            .expect("discard the cleared outputs");

        // Cells rebuilt by a reload keep their event wiring.
        let two_cells = NOTEBOOK_WITH_ONE_CODE_CELL.replace(
            r#""source": ["print('hello')"]
            }"#,
            r#""source": ["print('hello')"]
            },
            {
                "cell_type": "code",
                "id": "cell-two",
                "metadata": {},
                "execution_count": null,
                "outputs": [],
                "source": ["print('two')"]
            }"#,
        );
        assert_ne!(two_cells, NOTEBOOK_WITH_ONE_CODE_CELL);
        fs.insert_file(path, two_cells.clone().into_bytes()).await;
        cx.run_until_parked();
        assert_eq!(
            notebook_editor.read_with(cx, |editor, _| editor.cell_order.len()),
            2
        );
        let second_id = notebook_editor.read_with(cx, |editor, _| editor.cell_order[1].clone());
        code_cell(1, cx).update(cx, |_, cx| cx.emit(CellEvent::FocusedIn(second_id)));
        cx.run_until_parked();
        assert_eq!(
            notebook_editor.read_with(cx, |editor, _| editor.selected_cell_index),
            1
        );

        // Only the cells the file changed are rebuilt.
        let first_cell = code_cell(0, cx);
        let second_cell = code_cell(1, cx);
        let second_changed = two_cells.replace("print('two')", "print('two changed')");
        fs.insert_file(path, second_changed.clone().into_bytes())
            .await;
        cx.run_until_parked();
        assert_eq!(code_cell(0, cx).entity_id(), first_cell.entity_id());
        assert_ne!(code_cell(1, cx).entity_id(), second_cell.entity_id());
        assert_eq!(
            code_cell(1, cx).read_with(cx, |cell, cx| cell.current_source(cx)),
            "print('two changed')"
        );

        // A shorter reloaded notebook keeps the selection in range.
        fs.insert_file(path, NOTEBOOK_WITH_ONE_CODE_CELL.as_bytes().to_vec())
            .await;
        cx.run_until_parked();
        notebook_editor.read_with(cx, |editor, _| {
            assert_eq!(editor.cell_order.len(), 1);
            assert_eq!(editor.selected_cell_index, 0);
        });
        notebook_editor.update_in(cx, |editor, window, cx| editor.add_code_block(window, cx));
        assert_eq!(
            notebook_editor.read_with(cx, |editor, _| editor.cell_order.len()),
            2
        );
    }

    #[test]
    fn test_parse_notebook_text_rejects_non_object_cells() {
        let text = r#"{"cells": [1], "metadata": {}, "nbformat": 4, "nbformat_minor": 5}"#;
        assert!(parse_notebook_text(text).is_err());
    }

    #[derive(Debug)]
    struct FakeRunningKernel {
        request_tx: futures::channel::mpsc::Sender<JupyterMessage>,
        working_directory: std::path::PathBuf,
        execution_state: jupyter_protocol::ExecutionState,
    }

    impl crate::kernels::RunningKernel for FakeRunningKernel {
        fn request_tx(&self) -> futures::channel::mpsc::Sender<JupyterMessage> {
            self.request_tx.clone()
        }

        fn stdin_tx(&self) -> futures::channel::mpsc::Sender<JupyterMessage> {
            self.request_tx.clone()
        }

        fn working_directory(&self) -> &std::path::PathBuf {
            &self.working_directory
        }

        fn execution_state(&self) -> &jupyter_protocol::ExecutionState {
            &self.execution_state
        }

        fn set_execution_state(&mut self, state: jupyter_protocol::ExecutionState) {
            self.execution_state = state;
        }

        fn kernel_info(&self) -> Option<&jupyter_protocol::KernelInfoReply> {
            None
        }

        fn set_kernel_info(&mut self, _info: jupyter_protocol::KernelInfoReply) {}

        fn force_shutdown(&mut self, _window: &mut Window, _cx: &mut App) -> Task<Result<()>> {
            Task::ready(Ok(()))
        }

        fn kill(&mut self) {}
    }

    #[gpui::test]
    async fn test_running_a_cell_without_output_keeps_notebook_clean(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/notebooks"),
            json!({ "test.ipynb": NOTEBOOK_WITH_ONE_CODE_CELL }),
        )
        .await;
        let project = Project::test(fs.clone(), [path!("/notebooks").as_ref()], cx).await;
        cx.update(|cx| ReplStore::init(fs.clone(), cx));
        let project_path = project.read_with(cx, |project, cx| ProjectPath {
            worktree_id: project.worktrees(cx).next().unwrap().read(cx).id(),
            path: rel_path("test.ipynb").into(),
        });
        let notebook_item = cx
            .update(|cx| {
                NotebookItem::try_open(&project, &project_path, cx)
                    .expect("ipynb files should be openable as notebooks")
            })
            .await
            .expect("notebook should parse");
        let cx = cx.add_empty_window();
        let notebook_editor = cx.update(|window, cx| {
            cx.new(|cx| NotebookEditor::new(project.clone(), notebook_item, window, cx))
        });

        let (request_tx, _request_rx) = futures::channel::mpsc::channel(8);
        notebook_editor.update_in(cx, |editor, window, cx| {
            editor.kernel = Kernel::RunningKernel(Box::new(FakeRunningKernel {
                request_tx,
                working_directory: std::path::PathBuf::from(path!("/notebooks")),
                execution_state: jupyter_protocol::ExecutionState::Idle,
            }));
            let cell_id = editor.cell_order[0].clone();
            editor.execute_cell(cell_id, window, cx);
            assert!(editor.has_unsaved_changes(cx));
        });

        // An aborted request finishes without output or an execution count.
        notebook_editor.update(cx, |editor, cx| {
            let Some(Cell::Code(cell)) = editor.cell_map.get(&editor.cell_order[0]) else {
                panic!("expected a code cell");
            };
            cell.update(cx, |cell, _| cell.finish_execution());
            assert!(!editor.has_unsaved_changes(cx));
        });

        // Neither does a rerun that reports the execution count already saved.
        let counted = NOTEBOOK_WITH_ONE_CODE_CELL
            .replace(r#""execution_count": null"#, r#""execution_count": 1"#);
        assert_ne!(counted, NOTEBOOK_WITH_ONE_CODE_CELL);
        fs.insert_file(path!("/notebooks/test.ipynb"), counted.into_bytes())
            .await;
        cx.run_until_parked();
        notebook_editor.update_in(cx, |editor, window, cx| {
            assert_eq!(code_cell_execution_count(editor, cx), Some(1));
            let request: JupyterMessage = ExecuteRequest::new("print('hello')".to_string()).into();
            let cell_id = editor.cell_order[0].clone();
            editor
                .execution_requests
                .insert(request.header.msg_id.clone(), cell_id);
            let input = jupyter_protocol::ExecuteInput {
                code: "print('hello')".to_string(),
                execution_count: jupyter_protocol::ExecutionCount::new(1),
            };
            editor.route(&JupyterMessage::new(input, Some(&request)), window, cx);
            assert_eq!(code_cell_execution_count(editor, cx), Some(1));
            assert!(!editor.has_unsaved_changes(cx));
        });
    }

    fn code_cell_execution_count(editor: &NotebookEditor, cx: &App) -> Option<i32> {
        let cell_id = editor.cell_order.first()?;
        let Some(Cell::Code(cell)) = editor.cell_map.get(cell_id) else {
            return None;
        };
        cell.read(cx).execution_count()
    }

    /// When the configured interpreter doesn't exist (e.g. Python isn't installed),
    /// running a cell must not leave it stuck in the executing state. It should
    /// instead surface the kernel launch error as an error output on the cell.
    #[gpui::test]
    async fn test_run_cell_with_missing_interpreter_shows_error(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/notebooks"),
            json!({ "test.ipynb": NOTEBOOK_WITH_ONE_CODE_CELL }),
        )
        .await;

        let project = Project::test(fs.clone(), [path!("/notebooks").as_ref()], cx).await;
        cx.update(|cx| ReplStore::init(fs.clone(), cx));

        let worktree_id = project.read_with(cx, |project, cx| {
            project.worktrees(cx).next().unwrap().read(cx).id()
        });

        // Select a kernel whose interpreter doesn't exist, simulating a machine
        // where Python isn't installed properly. This is the same path the
        // kernel picker uses.
        let missing_interpreter = path!("/nonexistent/python3");
        let broken_spec = KernelSpecification::Jupyter(LocalKernelSpecification {
            name: "python3".to_string(),
            path: PathBuf::from(missing_interpreter),
            kernelspec: JupyterKernelspec {
                argv: vec![
                    missing_interpreter.to_string(),
                    "-m".to_string(),
                    "ipykernel_launcher".to_string(),
                    "-f".to_string(),
                    "{connection_file}".to_string(),
                ],
                display_name: "Python 3".to_string(),
                language: "python".to_string(),
                interrupt_mode: None,
                metadata: None,
                env: None,
            },
        });
        cx.update(|cx| {
            ReplStore::global(cx).update(cx, |store, cx| {
                store.set_active_kernelspec(worktree_id, broken_spec, cx);
            })
        });

        let notebook_item = cx
            .update(|cx| {
                NotebookItem::try_open(
                    &project,
                    &ProjectPath {
                        worktree_id,
                        path: rel_path("test.ipynb").into(),
                    },
                    cx,
                )
                .expect("ipynb files should be openable as notebooks")
            })
            .await
            .expect("notebook should parse");

        // Don't render the notebook UI itself: its animated kernel status icon
        // schedules a new frame on every render, which makes `run_until_parked`
        // spin forever in tests. The editor entity is created inside an empty
        // window instead; we are testing execution behavior, not rendering.
        let cx = cx.add_empty_window();

        // Launching a kernel probes real TCP ports on localhost, which the
        // deterministic test scheduler cannot drive.
        cx.executor().allow_parking();

        let editor = cx.update(|window, cx| {
            cx.new(|cx| NotebookEditor::new(project.clone(), notebook_item, window, cx))
        });

        // Creating the editor launches the kernel. Wait for the actual launch
        // task, which fails because the interpreter cannot be spawned.
        let pending_kernel = editor.read_with(cx, |editor, _| match &editor.kernel {
            Kernel::StartingKernel(task) => task.clone(),
            _ => panic!("kernel should be starting right after the editor is created"),
        });
        pending_kernel.await;

        editor.read_with(cx, |editor, _| {
            assert!(
                matches!(editor.kernel, Kernel::ErroredLaunch(_)),
                "kernel launch should fail, instead status is: {}",
                editor.kernel.status().to_string()
            );
        });

        // Run the (only) cell via the production action handler.
        editor.update_in(cx, |editor, window, cx| {
            editor.run_current_cell(&Run, window, cx);
        });

        editor.read_with(cx, |editor, cx| {
            let cell_id = editor.cell_order.first().expect("notebook has one cell");
            let Some(Cell::Code(cell)) = editor.cell_map.get(cell_id) else {
                panic!("expected a code cell");
            };
            let cell = cell.read(cx);

            assert!(
                !cell.is_executing(),
                "cell must not be stuck in the executing state when the kernel is not running"
            );

            let nbformat::v4::Cell::Code { outputs, .. } = cell.to_nbformat_cell(cx) else {
                panic!("expected a code cell");
            };
            match outputs.as_slice() {
                [nbformat::v4::Output::Error(error)] => {
                    assert_eq!(error.ename, "Kernel Error");
                    let traceback = error.traceback.join("\n");
                    assert!(
                        traceback.contains("the kernel failed to launch"),
                        "error output should explain why the cell could not run, got: {traceback}"
                    );
                }
                other => panic!("expected a single error output, got: {other:?}"),
            }
        });
    }

    /// Opening a notebook as a single file (its own worktree) leaves the
    /// worktree-relative path empty, so only the absolute path carries the
    /// `.ipynb` extension. `try_open` must still recognize it as a notebook.
    #[gpui::test]
    async fn test_open_single_file_notebook(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/notebooks"),
            json!({ "single.ipynb": NOTEBOOK_WITH_ONE_CODE_CELL }),
        )
        .await;

        let project =
            Project::test(fs.clone(), [path!("/notebooks/single.ipynb").as_ref()], cx).await;
        cx.update(|cx| ReplStore::init(fs.clone(), cx));

        let project_path = project.read_with(cx, |project, cx| {
            let worktree = project.worktrees(cx).next().unwrap();
            let worktree = worktree.read(cx);
            assert!(
                worktree.is_single_file(),
                "opening a bare .ipynb should create a single-file worktree"
            );
            ProjectPath {
                worktree_id: worktree.id(),
                path: worktree.root_entry().unwrap().path.clone(),
            }
        });

        assert!(
            project_path.path.extension().is_none(),
            "single-file worktree relative path should have no extension"
        );

        let notebook_item = cx
            .update(|cx| {
                NotebookItem::try_open(&project, &project_path, cx)
                    .expect("single-file .ipynb should open as a notebook")
            })
            .await
            .expect("notebook should parse");

        notebook_item.read_with(cx, |item, _| {
            assert_eq!(item.notebook.cells.len(), 1);
        });
    }

    /// Notebooks must be saved through the project rather than through the
    /// client's own filesystem, otherwise a remote notebook's path is resolved
    /// against the local machine and the save fails.
    #[gpui::test]
    async fn test_save_goes_through_the_project(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/notebooks"),
            json!({ "test.ipynb": NOTEBOOK_WITH_ONE_CODE_CELL }),
        )
        .await;

        let project = Project::test(fs.clone(), [path!("/notebooks").as_ref()], cx).await;
        cx.update(|cx| ReplStore::init(fs.clone(), cx));

        let project_path = project.read_with(cx, |project, cx| ProjectPath {
            worktree_id: project.worktrees(cx).next().unwrap().read(cx).id(),
            path: rel_path("test.ipynb").into(),
        });

        let notebook_item = cx
            .update(|cx| {
                NotebookItem::try_open(&project, &project_path, cx)
                    .expect("ipynb files should be openable as notebooks")
            })
            .await
            .expect("notebook should parse");

        // Held across the save: a save that bypasses the project writes the file
        // behind this buffer's back, leaving it stale.
        let buffer = project
            .update(cx, |project, cx| {
                project.open_buffer(project_path.clone(), cx)
            })
            .await
            .expect("notebook buffer should open");

        // Rendering the notebook animates the kernel status icon, which makes
        // `run_until_parked` spin forever; only the editor entity is needed here.
        let cx = cx.add_empty_window();
        let notebook_editor = cx.update(|window, cx| {
            cx.new(|cx| NotebookEditor::new(project.clone(), notebook_item, window, cx))
        });

        let cell_editor = notebook_editor.read_with(cx, |notebook_editor, cx| {
            let cell_id = notebook_editor
                .cell_order
                .first()
                .expect("notebook has one cell");
            let Some(Cell::Code(cell)) = notebook_editor.cell_map.get(cell_id) else {
                panic!("expected a code cell");
            };
            cell.read(cx).editor().clone()
        });
        cell_editor.update_in(cx, |cell_editor, window, cx| {
            cell_editor.set_text("print('goodbye')", window, cx);
        });

        notebook_editor
            .update(cx, |editor, cx| {
                editor.save_impl(
                    SaveDestination::NewPath(project_path.clone(), Some(language::DiskState::New)),
                    project.clone(),
                    cx,
                )
            })
            .await
            .expect_err("a destination created after confirmation must reject the save");
        assert!(
            notebook_editor.read_with(cx, |editor, cx| editor.is_dirty(cx)),
            "failed save must retain unsaved cell edits"
        );
        assert!(
            String::from_utf8(
                fs.read_file_sync(path!("/notebooks/test.ipynb"))
                    .expect("original file")
            )
            .expect("UTF-8")
            .contains("print('hello')")
        );

        let save = notebook_editor.update_in(cx, |notebook_editor, window, cx| {
            notebook_editor.save(SaveOptions::default(), project.clone(), window, cx)
        });
        cell_editor.update_in(cx, |editor, window, cx| {
            editor.set_text("print('new edit')", window, cx);
        });
        save.await.expect("saving the notebook should succeed");
        assert!(
            notebook_editor.read_with(cx, |editor, cx| editor.is_dirty(cx)),
            "edits made during the save must remain dirty"
        );

        let saved = String::from_utf8(
            fs.read_file_sync(path!("/notebooks/test.ipynb"))
                .expect("notebook should still exist"),
        )
        .expect("notebook should be valid UTF-8");
        assert!(
            saved.contains("print('goodbye')"),
            "the edited cell should be written to the notebook, got: {saved}"
        );

        buffer.read_with(cx, |buffer, _| {
            assert_eq!(
                buffer.text(),
                saved,
                "the project's buffer should hold the saved notebook"
            );
            assert!(!buffer.is_dirty(), "saving should leave the buffer clean");
        });
        notebook_editor
            .update_in(cx, |editor, window, cx| {
                editor.save(SaveOptions::default(), project.clone(), window, cx)
            })
            .await
            .expect("save the later edit");
        assert!(!notebook_editor.read_with(cx, |editor, cx| editor.is_dirty(cx)));
        assert!(
            String::from_utf8(
                fs.read_file_sync(path!("/notebooks/test.ipynb"))
                    .expect("saved file")
            )
            .expect("UTF-8")
            .contains("print('new edit')")
        );

        fs.pause_events();
        cell_editor.update_in(cx, |editor, window, cx| {
            editor.set_text("print('keep my edit')", window, cx);
        });
        let path = std::path::Path::new(path!("/notebooks/test.ipynb"));
        fs.insert_file(path, NOTEBOOK_WITH_ONE_CODE_CELL.as_bytes().to_vec())
            .await;
        notebook_editor
            .update_in(cx, |editor, window, cx| {
                editor.save(SaveOptions::default(), project.clone(), window, cx)
            })
            .await
            .expect_err("missed external replacement must conflict");
        assert!(notebook_editor.read_with(cx, |editor, cx| editor.has_conflict(cx)));
        assert!(notebook_editor.read_with(cx, |editor, cx| editor.is_dirty(cx)));

        let overwrite = notebook_editor.update_in(cx, |editor, window, cx| {
            editor.save(
                SaveOptions {
                    overwrite: true,
                    ..Default::default()
                },
                project.clone(),
                window,
                cx,
            )
        });
        // Change the destination before the spawned save can consume the approval.
        fs.set_mtime(path, fs.get_and_increment_mtime())
            .expect("second replacement");
        overwrite
            .await
            .expect_err("approval must stay bound to the confirmed identity");
        assert_eq!(
            fs.read_file_sync(path).expect("external file"),
            NOTEBOOK_WITH_ONE_CODE_CELL.as_bytes()
        );
        notebook_editor
            .update_in(cx, |editor, window, cx| {
                editor.save(
                    SaveOptions {
                        overwrite: true,
                        ..Default::default()
                    },
                    project.clone(),
                    window,
                    cx,
                )
            })
            .await
            .expect("confirm current replacement");
        assert!(!notebook_editor.read_with(cx, |editor, cx| editor.has_conflict(cx)));
        assert!(!notebook_editor.read_with(cx, |editor, cx| editor.is_dirty(cx)));
        assert!(
            String::from_utf8(fs.read_file_sync(path).expect("saved notebook"))
                .expect("UTF-8")
                .contains("print('keep my edit')")
        );

        fs.insert_file(path, NOTEBOOK_WITH_ONE_CODE_CELL.as_bytes().to_vec())
            .await;
        notebook_editor
            .update_in(cx, |editor, window, cx| {
                editor.save(SaveOptions::default(), project.clone(), window, cx)
            })
            .await
            .expect_err("new external replacement");
        notebook_editor
            .update_in(cx, |editor, window, cx| {
                editor.reload(project.clone(), window, cx)
            })
            .await
            .expect("discard notebook edits");
        notebook_editor.read_with(cx, |editor, cx| {
            assert!(!editor.has_conflict(cx));
            assert!(!editor.is_dirty(cx));
            let notebook = serde_json::to_string(&editor.to_notebook(cx)).expect("notebook JSON");
            assert!(
                notebook.contains("print('hello')"),
                "discard must read the external file"
            );
        });
    }
}
