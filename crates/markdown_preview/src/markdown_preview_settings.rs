use gpui::Pixels;
use settings::{IntoGpui, RegisterSetting, Settings};

/// The settings for the markdown preview.
#[derive(Clone, Copy, Debug, Default, RegisterSetting)]
pub struct MarkdownPreviewSettings {
    /// Whether to automatically open Markdown files in the preview.
    pub open_markdown_files_in_preview: bool,
    /// The maximum width of the rendered markdown content, or `None` to render
    /// content edge to edge.
    pub max_width: Option<Pixels>,
}

impl Settings for MarkdownPreviewSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let project_content = content.project.markdown_preview.as_ref();
        let content = content.markdown_preview.clone().unwrap_or_default();
        let limit_content_width = project_content
            .and_then(|settings| settings.limit_content_width)
            .or(content.limit_content_width)
            .unwrap_or(true);
        let max_width = if limit_content_width {
            project_content
                .and_then(|settings| settings.max_width)
                .or(content.max_width)
                .map(IntoGpui::into_gpui)
        } else {
            None
        };
        Self {
            open_markdown_files_in_preview: content.open_markdown_files_in_preview.unwrap_or(false),
            max_width,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{App, px};
    use settings::{
        LocalSettingsKind, LocalSettingsPath, SettingsLocation, SettingsStore, WorktreeId,
    };
    use util::rel_path::rel_path;

    #[gpui::test]
    fn preview_width_inherits_user_and_nested_project_settings(cx: &mut App) {
        let mut store = SettingsStore::new(cx, &settings::default_settings());
        assert_eq!(
            store.get::<MarkdownPreviewSettings>(None).max_width,
            Some(px(800.))
        );
        store
            .set_user_settings(
                r#"{"markdown_preview":{"max_width":960,"open_markdown_files_in_preview":true}}"#,
                cx,
            )
            .unwrap();

        let first_root = WorktreeId::from_usize(1);
        let second_root = WorktreeId::from_usize(2);
        for (root, directory, content) in [
            (
                first_root,
                "",
                r#"{"markdown_preview":{"limit_content_width":false}}"#,
            ),
            (
                first_root,
                "docs",
                r#"{"markdown_preview":{"limit_content_width":true}}"#,
            ),
            (
                first_root,
                "docs/narrow",
                r#"{"markdown_preview":{"max_width":640}}"#,
            ),
            (
                second_root,
                "",
                r#"{"markdown_preview":{"max_width":1200}}"#,
            ),
        ] {
            store
                .set_local_settings(
                    root,
                    LocalSettingsPath::InWorktree(rel_path(directory).into()),
                    LocalSettingsKind::Settings,
                    Some(content),
                    cx,
                )
                .unwrap();
        }

        for (root, path, expected) in [
            (first_root, "readme.md", None),
            (first_root, "docs/guide.md", Some(px(960.))),
            (first_root, "docs/narrow/guide.md", Some(px(640.))),
            (second_root, "readme.md", Some(px(1200.))),
        ] {
            let settings = store.get::<MarkdownPreviewSettings>(Some(SettingsLocation {
                worktree_id: root,
                path: rel_path(path),
            }));
            assert_eq!(settings.max_width, expected);
            assert!(settings.open_markdown_files_in_preview);
        }
        assert_eq!(
            store.get::<MarkdownPreviewSettings>(None).max_width,
            Some(px(960.))
        );

        store
            .set_user_settings(
                r#"{"markdown_preview":{"limit_content_width":false,"max_width":1000}}"#,
                cx,
            )
            .unwrap();
        assert_eq!(store.get::<MarkdownPreviewSettings>(None).max_width, None);
        assert_eq!(
            store
                .get::<MarkdownPreviewSettings>(Some(SettingsLocation {
                    worktree_id: first_root,
                    path: rel_path("docs/guide.md"),
                }))
                .max_width,
            Some(px(1000.))
        );
        assert_eq!(
            store
                .get::<MarkdownPreviewSettings>(Some(SettingsLocation {
                    worktree_id: second_root,
                    path: rel_path("readme.md"),
                }))
                .max_width,
            None
        );
    }
}
