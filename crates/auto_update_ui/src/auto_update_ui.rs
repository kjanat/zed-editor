use auto_update::{
    AutoUpdater, Check, PackageManagerCheck, UpdateCheckType, release_notes_asset_url,
    release_notes_url,
};
use db::kvp::Dismissable;
use editor::{Editor, MultiBuffer};
use gpui::{
    App, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, TaskExt, Window, actions,
    prelude::*,
};
use markdown_preview::markdown_preview_view::{MarkdownPreviewMode, MarkdownPreviewView};
use project::DisableAiSettings;
use release_channel::{AppVersion, ReleaseChannel};
use semver::Version;
use serde::Deserialize;
use settings::Settings as _;
use smol::io::AsyncReadExt;
use ui::{AnnouncementToast, DeltaIllustration, ListBulletItem, prelude::*};
use util::{ResultExt as _, markdown::MarkdownCodeBlock, maybe};
use workspace::{
    Workspace,
    notifications::{
        Notification, NotificationId, SuppressEvent, show_app_notification,
        simple_message_notification::MessageNotification,
    },
    workspace_error::{ErrorAction, ErrorSeverity, WorkspaceError},
};
use zed_actions::ShowUpdateNotification;

actions!(
    auto_update,
    [
        /// Opens the release notes for the current version in a new tab.
        ViewReleaseNotesLocally
    ]
);

pub fn init(cx: &mut App) {
    notify_if_app_was_updated(cx);
    cx.observe_new(|workspace: &mut Workspace, _window, cx| {
        workspace.register_action(|_, action, window, cx| check(action, window, cx));

        workspace.register_action(|workspace, _: &ViewReleaseNotesLocally, window, cx| {
            view_release_notes_locally(workspace, window, cx);
        });

        if matches!(
            ReleaseChannel::global(cx),
            ReleaseChannel::Nightly | ReleaseChannel::Dev
        ) {
            workspace.register_action(|_workspace, _: &ShowUpdateNotification, _window, cx| {
                show_update_notification(cx);
            });
        }
    })
    .detach();
}

pub fn check(_: &Check, window: &mut Window, cx: &mut App) {
    if let Some(explanation) = auto_update::update_explanation() {
        check_with_package_manager(explanation, window, cx);
        return;
    }

    if !ReleaseChannel::try_global(cx)
        .map(|channel| channel.poll_for_updates())
        .unwrap_or(false)
    {
        return;
    }

    if let Some(updater) = AutoUpdater::get(cx) {
        updater.update(cx, |updater, cx| updater.poll(UpdateCheckType::Manual, cx));
    } else {
        drop(window.prompt(
            gpui::PromptLevel::Info,
            "Could not check for updates",
            Some("Auto-updates disabled for non-bundled app."),
            &["OK"],
            cx,
        ));
    }
}

fn check_with_package_manager(explanation: String, window: &mut Window, cx: &mut App) {
    let Some(check) = auto_update::package_manager_update_check(cx) else {
        drop(window.prompt(
            gpui::PromptLevel::Info,
            "Zed was installed via a package manager.",
            Some(&explanation),
            &["OK"],
            cx,
        ));
        return;
    };

    let command = auto_update::update_command();
    window
        .spawn(cx, async move |cx| {
            let outcome = check.await;
            let detail = package_manager_prompt_detail(&outcome, &explanation, command.as_deref());
            cx.update(|window, cx| {
                drop(window.prompt(
                    gpui::PromptLevel::Info,
                    &outcome.message(),
                    detail.as_deref(),
                    &["OK"],
                    cx,
                ));
            })
            .log_err();
        })
        .detach();
}

fn package_manager_prompt_detail(
    outcome: &PackageManagerCheck,
    explanation: &str,
    command: Option<&str>,
) -> Option<String> {
    match outcome {
        PackageManagerCheck::UpToDate { .. } => None,
        PackageManagerCheck::Failed { .. } => outcome.detail(),
        PackageManagerCheck::UpdateAvailable { .. } => {
            let mut detail = outcome.detail().unwrap_or_default();
            if !explanation.trim().is_empty() {
                detail.push_str("\n\n");
                detail.push_str(explanation);
            }
            if let Some(command) = command.filter(|command| !command.trim().is_empty()) {
                detail.push_str("\n\nTo update, run:\n\n");
                detail.push_str(
                    &MarkdownCodeBlock {
                        tag: "sh",
                        text: command,
                    }
                    .to_string(),
                );
            }
            Some(detail)
        }
    }
}

#[derive(Deserialize)]
struct ReleaseNotesBody {
    title: String,
    release_notes: String,
}

struct ReleaseNotesError {
    url: Option<String>,
}

impl WorkspaceError for ReleaseNotesError {
    fn primary_message(&self) -> SharedString {
        "Couldn't load release notes".into()
    }
    fn severity(&self) -> ErrorSeverity {
        ErrorSeverity::Error
    }
    fn primary_action(&self) -> ErrorAction {
        self.url
            .clone()
            .map(|url| ErrorAction::link("View in Browser", url))
            .unwrap_or_else(ErrorAction::dismiss)
    }
}

fn notify_release_notes_failed_to_show(
    workspace: &mut Workspace,
    _window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let url = release_notes_url(cx);
    workspace.show_error(ReleaseNotesError { url }, cx);
}

fn view_release_notes_locally(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let release_channel = ReleaseChannel::global(cx);

    if matches!(
        release_channel,
        ReleaseChannel::Nightly | ReleaseChannel::Dev
    ) {
        if let Some(url) = release_notes_url(cx) {
            cx.open_url(&url);
        }
        return;
    }

    let client = client::Client::global(cx).http_client();
    let url = release_notes_asset_url(Some(AppVersion::global(cx)));

    let markdown = workspace
        .app_state()
        .languages
        .language_for_name("Markdown");

    cx.spawn_in(window, async move |workspace, cx| {
        let markdown = markdown.await.log_err();
        let response = client.get(&url, Default::default(), true).await;
        let Some(mut response) = response.log_err() else {
            workspace
                .update_in(cx, notify_release_notes_failed_to_show)
                .log_err();
            return;
        };

        let body: anyhow::Result<ReleaseNotesBody> = async {
            anyhow::ensure!(
                response.status().is_success(),
                "Failed to load fork release notes: HTTP {}",
                response.status()
            );
            let mut body = Vec::new();
            response.body_mut().read_to_end(&mut body).await?;
            Ok(serde_json::from_slice(&body)?)
        }
        .await;

        let res: Option<()> = maybe!(async {
            let body = body.log_err()?;
            let project = workspace
                .read_with(cx, |workspace, _| workspace.project().clone())
                .ok()?;
            let (language_registry, buffer) = project.update(cx, |project, cx| {
                (
                    project.languages().clone(),
                    project.create_buffer(markdown, false, cx),
                )
            });
            let buffer = buffer.await.ok()?;
            buffer.update(cx, |buffer, cx| {
                buffer.edit([(0..0, body.release_notes)], None, cx)
            });

            let buffer = cx.new(|cx| MultiBuffer::singleton(buffer, cx).with_title(body.title));

            let ws_handle = workspace.clone();
            workspace
                .update_in(cx, |workspace, window, cx| {
                    let editor =
                        cx.new(|cx| Editor::for_multibuffer(buffer, Some(project), window, cx));
                    let markdown_preview: Entity<MarkdownPreviewView> = MarkdownPreviewView::new(
                        MarkdownPreviewMode::Default,
                        editor,
                        ws_handle,
                        language_registry,
                        window,
                        cx,
                    );
                    workspace.add_item_to_active_pane(
                        Box::new(markdown_preview),
                        None,
                        true,
                        window,
                        cx,
                    );
                    cx.notify();
                })
                .ok()
        })
        .await;
        if res.is_none() {
            workspace
                .update_in(cx, notify_release_notes_failed_to_show)
                .log_err();
        }
    })
    .detach();
}

#[derive(Clone)]
struct AnnouncementContent {
    heading: SharedString,
    description: SharedString,
    bullet_items: Vec<SharedString>,
    primary_action_label: SharedString,
    secondary_action_label: SharedString,
    primary_action_url: SharedString,
    secondary_action_url: SharedString,
}

struct DeltaAnnouncement;

impl Dismissable for DeltaAnnouncement {
    const KEY: &'static str = "delta_announcement_dismissed";
}

fn announcement_for_version(version: &Version, cx: &App) -> Option<AnnouncementContent> {
    let version_with_delta = Version::new(1, 22, 0);
    if *version < version_with_delta
        || DisableAiSettings::get_global(cx).disable_ai
        || DeltaAnnouncement::dismissed(cx)
    {
        return None;
    }

    Some(AnnouncementContent {
        heading: "Introducing Delta".into(),
        description:
            "Built on DeltaDB, so your threads and code stay in sync across machines and teammates."
                .into(),
        bullet_items: vec![
            "Made by the Zed team, with the same quality and performance".into(),
            "Work with teammates and agents in the same thread, live or later".into(),
            "Pick up your thread on the web or your phone, without committing or pushing".into(),
        ],
        primary_action_label: "Try Delta".into(),
        secondary_action_label: "Learn More".into(),
        primary_action_url: "https://delta.dev/".into(),
        secondary_action_url: "https://delta.dev/docs/getting-started".into(),
    })
}

struct AnnouncementToastNotification {
    focus_handle: FocusHandle,
    content: AnnouncementContent,
}

impl AnnouncementToastNotification {
    fn new(content: AnnouncementContent, cx: &mut App) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            content,
        }
    }

    fn dismiss(&mut self, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
        DeltaAnnouncement::set_dismissed(true, cx);
    }
}

impl Focusable for AnnouncementToastNotification {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<DismissEvent> for AnnouncementToastNotification {}
impl EventEmitter<SuppressEvent> for AnnouncementToastNotification {}
impl Notification for AnnouncementToastNotification {}

impl Render for AnnouncementToastNotification {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let toast = AnnouncementToast::new()
            .illustration(DeltaIllustration::new())
            .heading(self.content.heading.clone())
            .description(self.content.description.clone())
            .bullet_items(
                self.content
                    .bullet_items
                    .iter()
                    .map(|item| ListBulletItem::new(item.clone())),
            )
            .primary_action_label(self.content.primary_action_label.clone())
            .secondary_action_label(self.content.secondary_action_label.clone())
            .primary_on_click(cx.listener({
                let url = self.content.primary_action_url.clone();
                move |this, _, _window, cx| {
                    telemetry::event!("Delta Announcement Main Click");
                    cx.open_url(&url);
                    this.dismiss(cx);
                }
            }))
            .secondary_on_click(cx.listener({
                let url = self.content.secondary_action_url.clone();
                move |_, _, _window, cx| {
                    telemetry::event!("Delta Announcement Secondary Click");
                    cx.open_url(&url);
                }
            }))
            .dismiss_on_click(cx.listener(|this, _, _window, cx| {
                telemetry::event!("Delta Announcement Dismiss");
                this.dismiss(cx);
            }));

        div()
            .self_end()
            .flex_none()
            .w(rems_from_px(400_f32))
            .max_w((window.viewport_size().width - window.rem_size() * 1.5).max(px(0.)))
            .child(toast)
    }
}

struct UpdateNotification;

fn show_update_notification(cx: &mut App) {
    let Some(updater) = AutoUpdater::get(cx) else {
        return;
    };

    let mut version = updater.read(cx).current_version();
    version.pre = semver::Prerelease::EMPTY;
    version.build = semver::BuildMetadata::EMPTY;
    let app_name = ReleaseChannel::global(cx).display_name();

    if let Some(content) = announcement_for_version(&version, cx) {
        show_app_notification(
            NotificationId::unique::<UpdateNotification>(),
            cx,
            move |cx| cx.new(|cx| AnnouncementToastNotification::new(content.clone(), cx)),
        );
    } else {
        show_app_notification(
            NotificationId::unique::<UpdateNotification>(),
            cx,
            move |cx| {
                let workspace_handle = cx.entity().downgrade();
                cx.new(|cx| {
                    MessageNotification::new(format!("Updated to {app_name} {}", version), cx)
                        .primary_message("View Release Notes")
                        .primary_on_click(move |window, cx| {
                            if let Some(workspace) = workspace_handle.upgrade() {
                                workspace.update(cx, |workspace, cx| {
                                    crate::view_release_notes_locally(workspace, window, cx);
                                })
                            }
                            cx.emit(DismissEvent);
                        })
                        .show_suppress_button(false)
                })
            },
        );
    }
}

/// Shows a notification across all workspaces if an update was previously automatically installed
/// and this notification had not yet been shown.
pub fn notify_if_app_was_updated(cx: &mut App) {
    let Some(updater) = AutoUpdater::get(cx) else {
        return;
    };

    if let ReleaseChannel::Nightly = ReleaseChannel::global(cx) {
        return;
    }

    let should_show_notification = updater.read(cx).should_show_update_notification(cx);

    cx.spawn(async move |cx| {
        let should_show_notification = should_show_notification.await?;

        if should_show_notification {
            cx.update(|cx| {
                show_update_notification(cx);
                updater.update(cx, |updater, cx| {
                    updater
                        .set_should_show_update_notification(false, cx)
                        .detach_and_log_err(cx);
                });
            });
        }
        anyhow::Ok(())
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use http_client::{AsyncBody, HttpClient, Response};
    use project::Project;
    use std::{pin::Pin, task::Poll};

    struct FailingReader;

    impl smol::io::AsyncRead for FailingReader {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            _buffer: &mut [u8],
        ) -> Poll<std::io::Result<usize>> {
            Poll::Ready(Err(std::io::Error::other("response body interrupted")))
        }
    }

    async fn assert_release_notes_failure(status: u16, body: AsyncBody, cx: &mut TestAppContext) {
        let app_state = cx.update(|cx| {
            cx.set_global(db::AppDatabase::test_new());
            release_channel::init_test(Version::new(1, 3, 10), ReleaseChannel::Stable, cx);
            let app_state = workspace::AppState::test(cx);
            client::Client::set_global(app_state.client.clone(), cx);
            app_state
        });
        let response = std::sync::Mutex::new(Some(body));
        app_state
            .client
            .http_client()
            .as_fake()
            .replace_handler(move |_, request| {
                assert_eq!(
                    request.uri().to_string(),
                    "https://github.com/kjanat/zed-editor/releases/download/v1.3.10/notes.json"
                );
                let body = response
                    .lock()
                    .expect("response lock poisoned")
                    .take()
                    .expect("release notes requested more than once");
                async move { Ok(Response::builder().status(status).body(body)?) }
            });
        let project = Project::test(app_state.fs.clone(), [], cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project, window, cx));
        cx.run_until_parked();
        workspace.update_in(cx, |workspace, window, cx| {
            assert!(workspace.notification_ids().is_empty());
            view_release_notes_locally(workspace, window, cx);
        });
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, _| {
            assert_eq!(
                workspace.notification_ids(),
                vec![NotificationId::unique::<ReleaseNotesError>()]
            );
        });
    }

    #[gpui::test]
    async fn release_notes_missing_asset_shows_error(cx: &mut TestAppContext) {
        assert_release_notes_failure(
            404,
            r#"{"title":"Release notes","release_notes":"Changes"}"#.into(),
            cx,
        )
        .await;
    }

    #[gpui::test]
    async fn release_notes_body_read_failure_shows_error(cx: &mut TestAppContext) {
        let reader =
            smol::io::Cursor::new(br#"{"title":"Release notes","release_notes":"Changes"}"#)
                .chain(FailingReader);
        assert_release_notes_failure(200, AsyncBody::from_reader(reader), cx).await;
    }

    #[gpui::test]
    async fn release_notes_malformed_json_shows_error(cx: &mut TestAppContext) {
        assert_release_notes_failure(200, "{".into(), cx).await;
    }

    #[test]
    fn package_manager_prompt_only_offers_a_command_for_an_update() {
        let installed = Version::new(1, 3, 7);
        let explanation = "Zed was installed via pacman.";
        let command = Some("pacman -Syu");
        assert_eq!(
            package_manager_prompt_detail(
                &PackageManagerCheck::UpToDate {
                    installed: installed.clone()
                },
                explanation,
                command
            ),
            None
        );
        assert_eq!(
            package_manager_prompt_detail(
                &PackageManagerCheck::Failed {
                    error: "Network is unreachable".into()
                },
                explanation,
                command
            )
            .as_deref(),
            Some("Network is unreachable")
        );
        let outcome = PackageManagerCheck::UpdateAvailable {
            installed,
            available: Version::new(1, 3, 8),
        };
        assert_eq!(
            package_manager_prompt_detail(&outcome, explanation, command).as_deref(),
            Some(
                "You are running 1.3.7.\n\nZed was installed via pacman.\n\nTo update, run:\n\n```sh\npacman -Syu\n```\n"
            )
        );
        for command in [None, Some("   ")] {
            assert_eq!(
                package_manager_prompt_detail(&outcome, explanation, command).as_deref(),
                Some("You are running 1.3.7.\n\nZed was installed via pacman.")
            );
        }
    }
}
