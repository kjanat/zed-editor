use std::str::FromStr;

use git::{
    BuildCommitPermalinkParams, BuildPermalinkParams, GitHostingProvider, ParsedGitRemote,
    RemoteUrl,
};
use itertools::Itertools as _;
use url::Url;

pub struct GithubGist;

impl GitHostingProvider for GithubGist {
    fn name(&self) -> String {
        "GitHub Gist".into()
    }

    fn base_url(&self) -> Url {
        Url::parse("https://gist.github.com").expect("valid GitHub Gist URL")
    }

    fn supports_avatars(&self) -> bool {
        false
    }

    fn format_line_number(&self, line: u32) -> String {
        format!("L{line}")
    }

    fn format_line_numbers(&self, start_line: u32, end_line: u32) -> String {
        format!("L{start_line}-L{end_line}")
    }

    fn parse_remote_url(&self, url: &str) -> Option<ParsedGitRemote> {
        let url = RemoteUrl::from_str(url).ok()?;
        if url.host_str()? != "gist.github.com" {
            return None;
        }

        let mut path_segments = url.path().trim_matches('/').split('/');
        let first = path_segments.next()?;
        let (owner, repo) = match path_segments.next() {
            Some(repo) => (first, repo),
            None => ("", first),
        };
        let repo = repo.strip_suffix(".git").unwrap_or(repo);
        if path_segments.next().is_some()
            || repo.is_empty()
            || !repo.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return None;
        }

        Some(ParsedGitRemote {
            owner: owner.into(),
            repo: repo.into(),
        })
    }

    fn build_commit_permalink(
        &self,
        remote: &ParsedGitRemote,
        params: BuildCommitPermalinkParams,
    ) -> Url {
        // Gist IDs are globally unique, so ownerless clone URLs need no API lookup.
        let mut permalink = self.base_url();
        permalink.set_path(&format!("/{}/{}", remote.repo, params.sha));
        permalink
    }

    fn build_permalink(&self, remote: ParsedGitRemote, params: BuildPermalinkParams) -> Url {
        let mut permalink =
            self.build_commit_permalink(&remote, BuildCommitPermalinkParams { sha: params.sha });
        // BuildPermalinkParams escapes the path, but Gist anchors use the filename.
        let path = urlencoding::decode_binary(params.path.as_bytes());
        let filename = path
            .split(|byte| !byte.is_ascii_alphanumeric() && *byte != b'_')
            .filter(|part| !part.is_empty())
            .map(String::from_utf8_lossy)
            .join("-")
            .to_ascii_lowercase();
        let mut fragment = format!("file-{filename}");
        // Gist renders Markdown without source line anchors, even with plain=1.
        if let Some(selection) = params.selection
            && !params.path.to_ascii_lowercase().ends_with(".md")
        {
            fragment.push('-');
            fragment.push_str(&self.line_fragment(&selection));
        }
        permalink.set_fragment(Some(&fragment));
        permalink
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use git::{GitHostingProviderRegistry, parse_git_remote_url, repository::repo_path};
    use pretty_assertions::assert_eq;

    use super::*;

    const GIST_ID: &str = "4e7ee9d5d8aa2ebfb4b2eb8812c327e3";
    const SHA: &str = "8aa963a11042b169c50eeccc440b2f7ab289cbf6";

    #[test]
    fn test_parse_remote_url() {
        for (url, owner) in [
            (format!("https://gist.github.com/{GIST_ID}"), ""),
            (format!("https://gist.github.com/{GIST_ID}.git"), ""),
            (format!("https://gist.github.com/{GIST_ID}.git/"), ""),
            (format!("https://user@gist.github.com/{GIST_ID}.git"), ""),
            (format!("git@gist.github.com:{GIST_ID}.git"), ""),
            (format!("ssh://git@gist.github.com/{GIST_ID}.git"), ""),
            (format!("https://gist.github.com/owner/{GIST_ID}"), "owner"),
            (format!("git@gist.github.com:owner/{GIST_ID}.git"), "owner"),
        ] {
            let parsed = GithubGist
                .parse_remote_url(&url)
                .expect("valid Gist remote");
            assert_eq!(parsed.owner.as_ref(), owner, "{url}");
            assert_eq!(parsed.repo.as_ref(), GIST_ID, "{url}");
        }
        assert_eq!(
            GithubGist.parse_remote_url("https://gist.github.com/123456.git"),
            Some(ParsedGitRemote {
                owner: "".into(),
                repo: "123456".into()
            })
        );
    }

    #[test]
    fn test_reject_invalid_remote_urls() {
        for url in [
            "https://github.com/owner/repo.git",
            "https://gist.github.com.example.com/123456.git",
            "https://gist.github.com/",
            "https://gist.github.com/.git",
            "https://gist.github.com/owner/",
            "https://gist.github.com/owner/.git",
            "https://gist.github.com/owner/not-a-gist.git",
            "https://gist.github.com/owner/123456/extra",
            "not a URL",
        ] {
            assert!(GithubGist.parse_remote_url(url).is_none(), "{url}");
        }
    }

    #[test]
    fn test_build_permalinks_through_registry() {
        let registry = Arc::new(GitHostingProviderRegistry::new());
        registry.register_hosting_provider(Arc::new(crate::Github::public_instance()));
        registry.register_hosting_provider(Arc::new(GithubGist));

        for (path, selection, fragment) in [
            ("main.ts", None, "file-main-ts"),
            ("main.ts", Some(0..0), "file-main-ts-L1"),
            ("main.ts", Some(6..6), "file-main-ts-L7"),
            ("main.ts", Some(6..9), "file-main-ts-L7-L10"),
            ("README.md", None, "file-readme-md"),
            ("README.md", Some(6..9), "file-readme-md"),
            (".gitignore", None, "file-gitignore"),
            ("My file_name.test.ts", None, "file-my-file_name-test-ts"),
            ("100% done!.txt", None, "file-100-done-txt"),
            ("transcript-🎫・531.html", None, "file-transcript-531-html"),
        ] {
            let (provider, remote) = parse_git_remote_url(
                registry.clone(),
                &format!("https://gist.github.com/{GIST_ID}"),
            )
            .expect("Gist provider registered");
            let permalink = provider.build_permalink(
                remote,
                BuildPermalinkParams::new(SHA, &repo_path(path), selection),
            );
            assert_eq!(
                permalink.as_str(),
                format!("https://gist.github.com/{GIST_ID}/{SHA}#{fragment}")
            );
        }
    }

    #[test]
    fn test_build_commit_permalink() {
        for url in [
            format!("https://gist.github.com/{GIST_ID}.git"),
            format!("git@gist.github.com:owner/{GIST_ID}.git"),
        ] {
            let remote = GithubGist
                .parse_remote_url(&url)
                .expect("valid Gist remote");
            assert_eq!(
                GithubGist
                    .build_commit_permalink(&remote, BuildCommitPermalinkParams { sha: SHA })
                    .as_str(),
                format!("https://gist.github.com/{GIST_ID}/{SHA}")
            );
            assert!(
                GithubGist
                    .build_create_pull_request_url(&remote, "main")
                    .is_none()
            );
        }
    }
}
