use futures::channel::oneshot;
use fuzzy::{StringMatch, StringMatchCandidate};

use core::cmp;
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, InteractiveElement,
    IntoElement, ParentElement, Render, SharedString, Subscription, Task, WeakEntity, Window, rems,
};
use picker::{Picker, PickerDelegate};
use std::sync::Arc;
use ui::{HighlightedLabel, ListItem, ListItemSpacing, prelude::*};
use util::ResultExt;
use workspace::{ModalView, Workspace};

pub struct PickerPrompt {
    pub picker: Entity<Picker<PickerPromptDelegate>>,
    _subscription: Subscription,
}

pub fn prompt(
    prompt: &str,
    options: Vec<SharedString>,
    workspace: WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) -> Task<Option<usize>> {
    if options.is_empty() {
        return Task::ready(None);
    } else if options.len() == 1 {
        return Task::ready(Some(0));
    }
    let prompt = prompt.to_string().into();

    window.spawn(cx, async move |cx| {
        // Modal branch picker has a longer trailoff than a popover one.
        let (tx, rx) = oneshot::channel();
        let delegate = PickerPromptDelegate::new(prompt, options, tx, 70);

        workspace
            .update_in(cx, |workspace, window, cx| {
                workspace
                    .toggle_modal(window, cx, move |window, cx| {
                        PickerPrompt::new(delegate, 34., window, cx)
                    })
                    .detach();
            })
            .ok();

        (rx.await).ok()
    })
}

impl PickerPrompt {
    fn new(
        delegate: PickerPromptDelegate,
        rem_width: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let picker =
            cx.new(|cx| Picker::uniform_list(delegate, window, cx).initial_width(rems(rem_width)));
        let _subscription = cx.subscribe(&picker, |_, _, _, cx| cx.emit(DismissEvent));
        Self {
            picker,
            _subscription,
        }
    }
}
impl ModalView for PickerPrompt {}
impl EventEmitter<DismissEvent> for PickerPrompt {}

impl Focusable for PickerPrompt {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for PickerPrompt {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .child(self.picker.clone())
            .on_mouse_down_out(cx.listener(|this, _, window, cx| {
                let picker = this.picker.clone();
                window.defer(cx, move |window, cx| {
                    picker.update(cx, |picker, cx| {
                        picker.cancel(&Default::default(), window, cx);
                    })
                });
            }))
    }
}

pub struct PickerPromptDelegate {
    prompt: Arc<str>,
    matches: Vec<StringMatch>,
    all_options: Vec<SharedString>,
    selected_index: usize,
    max_match_length: usize,
    tx: Option<oneshot::Sender<usize>>,
}

impl PickerPromptDelegate {
    pub fn new(
        prompt: Arc<str>,
        options: Vec<SharedString>,
        tx: oneshot::Sender<usize>,
        max_chars: usize,
    ) -> Self {
        Self {
            prompt,
            all_options: options,
            matches: vec![],
            selected_index: 0,
            max_match_length: max_chars,
            tx: Some(tx),
        }
    }
}

impl PickerDelegate for PickerPromptDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "picker prompt"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        self.prompt.clone()
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix;
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        cx.spawn_in(window, async move |picker, cx| {
            let candidates = picker.read_with(cx, |picker, _| {
                picker
                    .delegate
                    .all_options
                    .iter()
                    .enumerate()
                    .map(|(ix, option)| StringMatchCandidate::new(ix, option))
                    .collect::<Vec<StringMatchCandidate>>()
            });
            let Some(candidates) = candidates.log_err() else {
                return;
            };
            let matches: Vec<StringMatch> = if query.is_empty() {
                candidates
                    .into_iter()
                    .enumerate()
                    .map(|(index, candidate)| StringMatch {
                        candidate_id: index,
                        string: candidate.string,
                        positions: Vec::new(),
                        score: 0.0,
                    })
                    .collect()
            } else {
                fuzzy::match_strings(
                    &candidates,
                    &query,
                    true,
                    true,
                    10000,
                    &Default::default(),
                    cx.background_executor().clone(),
                )
                .await
            };
            picker
                .update(cx, |picker, _| {
                    let delegate = &mut picker.delegate;
                    delegate.matches = matches;
                    if delegate.matches.is_empty() {
                        delegate.selected_index = 0;
                    } else {
                        delegate.selected_index =
                            cmp::min(delegate.selected_index, delegate.matches.len() - 1);
                    }
                })
                .log_err();
        })
    }

    fn confirm(&mut self, _: bool, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(option) = self.matches.get(self.selected_index()) else {
            return;
        };

        self.tx.take().map(|tx| tx.send(option.candidate_id));
        cx.emit(DismissEvent);
    }

    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        cx.emit(DismissEvent);
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let hit = &self.matches.get(ix)?;
        let shortened_option = util::truncate_and_trailoff(&hit.string, self.max_match_length);

        Some(
            ListItem::new(format!("picker-prompt-menu-{ix}"))
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .map(|el| {
                    let highlights: Vec<_> = hit
                        .positions
                        .iter()
                        .filter(|&&index| index < self.max_match_length)
                        .copied()
                        .collect();

                    el.child(HighlightedLabel::new(shortened_option, highlights))
                }),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Modifiers, MouseButton, TestAppContext, point};
    use settings::SettingsStore;
    use std::{cell::Cell, rc::Rc};

    struct PromptHost {
        prompt: Entity<PickerPrompt>,
    }

    impl Render for PromptHost {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .child(div().w(px(600.)).h(px(400.)).child(self.prompt.clone()))
        }
    }

    #[gpui::test]
    async fn test_mouse_down_outside_dismisses_picker_prompt(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });

        let (tx, _rx) = oneshot::channel();
        let delegate = PickerPromptDelegate::new(
            "Pick an option".into(),
            vec!["first".into(), "second".into()],
            tx,
            70,
        );
        let (host, cx) = cx.add_window_view(|window, cx| PromptHost {
            prompt: cx.new(|cx| PickerPrompt::new(delegate, 34., window, cx)),
        });
        let prompt = host.read_with(cx, |host, _| host.prompt.clone());
        let dismissed = Rc::new(Cell::new(false));
        let _subscription = cx.update(|_, cx| {
            let dismissed = dismissed.clone();
            cx.subscribe(&prompt, move |_, _: &DismissEvent, _| dismissed.set(true))
        });
        cx.run_until_parked();

        let viewport = cx.update(|window, _| window.viewport_size());
        cx.simulate_mouse_down(
            point(viewport.width - px(1.), viewport.height - px(1.)),
            MouseButton::Left,
            Modifiers::none(),
        );
        cx.run_until_parked();

        assert!(dismissed.get());
    }
}
