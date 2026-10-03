# Upstream sync trust boundary

The scheduled workflow prepares a merge using the workflow run's `master`
commit. A disposable Docker container receives that checkout read-only and an
empty output directory. It receives no runner environment, home directory,
cache, or Docker socket. It runs as the runner's numeric user with all Linux
capabilities dropped. Network access remains available for public upstream and
formatter/toolchain downloads.

Upstream-controlled formatters, Cargo configuration, and executables may run
inside that container. Treat everything it produces as untrusted. Runner command
processing is disabled while its output is logged. After it exits, the inline
Perl step copies only bounded regular files; symlinks and special files must
never reach the artifact uploader.

The publisher runs on a separate runner and checks out only the trusted workflow
SHA. It imports the bundle as Git objects, verifies descent from that SHA, and
refuses any candidate that changes `.github`, including local actions, workflow
additions, deletions, and symlinks. It never checks out, builds, formats,
imports Python from, or executes scripts from the candidate. A changed `master`
or sync-branch race stops publication. Local commits on the existing sync branch
also stop its replacement.

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
5. `Cargo.lock` keeps the fork's version and is reconciled by `cargo metadata`.
6. mergiraf 0.20.0, pinned by checksum in the image, merges the remaining files
   by syntax tree. A file counts as resolved only when mergiraf exits 0 and
   `dprint` formats and checks it; every step is chained, so a failure leaves
   the file conflicted.

When nothing conflicts afterwards, the result is `clean` or `resolved` and goes
to the publisher. Otherwise the result is `partial`: the merge is committed with
its remaining conflict markers into `sync.bundle`, next to `human.txt` and
`conflict-report.md`. A partial merge is input for the resolver and is never
published.

## Claude resolver

The `resolve` job runs only for a `partial` result and has `contents: read`.

`resolve.sh setup` clones the trusted checkout into `/mnt/sync/candidate`,
imports the bundle, and rebuilds the merge for the exact recorded commits: HEAD
is the merge's first parent, which must equal the workflow's `master` SHA, and
MERGE_HEAD is its second parent. Nothing is fetched from upstream again. The
remaining paths get their unmerged index stages back, so the merge looks the
same as an interrupted `git merge`.

mise provisions the toolchain from the trusted checkout: `jdx/mise-action`
installs the bootstrap packages, including bubblewrap, socat and ripgrep, and
`resolve.sh setup` installs every tool, lazy ones included. The sandbox gets the
`mise env` of that configuration.

Candidate code runs only in `sandbox-exec`: `env -i` with an allowlist of
toolchain variables, then Anthropic's sandbox runtime (`srt`, bubblewrap). The
sandbox has its own PID namespace and `/proc`, `no_new_privs`, a seccomp filter
that blocks Unix sockets such as the Docker socket, no read access to the
runner's home (and so to the trusted checkout and runner temp files), writes
limited to the candidate and its caches under `/mnt/sync`, and network access
limited to crates.io, GitHub, and the dprint plugin hosts. Inside it is a full
shell with cargo, nextest, dprint, git and the repository's scripts.

Claude runs from the trusted checkout with `--setting-sources user`, so the
candidate's `CLAUDE.md`, `.claude/`, `.mcp.json`, skills and hooks are never
loaded; the candidate is only an `--add-dir`. Its settings come from
`resolve.sh`: a `PreToolUse` hook rewrites every Bash command into
`sandbox-exec`, and permission rules deny edits to the trusted checkout,
`~/.claude` and the resolver's inputs. The prompt is
`script/upstream-sync/resolve-prompt.md`, filled with the three SHAs, the
remaining paths and the conflict report. It points Claude at sections 4 and 5
of `.agents/skills/upstream-sync-conflict/SKILL.md`.

`resolve.sh validate` repeats the skill's validation itself, every command in
the sandbox: no unmerged paths, no conflict markers, `.github` and
`script/upstream-sync` unchanged, `git diff --cached --check`, `dprint check`,
`cargo check --locked --workspace --all-targets --keep-going`,
`script/clippy --no-deps` and `cargo nextest run` for the crates of the
resolved files, and the targeted tests Claude lists in `tests.txt`. It records
the validated index tree.

`resolve.sh package` commits that exact tree as `github-actions[bot]` with the
fork and upstream commits as parents and bundles it as result `resolved`, with
Claude's `resolution.md`. If Claude fails, any check fails, or the tree changed
after validation, it writes result `conflict` with a bounded `issue-body.md`:
the conflict report, the failed check and the last 80 lines of its output. The
publisher then opens or updates the `Upstream sync conflict` issue. Only the
bundle and these bounded files cross into the publisher, through the same
exporter as the prepare step.

New or unassigned sync PRs and issues are assigned to `kjanat`. Existing
assignees are preserved.

Creating or updating the `sync/upstream` PR can enable auto-merge with a merge
commit, preserving upstream history. The request must match the validated and
published head commit. GitHub waits for the existing required checks and branch
rules before merging. An existing auto-merge request using a merge commit is
kept without enabling it again. If enabling auto-merge fails, the workflow fails
so the PR can be handled manually.

The sandbox assumes the GitHub-hosted runner, Docker/kernel, pinned tool image,
and trusted workflow revision are not compromised. Do not mount host caches
into the container to speed it up.

The tool image preloads the root `.dprint.jsonc`, its plugins, and the Tombi
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

- the exact recorded SHAs, merge parents, and the refusal of a candidate for
  another fork commit;
- that leftover markers, compile errors, breakage in code that merged without a
  conflict, failing tests and `.github` changes all end as `conflict`;
- that the runner's environment and other processes' `/proc/*/environ` never
  reach a sandboxed command;
- that the home directory, the trusted checkout and Unix sockets are
  unreachable from the sandbox;
- that the hook routes Bash into the sandbox.

On Linux the sandbox tests need bubblewrap, socat and ripgrep, and Ubuntu 24.04
needs `kernel.apparmor_restrict_unprivileged_userns=0`. Other tests exercise
bundle validation, the publisher and hostile artifact file types without
contacting GitHub or publishing anything.
