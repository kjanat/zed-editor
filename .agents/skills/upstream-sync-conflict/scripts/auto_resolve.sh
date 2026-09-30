#!/usr/bin/env bash
set -euo pipefail

fork="${FORK_REF:-origin/master}"
upstream="${UPSTREAM_REF:-upstream/main}"

cd "$(git rev-parse --show-toplevel)"
if ! git rev-parse -q --verify MERGE_HEAD; then
	echo "No merge in progress. Run: git merge --no-ff --no-commit ${upstream}" >&2
	exit 1
fi
if [[ "$(git rev-parse MERGE_HEAD)" != "$(git rev-parse "${upstream}")" ]]; then
	echo "MERGE_HEAD is not ${upstream}" >&2
	exit 1
fi

base="$(git merge-base "${fork}" "${upstream}")"
work="$(mktemp -d)"
trap 'rm -rf "${work}"' EXIT
formatter=script/upstream-sync/prepare.sh

formatted() {
	git show "$1" | bash "${formatter}" --format-merge-input "$2"
}

git diff --name-only "${base}" "${upstream}" -- .github >"${work}/dropped"
git rm -r -q -f --ignore-unmatch -- .github
if git cat-file -e "${fork}:.github"; then
	git archive "${fork}" .github | tar -x
	git add -- .github
fi

if [[ -n "$(git ls-files --unmerged -- .rules)" ]]; then
	git rm -q -f -- .rules
	if git cat-file -e "${fork}:.rules"; then
		git show "${fork}:.rules" >.rules
		git add -- .rules
	fi
fi

: >"${work}/formatting-only"
: >"${work}/three-way"
: >"${work}/lockfile"
: >"${work}/lockfile-reconciled"
: >"${work}/human"

git diff --name-only --diff-filter=U -z >"${work}/conflicts"
mapfile -d '' conflicts <"${work}/conflicts"

for file in "${conflicts[@]}"; do
	if ! git cat-file -e ":2:${file}" || ! git cat-file -e ":3:${file}"; then
		printf '%s\n' "${file}" >>"${work}/human"
		continue
	fi
	if [[ "${file}" == Cargo.lock ]]; then
		git show ":2:Cargo.lock" >Cargo.lock
		printf '%s\n' "${file}" >>"${work}/lockfile"
		continue
	fi

	cp -- "${file}" "${work}/backup"
	if ! formatted "${base}:${file}" "${file}" >"${work}/base"; then
		printf '%s\n' "${file}" >>"${work}/human"
		continue
	fi

	if git show ":2:${file}" | cmp -s "${work}/base" -; then
		git show ":3:${file}" >"${file}"
		if dprint fmt "${file}" && dprint check "${file}"; then
			git add -- "${file}"
			printf '%s\n' "${file}" >>"${work}/formatting-only"
		else
			cp -- "${work}/backup" "${file}"
			printf '%s\n' "${file}" >>"${work}/human"
		fi
		continue
	fi

	if formatted ":2:${file}" "${file}" >"${work}/ours" \
		&& formatted ":3:${file}" "${file}" >"${work}/theirs" \
		&& git merge-file -p "${work}/ours" "${work}/base" "${work}/theirs" >"${work}/merged"; then
		cp -- "${work}/merged" "${file}"
		if dprint fmt "${file}" && dprint check "${file}"; then
			git add -- "${file}"
			printf '%s\n' "${file}" >>"${work}/three-way"
			continue
		fi
	fi
	cp -- "${work}/backup" "${file}"
	printf '%s\n' "${file}" >>"${work}/human"
done

if [[ -s "${work}/lockfile" && ! -s "${work}/human" ]]; then
	if cargo metadata --format-version 1 -q >"${work}/metadata.json"; then
		git add -- Cargo.lock
		mv -- "${work}/lockfile" "${work}/lockfile-reconciled"
		: >"${work}/lockfile"
	else
		printf 'Cargo.lock\n' >>"${work}/human"
		: >"${work}/lockfile"
	fi
fi

section() {
	printf '\n%s\n' "$1"
	if [[ -s "$2" ]]; then
		sed 's/^/- /' "$2"
	else
		printf -- '- none\n'
	fi
}

printf 'Fork: %s\nUpstream: %s\nBase: %s\n' "$(git rev-parse "${fork}")" "$(git rev-parse "${upstream}")" "${base}"
section 'Upstream .github changes dropped:' "${work}/dropped"
section 'Formatting-only, upstream taken:' "${work}/formatting-only"
section 'Formatted three-way:' "${work}/three-way"
section 'Cargo.lock reconciled with cargo metadata and staged:' "${work}/lockfile-reconciled"
section 'Cargo.lock, fork side written, unstaged (reconcile after the human files):' "${work}/lockfile"
section 'Needs a human:' "${work}/human"
