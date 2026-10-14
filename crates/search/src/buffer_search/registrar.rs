use gpui::{Action, App, Context, Div, Entity, InteractiveElement, Window, div};
use workspace::{Pane, Workspace};

use crate::BufferSearchBar;

/// Registrar inverts the dependency between search and its downstream user, allowing said downstream user to register search action without knowing exactly what those actions are.
pub trait SearchActionsRegistrar {
    fn register_handler<A: Action>(&mut self, callback: impl ActionExecutor<A>);
}

type SearchBarActionCallback<A> =
    fn(&mut BufferSearchBar, &A, &mut Window, &mut Context<BufferSearchBar>);

type GetSearchBar<T> =
    for<'a, 'b> fn(&'a T, &'a mut Window, &mut Context<'b, T>) -> Option<Entity<BufferSearchBar>>;

/// Registers search actions on a div that can be taken out.
pub struct DivRegistrar<'a, 'b, T: 'static> {
    div: Option<Div>,
    cx: &'a mut Context<'b, T>,
    search_getter: GetSearchBar<T>,
}

impl<'a, 'b, T: 'static> DivRegistrar<'a, 'b, T> {
    pub fn new(search_getter: GetSearchBar<T>, cx: &'a mut Context<'b, T>) -> Self {
        Self {
            div: Some(div()),
            cx,
            search_getter,
        }
    }
    pub fn into_div(self) -> Div {
        // This option is always Some; it's an option in the first place because we want to call methods
        // on div that require ownership.
        self.div.unwrap()
    }
}

impl<T: 'static> SearchActionsRegistrar for DivRegistrar<'_, '_, T> {
    fn register_handler<A: Action>(&mut self, callback: impl ActionExecutor<A>) {
        let getter = self.search_getter;
        self.div = self.div.take().map(|div| {
            div.on_action(self.cx.entity_listener(
                move |this: Entity<T>, action: &A, window, cx| {
                    let search_bar = this.update(cx, |this, cx| (getter)(this, window, cx));
                    let should_notify = search_bar
                        .map(|search_bar| {
                            search_bar.update(cx, |search_bar, cx| {
                                callback.execute(search_bar, action, window, cx)
                            })
                        })
                        .unwrap_or(false);
                    if should_notify {
                        this.update(cx, |_, cx| cx.notify());
                    } else {
                        cx.propagate();
                    }
                },
            ))
        });
    }
}

pub struct PaneDivRegistrar {
    div: Option<Div>,
    pane: Entity<Pane>,
}

impl PaneDivRegistrar {
    pub fn new(div: Div, pane: Entity<Pane>) -> Self {
        Self {
            div: Some(div),
            pane,
        }
    }

    pub fn into_div(self) -> Div {
        self.div.unwrap()
    }
}

impl SearchActionsRegistrar for PaneDivRegistrar {
    fn register_handler<A: Action>(&mut self, callback: impl ActionExecutor<A>) {
        let pane = self.pane.clone();
        self.div = self.div.take().map(|div| {
            div.on_action(move |action: &A, window: &mut Window, cx: &mut App| {
                let search_bar = pane
                    .read(cx)
                    .toolbar()
                    .read(cx)
                    .item_of_type::<BufferSearchBar>();
                let should_notify = search_bar
                    .map(|search_bar| {
                        search_bar.update(cx, |search_bar, cx| {
                            callback.execute(search_bar, action, window, cx)
                        })
                    })
                    .unwrap_or(false);
                if should_notify {
                    pane.update(cx, |_, cx| cx.notify());
                } else {
                    cx.propagate();
                }
            })
        });
    }
}

pub fn register_pane_search_actions(div: Div, pane: Entity<Pane>) -> Div {
    let mut registrar = PaneDivRegistrar::new(div, pane);
    BufferSearchBar::register(&mut registrar);
    registrar.into_div()
}

/// Register actions for an active pane.
impl SearchActionsRegistrar for Workspace {
    fn register_handler<A: Action>(&mut self, callback: impl ActionExecutor<A>) {
        self.register_action_renderer(move |div, _, _, cx| {
            let callback = callback.clone();
            div.on_action(cx.entity_listener(
                move |workspace: Entity<Workspace>, action: &A, window, cx| {
                    let modal_blocks_action = workspace.update(cx, |workspace, cx| {
                        workspace.has_active_modal(window, cx) && !workspace.hide_modal(window, cx)
                    });
                    if modal_blocks_action {
                        cx.propagate();
                        return;
                    }

                    let toolbar = workspace.read(cx).active_pane().read(cx).toolbar().clone();
                    let Some(search_bar) = toolbar.read(cx).item_of_type::<BufferSearchBar>()
                    else {
                        return;
                    };
                    let should_notify = search_bar.update(cx, |search_bar, cx| {
                        callback.execute(search_bar, action, window, cx)
                    });
                    if should_notify {
                        toolbar.update(cx, |_, cx| cx.notify());
                    } else {
                        cx.propagate();
                    }
                },
            ))
        });
    }
}

type DidHandleAction = bool;
/// Potentially executes the underlying action if some preconditions are met (e.g. buffer search bar is visible)
pub trait ActionExecutor<A: Action>: 'static + Clone {
    fn execute(
        &self,
        search_bar: &mut BufferSearchBar,
        action: &A,
        window: &mut Window,
        cx: &mut Context<BufferSearchBar>,
    ) -> DidHandleAction;
}

/// Run an action when the search bar has been dismissed from the panel.
pub struct ForDismissed<A>(pub(super) SearchBarActionCallback<A>);
impl<A> Clone for ForDismissed<A> {
    fn clone(&self) -> Self {
        Self(self.0)
    }
}

impl<A: Action> ActionExecutor<A> for ForDismissed<A> {
    fn execute(
        &self,
        search_bar: &mut BufferSearchBar,
        action: &A,
        window: &mut Window,
        cx: &mut Context<BufferSearchBar>,
    ) -> DidHandleAction {
        if search_bar.is_dismissed() {
            self.0(search_bar, action, window, cx);
            true
        } else {
            false
        }
    }
}

/// Run an action when the search bar is deployed.
pub struct ForDeployed<A>(pub(super) SearchBarActionCallback<A>);
impl<A> Clone for ForDeployed<A> {
    fn clone(&self) -> Self {
        Self(self.0)
    }
}

impl<A: Action> ActionExecutor<A> for ForDeployed<A> {
    fn execute(
        &self,
        search_bar: &mut BufferSearchBar,
        action: &A,
        window: &mut Window,
        cx: &mut Context<BufferSearchBar>,
    ) -> DidHandleAction {
        if search_bar.is_dismissed() || search_bar.active_searchable_item.is_none() {
            false
        } else {
            self.0(search_bar, action, window, cx);
            true
        }
    }
}

/// Run an action when the search bar has any matches or a pending external query,
/// regardless of whether it is visible or not.
pub struct WithResultsOrExternalQuery<A>(pub(super) SearchBarActionCallback<A>);
impl<A> Clone for WithResultsOrExternalQuery<A> {
    fn clone(&self) -> Self {
        Self(self.0)
    }
}

impl<A: Action> ActionExecutor<A> for WithResultsOrExternalQuery<A> {
    fn execute(
        &self,
        search_bar: &mut BufferSearchBar,
        action: &A,
        window: &mut Window,
        cx: &mut Context<BufferSearchBar>,
    ) -> DidHandleAction {
        #[cfg(not(target_os = "macos"))]
        let has_external_query = false;

        #[cfg(target_os = "macos")]
        let has_external_query = search_bar.pending_external_query.is_some();

        if has_external_query || search_bar.active_match_index.is_some() {
            self.0(search_bar, action, window, cx);
            true
        } else {
            false
        }
    }
}
