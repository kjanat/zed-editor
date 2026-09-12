# Upstream sync trust boundary

The scheduled workflow prepares a merge using the workflow run's `master`
commit. A disposable Docker container receives that checkout read-only and an
empty output directory. It receives no runner environment, credentials, home
directory, cache, or Docker socket. It runs as the runner's numeric user with
all Linux capabilities dropped. Network access remains available for public
upstream and formatter/toolchain downloads.

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

Changes to `.github` require manual review and integration; the automatic sync
opens an issue instead of publishing them to a same-repository PR.

New or unassigned sync PRs and issues are assigned to `kjanat`. Existing
assignees are preserved.

With `SYNC_TOKEN` configured, creating or updating the `sync/upstream` PR
enables auto-merge with a merge commit, preserving upstream history. The request
must match the validated and published head commit. GitHub waits for the
existing required checks and branch rules before merging. An existing auto-merge
request using a merge commit is kept without enabling it again. If enabling
auto-merge fails, the workflow fails so the PR can be handled manually.

`SYNC_TOKEN` is optional and referenced only in the publisher. Use a
fine-grained PAT or GitHub App token limited to this repository with the
required contents, pull requests, and issues permissions. Without it,
publication still uses `GITHUB_TOKEN`, but skips auto-merge; the required checks
and merge need manual action. Token scopes and repository settings are
unchanged.

The sandbox assumes the GitHub-hosted runner, Docker/kernel, pinned tool image,
and trusted workflow revision are not compromised. Do not pass Actions runtime
credentials or mount host caches into the container to speed it up.

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

Run the security tests with:

```sh
docker build -f script/upstream-sync/Dockerfile -t upstream-sync-test .
SYNC_TEST_IMAGE=upstream-sync-test python3 -m unittest discover -s script/upstream-sync -p 'test_*.py'
```

The container regressions run an upstream-supplied formatter against synthetic
token canaries and verify that the root JSON, TOML, and Rust formatters work
without network access as the runner user. Other tests exercise bundle validation
and hostile artifact file types without contacting GitHub or publishing anything.
