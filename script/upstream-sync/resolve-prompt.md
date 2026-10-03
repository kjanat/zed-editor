You are resolving the upstream sync merge for kjanat/zed-editor, a fork of zed-industries/zed. The fork is authoritative.

The merge is already in progress in `${candidate}`:

- fork commit (HEAD, first parent): `${fork}`
- upstream commit (MERGE_HEAD, second parent): `${upstream}`
- merge base: `${base}`

Do not fetch, merge, rebase, reset, commit or push. Resolve this exact merge in place. The automation commits it with these two parents after it validates your result.

The deterministic stages already ran: `.github` and `.rules` kept the fork's version, formatting-only and formatted three-way conflicts were resolved, mergiraf resolved what a syntax-aware merge could, and fork-side deletions were kept. Those results are already in the index. These paths still conflict and are unmerged in the index:

```text
${conflicts}
```

Follow sections 4 and 5 of `${root}/.agents/skills/upstream-sync-conflict/SKILL.md`, the procedure a person uses for this merge. Read it first. In short:

1. For every conflicted path, find out why each side changed it: `git log` between the merge base and each parent, `git show` of each commit, and the PR it names when the intent is unclear. The repository has full history.
2. Keep the fork's behaviour and adopt upstream's change. When both sides implemented the same thing, keep the fork's implementation and remove upstream's duplicate completely, including any part of it that merged without a conflict. Leave no dead code behind.
3. Resolve each file with the Edit tool, then `git add` it. Also `git add` any file you create.
4. Upstream changes that merged without a conflict can still break the fork, for example code that uses a field the fork moved. The compiler finds them. Fix them in this merge.
5. Iterate the way a developer does: edit, compile, read the errors, fix, test, lint. Use narrow checks while iterating, then finish with the full validation from section 5 of the skill:
   - `git diff --name-only --diff-filter=U` prints nothing
   - `git grep -n -E '^(<<<<<<<|>>>>>>>)( |$$)'` finds nothing
   - `git diff --cached --check`
   - `dprint check`
   - `cargo check --locked --workspace --all-targets --keep-going`
   - `./script/clippy --no-deps -p <crate>` for every crate holding a file you resolved
   - `cargo nextest run -p <crate>` for those crates, and every test that covers behaviour a conflict touched. When both sides fixed the same bug, run upstream's regression test for it against your result.

Every Bash command runs in a sandbox that starts in `${candidate}`, with the repository's mise toolchain and no access to your home directory. Each command starts in a fresh shell, so `cd` does not carry over between commands.

When you are done, write two files:

- `${out}/resolution.md`: for each conflicted path, one short paragraph saying what you kept, what you adopted from upstream, and which commits justify it. Then list every other file you changed and why, and the checks you ran with their results.
- `${out}/tests.txt`: one line per targeted test run that the automation must repeat, as `<package><TAB><nextest filterset>`, for example `worktree	test(test_new_directory_scan_does_not_miss_event_before_adding_watcher)`.

The automation runs the full validation again on its own and publishes only if every check passes. If you cannot produce a coherent merge, say why in `${out}/resolution.md` and stop. Do not leave conflict markers in place to make a check pass, and do not change `.github` or `script/upstream-sync`.

The prepare step's conflict report follows.

${report}
