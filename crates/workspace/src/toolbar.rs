use crate::ItemHandle;
use gpui::{
    AnyView, App, Context, Div, Entity, EntityId, EventEmitter, Global, KeyContext,
    ParentElement as _, Render, Styled, Window,
};
use language::LanguageRegistry;
use std::sync::Arc;
use ui::prelude::*;
use ui::{h_flex, v_flex};

pub struct PaneSearchBarCallbacks {
    pub setup_search_bar:
        fn(Option<Arc<LanguageRegistry>>, &Entity<Toolbar>, &mut Window, &mut App),
    pub wrap_div_with_search_actions: fn(Div, Entity<crate::Pane>) -> Div,
}

impl Global for PaneSearchBarCallbacks {}

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum ToolbarItemEvent {
    ChangeLocation(ToolbarItemLocation),
}

pub trait ToolbarItemView: Render + EventEmitter<ToolbarItemEvent> {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn crate::ItemHandle>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> ToolbarItemLocation;

    fn pane_focus_update(
        &mut self,
        _pane_focused: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn contribute_context(&self, _context: &mut KeyContext, _cx: &App) {}
}

trait ToolbarItemViewHandle: Send {
    fn id(&self) -> EntityId;
    fn boxed_clone(&self) -> Box<dyn ToolbarItemViewHandle>;
    fn to_any(&self) -> AnyView;
    fn set_active_pane_item(
        &self,
        active_pane_item: Option<&dyn ItemHandle>,
        window: &mut Window,
        cx: &mut App,
    ) -> ToolbarItemLocation;
    fn focus_changed(&self, pane_focused: bool, window: &mut Window, cx: &mut App);
    fn contribute_context(&self, context: &mut KeyContext, cx: &App);
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum ToolbarItemLocation {
    Hidden,
    PrimaryLeft,
    PrimaryRight,
    Secondary,
}

pub struct Toolbar {
    active_item: Option<Box<dyn ItemHandle>>,
    hidden: bool,
    can_navigate: bool,
    items: Vec<(Box<dyn ToolbarItemViewHandle>, ToolbarItemLocation)>,
}

impl Toolbar {
    fn has_any_visible_items(&self) -> bool {
        self.items
            .iter()
            .any(|(_item, location)| *location != ToolbarItemLocation::Hidden)
    }

    fn left_items(&self) -> impl Iterator<Item = &dyn ToolbarItemViewHandle> {
        self.items.iter().filter_map(|(item, location)| {
            if *location == ToolbarItemLocation::PrimaryLeft {
                Some(item.as_ref())
            } else {
                None
            }
        })
    }

    fn right_items(&self) -> impl Iterator<Item = &dyn ToolbarItemViewHandle> {
        self.items.iter().filter_map(|(item, location)| {
            if *location == ToolbarItemLocation::PrimaryRight {
                Some(item.as_ref())
            } else {
                None
            }
        })
    }

    fn secondary_items(&self) -> impl Iterator<Item = &dyn ToolbarItemViewHandle> {
        self.items.iter().rev().filter_map(|(item, location)| {
            if *location == ToolbarItemLocation::Secondary {
                Some(item.as_ref())
            } else {
                None
            }
        })
    }
}

impl Render for Toolbar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.has_any_visible_items() {
            return div();
        }

        let secondary_items = self.secondary_items().map(|item| item.to_any());

        let has_left_items = self.left_items().count() > 0;
        let has_right_items = self.right_items().count() > 0;

        v_flex()
            .group("toolbar")
            .relative()
            .py(DynamicSpacing::Base06.rems(cx))
            .px(DynamicSpacing::Base08.rems(cx))
            .when(has_left_items || has_right_items, |this| {
                this.gap(DynamicSpacing::Base06.rems(cx))
            })
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .bg(cx.theme().colors().toolbar_background)
            .when(has_left_items || has_right_items, |this| {
                this.child(
                    h_flex()
                        .items_start()
                        .justify_between()
                        .gap(DynamicSpacing::Base08.rems(cx))
                        .when(has_left_items, |this| {
                            this.child(
                                h_flex()
                                    .min_h_8()
                                    .flex_auto()
                                    .justify_start()
                                    .overflow_x_hidden()
                                    .children(self.left_items().map(|item| item.to_any())),
                            )
                        })
                        .when(has_right_items, |this| {
                            this.child(
                                h_flex()
                                    .h_8()
                                    .flex_row_reverse()
                                    .when(has_left_items, |this| this.flex_none())
                                    .justify_end()
                                    .children(self.right_items().map(|item| item.to_any())),
                            )
                        }),
                )
            })
            .children(secondary_items)
    }
}

impl Default for Toolbar {
    fn default() -> Self {
        Self::new()
    }
}

impl Toolbar {
    pub fn new() -> Self {
        Self {
            active_item: None,
            items: Default::default(),
            hidden: false,
            can_navigate: true,
        }
    }

    pub fn set_can_navigate(&mut self, can_navigate: bool, cx: &mut Context<Self>) {
        self.can_navigate = can_navigate;
        cx.notify();
    }

    pub fn add_item<T>(&mut self, item: Entity<T>, window: &mut Window, cx: &mut Context<Self>)
    where
        T: 'static + ToolbarItemView,
    {
        cx.subscribe(&item, |this, item, event, cx| {
            if let Some((_, current_location)) = this
                .items
                .iter_mut()
                .find(|(i, _)| i.id() == item.entity_id())
            {
                match event {
                    ToolbarItemEvent::ChangeLocation(new_location) => {
                        if new_location != current_location {
                            *current_location = *new_location;
                            cx.notify();
                        }
                    }
                }
            }
        })
        .detach();
        self.items
            .push((Box::new(item.clone()), ToolbarItemLocation::Hidden));
        let toolbar = cx.entity();
        window.defer(cx, move |window, cx| {
            let active_item = toolbar
                .read(cx)
                .active_item
                .as_ref()
                .map(|active_item| active_item.boxed_clone());
            let location = item.set_active_pane_item(active_item.as_deref(), window, cx);
            toolbar.update(cx, |toolbar, cx| {
                toolbar.apply_locations([(item.entity_id(), location)], cx);
            });
        });
        cx.notify();
    }

    pub fn set_active_item_for(
        toolbar: &Entity<Self>,
        item: Option<&dyn ItemHandle>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let hidden = item.is_some_and(|item| !item.show_toolbar(cx));
        let toolbar_items = toolbar.read(cx).item_handles();
        let locations = toolbar_items
            .iter()
            .map(|toolbar_item| {
                (
                    toolbar_item.id(),
                    toolbar_item.set_active_pane_item(item, window, cx),
                )
            })
            .collect::<Vec<_>>();
        toolbar.update(cx, |toolbar, cx| {
            toolbar.active_item = item.map(|item| item.boxed_clone());
            toolbar.hidden = hidden;
            toolbar.apply_locations(locations, cx);
        });
    }

    pub fn focus_changed(toolbar: &Entity<Self>, focused: bool, window: &mut Window, cx: &mut App) {
        let toolbar_items = toolbar.read(cx).item_handles();
        for toolbar_item in toolbar_items {
            toolbar_item.focus_changed(focused, window, cx);
        }
    }

    fn item_handles(&self) -> Vec<Box<dyn ToolbarItemViewHandle>> {
        self.items
            .iter()
            .map(|(toolbar_item, _)| toolbar_item.boxed_clone())
            .collect()
    }

    fn apply_locations(
        &mut self,
        locations: impl IntoIterator<Item = (EntityId, ToolbarItemLocation)>,
        cx: &mut Context<Self>,
    ) {
        for (item_id, new_location) in locations {
            if let Some((_, current_location)) = self
                .items
                .iter_mut()
                .find(|(toolbar_item, _)| toolbar_item.id() == item_id)
                && *current_location != new_location
            {
                *current_location = new_location;
                cx.notify();
            }
        }
    }

    pub fn item_of_type<T: ToolbarItemView>(&self) -> Option<Entity<T>> {
        self.items
            .iter()
            .find_map(|(item, _)| item.to_any().downcast().ok())
    }

    pub fn hidden(&self) -> bool {
        self.hidden
    }

    pub fn contribute_context(&self, context: &mut KeyContext, cx: &App) {
        for (item, location) in &self.items {
            if *location != ToolbarItemLocation::Hidden {
                item.contribute_context(context, cx);
            }
        }
    }
}

impl<T: ToolbarItemView> ToolbarItemViewHandle for Entity<T> {
    fn id(&self) -> EntityId {
        self.entity_id()
    }

    fn boxed_clone(&self) -> Box<dyn ToolbarItemViewHandle> {
        Box::new(self.clone())
    }

    fn to_any(&self) -> AnyView {
        self.clone().into()
    }

    fn set_active_pane_item(
        &self,
        active_pane_item: Option<&dyn ItemHandle>,
        window: &mut Window,
        cx: &mut App,
    ) -> ToolbarItemLocation {
        self.update(cx, |this, cx| {
            this.set_active_pane_item(active_pane_item, window, cx)
        })
    }

    fn focus_changed(&self, pane_focused: bool, window: &mut Window, cx: &mut App) {
        self.update(cx, |this, cx| {
            this.pane_focus_update(pane_focused, window, cx);
            cx.notify();
        });
    }

    fn contribute_context(&self, context: &mut KeyContext, cx: &App) {
        self.read(cx).contribute_context(context, cx)
    }
}
