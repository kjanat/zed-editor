use editor::{Editor, MultiBuffer};
use gpui::{AnyElement, Entity, Modifiers, WeakEntity};
use markdown_preview::markdown_preview_view::MarkdownPreviewView;
use svg_preview::svg_preview_view::SvgPreviewView;
use tabular_data_preview::TabularDataPreviewPane;
use ui::{Tooltip, prelude::*, text_for_keystroke};
use workspace::{ItemHandle, Workspace};

use super::QuickActionBar;

enum PreviewTarget {
    Markdown(Entity<Editor>),
    Svg(Entity<MultiBuffer>),
    TabularData(Entity<Editor>),
}

impl QuickActionBar {
    pub fn render_preview_button(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        // Resolve against this toolbar's own pane item rather than the
        // workspace's focused item, so each pane's button reflects and
        // targets the content of the pane it belongs to.
        let active_item = self.active_item.as_ref()?;
        let editor = active_item.act_as::<Editor>(cx);

        let preview_target = if let Some(editor) = &editor
            && MarkdownPreviewView::is_markdown_file(editor, cx)
        {
            PreviewTarget::Markdown(editor.clone())
        } else if let Some(buffer) = active_item.act_as::<MultiBuffer>(cx)
            && SvgPreviewView::is_svg_file(&buffer, cx)
        {
            PreviewTarget::Svg(buffer)
        } else if let Some(editor) = editor
            && TabularDataPreviewPane::is_tabular_data_file(&editor, cx)
        {
            PreviewTarget::TabularData(editor)
        } else {
            return None;
        };

        let (button_id, tooltip_text, open_action_for_tooltip) = match &preview_target {
            PreviewTarget::Markdown(_) => (
                "toggle-markdown-preview",
                "Preview Markdown",
                &markdown_preview::OpenPreview as &dyn gpui::Action,
            ),
            PreviewTarget::Svg(_) => (
                "toggle-svg-preview",
                "Preview SVG",
                &svg_preview::OpenPreview as &dyn gpui::Action,
            ),
            PreviewTarget::TabularData(_) => (
                "toggle-tabular-preview",
                "Preview Tabular Data",
                &tabular_data_preview::OpenPreview as &dyn gpui::Action,
            ),
        };

        let alt_click = gpui::Keystroke {
            key: "click".into(),
            modifiers: Modifiers::alt(),
            ..Default::default()
        };

        let button = IconButton::new(button_id, IconName::Eye)
            .icon_size(IconSize::Small)
            .style(ButtonStyle::Subtle)
            .tooltip(move |_window, cx| {
                Tooltip::with_meta(
                    tooltip_text,
                    Some(open_action_for_tooltip),
                    format!(
                        "{} to open in a split",
                        text_for_keystroke(&alt_click.modifiers, &alt_click.key, cx)
                    ),
                    cx,
                )
            })
            .on_click({
                let workspace_handle = self.workspace.clone();
                let active_item = active_item.boxed_clone();
                move |_, window, cx| {
                    let open_to_the_side = window.modifiers().alt;
                    open_preview(
                        &workspace_handle,
                        active_item.as_ref(),
                        &preview_target,
                        open_to_the_side,
                        window,
                        cx,
                    );
                }
            });

        Some(button.into_any_element())
    }
}

fn open_preview(
    workspace_handle: &WeakEntity<Workspace>,
    active_item: &dyn ItemHandle,
    preview_target: &PreviewTarget,
    open_to_the_side: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(workspace) = workspace_handle.upgrade() else {
        return;
    };
    let Some(pane) = workspace.read(cx).pane_for(active_item) else {
        return;
    };
    match preview_target {
        PreviewTarget::Markdown(editor) => {
            let editor = editor.clone();
            if open_to_the_side {
                MarkdownPreviewView::open_preview_to_the_side_of_pane(
                    &workspace, editor, pane, window, cx,
                );
            } else {
                MarkdownPreviewView::open_preview_in_pane(&workspace, editor, pane, window, cx);
            }
        }
        PreviewTarget::Svg(buffer) => {
            let buffer = buffer.clone();
            if open_to_the_side {
                SvgPreviewView::open_preview_to_the_side_of_pane(
                    &workspace, buffer, pane, window, cx,
                );
            } else {
                SvgPreviewView::open_preview_in_pane(&workspace, buffer, pane, window, cx);
            }
        }
        PreviewTarget::TabularData(editor) => {
            let editor = editor.clone();
            if open_to_the_side {
                TabularDataPreviewPane::open_preview_to_the_side_of_pane(
                    &workspace, editor, pane, window, cx,
                );
            } else {
                TabularDataPreviewPane::open_preview_in_pane(editor, pane, window, cx);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zed::tests::{
        add_item_hooks, add_item_reading_containers, assert_item_added_next_to_reader, init_test,
        window_with_item_reading_containers,
    };
    use gpui::{TestAppContext, VisualTestContext};
    use project::Project;
    use serde_json::json;
    use util::{path, rel_path::rel_path};
    use workspace::{Pane, SplitDirection, item::test::HookBits};

    async fn open_file_in_pane(
        workspace: &Entity<Workspace>,
        pane: &Entity<Pane>,
        file: &str,
        cx: &mut VisualTestContext,
    ) -> Box<dyn ItemHandle> {
        let worktree_id = workspace.read_with(cx, |workspace, cx| {
            workspace
                .project()
                .read(cx)
                .worktrees(cx)
                .next()
                .unwrap()
                .read(cx)
                .id()
        });
        let item = workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.open_path(
                    (worktree_id, rel_path(file)),
                    Some(pane.downgrade()),
                    true,
                    window,
                    cx,
                )
            })
            .await
            .unwrap();
        cx.run_until_parked();
        item
    }

    #[gpui::test]
    async fn test_open_preview_button_next_to_items_that_read_their_containers(
        cx: &mut TestAppContext,
    ) {
        let app_state = init_test(cx);
        app_state
            .fs
            .as_fake()
            .insert_tree(path!("/root"), json!({ "note.md": "# Note\n" }))
            .await;
        let project = Project::test(app_state.fs.clone(), [path!("/root").as_ref()], cx).await;
        let (workspace, pane, reader, mut cx) =
            window_with_item_reading_containers(project, add_item_hooks(), cx);
        let item = open_file_in_pane(&workspace, &pane, "note.md", &mut cx).await;
        let editor = item.downcast::<Editor>().unwrap();
        reader.read_with(&cx, |reader, _| reader.container_reads.set(0));

        cx.update(|window, cx| {
            open_preview(
                &workspace.downgrade(),
                item.as_ref(),
                &PreviewTarget::Markdown(editor),
                false,
                window,
                cx,
            )
        });
        cx.run_until_parked();

        assert_item_added_next_to_reader::<MarkdownPreviewView>(&reader, &pane, 3, &mut cx);
    }

    #[gpui::test]
    async fn test_open_preview_button_to_the_side_next_to_items_that_read_their_containers(
        cx: &mut TestAppContext,
    ) {
        let app_state = init_test(cx);
        app_state
            .fs
            .as_fake()
            .insert_tree(path!("/root"), json!({ "image.svg": "<svg></svg>" }))
            .await;
        let project = Project::test(app_state.fs.clone(), [path!("/root").as_ref()], cx).await;
        let (workspace, source_pane, _, mut cx) =
            window_with_item_reading_containers(project, HookBits::default(), cx);
        let target_pane = workspace.update_in(&mut cx, |workspace, window, cx| {
            workspace.split_pane(source_pane.clone(), SplitDirection::Right, window, cx)
        });
        let reader =
            add_item_reading_containers(&workspace, &target_pane, add_item_hooks(), &mut cx);
        let item = open_file_in_pane(&workspace, &source_pane, "image.svg", &mut cx).await;
        let editor = item.downcast::<Editor>().unwrap();
        let buffer = editor.read_with(&cx, |editor, _| editor.buffer().clone());
        reader.read_with(&cx, |reader, _| reader.container_reads.set(0));

        cx.update(|window, cx| {
            open_preview(
                &workspace.downgrade(),
                item.as_ref(),
                &PreviewTarget::Svg(buffer),
                true,
                window,
                cx,
            )
        });
        cx.run_until_parked();

        assert_item_added_next_to_reader::<SvgPreviewView>(&reader, &target_pane, 2, &mut cx);
        source_pane.read_with(&cx, |pane, _| {
            assert_eq!(
                pane.active_item().map(|item| item.item_id()),
                Some(editor.entity_id()),
                "the source pane keeps the svg editor active"
            );
        });
    }
}
