# Upstream sync trust boundary

`fork_upstream_sync` prepares a merge from its run's `master` commit. A disposable Docker container receives that checkout read-only and an
empty output directory. It receives no runner environment, home directory,
cache, or Docker socket. It runs as the runner's numeric user with all Linux
capabilities dropped. Network access remains available for public upstream and
formatter/toolchain downloads.

Upstream-controlled formatters, Cargo configuration, and executables may run
inside that container. Treat everything it produces as untrusted. The
`Prepare the candidate` step disables runner command processing while it logs
the container's output. After the container exits, the `Export bounded output`
step copies only bounded regular files; symlinks and special files must never
reach `actions/upload-artifact`.

`open-sync-pr.py` runs on a separate runner and checks out only the trusted workflow
SHA. It imports the bundle as Git objects, verifies descent from that SHA, and
refuses any candidate that changes `.github`, including local actions, workflow
additions, deletions, and symlinks. It never checks out, builds, formats,
imports Python from, or executes scripts from the candidate. A `master` that
moved during the run still gets the PR as long as it contains the run's commit;
a rewritten `master` or a sync-branch race stops the push. When `sync/upstream`
has commits that `github-actions[bot]` did not make, `open-sync-pr.py` leaves
them alone and opens the PR from `sync/upstream-<commit>` instead.

The fork keeps its own automation: the merge restores `.github` from `master`,
so upstream changes there, including new workflows and edits to workflows the
fork removed, are dropped and listed in the sync PR instead of stopping the
sync. Adopting one means porting it by hand.

Conflicts in the root `.rules` file are resolved with the fork's `master`
version, including keeping a fork-side deletion. They do not block the sync.
Upstream changes that merge cleanly are retained.

## Deterministic merge stages

`prepare.sh` resolves, in order:

1. `.github` and `.rules` from `master`.
2. Files the fork only reformatted, by taking upstream's version and formatting
   it.
3. A three-way merge of all three versions formatted the same way. Markdown is
   wrapped at 80 columns for this merge so upstream paragraph edits survive the
   fork's wrapping.
4. A file deleted in the fork stays deleted when upstream changes it.
5. `Cargo.lock` keeps the fork's version, and `cargo metadata` reconciles it.
6. mergiraf 0.20.0 merges the remaining files by syntax tree.
   `script/upstream-sync/Dockerfile` pins it by checksum. A file counts as resolved only when mergiraf exits 0 and
   `dprint` formats and checks it; every step is chained, so a failure leaves
   the file conflicted.

When nothing conflicts afterwards, the result is `clean` or `resolved` and goes
to `open-sync-pr.py`. Otherwise the result is `partial`: the merge is committed
with its remaining conflict markers into `sync.bundle`, next to `human.txt` and
`conflict-report.md`. A partial merge is input for the resolver and never
becomes the sync PR.

## Claude resolver

A `partial` result runs two more jobs, both with `contents: read`. The `prepare`
job skips both while a PR from a `sync/upstream*` branch is open, and while the
open `upstream-sync-conflict` issue records a Claude attempt on the same
`master` commit. Every failure report carries that record. Closing the issue
lets the next run try again.

The `resolve` job gives Claude the fork checkout, the prepare output in
`$RUNNER_TEMP/sync-candidate`, and the toolchain from `jdx/mise-action`. Its
prompt, `resolve-prompt.md`, says where to look and how to judge the merge. It
names no conflicts. Claude runs the steps itself:

- `resolve.sh setup <prepare-output>` turns the checkout into the recorded
  merge. HEAD is the merge's first parent, which must equal the workflow's
  `master` SHA, and MERGE_HEAD is its second parent. `setup` fetches nothing
  from upstream again. The remaining paths get their unmerged index stages back, so
  the merge looks the same as an interrupted `git merge`.
- `resolve.sh check` runs the validation in place and prints it as it runs: no
  unmerged paths, no conflict markers, `.github`, `script/upstream-sync` and
  `script/clippy` unchanged, `git diff --cached --check`, `dprint check`,
  `cargo check --locked --workspace --all-targets --keep-going`, then
  `script/clippy --no-deps` and `cargo nextest run` for the crates of the
  conflicted files, and every test in Claude's `.sync/tests.txt`. `check`
  writes its logs to `.sync/`, a gitignored directory at the repository root.

Both jobs run their own `resolve.sh` from `.sync-scripts`, a second, sparse
checkout of the workflow SHA with only `script/upstream-sync` and
`script/clippy`. After Claude, `resolve.sh export` commits the index with the
fork and upstream commits as parents and uploads the bundle with Claude's
`resolution.md` and `tests.txt`. If Claude committed its merge, `export` uses
that commit's tree. When no finished merge exists, `export` saves the whole
working tree, conflict markers and new files included, as `unfinished.bundle`.

Before Claude starts, the `resolve` job runs the export command once, so a
broken command fails the job before Claude's run. Right after Claude, a step
that does not use `resolve.sh` writes the whole checkout as `checkout.patch`
and copies `.sync/` as `state/`, Claude's transcript as
`claude-execution-output.json` and its session files as `claude-projects/`.
That step replaces the token with `***` in every file before the upload, and
the Claude step writes its full transcript into the job log. The `verify` job
uploads its own `.sync/` as `upstream-sync-verify-logs`.

The `verify` job checks the export on a clean runner. `resolve.sh verify`
requires the exported merge's parents to be exactly the prepared fork and
upstream commits and rejects any change to `.github`, `script/upstream-sync` or
`script/clippy` before it checks out the merge's tree. Then it runs the same
checks. On success it commits that tree as `github-actions[bot]`
and writes result `resolved` with Claude's notes. The notes end with a list of
Claude's changes outside the conflicted paths. Otherwise it writes result `conflict` with a
bounded `issue-body.md`: the conflict report, the failed check with the last 80
lines of its output, and Claude's notes. `open-sync-pr.py` then opens or updates
the `Upstream sync conflict` issue. The issue links the run and names the bundle
in its `upstream-sync-resolution` artifact. When the `verify` job itself fails
or times out, `open-sync-pr.py` still opens or updates that issue, with the
conflict report and a link to the run. Only the bundle and these bounded files
reach it, through the same exporter as the prepare step.

The Claude step stops after 280 of the job's 300 minutes, so the export and
upload always run. GitHub keeps the candidate, resolution and verified
artifacts for 30 days. A failed `open-sync-pr` job therefore loses nothing: a
re-run of that job pushes the same verified merge and opens the PR.

`open-sync-pr.py` checks for an open PR from the branch right after the push,
then edits that PR or creates one. The workflow's concurrency group runs one
sync at a time, so no second PR can appear in between.

New or unassigned sync PRs and issues are assigned to `kjanat`. Existing
assignees are preserved.

Creating or updating the `sync/upstream` PR can enable auto-merge with a merge
commit, preserving upstream history. The request must match the validated and
pushed head commit. GitHub waits for the existing required checks and branch
rules before merging. `open-sync-pr.py` keeps an existing auto-merge request
for a merge commit and does not enable it again. It tries to enable auto-merge
three times. If all three fail, the run fails, and you merge the PR by hand.
Read-only `gh` and `git` calls and `gh pr edit` get three attempts too.

The container assumes the GitHub-hosted runner, Docker/kernel, pinned tool image,
and trusted workflow revision are not compromised. Do not mount host caches
into the container to speed it up.

The `upstream-sync-tools` image preloads the root `.dprint.jsonc`, its plugins, and the Tombi
and rustfmt setup commands. It includes `tombi.toml`, `rustfmt.toml`,
`.cargo/config.toml`, and `rust-toolchain.toml` while warming the cache at
`/tmp/work`, the same path used during preparation. The Rust toolchain has its
own layer so formatter configuration changes do not reinstall it.

The exec plugin runs setup commands once per process. The root Tombi setup
checks that the executable works before installing it; rustup already reuses an
installed rustfmt component. Repeated dprint invocations reuse those tools.

`DPRINT_CACHE_DIR=/opt/dprint-cache` and the installed tools are writable by the
runner's numeric user inside the container. Buildx caches the image layers;
runtime writes disappear with the container and are never exported to a host
cache. Formatting uses the candidate checkout's root configuration, so changed
plugins or setup keys can still trigger downloads in the isolated container.

## Tests

```sh
mise run test-upstream-sync
```

The task builds the container image and runs every test. The container
regressions run an upstream-supplied formatter and verify that the root JSON,
TOML, and Rust formatters work without network access as the runner user.
`test_resolve.py` builds fixture repositories with a small cargo workspace and a
stub agent, and checks:

- the exact recorded SHAs and the refusal of a candidate for another fork
  commit;
- that `check` runs in place and names the failing check;
- that leftover markers, compile errors, breakage in code that merged without a
  conflict, failing tests, automation changes, a missing resolution and a
  resolution with other parents all end as `conflict`;
- that a verified resolution has the fork and upstream as parents and lists the
  files it changed outside the conflicts.

Other tests exercise bundle validation, `open-sync-pr.py` and hostile artifact
file types without contacting GitHub or pushing anything.
