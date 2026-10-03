---
name: upstream-sync-conflict
description: Resolves an "Upstream sync conflict" issue in kjanat/zed-editor by merging zed-industries/zed main into the fork by hand, keeping the fork's behavior, validating the result, and opening the sync PR that closes the issue. Use this whenever an issue is labeled `upstream-sync-conflict`, the user mentions the upstream sync, `sync/upstream`, the `fork_upstream_sync` workflow, merging upstream Zed into the fork, or pastes `gh issue list --label upstream-sync-conflict` output, even when they only say "tackle the sync issue".
---

# Upstream sync conflict

The `fork_upstream_sync` workflow runs `script/upstream-sync/prepare.sh` twice a day to merge upstream `main` into `master`. It resolves conflicts that come from formatting and conflicts that merge cleanly once both sides are formatted the same way. When a file still conflicts, it aborts, and `open-sync-pr.py` opens an issue titled `Upstream sync conflict` and labeled `upstream-sync-conflict`, or comments on the one already open. This skill does that merge by hand and ships it as a pull request.

The result is one merge commit whose second parent is upstream `main`, and the PR is merged into `master` with a merge commit. Squashing or rebasing drops upstream ancestry, and the next sync then conflicts on the same files again.

## 1. Read the issue

```sh
GH_PAGER=cat gh issue list -R kjanat/zed-editor --label upstream-sync-conflict --state open
GH_PAGER=cat gh issue view <N> -R kjanat/zed-editor --comments
```

Read the body and every comment. Each scheduled run comments on the open issue, so the newest comment has the most recent conflict list. Collect:

- the files under **Needs a human**, and the fork and upstream commits listed for each file
- the **Upstream automation dropped** list, which goes into the PR body
- any note that the report was truncated

Upstream has usually moved on since the report was written, and you merge the current `upstream/main`. Use the issue to understand intent. Take the conflict list from your own merge.

The PR closes every open issue labeled `upstream-sync-conflict`.

## 2. Set up

The `fork_upstream_sync` workflow owns `sync/upstream` while it has an open PR. GitHub deletes the branch after each merge, so it normally does not exist. Check both before creating it:

```sh
git ls-remote origin refs/heads/sync/upstream
GH_PAGER=cat gh pr list -R kjanat/zed-editor --head sync/upstream --state open
```

If either returns something, stop and ask the user how to proceed.

```sh
git remote get-url upstream
```

If that fails, add the remote with `git remote add upstream https://github.com/zed-industries/zed.git`. Then fetch:

```sh
git fetch origin master --no-tags
git fetch upstream main --no-tags
```

Work in the main checkout. Cargo's build directory is keyed on the workspace path, so a second worktree rebuilds all of Zed from nothing. `git status --short` must print nothing. Stop and ask the user if it prints anything.

A local `sync/upstream` left from an earlier sync can be reset when `origin/master` already contains it:

```sh
git merge-base --is-ancestor sync/upstream origin/master
git switch -C sync/upstream origin/master
```

Run the second command only when the first exits 0, or when the branch does not exist. Otherwise stop and ask.

Use `origin/master` as the fork side everywhere. The local `master` can be behind. The issue's commands say `master` because the `prepare` job's clone has only that one branch.

## 3. Merge and apply the automatic resolutions

```sh
git merge --no-ff --no-commit upstream/main
bash .agents/skills/upstream-sync-conflict/scripts/auto_resolve.sh
```

`scripts/auto_resolve.sh` applies `prepare.sh`'s resolutions to the merge in progress:

- `.github` is restored from `origin/master`, and upstream's `.github` changes are listed as dropped
- `.rules` conflicts keep the fork's version, including a deletion on the fork side
- a file the fork only reformatted takes upstream's version, reformatted
- other files are merged three-way after formatting all three versions the same way
- `Cargo.lock` gets the fork's version, and `cargo metadata` reconciles it once nothing else conflicts
- the files that still conflict are printed under **Needs a human**

Use `scripts/auto_resolve.sh` instead of the `### Resolve` block in the issue. That block runs `git checkout` on files, which is not allowed here, and it resolves against the local `master`.

Check every automatic resolution with `git diff --cached origin/master -- <file>`. Compare it with `git diff "$(git merge-base origin/master upstream/main)" upstream/main -- <file>`. The two diffs should contain the same changes. The issue labels these files "verify, do not resolve" because a formatting merge can drop a line without any conflict.

## 4. Resolve the rest by hand

For each file under **Needs a human**:

1. Find out why each side changed it:

   ```sh
   BASE="$(git merge-base origin/master upstream/main)"
   git log --oneline "$BASE"..origin/master -- <file>
   git log --oneline origin/master..upstream/main -- <file>
   git show <sha> -- <file>
   ```

   Fork commit subjects end in a PR number. Read that PR when the intent is not clear from the diff.
2. Write a result that keeps the fork's behavior and also adopts upstream's change. Taking one side as a whole is only correct when the other side's change already exists in another form. Say so in the PR body when you do it.
3. Edit the conflicted file with the Edit tool, then `git add <file>`.

These patterns came up in earlier syncs:

- **Imports.** Combine both sides' `use` lists. The compiler reports anything neither side uses.
- **Tests.** When both sides appended tests at the same place, keep both sets.
- **`crates/proto/proto/zed.proto` message numbers.** Both sides take the next free number in `Envelope`. Upstream keeps its numbers. Move the fork's messages above upstream's highest number and move the `// current max` comment to the new highest. Upstream keeps allocating numbers, so the fork is the side that moves every time.
- **Keymaps** (`assets/keymaps/default-*.json`). Keep the fork's bindings and add upstream's new bindings to every platform file.
- **Docs.** Take upstream's content and keep the fork's formatting.
- **`.zed/settings.json`.** Keep the fork's version unchanged.
- **Upstream API changes.** These can break fork-only code in files that merged without conflicts. The compile step in section 5 finds them. Fix them in the merge commit.

Anything beyond adapting to upstream, such as a fork bug the merge exposes, goes in a separate commit after the merge commit.

After you stage the hand resolutions, run `scripts/auto_resolve.sh` again. It leaves the hand-resolved files alone and reconciles `Cargo.lock` now that nothing else conflicts:

```sh
bash .agents/skills/upstream-sync-conflict/scripts/auto_resolve.sh
```

Review `git diff --cached origin/master -- Cargo.lock`. Every version change in it should also appear in `git diff "$BASE" upstream/main -- Cargo.lock`, or follow from a manifest change on the fork side. An unrelated upgrade means cargo resolved something fresh. Find out which manifest caused it before continuing.

## 5. Validate

Run each check as its own command and read the complete output.

```sh
git diff --name-only --diff-filter=U
git grep -n -e '^<<<<<<< ' -e '^>>>>>>> '
git diff --cached --check
dprint check
```

Compile the whole workspace, tests included. Upstream renames and moves break fork-only code in files that merged without a conflict, such as a fork test calling a function upstream moved to another crate:

```sh
cargo check --locked --workspace --all-targets --keep-going
```

`--keep-going` reports every crate that fails. Without it cargo stops at the first one. Then lint every crate that contains a hand-resolved file:

```sh
./script/clippy --no-deps -p <crate> -p <crate>
```

`./script/clippy` ends with `cargo shear`, which reports the unused `title_bar` self-dependency that already exists on `master`.

When a conflict was in test code, run those tests by name:

```sh
cargo nextest run -p project -E 'test(test_client_clipboard_)'
```

Record each check and its result for the PR body, including any check you did not run.

## 6. Commit

```sh
git rev-list --count origin/master..upstream/main
git rev-parse upstream/main
```

Write the message to a file in the scratchpad and run `git commit -F <file>`:

```text
Merge upstream and <what the resolution kept or changed>

Merge all <count> upstream commits through <upstream sha>. <One or
two sentences on which fork behavior was kept and which upstream
change was adopted in the conflicted areas.>

Closes #<N>.
```

Examples of earlier subjects: `Merge upstream and preserve watcher recovery`, `Merge upstream while preserving fork IDE settings`, `Merge upstream and resolve sync conflicts`.

Files that upstream added arrive in upstream's formatting. When `dprint check` reports them after the merge commit, run `dprint fmt` on those files and commit them separately as `Apply this fork's formatting to the upstream merge`, as the sync automation does.

## 7. Push and open the PR

```sh
git push -u origin sync/upstream
```

```sh
gh pr create -R kjanat/zed-editor --base master --head sync/upstream \
  --title "<commit subject>" --body-file <file> \
  --label upstream-sync-conflict --label build --assignee kjanat
```

Also add `bug`, `documentation`, or `testing` when the resolution changed code, docs, or tests in that area. Body:

```markdown
Resolve the upstream sync blocked in #<N> and merge all <count> upstream commits through <upstream sha>.

<For each hand-resolved file or group of files: how it was resolved and which fork behavior it keeps.>

Upstream automation dropped (port by hand if wanted):

- `<path>`

Validation: <each check and its result. List the skipped checks too.>

Merge with a merge commit to keep upstream ancestry.

Closes #<N>.

Release Notes:

- <user-visible upstream changes, or N/A>
```

Leave out the dropped-automation list when it is empty. Write one `Closes #<N>.` line per open sync-conflict issue. Wrap any `@scope` package name in backticks so GitHub does not turn it into a mention. When you found a non-obvious pattern worth adding to `.rules`, add a `Suggested .rules additions` section, as the repo's `CLAUDE.md` asks.

Do not watch CI after opening the PR.

## 8. Report and clean up

Give the user the PR URL, one line per hand-resolved file, and the validation results, including every skipped check.

## While the PR is open

- The next scheduled `fork_upstream_sync` run merges against `master`, hits the same conflicts, and comments on the issue again. Read new comments for files your merge does not cover.
- If upstream moves on and you need its new commits, merge `upstream/main` into `sync/upstream` again. Do not rebase.
- `open-sync-pr.py` opens an `Upstream sync needs attention` issue when the open sync PR is blocked or its checks fail.
