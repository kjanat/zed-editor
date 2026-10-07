#!/usr/bin/env bash
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(git rev-parse --show-toplevel)"
git_dir="$(git -C "${root}" rev-parse --absolute-git-dir)"
state="${root}/.sync"
marker_pattern='^(<<<<<<<|>>>>>>>)( |$)'
bot_name='github-actions[bot]'
bot_email='41898282+github-actions[bot]@users.noreply.github.com'
export GIT_AUTHOR_NAME="${bot_name}" GIT_AUTHOR_EMAIL="${bot_email}"
export GIT_COMMITTER_NAME="${bot_name}" GIT_COMMITTER_EMAIL="${bot_email}"

die() {
	echo "resolve.sh: $*" >&2
	exit 1
}

usage() {
	cat >&2 <<'EOF'
usage: resolve.sh setup <prepare-output>
       resolve.sh check
       resolve.sh export <prepare-output> <output>
       resolve.sh verify <prepare-output> <resolver-output> <output>
EOF
	exit 2
}

repository() {
	git -C "${root}" -c core.hooksPath=/dev/null "$@"
}

merge_shas() {
	local artifact="$1" merge
	repository fetch -q "${artifact}/sync.bundle" "+refs/heads/sync/upstream:refs/sync/prepared"
	prepared="$(repository rev-parse refs/sync/prepared)"
	merge="$(repository rev-list --min-parents=2 --max-count=1 "${prepared}")"
	[[ -n "${merge}" ]] || die "the prepared candidate holds no merge commit"
	fork="$(repository rev-parse "${merge}^1")"
	upstream="$(repository rev-parse "${merge}^2")"
	base="$(repository merge-base "${fork}" "${upstream}")"
}

require_fork_checkout() {
	local trusted
	trusted="$(repository rev-parse HEAD)"
	[[ "${fork}" == "${trusted}" ]] || die "the candidate merges ${fork}, not the trusted fork commit ${trusted}"
}

setup() {
	local artifact file stage revision entry mode object
	artifact="$(cd "$1" && pwd -P)"
	[[ "$(<"${artifact}/result")" == partial ]] || die "prepare did not report a partial merge"
	[[ ! -e "${git_dir}/MERGE_HEAD" ]] || die "a merge is already in progress"
	merge_shas "${artifact}"
	require_fork_checkout
	mkdir -p "${state}"
	cp "${artifact}/human.txt" "${state}/human.txt"

	repository read-tree -u --reset "${prepared}"
	printf '%s\n' "${upstream}" >"${git_dir}/MERGE_HEAD"
	printf 'Merge upstream main\n' >"${git_dir}/MERGE_MSG"
	while IFS= read -r file; do
		[[ -n "${file}" ]] || continue
		repository update-index --force-remove -- "${file}"
		for stage in 1 2 3; do
			case "${stage}" in
				1) revision="${base}" ;;
				2) revision="${fork}" ;;
				3) revision="${upstream}" ;;
			esac
			entry="$(repository ls-tree "${revision}" -- "${file}")"
			if [[ -n "${entry}" ]]; then
				read -r mode _ object <<<"${entry%%$'\t'*}"
				printf '%s %s %s\t%s\n' "${mode}" "${object}" "${stage}" "${file}"
			fi
		done | repository update-index --index-info
	done <"${state}/human.txt"

	mise install -C "${root}" --include-lazy
	printf 'fork %s\nupstream %s\nbase %s\n' "${fork}" "${upstream}" "${base}"
	repository diff --name-only --diff-filter=U
}

in_root() {
	(cd "${root}" && bash -o pipefail -c "$1")
}

run_check() {
	local name="$1" command="$2" status=0
	printf '\n$ %s\n' "${command}" | tee -a "${state}/validation.log"
	in_root "${command}" 2>&1 | tee "${state}/check.log" || status=$?
	cat "${state}/check.log" >>"${state}/validation.log"
	if ((status != 0)); then
		printf '%s\n' "${name}" >"${state}/failed-check"
		return 1
	fi
}

packages_of() {
	in_root "cargo metadata --no-deps --format-version 1 >$(printf '%q' "${state}/metadata.json")"
	python3 - "${root}" "${state}" <<'PY'
import json
import sys
from pathlib import Path

root = Path(sys.argv[1]).resolve()
state = Path(sys.argv[2])
metadata = json.loads((state / "metadata.json").read_text())
roots = sorted(
    (
        Path(package["manifest_path"]).resolve().parent,
        package["name"],
    )
    for package in metadata["packages"]
    if package["id"] in metadata["workspace_members"]
)
found = set()
for line in (state / "human.txt").read_text().splitlines():
    path = (root / line).resolve()
    owners = [(len(owner.parts), name) for owner, name in roots if path.is_relative_to(owner)]
    if owners:
        found.add(max(owners)[1])
print(" ".join(sorted(found)))
PY
}

check() {
	local fork packages package_args package tree test_package expression
	fork="$(repository rev-parse HEAD)"
	mkdir -p "${state}"
	rm -f "${state}/validated-tree" "${state}/failed-check" "${state}/check.log"
	: >"${state}/validation.log"

	run_check "unmerged paths" "[[ -z \"\$(git diff --name-only --diff-filter=U)\" ]] || { git diff --name-only --diff-filter=U; exit 1; }" || return 1
	run_check "staging" 'git add -u' || return 1
	run_check "conflict markers" "! git grep -n -E '${marker_pattern}'" || return 1
	run_check "automation unchanged" "git diff --cached --quiet ${fork} -- .github script/upstream-sync script/clippy || { git diff --cached --stat ${fork} -- .github script/upstream-sync script/clippy; exit 1; }" || return 1
	run_check "whitespace" 'git diff --cached --check' || return 1
	run_check "formatting" 'dprint check' || return 1
	run_check "workspace compile" 'cargo check --locked --workspace --all-targets --keep-going' || return 1

	packages="$(packages_of)"
	if [[ -n "${packages}" ]]; then
		package_args=""
		for package in ${packages}; do
			package_args+=" -p ${package}"
		done
		run_check "lint" "bash $(printf '%q' "${here}/../clippy") --no-deps${package_args}" || return 1
		run_check "tests" "cargo nextest run --locked --no-fail-fast${package_args}" || return 1
	fi
	if [[ -s "${state}/tests.txt" ]]; then
		while IFS=$'\t' read -r test_package expression; do
			[[ -n "${test_package}" && -n "${expression}" ]] || continue
			run_check "targeted tests" "cargo nextest run --locked --no-fail-fast -p $(printf '%q' "${test_package}") -E $(printf '%q' "${expression}")" || return 1
		done <"${state}/tests.txt"
	fi

	tree="$(repository write-tree)"
	printf '%s\n' "${tree}" >"${state}/validated-tree"
	printf '\nEvery check passed for tree %s.\n' "${tree}"
}

export_unfinished() {
	local output="$1" index="${state}/unfinished.index" tree commit
	mkdir -p "${state}"
	rm -f "${index}"
	GIT_INDEX_FILE="${index}" repository read-tree HEAD
	GIT_INDEX_FILE="${index}" repository add -A
	tree="$(GIT_INDEX_FILE="${index}" repository write-tree)"
	commit="$(repository commit-tree "${tree}" -p HEAD -m "Unfinished upstream merge of ${upstream}")"
	repository update-ref refs/sync/unfinished "${commit}"
	repository bundle create "${output}/unfinished.bundle" refs/sync/unfinished "^${fork}"
	echo "resolve.sh: saved the working tree as ${commit} in unfinished.bundle" >&2
}

export_resolution() {
	local artifact output="$2" name tree commit
	artifact="$(cd "$1" && pwd -P)"
	mkdir -p "${output}"
	for name in resolution.md tests.txt; do
		if [[ -f "${state}/${name}" ]]; then
			cp "${state}/${name}" "${output}/${name}"
		fi
	done
	merge_shas "${artifact}"
	if [[ "$(cat "${git_dir}/MERGE_HEAD" 2>/dev/null)" != "${upstream}" ]] \
		&& ! { repository merge-base --is-ancestor "${fork}" HEAD && repository merge-base --is-ancestor "${upstream}" HEAD; }; then
		echo "resolve.sh: HEAD neither merges nor contains ${upstream}, so there is no resolution to export" >&2
		export_unfinished "${output}"
		return 0
	fi
	if ! tree="$(repository write-tree)"; then
		echo "resolve.sh: the index still has unmerged paths, so there is no resolution to export" >&2
		export_unfinished "${output}"
		return 0
	fi
	commit="$(repository commit-tree "${tree}" -p "${fork}" -p "${upstream}" -m 'Resolve the upstream merge')"
	repository update-ref refs/sync/resolved "${commit}"
	repository bundle create "${output}/resolution.bundle" refs/sync/resolved "^${fork}"
}

bounded() {
	LC_ALL=C awk -v limit="$1" '{ size += length($0) + 1; if (size > limit) exit; print }'
}

announce() {
	cat "$1"
	if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then
		cat "$1" >>"${GITHUB_STEP_SUMMARY}"
	fi
}

fenced() {
	local fence
	fence="$(bash "${here}/prepare.sh" --markdown-fence <"$1")"
	printf '%s\n' "${fence}"
	cat "$1"
	printf '%s\n' "${fence}"
}

verify() {
	local artifact resolver output="$3" resolved="" tree="" commit file kept
	artifact="$(cd "$1" && pwd -P)"
	resolver="$2"
	mkdir -p "${output}" "${state}"
	rm -f "${state}/failed-check" "${state}/failure-reason" "${state}/check.log" "${state}/tests.txt" "${state}/validated-tree"
	merge_shas "${artifact}"
	require_fork_checkout
	cp "${artifact}/human.txt" "${state}/human.txt"
	for file in resolution.md tests.txt; do
		if [[ ! -f "${resolver}/${file}" && -f "${resolver}/state/${file}" ]]; then
			cp "${resolver}/state/${file}" "${resolver}/${file}"
		fi
	done
	if [[ -f "${resolver}/tests.txt" ]]; then
		bounded 8000 <"${resolver}/tests.txt" >"${state}/tests.txt"
	fi

	if [[ -s "${resolver}/resolution.bundle" ]] \
		&& repository fetch -q "${resolver}/resolution.bundle" "+refs/sync/resolved:refs/sync/resolved"; then
		resolved="$(repository rev-parse refs/sync/resolved)"
		if [[ "$(repository rev-list --parents -n 1 "${resolved}")" != "${resolved} ${fork} ${upstream}" ]]; then
			printf "Claude's merge commit does not have %s and %s as its parents.\n" "${fork}" "${upstream}" >"${state}/failure-reason"
		elif ! repository diff --quiet "${fork}" "${resolved}" -- .github script/upstream-sync script/clippy; then
			{
				printf "Claude's merge changes workflow or sync script files:\n\n"
				repository diff --name-only "${fork}" "${resolved}" -- .github script/upstream-sync script/clippy \
					| while IFS= read -r file; do printf -- "- \`%s\`\n" "${file}"; done
			} >"${state}/failure-reason"
		else
			repository read-tree -u --reset "${resolved}"
			if (cd "${root}" && bash "${here}/resolve.sh" check); then
				tree="$(<"${state}/validated-tree")"
			fi
		fi
	fi

	if [[ -n "${tree}" ]]; then
		commit="$(
			{
				printf 'Merge upstream and resolve sync conflicts\n\n'
				printf 'Merge upstream %s into %s. Claude resolved the conflicts that\n' "${upstream}" "${fork}"
				printf 'formatting, mergiraf and the lockfile rules could not.\n'
			} | repository commit-tree "${tree}" -p "${fork}" -p "${upstream}"
		)"
		repository update-ref refs/heads/sync/upstream "${commit}"
		repository bundle create "${output}/sync.bundle" refs/heads/sync/upstream "^${fork}"
		if [[ -s "${resolver}/resolution.md" ]]; then
			bounded 13000 <"${resolver}/resolution.md" >"${output}/resolution.md"
		else
			printf 'Claude left no summary of its resolution.\n' >"${output}/resolution.md"
		fi
		repository diff --name-only "${prepared}" "${resolved}" | grep -vxF -f "${state}/human.txt" >"${state}/beyond.txt" || true
		if [[ -s "${state}/beyond.txt" ]]; then
			{
				printf '\n### Files changed beyond the conflicts\n\n'
				while IFS= read -r file; do
					printf -- "- \`%s\`\n" "${file}"
				done <"${state}/beyond.txt"
			} | bounded 2500 >>"${output}/resolution.md"
		fi
		cp "${artifact}"/{formatting-only,formatted-three-way,structured,lockfiles,fork-deleted,dropped-github}.txt "${output}/"
		printf 'resolved\n' >"${output}/result"
		printf "## Verification passed\n\nClaude's resolution passed every check. The candidate is %s.\n\n" "${commit}" >"${state}/verdict.md"
		announce "${state}/verdict.md"
		return 0
	fi

	{
		printf '## Automatic resolution failed\n\n'
		if [[ -s "${state}/failed-check" ]]; then
			printf 'Validation failed at **%s**.\n\n' "$(<"${state}/failed-check")"
			tail -n 80 "${state}/check.log" | cut -c1-400 >"${state}/failed-tail.log"
			printf '<details><summary>Last 80 lines</summary>\n\n'
			fenced "${state}/failed-tail.log"
			printf '</details>\n\n'
		elif [[ -s "${state}/failure-reason" ]]; then
			cat "${state}/failure-reason"
			printf '\n'
		else
			printf 'Claude produced no merge.\n\n'
		fi
		if [[ -s "${resolver}/resolution.md" ]]; then
			bounded 15000 <"${resolver}/resolution.md" >"${state}/notes.md"
			printf "<details><summary>Claude's notes</summary>\n\n"
			fenced "${state}/notes.md"
			printf '</details>\n\n'
		fi
		kept=()
		for file in resolution.bundle unfinished.bundle checkout.patch state claude-execution-output.json claude-projects; do
			if [[ -e "${resolver}/${file}" ]]; then
				kept+=("\`${file}\`")
			fi
		done
		if ((${#kept[@]} > 0)) && [[ -n "${GITHUB_RUN_ID:-}" ]]; then
			printf "The \`upstream-sync-resolution\` artifact of %s/%s/actions/runs/%s keeps %s for 30 days.\n\n" \
				"${GITHUB_SERVER_URL:-https://github.com}" "${GITHUB_REPOSITORY:-kjanat/zed-editor}" "${GITHUB_RUN_ID}" "${kept[*]}"
		fi
		printf '<!-- claude-attempt fork=%s upstream=%s -->\n' "${fork}" "${upstream}"
	} >"${state}/failure.md"
	{
		bounded $((59000 - $(wc -c <"${state}/failure.md"))) <"${artifact}/conflict-report.md"
		printf '\n'
		cat "${state}/failure.md"
	} >"${output}/issue-body.md"
	printf 'conflict\n' >"${output}/result"
	announce "${state}/failure.md"
}

[[ $# -ge 1 ]] || usage
command="$1"
shift
case "${command}" in
	setup)
		[[ $# -eq 1 ]] || usage
		setup "$@"
		;;
	check)
		[[ $# -eq 0 ]] || usage
		check
		;;
	export)
		[[ $# -eq 2 ]] || usage
		export_resolution "$@"
		;;
	verify)
		[[ $# -eq 3 ]] || usage
		verify "$@"
		;;
	*) usage ;;
esac
