import unittest

from linkify import (
    Commit,
    Kind,
    Number,
    Repositories,
    Repository,
    Resolved,
    UnresolvedReferences,
    linkify,
)

FORK = Repository("kjanat", "zed-editor")
ZED = Repository("zed-industries", "zed")
PULLS = {Number(ZED, 62818), Number(FORK, 9), Number(FORK, 167)}
ISSUES = {Number(ZED, 63177), Number(FORK, 33)}
COMMITS = {
    Commit(FORK, "cf515b9860"),
    Commit(ZED, "399258feea"),
    Commit(ZED, "40180d9c40"),
    Commit(FORK, "40180d9c40"),
}


def resolve(numbers: set[Number], commits: set[Commit]) -> Resolved:
    kinds: dict[Number, Kind] = {number: "pull" for number in numbers & PULLS}
    kinds |= {number: "issues" for number in numbers & ISSUES}
    return Resolved(kinds, commits & COMMITS)


def run(text: str) -> str:
    return linkify(text, Repositories(FORK, ZED), resolve)


class LinkifyTests(unittest.TestCase):
    def test_upstream_pull_request(self):
        self.assertEqual(
            run("prompt skips it (zed-industries/zed#62818)."),
            "prompt skips it ([zed-industries/zed#62818](https://github.com/zed-industries/zed/pull/62818)).",
        )

    def test_upstream_issue(self):
        self.assertEqual(
            run("fixes zed-industries/zed#63177"),
            "fixes [zed-industries/zed#63177](https://github.com/zed-industries/zed/issues/63177)",
        )

    def test_fork_number_names_the_fork(self):
        self.assertEqual(
            run("(#9, closes #33)"),
            "([kjanat/zed-editor#9](https://github.com/kjanat/zed-editor/pull/9), "
            "closes [kjanat/zed-editor#33](https://github.com/kjanat/zed-editor/issues/33))",
        )

    def test_named_commit(self):
        self.assertEqual(
            run("as of 2026-08-29 (zed-industries/zed@399258feea)."),
            'as of 2026-08-29 (<a href="https://github.com/zed-industries/zed/commit/399258feea">'
            "zed-industries@<tt>399258feea</tt></a>).",
        )

    def test_bare_fork_commit(self):
        self.assertEqual(
            run("(cf515b9860, #167)"),
            '(<a href="https://github.com/kjanat/zed-editor/commit/cf515b9860">kjanat@<tt>cf515b9860</tt></a>, '
            "[kjanat/zed-editor#167](https://github.com/kjanat/zed-editor/pull/167))",
        )

    def test_bare_upstream_commit_links_to_the_parent(self):
        self.assertEqual(
            run("merges upstream through 40180d9c40 by hand"),
            'merges upstream through <a href="https://github.com/zed-industries/zed/commit/40180d9c40">'
            "zed-industries@<tt>40180d9c40</tt></a> by hand",
        )

    def test_unknown_hex_word_stays(self):
        self.assertEqual(
            run("mode 0600 and deadbeef12 stay"), "mode 0600 and deadbeef12 stay"
        )

    def test_protected_text_stays(self):
        text = (
            "`#9` and [#33](https://example.com/#9) and https://github.com/kjanat/zed-editor/compare/v1.0.0...dev-7ba3387\n"
            "```sh\ngit show cf515b9860 # zed-industries/zed#62818\n```\n"
            "dev-a74d2b1 and ## headings"
        )
        self.assertEqual(run(text), text)

    def test_running_twice_changes_nothing(self):
        once = run(
            "(#9) zed-industries/zed#62818 zed-industries/zed@399258feea cf515b9860"
        )
        self.assertEqual(run(once), once)

    def test_unresolved_numbers_and_named_commits_fail(self):
        with self.assertRaises(UnresolvedReferences) as raised:
            run("#404 and zed-industries/zed@abcdef1234")
        self.assertEqual(
            str(raised.exception),
            "kjanat/zed-editor#404, zed-industries/zed@abcdef1234",
        )


if __name__ == "__main__":
    unittest.main()
