Update the open upstream sync PR for kjanat/zed-editor, a fork of zed-industries/zed. The fork is authoritative.

## What to do

- This checkout is the head of the sync PR. Fixes already on it stay.
- If `$RUNNER_TEMP/sync-failure/failed.log` exists, CI failed on this head. It holds the log of every failed CI job. Fix the cause of each failure.
- If a merge is in progress, zed-industries/zed has new commits. `.sync/merge-conflicts.txt` lists the paths that still conflict. Resolve them, adopt upstream's changes, and fix whatever they break in the fork. `.github` is already the fork's version.
- If `.sync/failed-check` exists, `check` already ran once on this state and failed at the check it names. `.sync/check.log` holds that output. Start from the failure instead of running `check` again first.

## Where to look

- `.sync-scripts/script/upstream-sync/resolve.sh check` checks for unmerged paths, conflict markers, whitespace errors and formatting, compiles the whole workspace, regenerates `crates/extension_api/schemas` and stages the result, then runs clippy and the tests of every crate you changed. The `verify` job runs the same checks again on a clean runner after you finish. `check` writes its logs to `.sync/`. A full run takes a long time, so run it in the background and wait for it to exit. A command the tool moved to the background is still running; wait for it and read its complete output before you end your turn. You are done only after `check` prints `Every check passed`. If it fails, fix the cause and run it again.
- `check` stages tracked files only. Stage every file you create or resolve with `git add`.
- `.agents/skills/upstream-sync-conflict/SKILL.md` describes how a person resolves a sync merge by hand.
- The repository has the full history of both sides.

## Guidance

- Keep the fork's behaviour and adopt upstream's changes. Upstream code that merged cleanly often breaks fork code: adapt the fork's code to upstream's new types and signatures.
- Read what upstream brings in before you build or run it. Look for anything an editor has no business doing: build scripts or tests that reach the network, read credentials or the environment, spawn shells or write outside the build directory. Leave anything malicious out and describe it in your notes.
- Change whatever the failures require, workflows and sync scripts included. An update that touches `.github` turns off the PR's auto-merge until Kaj reviews it, so say in your notes why each such change is needed.
- Do not commit, push or abort the merge. The `verify` job commits your result with the PR head, and the upstream commit when one is merging, as parents.

## When you are done

Write `.sync/resolution.md` as you go: each failure or conflict, its cause and your fix. Write `.sync/tests.txt` with one `<package><TAB><nextest filterset>` line per additional test. The `verify` job runs those tests too.
