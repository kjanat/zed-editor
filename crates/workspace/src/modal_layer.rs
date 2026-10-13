use gpui::{
    AnyView, App, DismissEvent, Entity, EventEmitter, FocusHandle, Global, ManagedView,
    MouseButton, MouseDownEvent, Subscription, WeakEntity, WeakFocusHandle,
};
use ui::prelude::*;

#[derive(Debug)]
pub enum DismissDecision {
    Dismiss(bool),
    Pending,
}

// A modal that hosts a picker forwards its own focus handle to the inner picker,
// so the modal layer recognizes a reopenable picker by focus identity rather than
// requiring each modal wrapper to implement its own opt-in.
#[derive(Default)]
struct ReopenablePickerRegistry {
    handles: Vec<WeakFocusHandle>,
}

impl Global for ReopenablePickerRegistry {}

pub fn register_reopenable_picker(focus_handle: &FocusHandle, cx: &mut App) {
    let registry = cx.default_global::<ReopenablePickerRegistry>();
    registry.handles.retain(|handle| handle.upgrade().is_some());
    if !registry.handles.iter().any(|handle| handle == focus_handle) {
        registry.handles.push(focus_handle.downgrade());
    }
}

pub fn deregister_reopenable_picker(focus_handle: &FocusHandle, cx: &mut App) {
    let registry = cx.default_global::<ReopenablePickerRegistry>();
    registry
        .handles
        .retain(|handle| handle.upgrade().is_some() && handle != focus_handle);
}

fn focus_handle_is_reopenable(focus_handle: &FocusHandle, cx: &App) -> bool {
    cx.try_global::<ReopenablePickerRegistry>()
        .is_some_and(|registry| {
            registry
                .handles
                .iter()
                .filter_map(|handle| handle.upgrade())
                .any(|handle| &handle == focus_handle)
        })
}

pub trait ModalView: ManagedView {
    fn on_before_dismiss(
        &mut self,
        _window: &mut Window,
        _: &mut Context<Self>,
    ) -> DismissDecision {
        DismissDecision::Dismiss(true)
    }

    fn on_dismissed(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {}

    fn fade_out_background(&self) -> bool {
        false
    }

    fn render_bare(&self) -> bool {
        false
    }
}

trait ModalViewHandle {
    fn on_before_dismiss(&self, window: &mut Window, cx: &mut App) -> DismissDecision;
    fn on_dismissed(&self, window: &mut Window, cx: &mut App);
    fn boxed_clone(&self) -> Box<dyn ModalViewHandle>;
    fn view(&self) -> AnyView;
    fn focus_handle(&self, cx: &App) -> FocusHandle;
    fn subscribe_dismiss(
        &self,
        modal_layer: WeakEntity<ModalLayer>,
        window: &mut Window,
        cx: &mut App,
    ) -> Subscription;
    fn fade_out_background(&self, cx: &mut App) -> bool;
    fn render_bare(&self, cx: &mut App) -> bool;
}

impl<V: ModalView> ModalViewHandle for Entity<V> {
    fn on_before_dismiss(&self, window: &mut Window, cx: &mut App) -> DismissDecision {
        self.update(cx, |this, cx| this.on_before_dismiss(window, cx))
    }

    fn on_dismissed(&self, window: &mut Window, cx: &mut App) {
        self.update(cx, |this, cx| this.on_dismissed(window, cx))
    }

    fn boxed_clone(&self) -> Box<dyn ModalViewHandle> {
        Box::new(self.clone())
    }

    fn view(&self) -> AnyView {
        self.clone().into()
    }

    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.read(cx).focus_handle(cx)
    }

    fn subscribe_dismiss(
        &self,
        modal_layer: WeakEntity<ModalLayer>,
        window: &mut Window,
        cx: &mut App,
    ) -> Subscription {
        window.subscribe(self, cx, move |_, _: &DismissEvent, window, cx| {
            if let Some(modal_layer) = modal_layer.upgrade() {
                ModalLayer::hide(&modal_layer, window, cx);
            }
        })
    }

    fn fade_out_background(&self, cx: &mut App) -> bool {
        self.read(cx).fade_out_background()
    }

    fn render_bare(&self, cx: &mut App) -> bool {
        self.read(cx).render_bare()
    }
}

pub struct ActiveModal {
    modal: Box<dyn ModalViewHandle>,
    modal_focus_handle: FocusHandle,
    _subscriptions: [Subscription; 2],
    previous_focus_handle: Option<FocusHandle>,
    focus_handle: FocusHandle,
}

struct StashedModal {
    modal: Box<dyn ModalViewHandle>,
    modal_focus_handle: FocusHandle,
}

pub struct ModalLayer {
    active_modal: Option<ActiveModal>,
    // Kept alive (hidden) rather than dropped on dismissal so `ReopenLastPicker`
    // can reveal it again with its exact prior state. Left intact across
    // non-reopenable modals (e.g. the command palette), which may trigger the reopen.
    stashed_modal: Option<StashedModal>,
    // Set when a reveal was requested while another modal was still active (e.g. the
    // which-key popup that is dismissed only after the reopen action fires). The reveal
    // then happens once that modal closes, so it doesn't matter whether the triggering
    // modal is dismissed before or after the action dispatches.
    reveal_stash_when_free: bool,
    dismiss_on_focus_lost: bool,
}

pub(crate) struct ModalOpenedEvent;

impl EventEmitter<ModalOpenedEvent> for ModalLayer {}

impl Default for ModalLayer {
    fn default() -> Self {
        Self::new()
    }
}

impl ModalLayer {
    pub fn new() -> Self {
        Self {
            active_modal: None,
            stashed_modal: None,
            reveal_stash_when_free: false,
            dismiss_on_focus_lost: false,
        }
    }

    /// Toggles a modal of type `V`. If a modal of the same type is currently active,
    /// it will be hidden. If a different modal is active, it will be replaced with the new one.
    /// If no modal is active, the new modal will be shown.
    ///
    /// If closing the current modal fails (e.g., due to `on_before_dismiss` returning
    /// `DismissDecision::Dismiss(false)` or `DismissDecision::Pending`), the new modal
    /// will not be shown.
    pub fn toggle<V, B>(
        modal_layer: &Entity<Self>,
        window: &mut Window,
        cx: &mut App,
        build_view: B,
    ) -> Option<Entity<V>>
    where
        V: ModalView,
        B: FnOnce(&mut Window, &mut Context<V>) -> V,
    {
        // Opening a modal explicitly supersedes any reveal that was waiting for the
        // layer to become free.
        let (active_modal_is_same_type, previous_focus_handle) =
            modal_layer.update(cx, |modal_layer, cx| {
                modal_layer.reveal_stash_when_free = false;
                match &modal_layer.active_modal {
                    Some(active_modal) => (
                        Some(active_modal.modal.view().downcast::<V>().is_ok()),
                        active_modal.previous_focus_handle.clone(),
                    ),
                    None => (None, window.focused(cx)),
                }
            });
        if let Some(should_close) = active_modal_is_same_type {
            let did_close = Self::hide(modal_layer, window, cx);
            if should_close || !did_close {
                return None;
            }
        }
        let new_modal = cx.new(|cx| build_view(window, cx));
        let modal_focus_handle = new_modal.focus_handle(cx);
        modal_layer.update(cx, |modal_layer, cx| {
            modal_layer.show_modal(
                Box::new(new_modal.clone()),
                modal_focus_handle,
                previous_focus_handle,
                window,
                cx,
            );
            cx.emit(ModalOpenedEvent);
        });
        Some(new_modal)
    }

    /// Shows a modal and sets up subscriptions for dismiss events and focus tracking.
    /// The modal is automatically focused after being shown.
    fn show_modal(
        &mut self,
        modal: Box<dyn ModalViewHandle>,
        modal_focus_handle: FocusHandle,
        previous_focus_handle: Option<FocusHandle>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let focus_handle = cx.focus_handle();
        let modal_layer = cx.weak_entity();
        let dismiss_subscription = modal.subscribe_dismiss(modal_layer.clone(), window, cx);
        let focus_out_subscription =
            window.on_focus_out(&focus_handle, cx, move |_, window, cx| {
                if let Some(modal_layer) = modal_layer.upgrade()
                    && modal_layer.read(cx).dismiss_on_focus_lost
                {
                    Self::hide(&modal_layer, window, cx);
                }
            });
        self.active_modal = Some(ActiveModal {
            modal,
            modal_focus_handle: modal_focus_handle.clone(),
            _subscriptions: [dismiss_subscription, focus_out_subscription],
            previous_focus_handle,
            focus_handle,
        });
        cx.defer_in(window, move |_, window, cx| {
            window.focus(&modal_focus_handle, cx);
        });
        cx.notify();
    }

    /// Reveals the most recently stashed reopenable modal, if any. Returns whether
    /// a modal was revealed.
    pub fn reveal_stashed_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.stashed_modal.is_none() {
            return false;
        }
        if self.active_modal.is_some() {
            // Another modal is still closing (e.g. the which-key popup that dismisses
            // only after this action fires). Reveal once it is gone; see `finish_hide`.
            self.reveal_stash_when_free = true;
            return false;
        }
        let Some(stashed_modal) = self.stashed_modal.take() else {
            return false;
        };
        let previous_focus_handle = window.focused(cx);
        self.show_modal(
            stashed_modal.modal,
            stashed_modal.modal_focus_handle,
            previous_focus_handle,
            window,
            cx,
        );
        cx.emit(ModalOpenedEvent);
        true
    }

    /// Attempts to hide the currently active modal.
    ///
    /// The modal's `on_before_dismiss` method is called to determine if dismissal should proceed.
    /// If dismissal is allowed, the modal is removed and focus is restored to the previously
    /// focused element.
    ///
    /// Returns `true` if the modal was successfully hidden, `false` otherwise.
    pub fn hide(modal_layer: &Entity<Self>, window: &mut Window, cx: &mut App) -> bool {
        let Some(modal) = modal_layer
            .read(cx)
            .active_modal
            .as_ref()
            .map(|active_modal| active_modal.modal.boxed_clone())
        else {
            modal_layer.update(cx, |modal_layer, _| {
                modal_layer.dismiss_on_focus_lost = false;
            });
            return false;
        };
        let decision = modal.on_before_dismiss(window, cx);
        modal_layer.update(cx, |modal_layer, cx| {
            modal_layer.finish_hide(modal, decision, window, cx)
        })
    }

    fn finish_hide(
        &mut self,
        modal: Box<dyn ModalViewHandle>,
        decision: DismissDecision,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        match decision {
            DismissDecision::Dismiss(true) => {}
            DismissDecision::Dismiss(false) => {
                self.dismiss_on_focus_lost = true;
                return false;
            }
            DismissDecision::Pending => {
                self.dismiss_on_focus_lost = false;
                return false;
            }
        }

        let modal_id = modal.view().entity_id();
        let Some(active_modal) = self
            .active_modal
            .take_if(|active_modal| active_modal.modal.view().entity_id() == modal_id)
        else {
            return false;
        };
        let reopenable = focus_handle_is_reopenable(&active_modal.modal_focus_handle, cx);
        if let Some(previous_focus) = active_modal.previous_focus_handle
            && active_modal.focus_handle.contains_focused(window, cx)
        {
            previous_focus.focus(window, cx);
        }
        if reopenable {
            self.stashed_modal = Some(StashedModal {
                modal: active_modal.modal,
                modal_focus_handle: active_modal.modal_focus_handle,
            });
        }
        cx.notify();
        window.defer(cx, move |window, cx| modal.on_dismissed(window, cx));
        self.dismiss_on_focus_lost = false;
        // A reveal was requested while this modal was still active; now that the layer
        // is free, honor it.
        if self.reveal_stash_when_free && self.active_modal.is_none() {
            self.reveal_stash_when_free = false;
            self.reveal_stashed_modal(window, cx);
        }
        true
    }

    /// Returns the currently active modal if it is of type `V`.
    pub fn active_modal<V>(&self) -> Option<Entity<V>>
    where
        V: 'static,
    {
        let active_modal = self.active_modal.as_ref()?;
        active_modal.modal.view().downcast::<V>().ok()
    }

    pub fn has_active_modal(&self) -> bool {
        self.active_modal.is_some()
    }
}

impl Render for ModalLayer {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(active_modal) = &self.active_modal else {
            return div().into_any_element();
        };

        if active_modal.modal.render_bare(cx) {
            return active_modal.modal.view().into_any_element();
        }

        div()
            .absolute()
            .size_full()
            .inset_0()
            .occlude()
            .when(active_modal.modal.fade_out_background(cx), |this| {
                let mut background = cx.theme().colors().elevated_surface_background;
                background.fade_out(0.2);
                this.bg(background)
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.entity_listener(
                    |modal_layer: Entity<Self>, _: &MouseDownEvent, window, cx| {
                        Self::hide(&modal_layer, window, cx);
                    },
                ),
            )
            .child(
                v_flex()
                    .h(px(0.0))
                    .top_20()
                    .items_center()
                    .track_focus(&active_modal.focus_handle)
                    .child(
                        h_flex()
                            .occlude()
                            .child(active_modal.modal.view())
                            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                cx.stop_propagation();
                            }),
                    ),
            )
            .into_any_element()
    }
}
