#!/usr/bin/env bash
set -euo pipefail
format_merge_input() (
	set -euo pipefail
	case "$1" in
		*.json | *.jsonc)
			# The normal formatter preserves array line breaks and trailing commas,
			# which can turn adjacent, independent changes into merge conflicts.
			configuration=$(mktemp .sync-merge-format.XXXXXX.json)
			trap 'rm -f "$configuration"' EXIT
			printf '%s\n' '{"extends":"./.dprint.jsonc","json":{"array.preferSingleLine":true,"trailingCommas":"always"}}' >"$configuration"
			dprint fmt --config "$configuration" --stdin "$1"
			;;
		*) dprint fmt --stdin "$1" ;;
	esac
)
if [[ "${1:-}" == --format-merge-input ]]; then
	format_merge_input "$2"
	exit 0
fi
# Only /source (read-only) and /output cross the container boundary.
# Do not pass runner credentials or mount a host home directory here.
git clone --no-local /source /tmp/work
cd /tmp/work
git switch --detach
git branch -f master HEAD
git remote set-url origin https://github.com/kjanat/zed-editor.git
git remote add upstream "${UPSTREAM_URL:-https://github.com/zed-industries/zed.git}"
git fetch upstream main --no-tags
export SYNC_BRANCH=sync/upstream
export GITHUB_OUTPUT=/tmp/merge-output
export GITHUB_STEP_SUMMARY=/tmp/merge-summary
if [[ $(git rev-list --count master..upstream/main) == 0 ]]; then
	echo unchanged >/output/result
	exit 0
fi
attempt_merge() {
	set -euo pipefail
	git config user.name "github-actions[bot]"
	git config user.email "41898282+github-actions[bot]@users.noreply.github.com"
	git switch -c "${SYNC_BRANCH}"

	BASE="$(git merge-base master upstream/main)"
	UPSTREAM_SHA="$(git rev-parse upstream/main)"
	: >/tmp/formatting-only.txt
	: >/tmp/formatted-three-way.txt
	: >/tmp/lockfiles.txt
	: >/tmp/human.txt

	if git merge --no-ff --no-commit upstream/main; then
		git commit --message "Merge upstream main"
		echo "result=clean" >>"${GITHUB_OUTPUT}"
		return 0
	fi

	git diff --name-only --diff-filter=U -z >/tmp/conflicts.zlist
	mapfile -d '' CONFLICTS </tmp/conflicts.zlist
	printf '%s\n' "${CONFLICTS[@]}" | tee -a "${GITHUB_STEP_SUMMARY}"

	for FILE in "${CONFLICTS[@]}"; do
		# Modify/delete conflicts cannot be checked out or restored as a three-way merge.
		if ! git cat-file -e ":2:${FILE}" || ! git cat-file -e ":3:${FILE}"; then
			printf '%s\n' "${FILE}" >>/tmp/human.txt
			continue
		fi

		if [[ "${FILE}" == "Cargo.lock" ]]; then
			git checkout --ours -- "${FILE}"
			printf '%s\n' "${FILE}" >>/tmp/lockfiles.txt
			continue
		fi

		BASE_FORMATTED="$(mktemp)"
		MASTER_FILE="$(mktemp)"
		MASTER_FORMATTED="$(mktemp)"
		UPSTREAM_FORMATTED="$(mktemp)"
		MERGED_FILE="$(mktemp)"
		if git show "${BASE}:${FILE}" | format_merge_input "${FILE}" >"${BASE_FORMATTED}" \
			&& git show "master:${FILE}" >"${MASTER_FILE}"; then
			if cmp -s "${BASE_FORMATTED}" "${MASTER_FILE}"; then
				if git checkout --theirs -- "${FILE}" && dprint fmt "${FILE}" && dprint check "${FILE}"; then
					git add "${FILE}"
					printf '%s\n' "${FILE}" >>/tmp/formatting-only.txt
				else
					git checkout --conflict=merge -- "${FILE}"
					printf '%s\n' "${FILE}" >>/tmp/human.txt
				fi
			elif git show "master:${FILE}" | format_merge_input "${FILE}" >"${MASTER_FORMATTED}" \
				&& git show "upstream/main:${FILE}" | format_merge_input "${FILE}" >"${UPSTREAM_FORMATTED}" \
				&& git merge-file -p "${MASTER_FORMATTED}" "${BASE_FORMATTED}" "${UPSTREAM_FORMATTED}" >"${MERGED_FILE}"; then
				cp "${MERGED_FILE}" "${FILE}"
				if dprint fmt "${FILE}" && dprint check "${FILE}"; then
					git add "${FILE}"
					printf '%s\n' "${FILE}" >>/tmp/formatted-three-way.txt
				else
					git checkout --conflict=merge -- "${FILE}"
					printf '%s\n' "${FILE}" >>/tmp/human.txt
				fi
			else
				git checkout --conflict=merge -- "${FILE}"
				printf '%s\n' "${FILE}" >>/tmp/human.txt
			fi
		else
			printf '%s\n' "${FILE}" >>/tmp/human.txt
		fi
		rm -f "${BASE_FORMATTED}" "${MASTER_FILE}" "${MASTER_FORMATTED}" "${UPSTREAM_FORMATTED}" "${MERGED_FILE}"
	done

	if [[ ! -s /tmp/human.txt && -s /tmp/lockfiles.txt ]]; then
		if cargo metadata --format-version 1 -q >/dev/null; then
			git add Cargo.lock
		else
			git checkout --conflict=merge -- Cargo.lock
			: >/tmp/lockfiles.txt
			printf 'Cargo.lock\n' >>/tmp/human.txt
		fi
	fi

	if [[ ! -s /tmp/human.txt ]]; then
		CHECK_FILES=()
		if [[ -s /tmp/formatting-only.txt ]]; then
			mapfile -t FORMATTING_ONLY_FILES </tmp/formatting-only.txt
			CHECK_FILES+=("${FORMATTING_ONLY_FILES[@]}")
		fi
		if [[ -s /tmp/formatted-three-way.txt ]]; then
			mapfile -t THREE_WAY_FILES </tmp/formatted-three-way.txt
			CHECK_FILES+=("${THREE_WAY_FILES[@]}")
		fi
		if [[ "${#CHECK_FILES[@]}" -gt 0 ]]; then
			dprint check "${CHECK_FILES[@]}"
		fi
		{
			printf 'Merge upstream with automatic conflict resolution\n\n'
			printf 'Formatting-only:\n'
			while IFS= read -r FILE; do printf -- '- %s\n' "${FILE}"; done </tmp/formatting-only.txt
			printf '\nFormatted three-way:\n'
			while IFS= read -r FILE; do printf -- '- %s\n' "${FILE}"; done </tmp/formatted-three-way.txt
			printf '\nLockfile:\n'
			while IFS= read -r FILE; do printf -- '- %s\n' "${FILE}"; done </tmp/lockfiles.txt
		} >/tmp/merge-message.txt
		git commit --file /tmp/merge-message.txt
		echo "result=resolved" >>"${GITHUB_OUTPUT}"
		return 0
	fi

	DATE="$(date -u +%Y-%m-%d)"
	BEHIND="$(git rev-list --count master..upstream/main)"
	HUMAN_COUNT="$(wc -l </tmp/human.txt)"
	{
		printf '## Upstream sync conflict, %s\n\n' "${DATE}"
		printf "Upstream is %s commits ahead (https://github.com/zed-industries/zed/compare/%s...%s).\n\n" \
			"${BEHIND}" "${BASE}" "${UPSTREAM_SHA}"
		printf '### Resolved automatically (verify, do not resolve)\n'
		if [[ -s /tmp/formatting-only.txt ]]; then
			while IFS= read -r FILE; do
				printf -- "- \`%s\`: formatting-only, upstream taken and reformatted\n" "${FILE}"
			done </tmp/formatting-only.txt
		fi
		if [[ -s /tmp/formatted-three-way.txt ]]; then
			while IFS= read -r FILE; do
				printf -- "- \`%s\`: formatted three-way, fork and upstream changes retained\n" "${FILE}"
			done </tmp/formatted-three-way.txt
		fi
		if [[ -s /tmp/lockfiles.txt ]]; then
			printf -- "- \`Cargo.lock\`: fork side kept; cargo reconciles it after human files are resolved\n"
		fi
		printf '\n### Needs a human (%s files)\n' "${HUMAN_COUNT}"
		while IFS= read -r FILE; do
			HUNKS="$(grep -c '^<<<<<<< ' "${FILE}" || true)"
			FORK_LOG="$(git log --format="- \`%h\` %s" -5 "${BASE}..master" -- "${FILE}")"
			UPSTREAM_LOG="$(git log --format="- \`%h\` %s" -5 "master..upstream/main" -- "${FILE}")"
			DIFF_LINES="$(git diff --cc -- "${FILE}" | wc -l)"
			if [[ "${HUNKS}" == "1" ]]; then
				HUNK_LABEL="hunk"
			else
				HUNK_LABEL="hunks"
			fi
			printf "#### \`%s\` - %s conflict %s\n\n" "${FILE}" "${HUNKS}" "${HUNK_LABEL}"
			printf '<details><summary>Relevant commits</summary>\n\n'
			printf '**Fork**\n\n'
			if [[ -n "${FORK_LOG}" ]]; then printf '%s\n' "${FORK_LOG}"; else printf '_None._\n'; fi
			printf '\n**Upstream**\n\n'
			if [[ -n "${UPSTREAM_LOG}" ]]; then printf '%s\n' "${UPSTREAM_LOG}"; else printf '_None._\n'; fi
			printf '\n</details>\n\n'
			if ((DIFF_LINES > 200)); then
				printf '<details><summary>Conflict diff (first 200 of %s lines)</summary>\n\n' "${DIFF_LINES}"
			else
				printf '<details><summary>Conflict diff</summary>\n\n'
			fi
			printf '```diff\n'
			git diff --cc -- "${FILE}" | awk 'NR <= 200 { print }'
			printf '```\n</details>\n\n'
		done </tmp/human.txt
		printf '### Resolve\n```sh\n'
		printf 'git fetch upstream main && git merge --no-commit upstream/main\n'
		printf "BASE=\"\$(git merge-base master upstream/main)\"\n"
		while IFS= read -r FILE; do
			printf 'git checkout --theirs -- %q && dprint fmt %q && git add %q\n' "${FILE}" "${FILE}" "${FILE}"
		done </tmp/formatting-only.txt
		if [[ -s /tmp/formatted-three-way.txt ]]; then
			printf 'set -euo pipefail\n'
			printf 'resolve_formatted_three_way() {\n'
			printf "  local file=\"\$1\" base_formatted master_formatted upstream_formatted merged_file\n"
			printf "  base_formatted=\"\$(mktemp)\"\n"
			printf "  master_formatted=\"\$(mktemp)\"\n"
			printf "  upstream_formatted=\"\$(mktemp)\"\n"
			printf "  merged_file=\"\$(mktemp)\"\n"
			printf "  trap 'rm -f \"\${base_formatted}\" \"\${master_formatted}\" \"\${upstream_formatted}\" \"\${merged_file}\"' EXIT\n"
			printf "  git show \"\${BASE}:\${file}\" | bash script/upstream-sync/prepare.sh --format-merge-input \"\${file}\" > \"\${base_formatted}\"\n"
			printf "  git show \"master:\${file}\" | bash script/upstream-sync/prepare.sh --format-merge-input \"\${file}\" > \"\${master_formatted}\"\n"
			printf "  git show \"upstream/main:\${file}\" | bash script/upstream-sync/prepare.sh --format-merge-input \"\${file}\" > \"\${upstream_formatted}\"\n"
			printf "  git merge-file -p \"\${master_formatted}\" \"\${base_formatted}\" \"\${upstream_formatted}\" > \"\${merged_file}\"\n"
			printf "  cp \"\${merged_file}\" \"\${file}\"\n"
			printf "  dprint fmt \"\${file}\" && git add \"\${file}\"\n"
			printf "  rm -f \"\${base_formatted}\" \"\${master_formatted}\" \"\${upstream_formatted}\" \"\${merged_file}\"\n"
			printf '  trap - EXIT\n'
			printf '}\n'
			while IFS= read -r FILE; do
				printf 'resolve_formatted_three_way %q\n' "${FILE}"
			done </tmp/formatted-three-way.txt
		fi
		if [[ -s /tmp/lockfiles.txt ]]; then
			printf 'git checkout --ours -- Cargo.lock\n'
		fi
		printf '# resolve the files above, then:\n'
		if [[ -s /tmp/lockfiles.txt ]]; then
			printf 'dprint check && cargo metadata --format-version 1 -q > /dev/null && git add Cargo.lock && git commit\n'
		else
			printf 'dprint check && git commit\n'
		fi
		printf '```\n'
	} >/tmp/issue-body.md

	git merge --abort
	echo "result=conflict" >>"${GITHUB_OUTPUT}"

}
attempt_merge
result=$(sed -n "s/^result=//p" "$GITHUB_OUTPUT")
case "$result" in
	clean | resolved)
		dprint fmt
		if ! git diff --quiet; then
			git commit --all --message "Apply this fork's formatting to the upstream merge"
		fi
		git bundle create /output/sync.bundle refs/heads/sync/upstream ^master
		;;
	conflict)
		cp /tmp/issue-body.md /output/issue-body.md
		;;
	*) exit 1 ;;
esac
if [[ "$result" == resolved ]]; then
	cp /tmp/formatting-only.txt /tmp/formatted-three-way.txt /tmp/lockfiles.txt /output/
fi
printf "%s\n" "$result" >/output/result
