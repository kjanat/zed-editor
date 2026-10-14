mod highlights_tree_view;
mod key_context_view;
pub mod lsp_button;
pub mod lsp_log_view;
mod syntax_tree_view;

#[cfg(test)]
mod lsp_log_view_tests;

use gpui::{App, AppContext, Entity};

pub use highlights_tree_view::{HighlightsTreeToolbarItemView, HighlightsTreeView};
pub use lsp_log_view::LspLogView;
pub use syntax_tree_view::{SyntaxTreeToolbarItemView, SyntaxTreeView};
use ui::{Context, Window};
use workspace::{Item, ItemHandle, SplitDirection, Workspace};

pub fn init(cx: &mut App) {
    highlights_tree_view::init(cx);
    lsp_log_view::init(false, cx);
    syntax_tree_view::init(cx);
    key_context_view::init(cx);
}

fn get_or_create_tool<T>(
    workspace: &Entity<Workspace>,
    destination: SplitDirection,
    window: &mut Window,
    cx: &mut App,
    new_tool: impl FnOnce(&mut Window, &mut Context<T>) -> T,
) -> Entity<T>
where
    T: Item,
{
    let existing_tool = workspace.read(cx).item_of_type::<T>(cx);
    if let Some(item) = existing_tool {
        return item;
    }

    let new_tool = cx.new(|cx| new_tool(window, cx));
    let destination_pane = Workspace::find_pane_in_direction(workspace, destination, cx);
    match destination_pane {
        Some(right_pane) => {
            Workspace::add_item(
                right_pane,
                new_tool.boxed_clone(),
                None,
                true,
                true,
                window,
                cx,
            );
        }
        None => {
            Workspace::split_item(workspace, destination, new_tool.boxed_clone(), window, cx);
        }
    }
    new_tool
}
