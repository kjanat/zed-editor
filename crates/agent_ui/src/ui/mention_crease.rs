use std::{path::PathBuf, time::Duration};

use acp_thread::MentionUri;
use agent_client_protocol::schema::v2 as acp;
use editor::Editor;
use gpui::{
    Animation, AnimationExt, AnyView, Context, Entity, IntoElement, TaskExt, WeakEntity, Window,
    pulsating_between,
};
use language::Buffer;
use rope::Point;
use settings::Settings;
use theme_settings::ThemeSettings;
use ui::{ButtonLike, TintColor, Tooltip, prelude::*};
use workspace::{OpenOptions, Workspace};

use crate::open_abs_path_at_point;

#[derive(IntoElement)]
pub struct MentionCrease {
    id: ElementId,
    icon: SharedString,
    label: SharedString,
    mention_uri: Option<MentionUri>,
    workspace: Option<WeakEntity<Workspace>>,
    is_toggled: bool,
    is_loading: bool,
    tooltip: Option<SharedString>,
    image_preview: Option<Box<dyn Fn(&mut Window, &mut App) -> AnyView + 'static>>,
}

impl MentionCrease {
    pub fn new(
        id: impl Into<ElementId>,
        icon: impl Into<SharedString>,
        label: impl Into<SharedString>,
    ) -> Self {
        Self {
            id: id.into(),
            icon: icon.into(),
            label: label.into(),
            mention_uri: None,
            workspace: None,
            is_toggled: false,
            is_loading: false,
            tooltip: None,
            image_preview: None,
        }
    }

    pub fn mention_uri(mut self, mention_uri: Option<MentionUri>) -> Self {
        self.mention_uri = mention_uri;
        self
    }

    pub fn workspace(mut self, workspace: Option<WeakEntity<Workspace>>) -> Self {
        self.workspace = workspace;
        self
    }

    pub fn is_toggled(mut self, is_toggled: bool) -> Self {
        self.is_toggled = is_toggled;
        self
    }

    pub fn is_loading(mut self, is_loading: bool) -> Self {
        self.is_loading = is_loading;
        self
    }

    pub fn tooltip(mut self, tooltip: impl Into<SharedString>) -> Self {
        self.tooltip = Some(tooltip.into());
        self
    }

    pub fn image_preview(
        mut self,
        builder: impl Fn(&mut Window, &mut App) -> AnyView + 'static,
    ) -> Self {
        self.image_preview = Some(Box::new(builder));
        self
    }
}

impl RenderOnce for MentionCrease {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let settings = ThemeSettings::get_global(cx);
        let font_size = settings.agent_buffer_font_size(cx);
        let buffer_font = settings.buffer_font.clone();
        let is_loading = self.is_loading;
        let tooltip = self.tooltip;
        let image_preview = self.image_preview;

        let button_height = DefiniteLength::Absolute(AbsoluteLength::Pixels(
            px(window.line_height().into()) - px(1.),
        ));

        ButtonLike::new(self.id)
            .style(ButtonStyle::Outlined)
            .size(ButtonSize::Compact)
            .height(button_height)
            .selected_style(ButtonStyle::Tinted(TintColor::Accent))
            .toggle_state(self.is_toggled)
            .when_some(
                self.mention_uri.clone().zip(self.workspace.clone()),
                |this, (mention_uri, workspace)| {
                    this.on_click(move |_event, window, cx| {
                        open_mention_uri(mention_uri.clone(), &workspace, window, cx);
                    })
                },
            )
            .child(
                h_flex()
                    .pb_px()
                    .gap_1()
                    .font(buffer_font)
                    .text_size(font_size)
                    .child(
                        Icon::from_path(self.icon.clone())
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(self.label.clone())
                    .map(|this| {
                        if is_loading {
                            this.with_animation(
                                "loading-context-crease",
                                Animation::new(Duration::from_secs(2))
                                    .repeat()
                                    .with_easing(pulsating_between(0.4, 0.8)),
                                |label, delta| label.opacity(delta),
                            )
                            .into_any()
                        } else {
                            this.into_any()
                        }
                    }),
            )
            .map(|button| {
                if let Some(image_preview) = image_preview {
                    button.hoverable_tooltip(image_preview)
                } else {
                    button.when_some(tooltip, |this, tooltip_text| {
                        this.tooltip(Tooltip::text(tooltip_text))
                    })
                }
            })
    }
}

fn open_mention_uri(
    mention_uri: MentionUri,
    workspace: &WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(workspace) = workspace.upgrade() else {
        return;
    };

    match &mention_uri {
        MentionUri::Skill {
            skill_file_path, ..
        } => {
            open_skill_file(&workspace, skill_file_path.clone(), window, cx);
            return;
        }
        MentionUri::Rule { name, .. } => {
            open_migrated_rule(&workspace, name, window, cx);
            return;
        }
        _ => {}
    }

    workspace.update(cx, |workspace, cx| match mention_uri {
        MentionUri::File { abs_path } => {
            open_abs_path_at_point(workspace, abs_path, None, window, cx);
        }
        MentionUri::Symbol {
            abs_path,
            line_range,
            ..
        } => {
            open_abs_path_at_point(
                workspace,
                abs_path,
                Some(Point::new(*line_range.start(), 0)),
                window,
                cx,
            );
        }
        MentionUri::Selection {
            abs_path: Some(abs_path),
            line_range,
            column,
        } => {
            open_abs_path_at_point(
                workspace,
                abs_path,
                Some(Point::new(*line_range.start(), column.unwrap_or(0))),
                window,
                cx,
            );
        }
        MentionUri::Directory { abs_path } => {
            reveal_in_project_panel(workspace, abs_path, cx);
        }
        MentionUri::Thread { id, name } => {
            open_thread(workspace, id, name, window, cx);
        }
        MentionUri::Fetch { url } => {
            cx.open_url(url.as_str());
        }
        MentionUri::Skill { .. }
        | MentionUri::Rule { .. }
        | MentionUri::PastedImage { .. }
        | MentionUri::Selection { abs_path: None, .. }
        | MentionUri::Diagnostics { .. }
        | MentionUri::TerminalSelection { .. }
        | MentionUri::GitDiff { .. }
        | MentionUri::MergeConflict { .. } => {}
    });
}

/// Notify the user that rules became skills and open the skill the rule was
/// migrated into. Migrated skills live in the local global skills dir, so the
/// file is always resolved against the local filesystem (local, SSH, or
/// collab). Does nothing else when no matching skill exists.
pub(crate) fn open_migrated_rule(
    workspace: &Entity<Workspace>,
    name: &str,
    window: &mut Window,
    cx: &mut App,
) {
    struct RulesMigratedToSkillsToast;
    workspace.update(cx, |workspace, cx| {
        workspace.show_toast(
            workspace::Toast::new(
                workspace::notifications::NotificationId::unique::<RulesMigratedToSkillsToast>(),
                "Rules have been migrated to Skills.",
            )
            .on_click("View docs", |_, cx| {
                cx.open_url("https://zed.dev/docs/ai/skills");
            })
            .autohide(),
            cx,
        );
    });

    let Some(slug) = agent_skills::slugify_skill_name(name) else {
        return;
    };
    let skill_file_path = agent_skills::global_skills_dir()
        .join(slug)
        .join(agent_skills::SKILL_FILE_NAME);

    let is_local = workspace.read(cx).project().read(cx).is_local();
    let fs = workspace.read(cx).app_state().fs.clone();
    let workspace = workspace.downgrade();
    window
        .spawn(cx, async move |cx| {
            if is_local {
                // Local project: open the editable on-disk file if it exists.
                if fs.is_file(&skill_file_path).await {
                    let workspace = workspace
                        .upgrade()
                        .ok_or_else(|| anyhow::anyhow!("workspace was released"))?;
                    cx.update(|window, cx| {
                        open_skill_file(&workspace, skill_file_path, window, cx);
                    })?;
                }
                return Ok(());
            }

            // Remote/collab: `open_abs_path` targets the remote project, where this
            // local file doesn't exist, so read it locally and show it read-only.
            let Ok(content) = fs.load(&skill_file_path).await else {
                return Ok(()); // No readable migrated skill: do nothing.
            };
            let title = skill_content_buffer_title(&skill_file_path);
            let workspace = workspace
                .upgrade()
                .ok_or_else(|| anyhow::anyhow!("workspace was released"))?;
            cx.update(|window, cx| {
                open_skill_content_buffer(&workspace, title, content, window, cx);
            })
        })
        .detach_and_log_err(cx);
}

fn open_skill_file(
    workspace: &Entity<Workspace>,
    skill_file_path: PathBuf,
    window: &mut Window,
    cx: &mut App,
) {
    // Built-in skills have synthetic paths with no on-disk file, so show their
    // embedded content in a local buffer instead.
    if let Some(content) = agent_skills::builtin_skill_content(&skill_file_path) {
        let title = skill_content_buffer_title(&skill_file_path);
        open_skill_content_buffer(workspace, title, content, window, cx);
        return;
    }

    workspace.update(cx, |workspace, cx| {
        workspace
            .open_abs_path(
                skill_file_path,
                OpenOptions {
                    focus: Some(true),
                    ..Default::default()
                },
                window,
                cx,
            )
            .detach_and_log_err(cx);
    });
}

fn skill_content_buffer_title(skill_file_path: &std::path::Path) -> String {
    skill_file_path
        .parent()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "skill".into())
}

/// Open `content` as a local, read-only Markdown buffer, for skills with no
/// openable file in the active project (built-in skills, and migrated rules on
/// remote/collab projects). It's deliberately not registered with the project's
/// buffer store: that keeps it out of search and avoids
/// `Project::create_local_buffer` panicking on remote projects.
fn open_skill_content_buffer(
    workspace: &Entity<Workspace>,
    title: String,
    content: impl Into<String>,
    window: &mut Window,
    cx: &mut App,
) {
    let languages = workspace.read(cx).project().read(cx).languages().clone();
    let buffer = cx.new(|cx| Buffer::local(content, cx));
    // Set markdown highlighting asynchronously — the buffer
    // opens instantly and the highlighting appears once loaded.
    cx.spawn({
        let buffer = buffer.clone();
        async move |cx| {
            if let Ok(markdown) = languages.language_for_name("Markdown").await {
                buffer.update(cx, |buffer, cx| buffer.set_language(Some(markdown), cx));
            }
        }
    })
    .detach();
    let editor = cx.new(|cx| {
        let mut editor = Editor::for_buffer(buffer, None, window, cx);
        editor.set_read_only(true);
        editor
            .buffer()
            .update(cx, |buffer, cx| buffer.set_title(title, cx));
        editor
    });
    let pane = workspace.read(cx).active_pane().clone();
    Workspace::add_item(pane, Box::new(editor), None, true, true, window, cx);
}

fn reveal_in_project_panel(
    workspace: &mut Workspace,
    abs_path: PathBuf,
    cx: &mut Context<Workspace>,
) {
    let project = workspace.project();
    let Some(entry_id) = project.update(cx, |project, cx| {
        let path = project.find_project_path(&abs_path, cx)?;
        project.entry_for_path(&path, cx).map(|entry| entry.id)
    }) else {
        return;
    };

    project.update(cx, |_, cx| {
        cx.emit(project::Event::RevealInProjectPanel(entry_id));
    });
}

fn open_thread(
    workspace: &mut Workspace,
    id: acp::SessionId,
    name: String,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    use crate::{Agent, AgentPanel, AgentThreadSource, thread_metadata_store::ThreadMetadataStore};

    let Some(panel) = workspace.panel::<AgentPanel>(cx) else {
        return;
    };

    // Right now we only support loading threads in the native agent.
    panel.update(cx, |panel, cx| {
        let thread_id = ThreadMetadataStore::try_global(cx)
            .and_then(|store| store.read(cx).entry_by_session(&id).map(|m| m.thread_id));
        if let Some(thread_id) = thread_id {
            panel.load_agent_thread(
                Agent::NativeAgent,
                thread_id,
                None,
                Some(name.into()),
                true,
                AgentThreadSource::AgentPanel,
                window,
                cx,
            );
        } else {
            panel.open_thread(id, None, Some(name.into()), window, cx);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation_view::tests::init_test;
    use gpui::{TestAppContext, VisualTestContext};
    use project::{FakeFs, Project};
    use serde_json::json;
    use std::sync::Arc;
    use util::path;
    use workspace::{
        MultiWorkspace, Pane,
        item::test::{HookBits, TestItem},
    };

    async fn workspace_with_item_reading_containers(
        cx: &mut TestAppContext,
    ) -> (
        Arc<FakeFs>,
        Entity<Workspace>,
        Entity<TestItem>,
        &mut VisualTestContext,
    ) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/project"), json!({ "file.txt": "" }))
            .await;
        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace =
            multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
        let pane_handle = pane.downgrade();
        let workspace_handle = workspace.downgrade();
        let item = cx.new(|cx| {
            TestItem::new(cx)
                .with_pane_reader(pane_handle)
                .with_workspace_reader(workspace_handle)
        });
        cx.update(|window, cx| {
            Pane::add_item(&pane, Box::new(item.clone()), true, true, None, window, cx);
        });
        cx.run_until_parked();
        item.read_with(cx, |item, _| item.read_containers_in(HookBits::BUFFER_KIND));
        (fs, workspace, item, cx)
    }

    #[gpui::test]
    async fn test_open_builtin_skill_mention_items_that_read_their_containers(
        cx: &mut TestAppContext,
    ) {
        let (_fs, workspace, item, cx) = workspace_with_item_reading_containers(cx).await;
        let skill_file_path =
            PathBuf::from("<built-in>/create-skill").join(agent_skills::SKILL_FILE_NAME);

        cx.update(|window, cx| {
            open_mention_uri(
                MentionUri::Skill {
                    name: "create-skill".to_string(),
                    source: "built-in".to_string(),
                    skill_file_path,
                },
                &workspace.downgrade(),
                window,
                cx,
            );
        });
        cx.run_until_parked();

        assert!(item.read_with(cx, |item, _| item.container_reads.get() > 0));
        workspace.read_with(cx, |workspace, cx| {
            let editor = workspace
                .active_item(cx)
                .and_then(|active| active.downcast::<Editor>())
                .expect("the built-in skill should open in an editor");
            assert_eq!(
                editor.read(cx).buffer().read(cx).title(cx).as_ref(),
                "create-skill"
            );
        });
    }

    #[gpui::test]
    async fn test_open_migrated_rule_mention_items_that_read_their_containers(
        cx: &mut TestAppContext,
    ) {
        let (fs, workspace, item, cx) = workspace_with_item_reading_containers(cx).await;
        let skill_file_path = agent_skills::global_skills_dir()
            .join("my-rule")
            .join(agent_skills::SKILL_FILE_NAME);
        fs.insert_file(&skill_file_path, b"rule body".to_vec())
            .await;

        cx.update(|window, cx| {
            open_mention_uri(
                MentionUri::Rule {
                    id: serde_json::Value::Null,
                    name: "my-rule".to_string(),
                },
                &workspace.downgrade(),
                window,
                cx,
            );
        });
        cx.run_until_parked();

        assert!(item.read_with(cx, |item, _| item.container_reads.get() > 0));
        workspace.read_with(cx, |workspace, cx| {
            let editor = workspace
                .active_item(cx)
                .and_then(|active| active.downcast::<Editor>())
                .expect("the migrated skill should open in an editor");
            let buffer = editor
                .read(cx)
                .buffer()
                .read(cx)
                .as_singleton()
                .expect("the skill file should open as a single buffer");
            assert_eq!(buffer.read(cx).text(), "rule body");
        });
    }
}
