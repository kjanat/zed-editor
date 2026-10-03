use std::str::FromStr;
use std::sync::Arc;

use url::Url;

use git::{
    BuildCommitPermalinkParams, BuildPermalinkParams, GitHostingProvider, ParsedGitRemote,
    RemoteUrl,
};

pub struct Gerrit {
    name: String,
    base_url: Url,
}

impl Gerrit {
    pub fn new(name: impl Into<String>, base_url: Url) -> Self {
        Self {
            name: name.into(),
            base_url,
        }
    }

    fn build_url(&self, path: &str) -> Url {
        let mut url = self.base_url();
        let base_path = url.path().trim_end_matches('/').to_string();
        url.set_path(&format!("{base_path}/{path}"));
        url
    }
}

#[async_trait::async_trait]
impl GitHostingProvider for Gerrit {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn base_url(&self) -> Url {
        self.base_url.clone()
    }

    fn supports_avatars(&self) -> bool {
        false
    }

    fn format_line_number(&self, line: u32) -> String {
        format!("{line}")
    }

    fn format_line_numbers(&self, start_line: u32, _end_line: u32) -> String {
        format!("{start_line}")
    }

    fn parse_remote_url(&self, url: &str) -> Option<ParsedGitRemote> {
        let url = RemoteUrl::from_str(url).ok()?;

        let host = url.host_str()?;
        if host != self.base_url.host_str()? {
            return None;
        }

        let path_segments = url.path_segments()?.collect::<Vec<_>>();
        let joined_path = path_segments.join("/");
        let base_path = self.base_url.path().trim_matches('/');
        let repo_path = if base_path.is_empty() {
            joined_path.as_str()
        } else {
            joined_path
                .strip_prefix(base_path)
                .and_then(|path| path.strip_prefix('/'))
                .unwrap_or(joined_path.as_str())
        };
        let repo = repo_path.trim_start_matches("a/").trim_end_matches(".git");

        Some(ParsedGitRemote {
            owner: Arc::from(""),
            repo: repo.into(),
        })
    }

    fn build_commit_permalink(
        &self,
        remote: &ParsedGitRemote,
        params: BuildCommitPermalinkParams,
    ) -> Url {
        let BuildCommitPermalinkParams { sha } = params;
        let ParsedGitRemote { owner: _, repo } = remote;

        self.build_url(&format!("{repo}/+/{sha}"))
    }

    fn build_permalink(&self, remote: ParsedGitRemote, params: BuildPermalinkParams) -> Url {
        let ParsedGitRemote { owner: _, repo } = remote;
        let BuildPermalinkParams {
            sha,
            path,
            selection,
        } = params;

        let mut permalink = self.build_url(&format!("{repo}/+/{sha}/{path}"));
        permalink.set_fragment(
            selection
                .map(|selection| self.line_fragment(&selection))
                .as_deref(),
        );
        permalink
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use git::repository::repo_path;
    use pretty_assertions::assert_eq;

    use super::*;

    fn gerrit_instance() -> Gerrit {
        Gerrit::new("Gerrit", Url::parse("https://gerrit.example.com").unwrap())
    }

    fn gerrit_instance_with_path_prefix() -> Gerrit {
        Gerrit::new("Gerrit", Url::parse("https://example.com/gerrit").unwrap())
    }

    #[test]
    fn test_parse_remote_url_strips_base_path_prefix() {
        let parsed_remote = gerrit_instance_with_path_prefix()
            .parse_remote_url("https://example.com/gerrit/a/my-project.git")
            .unwrap();

        assert_eq!(
            parsed_remote,
            ParsedGitRemote {
                owner: Arc::from(""),
                repo: "my-project".into(),
            }
        );
    }

    #[test]
    fn test_parse_remote_url_given_ssh_url_with_base_path_prefix() {
        let parsed_remote = gerrit_instance_with_path_prefix()
            .parse_remote_url("ssh://example.com:29418/my-project")
            .unwrap();

        assert_eq!(
            parsed_remote,
            ParsedGitRemote {
                owner: Arc::from(""),
                repo: "my-project".into(),
            }
        );
    }

    #[test]
    fn test_build_commit_permalink_with_base_path_prefix() {
        let permalink = gerrit_instance_with_path_prefix().build_commit_permalink(
            &ParsedGitRemote {
                owner: Arc::from(""),
                repo: "my-project".into(),
            },
            BuildCommitPermalinkParams {
                sha: "abc123def456",
            },
        );

        assert_eq!(
            permalink.to_string(),
            "https://example.com/gerrit/my-project/+/abc123def456"
        );
    }

    #[test]
    fn test_build_permalink_with_base_path_prefix() {
        let permalink = gerrit_instance_with_path_prefix().build_permalink(
            ParsedGitRemote {
                owner: Arc::from(""),
                repo: "my-project".into(),
            },
            BuildPermalinkParams::new("abc123def456", &repo_path("src/main.rs"), Some(41..41)),
        );

        assert_eq!(
            permalink.to_string(),
            "https://example.com/gerrit/my-project/+/abc123def456/src/main.rs#42"
        );
    }

    #[test]
    fn test_parse_remote_url_given_https_url() {
        let parsed_remote = gerrit_instance()
            .parse_remote_url("https://gerrit.example.com/my-project")
            .unwrap();

        assert_eq!(
            parsed_remote,
            ParsedGitRemote {
                owner: Arc::from(""),
                repo: "my-project".into(),
            }
        );
    }

    #[test]
    fn test_parse_remote_url_strips_a_prefix() {
        let parsed_remote = gerrit_instance()
            .parse_remote_url("https://gerrit.example.com/a/my-project.git")
            .unwrap();

        assert_eq!(
            parsed_remote,
            ParsedGitRemote {
                owner: Arc::from(""),
                repo: "my-project".into(),
            }
        );
    }

    #[test]
    fn test_parse_remote_url_given_ssh_url() {
        let parsed_remote = gerrit_instance()
            .parse_remote_url("git@gerrit.example.com:my-project.git")
            .unwrap();

        assert_eq!(
            parsed_remote,
            ParsedGitRemote {
                owner: Arc::from(""),
                repo: "my-project".into(),
            }
        );
    }

    #[test]
    fn test_build_commit_permalink() {
        let permalink = gerrit_instance().build_commit_permalink(
            &ParsedGitRemote {
                owner: Arc::from(""),
                repo: "my-project".into(),
            },
            BuildCommitPermalinkParams {
                sha: "abc123def456",
            },
        );

        assert_eq!(
            permalink.to_string(),
            "https://gerrit.example.com/my-project/+/abc123def456"
        );
    }

    #[test]
    fn test_build_permalink() {
        let permalink = gerrit_instance().build_permalink(
            ParsedGitRemote {
                owner: Arc::from(""),
                repo: "my-project".into(),
            },
            BuildPermalinkParams::new("abc123def456", &repo_path("src/main.rs"), None),
        );

        assert_eq!(
            permalink.to_string(),
            "https://gerrit.example.com/my-project/+/abc123def456/src/main.rs"
        );
    }

    #[test]
    fn test_build_permalink_with_single_line_selection() {
        let permalink = gerrit_instance().build_permalink(
            ParsedGitRemote {
                owner: Arc::from(""),
                repo: "my-project".into(),
            },
            BuildPermalinkParams::new("abc123def456", &repo_path("src/main.rs"), Some(41..41)),
        );

        assert_eq!(
            permalink.to_string(),
            "https://gerrit.example.com/my-project/+/abc123def456/src/main.rs#42"
        );
    }

    #[test]
    fn test_build_permalink_with_multi_line_selection() {
        let permalink = gerrit_instance().build_permalink(
            ParsedGitRemote {
                owner: Arc::from(""),
                repo: "my-project".into(),
            },
            BuildPermalinkParams::new("abc123def456", &repo_path("src/main.rs"), Some(41..55)),
        );

        // Gitiles only supports single line anchors
        assert_eq!(
            permalink.to_string(),
            "https://gerrit.example.com/my-project/+/abc123def456/src/main.rs#42"
        );
    }
}
