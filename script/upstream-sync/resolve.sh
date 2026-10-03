#!/usr/bin/env bash
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(git -C "${here}" rev-parse --show-toplevel)"
marker_pattern='^(<<<<<<<|>>>>>>>)( |$)'
bot_name='github-actions[bot]'
bot_email='41898282+github-actions[bot]@users.noreply.github.com'

die() {
	echo "resolve.sh: $*" >&2
	exit 1
}

usage() {
	cat >&2 <<'EOF'
usage: resolve.sh setup <prepare-output> <work>
       resolve.sh validate <work>
       resolve.sh package <work> <output> <prepare-output>
EOF
	exit 2
}

in_sandbox() {
	SYNC_WORK="${work}" "${here}/sandbox-exec" "$1"
}

shas() {
	local key value
	while IFS='=' read -r key value; do
		case "${key}" in
			fork) fork="${value}" ;;
			upstream) upstream="${value}" ;;
			base) base="${value}" ;;
		esac
	done <"${work}/in/shas"
}

write_sandbox_config() {
	local srt mise_data rustup_home
	srt="$(mise which -C "${root}" srt)"
	mise_data="${MISE_DATA_DIR:-${XDG_DATA_HOME:-${HOME}/.local/share}/mise}"
	rustup_home="${RUSTUP_HOME:-${HOME}/.rustup}"
	printf '%s\n' "${srt}" >"${work}/sandbox/srt-path"

	mise env -C "${root}" --json >"${work}/sandbox/mise-env.json"
	python3 - "${work}" "${mise_data}" "${rustup_home}" <<'PY'
import json
import os
import sys
from pathlib import Path

work, mise_data, rustup_home = sys.argv[1:]
home = os.path.realpath(os.path.expanduser("~"))
tool_env = json.loads(Path(work, "sandbox", "mise-env.json").read_text())
path = tool_env.get("PATH", os.environ["PATH"])

environment = {
    key: value
    for key, value in tool_env.items()
    if key == "PATH" or key.startswith(("CARGO_", "RUST", "WASI_"))
}
environment.update({
    "PATH": path,
    "HOME": f"{work}/home",
    "TMPDIR": f"{work}/tmp",
    "CARGO_HOME": f"{work}/cargo",
    "RUSTUP_HOME": rustup_home,
    "DPRINT_CACHE_DIR": f"{work}/dprint-cache",
    "MISE_DATA_DIR": mise_data,
    "MISE_CACHE_DIR": f"{work}/mise-cache",
    "MISE_STATE_DIR": f"{work}/mise-state",
    "MISE_TRUSTED_CONFIG_PATHS": f"{work}/candidate",
    "MISE_YES": "1",
    "CI": "true",
    "GITHUB_ACTIONS": "true",
    "TERM": "dumb",
    "LANG": "C.UTF-8",
})
Path(work, "sandbox", "env").write_text(
    "".join(f"{key}={value}\n" for key, value in sorted(environment.items()))
)

readable = {os.path.realpath(mise_data), os.path.realpath(rustup_home)}
for entry in path.split(os.pathsep):
    resolved = os.path.realpath(entry)
    if entry and (resolved == home or resolved.startswith(home + os.sep)):
        readable.add(resolved)
writable = [
    f"{work}/{name}"
    for name in (
        "candidate",
        "home",
        "tmp",
        "cargo",
        "dprint-cache",
        "mise-cache",
        "mise-state",
        "out",
    )
]
settings = {
    "network": {
        "allowedDomains": [
            "crates.io",
            "index.crates.io",
            "static.crates.io",
            "github.com",
            "codeload.github.com",
            "objects.githubusercontent.com",
            "release-assets.githubusercontent.com",
            "plugins.dprint.dev",
            "cdn.jsdelivr.net",
            "registry.npmjs.org",
        ],
        "deniedDomains": [],
    },
    "filesystem": {
        "denyRead": [home, "/root", "/var/run/docker.sock", "/run/docker.sock"],
        "allowRead": sorted(readable),
        "allowWrite": writable,
        "denyWrite": [],
    },
}
Path(work, "sandbox", "srt.json").write_text(json.dumps(settings, indent=2) + "\n")
PY
}

write_claude_settings() {
	python3 - "${work}" "${root}" "${here}" <<'PY'
import json
import os
import sys
from pathlib import Path

work, root, here = sys.argv[1:]
home = os.path.realpath(os.path.expanduser("~"))
protected = [root, f"{home}/.claude", f"{work}/sandbox", f"{work}/in"]
deny = ["Monitor", "PowerShell"]
for path in protected:
    for tool in ("Edit", "Write", "NotebookEdit"):
        deny.append(f"{tool}(/{path}/**)")
settings = {
    "permissions": {"deny": deny},
    "disableAllHooks": False,
    "enableAllProjectMcpServers": False,
    "hooks": {
        "PreToolUse": [
            {
                "matcher": "Bash",
                "hooks": [
                    {"type": "command", "command": f"{here}/sandbox-hook {work}"}
                ],
            }
        ]
    },
}
Path(work, "sandbox", "claude-settings.json").write_text(
    json.dumps(settings, indent=2) + "\n"
)
PY
}

write_prompt() {
	shas
	python3 - "${work}" "${root}" "${fork}" "${upstream}" "${base}" <<'PY'
import sys
from pathlib import Path
from string import Template

work, root, fork, upstream, base = sys.argv[1:]
template = Template(Path(root, "script/upstream-sync/resolve-prompt.md").read_text())
inputs = Path(work, "in")
prompt = template.substitute(
    fork=fork,
    upstream=upstream,
    base=base,
    work=work,
    candidate=f"{work}/candidate",
    out=f"{work}/out",
    root=root,
    conflicts=inputs.joinpath("human.txt").read_text().strip() or "(none)",
    report=inputs.joinpath("conflict-report.md").read_text(),
)
Path(work, "prompt.md").write_text(prompt)
PY
}

setup() {
	local artifact="$1" head merge fork upstream base trusted file stage revision entry mode object
	work="$2"
	artifact="$(cd "${artifact}" && pwd -P)"
	[[ "$(<"${artifact}/result")" == partial ]] || die "prepare did not report a partial merge"
	mkdir -p "${work}"
	work="$(cd "${work}" && pwd -P)"
	[[ ! -e "${work}/candidate" ]] || die "${work}/candidate already exists"
	mkdir -p "${work}"/{sandbox,home,tmp,cargo,dprint-cache,mise-cache,mise-state,in/lists,out}
	cp "${artifact}/conflict-report.md" "${artifact}/human.txt" "${work}/in/"
	cp "${artifact}"/{formatting-only,formatted-three-way,structured,lockfiles,fork-deleted,dropped-github}.txt "${work}/in/lists/"

	git clone -q --no-local --no-checkout "${root}" "${work}/candidate"
	git -C "${work}/candidate" config core.hooksPath /dev/null
	git -C "${work}/candidate" fetch -q "${artifact}/sync.bundle" "+refs/heads/sync/upstream:refs/sync/candidate"
	head="$(git -C "${work}/candidate" rev-parse refs/sync/candidate)"
	merge="$(git -C "${work}/candidate" rev-list --min-parents=2 --max-count=1 "${head}")"
	[[ -n "${merge}" ]] || die "the candidate holds no merge commit"
	fork="$(git -C "${work}/candidate" rev-parse "${merge}^1")"
	upstream="$(git -C "${work}/candidate" rev-parse "${merge}^2")"
	trusted="$(git -C "${root}" rev-parse HEAD)"
	[[ "${fork}" == "${trusted}" ]] || die "the candidate merges ${fork}, not the trusted fork commit ${trusted}"
	base="$(git -C "${work}/candidate" merge-base "${fork}" "${upstream}")"
	printf 'fork=%s\nupstream=%s\nbase=%s\n' "${fork}" "${upstream}" "${base}" >"${work}/in/shas"

	git -C "${work}/candidate" checkout -q --detach "${fork}"
	git -C "${work}/candidate" read-tree -u --reset "${head}"
	git -C "${work}/candidate" update-ref MERGE_HEAD "${upstream}"
	printf 'Merge upstream main\n' >"${work}/candidate/.git/MERGE_MSG"
	while IFS= read -r file; do
		[[ -n "${file}" ]] || continue
		git -C "${work}/candidate" update-index --force-remove -- "${file}"
		for stage in 1 2 3; do
			case "${stage}" in
				1) revision="${base}" ;;
				2) revision="${fork}" ;;
				3) revision="${upstream}" ;;
			esac
			entry="$(git -C "${work}/candidate" ls-tree "${revision}" -- "${file}")"
			if [[ -n "${entry}" ]]; then
				read -r mode _ object <<<"${entry%%$'\t'*}"
				printf '%s %s %s\t%s\n' "${mode}" "${object}" "${stage}" "${file}"
			fi
		done | git -C "${work}/candidate" update-index --index-info
	done <"${work}/in/human.txt"

	cp "${root}/script/clippy" "${work}/sandbox/clippy"
	mise install -C "${root}" --include-lazy
	write_sandbox_config
	write_claude_settings
	in_sandbox 'cargo fetch'
	in_sandbox 'dprint output-resolved-config >/dev/null'
	write_prompt
}

packages_of() {
	in_sandbox "cargo metadata --no-deps --format-version 1 >${work}/out/metadata.json"
	python3 - "${work}" <<'PY'
import json
import sys
from pathlib import Path

work = Path(sys.argv[1])
candidate = (work / "candidate").resolve()
metadata = json.loads((work / "out/metadata.json").read_text())
roots = sorted(
    (
        Path(package["manifest_path"]).resolve().parent,
        package["name"],
    )
    for package in metadata["packages"]
    if package["id"] in metadata["workspace_members"]
)
found = set()
for line in (work / "in/human.txt").read_text().splitlines():
    path = (candidate / line).resolve()
    owners = [(len(root.parts), name) for root, name in roots if path.is_relative_to(root)]
    if owners:
        found.add(max(owners)[1])
print(" ".join(sorted(found)))
PY
}

check() {
	local name="$1" command="$2"
	printf '\n$ %s\n' "${command}" >>"${work}/out/validation.log"
	if ! in_sandbox "${command}" >"${work}/out/check.log" 2>&1; then
		cat "${work}/out/check.log" >>"${work}/out/validation.log"
		printf '%s\n' "${name}" >"${work}/out/failed-check"
		cp "${work}/out/check.log" "${work}/out/failed-check.log"
		return 1
	fi
	cat "${work}/out/check.log" >>"${work}/out/validation.log"
}

validate() {
	local packages package_args tree test_package expression
	work="$(cd "$1" && pwd -P)"
	shas
	rm -f "${work}/out/validated-tree" "${work}/out/failed-check" "${work}/out/failed-check.log"
	: >"${work}/out/validation.log"

	check "unmerged paths" "[[ -z \"\$(git diff --name-only --diff-filter=U)\" ]] || { git diff --name-only --diff-filter=U; exit 1; }" || return 1
	check "staging" 'git add -u' || return 1
	check "conflict markers" "! git grep -n -E '${marker_pattern}'" || return 1
	check "automation unchanged" "git diff --cached --quiet ${fork} -- .github script/upstream-sync || { git diff --cached --stat ${fork} -- .github script/upstream-sync; exit 1; }" || return 1
	check "whitespace" 'git diff --cached --check' || return 1
	check "formatting" 'dprint check' || return 1
	check "workspace compile" 'cargo check --locked --workspace --all-targets --keep-going' || return 1

	packages="$(packages_of)"
	if [[ -n "${packages}" ]]; then
		package_args=""
		for package in ${packages}; do
			package_args+=" -p ${package}"
		done
		check "lint" "bash ${work}/sandbox/clippy --no-deps${package_args}" || return 1
		check "tests" "cargo nextest run --locked --no-fail-fast${package_args}" || return 1
	fi
	if [[ -s "${work}/out/tests.txt" ]]; then
		while IFS=$'\t' read -r test_package expression; do
			[[ -n "${test_package}" && -n "${expression}" ]] || continue
			check "targeted tests" "cargo nextest run --locked --no-fail-fast -p $(printf '%q' "${test_package}") -E $(printf '%q' "${expression}")" || return 1
		done <"${work}/out/tests.txt"
	fi

	tree="$(git -C "${work}/candidate" write-tree)"
	printf '%s\n' "${tree}" >"${work}/out/validated-tree"
}

bounded() {
	LC_ALL=C awk -v limit="$1" '{ size += length($0) + 1; if (size > limit) exit; print }'
}

package() {
	local output="$2" artifact="$3" tree="" validated="" commit fence
	work="$1"
	mkdir -p "${output}" "${work}/out"
	work="$(cd "${work}" && pwd -P)"
	if [[ ! -s "${work}/in/conflict-report.md" ]]; then
		mkdir -p "${work}/in"
		cp "${artifact}/conflict-report.md" "${work}/in/conflict-report.md"
	fi
	if [[ -s "${work}/in/shas" && -s "${work}/out/validated-tree" ]]; then
		shas
		tree="$(git -C "${work}/candidate" write-tree)"
		validated="$(<"${work}/out/validated-tree")"
	fi
	if [[ -n "${validated}" && "${tree}" == "${validated}" ]]; then
		commit="$(
			{
				printf 'Merge upstream and resolve sync conflicts\n\n'
				printf 'Merge upstream %s into %s. Claude resolved the conflicts that\n' "${upstream}" "${fork}"
				printf 'formatting, mergiraf and the lockfile rules could not.\n'
			} | GIT_AUTHOR_NAME="${bot_name}" GIT_AUTHOR_EMAIL="${bot_email}" \
				GIT_COMMITTER_NAME="${bot_name}" GIT_COMMITTER_EMAIL="${bot_email}" \
				git -C "${work}/candidate" commit-tree "${tree}" -p "${fork}" -p "${upstream}"
		)"
		git -C "${work}/candidate" update-ref refs/heads/sync/upstream "${commit}"
		git -C "${work}/candidate" bundle create "${output}/sync.bundle" refs/heads/sync/upstream "^${fork}"
		if [[ -s "${work}/out/resolution.md" ]]; then
			bounded 15000 <"${work}/out/resolution.md" >"${output}/resolution.md"
		else
			printf 'Claude left no summary of its resolution.\n' >"${output}/resolution.md"
		fi
		cp "${work}/in/lists/"*.txt "${output}/"
		printf 'resolved\n' >"${output}/result"
		return 0
	fi

	{
		printf '## Automatic resolution failed\n\n'
		if [[ -s "${work}/out/failed-check" ]]; then
			printf 'Validation failed at **%s**.\n\n' "$(<"${work}/out/failed-check")"
			if [[ -s "${work}/out/failed-check.log" ]]; then
				tail -n 80 "${work}/out/failed-check.log" | cut -c1-400 >"${work}/out/failed-tail.log"
				fence="$(bash "${here}/prepare.sh" --markdown-fence <"${work}/out/failed-tail.log")"
				printf '<details><summary>Last 80 lines</summary>\n\n%s\n' "${fence}"
				cat "${work}/out/failed-tail.log"
				printf '%s\n</details>\n\n' "${fence}"
			fi
		elif [[ -n "${validated}" ]]; then
			printf 'The candidate changed after validation.\n\n'
		elif [[ -s "${work}/in/shas" ]]; then
			printf 'Validation did not run.\n\n'
		else
			printf 'The resolver could not rebuild the merge.\n\n'
		fi
	} >"${work}/out/failure.md"
	{
		bounded $((59000 - $(wc -c <"${work}/out/failure.md"))) <"${work}/in/conflict-report.md"
		printf '\n'
		cat "${work}/out/failure.md"
	} >"${output}/issue-body.md"
	printf 'conflict\n' >"${output}/result"
}

[[ $# -ge 1 ]] || usage
command="$1"
shift
case "${command}" in
	setup)
		[[ $# -eq 2 ]] || usage
		setup "$@"
		;;
	validate)
		[[ $# -eq 1 ]] || usage
		validate "$@"
		;;
	package)
		[[ $# -eq 3 ]] || usage
		package "$@"
		;;
	*) usage ;;
esac
