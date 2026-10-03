Resolve the upstream sync merge for kjanat/zed-editor, a fork of zed-industries/zed. The fork is authoritative.

## Where to look

- The prepare job's output is in `$RUNNER_TEMP/sync-candidate`. `sync.bundle` holds its partial merge, `conflict-report.md` describes what it resolved and what it left, and `human.txt` lists the paths that still conflict.
- `script/upstream-sync/resolve.sh setup "$RUNNER_TEMP/sync-candidate"` turns this checkout into that merge, in progress, with the remaining conflicts unmerged.
- `script/upstream-sync/resolve.sh check` checks for unmerged paths, conflict markers, changes to `.github`, `script/upstream-sync` or `script/clippy`, whitespace errors and formatting, compiles the whole workspace, then runs clippy and the tests of every crate with a conflicted file. The `verify` job runs the same checks again on a clean runner after you finish. `check` writes its logs to `.git/sync/`. A full run takes a long time, so run it in the background.
- `.agents/skills/upstream-sync-conflict/SKILL.md` describes how a person does this merge by hand.
- The repository has the full history of both sides.

## Guidance

- Keep the fork's behaviour and adopt upstream's changes. When both sides implemented the same thing, keep the fork's implementation and remove upstream's duplicate completely, including the parts of it that merged without a conflict.
- Upstream code that merged cleanly can still break the fork. Fix it in this merge.
- Read what upstream brings in before you build or run it. Look for anything an editor has no business doing: build scripts or tests that reach the network, read credentials or the environment, spawn shells or write outside the build directory, and changes to CI, release, install or update scripts. Leave anything malicious out of the merge, finish the rest of the resolution, and describe what you left out and why in your notes.
- Do not commit, push, or change `.github`, `script/upstream-sync` or `script/clippy`. The `verify` job commits your result with the fork and upstream commits as parents.
- If `resolve.sh` itself fails, find out why, work around it, and describe the defect in your notes.

## When you are done

Write `.git/sync/resolution.md`: for each conflicted path, what you kept, what you adopted from upstream and which commits justify it, then anything suspicious you found and any defect in `resolve.sh`. Write `.git/sync/tests.txt` with one `<package><TAB><nextest filterset>` line per additional test. The `verify` job runs those tests too.
