use gpui::{
    App, Context, Empty, EventEmitter, IntoElement, ParentElement, Render, SharedString, Window,
};
use settings::Settings;
use ui::{Button, Tooltip, prelude::*};
use util::paths::PathStyle;

use crate::{
    HideStatusItem, StatusItemView, item::ItemHandle, workspace_settings::StatusBarSettings,
};

pub struct ActiveFileName {
    project_path: Option<SharedString>,
    full_path: Option<SharedString>,
}

impl ActiveFileName {
    pub fn new() -> Self {
        Self {
            project_path: None,
            full_path: None,
        }
    }
}

impl Render for ActiveFileName {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !StatusBarSettings::get_global(cx).show_active_file {
            return Empty.into_any_element();
        }

        let Some(project_path) = self.project_path.clone() else {
            return Empty.into_any_element();
        };

        let tooltip_text = self
            .full_path
            .clone()
            .unwrap_or_else(|| project_path.clone());

        div()
            .child(
                Button::new("active-file-name-button", project_path)
                    .label_size(LabelSize::Small)
                    .tooltip(Tooltip::text(tooltip_text)),
            )
            .into_any_element()
    }
}

impl EventEmitter<crate::ToolbarItemEvent> for ActiveFileName {}

impl StatusItemView for ActiveFileName {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(item) = active_pane_item {
            self.project_path = item
                .project_path(cx)
                .map(|path| path.path.display(PathStyle::local()).into_owned().into());
            self.full_path = item.tab_tooltip_text(cx);
        } else {
            self.project_path = None;
            self.full_path = None;
        }
        cx.notify();
    }

    fn hide_setting(&self, _: &App) -> Option<HideStatusItem> {
        Some(HideStatusItem::new(|settings| {
            settings.status_bar.get_or_insert_default().show_active_file = Some(false);
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Pane, SplitDirection, Workspace,
        item::test::{HookBits, TestItem, TestProjectItem},
        tests::init_test,
    };
    use fs::FakeFs;
    use gpui::{AppContext as _, Entity, TestAppContext, VisualTestContext};
    use project::Project;

    fn add_item_reading_containers(
        workspace: &Entity<Workspace>,
        pane: &Entity<Pane>,
        id: u64,
        path: &str,
        cx: &mut VisualTestContext,
    ) -> Entity<TestItem> {
        let workspace_handle = workspace.downgrade();
        let pane_handle = pane.downgrade();
        let item = cx.new(|cx| {
            TestItem::new(cx)
                .with_project_items(&[TestProjectItem::new(id, path, cx)])
                .with_pane_reader(pane_handle)
                .with_workspace_reader(workspace_handle)
        });
        cx.update(|window, cx| {
            Pane::add_item(pane, Box::new(item.clone()), true, true, None, window, cx);
        });
        cx.run_until_parked();
        item.read_with(cx, |item, _| {
            item.read_containers_in(HookBits::ACTIVE_PROJECT_PATH | HookBits::TAB_TOOLTIP_TEXT)
        });
        item
    }

    fn shown_path(
        active_file_name: &Entity<ActiveFileName>,
        cx: &mut VisualTestContext,
    ) -> Option<SharedString> {
        active_file_name.read_with(cx, |active_file_name, _| {
            active_file_name.project_path.clone()
        })
    }

    fn container_reads(item: &Entity<TestItem>, cx: &mut VisualTestContext) -> usize {
        item.read_with(cx, |item, _| item.container_reads.get())
    }

    #[gpui::test]
    async fn test_active_file_name_shows_items_that_read_their_containers(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project, window, cx));
        let left_pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
        let left_item = add_item_reading_containers(&workspace, &left_pane, 1, "left.txt", cx);

        let active_file_name = cx.new(|_| ActiveFileName::new());
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.status_bar().update(cx, |status_bar, cx| {
                status_bar.add_left_item(active_file_name.clone(), window, cx);
            });
        });
        cx.run_until_parked();
        assert_eq!(shown_path(&active_file_name, cx), Some("left.txt".into()));
        assert!(
            container_reads(&left_item, cx) > 0,
            "adding the status item should show the active item's path"
        );

        let right_pane = workspace.update_in(cx, |workspace, window, cx| {
            workspace.split_pane(left_pane.clone(), SplitDirection::Right, window, cx)
        });
        let right_item = add_item_reading_containers(&workspace, &right_pane, 2, "right.txt", cx);
        left_item.update_in(cx, |_, window, cx| cx.focus_self(window));
        cx.run_until_parked();
        assert_eq!(shown_path(&active_file_name, cx), Some("left.txt".into()));

        right_item.update_in(cx, |_, window, cx| cx.focus_self(window));
        cx.run_until_parked();
        assert_eq!(shown_path(&active_file_name, cx), Some("right.txt".into()));
        assert!(
            container_reads(&right_item, cx) > 0,
            "focusing a pane should show its active item's path"
        );
    }
}
