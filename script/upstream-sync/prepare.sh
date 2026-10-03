#!/usr/bin/env bash
set -euo pipefail
restore_fork_rules() {
	[[ -n "$(git ls-files --unmerged -- .rules)" ]] || return 0
	# Rules conflicts must not block the sync or revive rules the fork removed.
	git rm -r -q -f --ignore-unmatch -- .rules
	if git cat-file -e master:.rules 2>/dev/null; then
		git checkout master -- .rules
	fi
}
format_merge_input() (
	set -euo pipefail
	case "$1" in
		# The normal formatter preserves array line breaks and trailing commas,
		# which can turn adjacent, independent changes into merge conflicts.
		*.json | *.jsonc) overrides='"json":{"array.preferSingleLine":true,"trailingCommas":"always"}' ;;
		# The fork wraps upstream's markdown at 80 columns, and the normal formatter maintains wrapping.
		*.md) overrides='"markdown":{"textWrap":"always"}' ;;
		*)
			dprint fmt --stdin "$1"
			return
			;;
	esac
	configuration=$(mktemp .sync-merge-format.XXXXXX.json)
	trap 'rm -f "$configuration"' EXIT
	printf '{"extends":"./.dprint.jsonc",%s}\n' "$overrides" >"$configuration"
	dprint fmt --config "$configuration" --stdin "$1"
)
# Callers test the result, which disables errexit inside, so every step is chained.
structured_merge() (
	base=$(mktemp) ours=$(mktemp) theirs=$(mktemp) merged=$(mktemp)
	trap 'rm -f "$base" "$ours" "$theirs" "$merged"' EXIT
	git show ":1:$1" >"$base" \
		&& git show ":2:$1" >"$ours" \
		&& git show ":3:$1" >"$theirs" \
		&& mergiraf merge --timeout 0 --path-name "$1" "$base" "$ours" "$theirs" --output "$merged" \
		&& cp "$merged" "$1" \
		&& dprint fmt "$1" \
		&& dprint check "$1"
)
# The exporter refuses conflict-report.md above 60000 bytes, and GitHub rejects
# pull request bodies above 65536 characters, so the report must fit a fixed budget.
REPORT_BUDGET="${SYNC_REPORT_BUDGET:-56000}"
# Appends whole lines only, so multibyte characters are never split.
append_lines_within() {
	LC_ALL=C awk -v limit="$1" '{ size += length($0) + 1; if (size > limit) exit 1; print }'
}
# CommonMark closes a fence only with a backtick run at least as long as its opener.
markdown_fence() {
	LC_ALL=C awk '
		{ while (match($0, /`+/)) { if (RLENGTH > longest) longest = RLENGTH; $0 = substr($0, RSTART + RLENGTH) } }
		END { fence = "```"; while (length(fence) <= longest) fence = fence "`"; print fence }'
}
assemble_report() (
	set -euo pipefail
	parts="$1" budget="$2"
	notice_reserve=512
	summary_size=$(wc -c <"${parts}/summary.md")
	resolve_size=$(wc -c <"${parts}/resolve.md")
	if ((summary_size + resolve_size + notice_reserve > budget)); then
		append_lines_within $((budget - notice_reserve)) <"${parts}/summary.md" || true
		printf "\n> [!WARNING]\n> This report exceeded %s bytes and was truncated. Run \`git grep -n '^<<<<<<< '\` on the sync branch to find every conflict.\n" "${budget}"
		return 0
	fi
	cat "${parts}/summary.md"
	remaining=$((budget - summary_size - resolve_size - notice_reserve))
	omitted=0
	for detail in "${parts}"/details/*.md; do
		[[ -e "${detail}" ]] || continue
		detail_size=$(wc -c <"${detail}")
		if ((omitted == 0 && detail_size <= remaining)); then
			cat "${detail}"
			remaining=$((remaining - detail_size))
		else
			omitted=$((omitted + 1))
		fi
	done
	if ((omitted > 0)); then
		printf '> [!NOTE]\n> Details for %s files were omitted to keep this report under %s bytes. Their conflict markers are committed on the sync branch.\n\n' "${omitted}" "${budget}"
	fi
	cat "${parts}/resolve.md"
)
if [[ "${1:-}" == --format-merge-input ]]; then
	format_merge_input "$2"
	exit 0
fi
if [[ "${1:-}" == --markdown-fence ]]; then
	markdown_fence
	exit 0
fi
if [[ "${1:-}" == --restore-fork-rules ]]; then
	restore_fork_rules
	exit 0
fi
# Only /source (read-only) and /output cross the container boundary.
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
	: >/tmp/fork-deleted.txt
	: >/tmp/structured.txt
	: >/tmp/human.txt

	if ! git merge --no-ff --no-commit upstream/main && ! git rev-parse -q --verify MERGE_HEAD >/dev/null; then
		return 1
	fi
	# The fork keeps its own automation and the publisher rejects any change to
	# .github, so upstream changes there are dropped instead of stalling the sync.
	git diff --name-only "${BASE}" upstream/main -- .github >/tmp/dropped-github.txt
	git rm -r -q -f --ignore-unmatch -- .github
	if git cat-file -e master:.github 2>/dev/null; then
		git checkout master -- .github
	fi
	restore_fork_rules

	if [[ -z "$(git diff --name-only --diff-filter=U)" ]]; then
		git commit --message "Merge upstream main"
		echo "result=clean" >>"${GITHUB_OUTPUT}"
		return 0
	fi

	git diff --name-only --diff-filter=U -z >/tmp/conflicts.zlist
	mapfile -d '' CONFLICTS </tmp/conflicts.zlist
	printf '%s\n' "${CONFLICTS[@]}" | tee -a "${GITHUB_STEP_SUMMARY}"

	for FILE in "${CONFLICTS[@]}"; do
		if ! git cat-file -e ":2:${FILE}"; then
			git rm -q -f -- "${FILE}"
			printf '%s\n' "${FILE}" >>/tmp/fork-deleted.txt
			continue
		fi
		# Modify/delete conflicts cannot be checked out or restored as a three-way merge.
		if ! git cat-file -e ":3:${FILE}"; then
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
			# Taking upstream's markdown as is would drop the fork's wrapping.
			if [[ "${FILE}" != *.md ]] && cmp -s "${BASE_FORMATTED}" "${MASTER_FILE}"; then
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

	if [[ -s /tmp/human.txt ]]; then
		mapfile -t UNRESOLVED </tmp/human.txt
		: >/tmp/human.txt
		for FILE in "${UNRESOLVED[@]}"; do
			if git cat-file -e ":1:${FILE}" && git cat-file -e ":3:${FILE}" && structured_merge "${FILE}"; then
				git add "${FILE}"
				printf '%s\n' "${FILE}" >>/tmp/structured.txt
			else
				if git cat-file -e ":3:${FILE}"; then
					git checkout --conflict=merge -- "${FILE}"
				fi
				printf '%s\n' "${FILE}" >>/tmp/human.txt
			fi
		done
	fi

	if [[ ! -s /tmp/human.txt && -s /tmp/lockfiles.txt ]]; then
		if cargo metadata --format-version 1 -q >/dev/null; then
			git add Cargo.lock
		else
			git checkout --conflict=merge -- Cargo.lock
			: >/tmp/lockfiles.txt
			printf 'Cargo.lock\n' >>/tmp/human.txt
		fi
	fi

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
	write_merge_message() {
		printf '%s\n\n' "$1"
		printf 'Formatting-only:\n'
		while IFS= read -r FILE; do printf -- '- %s\n' "${FILE}"; done </tmp/formatting-only.txt
		printf '\nFormatted three-way:\n'
		while IFS= read -r FILE; do printf -- '- %s\n' "${FILE}"; done </tmp/formatted-three-way.txt
		printf '\nStructured merge:\n'
		while IFS= read -r FILE; do printf -- '- %s\n' "${FILE}"; done </tmp/structured.txt
		printf '\nLockfile:\n'
		while IFS= read -r FILE; do printf -- '- %s\n' "${FILE}"; done </tmp/lockfiles.txt
		printf '\nDeleted in the fork:\n'
		while IFS= read -r FILE; do printf -- '- %s\n' "${FILE}"; done </tmp/fork-deleted.txt
		if [[ -s /tmp/human.txt ]]; then
			printf '\nConflicts committed for a human to resolve:\n'
			while IFS= read -r FILE; do printf -- '- %s\n' "${FILE}"; done </tmp/human.txt
		fi
	}

	if [[ ! -s /tmp/human.txt ]]; then
		write_merge_message 'Merge upstream with automatic conflict resolution' >/tmp/merge-message.txt
		git commit --file /tmp/merge-message.txt
		echo "result=resolved" >>"${GITHUB_OUTPUT}"
		return 0
	fi

	BEHIND="$(git rev-list --count master..upstream/main)"
	HUMAN_COUNT="$(wc -l </tmp/human.txt)"
	rm -rf /tmp/report
	mkdir -p /tmp/report/details
	{
		printf "Upstream is %s commits ahead (https://github.com/zed-industries/zed/compare/%s...%s).\n\n" \
			"${BEHIND}" "${BASE}" "${UPSTREAM_SHA}"
		printf '## Resolved automatically (verify, do not resolve)\n\n'
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
		while IFS= read -r FILE; do
			printf -- "- \`%s\`: structured merge, fork and upstream changes retained\n" "${FILE}"
		done </tmp/structured.txt
		if [[ -s /tmp/lockfiles.txt ]]; then
			printf -- "- \`Cargo.lock\`: fork side kept; cargo reconciles it after human files are resolved\n"
		fi
		while IFS= read -r FILE; do
			printf -- "- \`%s\`: deleted in the fork, upstream changes dropped\n" "${FILE}"
		done </tmp/fork-deleted.txt
		if [[ -s /tmp/dropped-github.txt ]]; then
			printf '\n## Upstream automation dropped (port by hand if wanted)\n\n'
			while IFS= read -r FILE; do
				printf -- "- \`%s\`\n" "${FILE}"
			done </tmp/dropped-github.txt
		fi
		printf '\n## Needs a human (%s files)\n\n' "${HUMAN_COUNT}"
		while IFS= read -r FILE; do
			if git cat-file -e ":3:${FILE}"; then
				HUNKS="$(grep -c '^<<<<<<< ' "${FILE}" || true)"
				if [[ "${HUNKS}" == "1" ]]; then
					printf -- "- \`%s\`: 1 conflict hunk\n" "${FILE}"
				else
					printf -- "- \`%s\`: %s conflict hunks\n" "${FILE}" "${HUNKS}"
				fi
			else
				printf -- "- \`%s\`: deleted upstream, the fork's version is committed\n" "${FILE}"
			fi
		done </tmp/human.txt
		printf '\n'
	} >/tmp/report/summary.md
	DETAIL_INDEX=0
	while IFS= read -r FILE; do
		DETAIL_INDEX=$((DETAIL_INDEX + 1))
		{
			HUNKS="$(grep -c '^<<<<<<< ' "${FILE}" || true)"
			FORK_LOG="$(git log --format="- \`%h\` %s" -5 "${BASE}..master" -- "${FILE}")"
			UPSTREAM_LOG="$(git log --format="- \`%h\` %s" -5 "master..upstream/main" -- "${FILE}")"
			DIFF_LINES="$(git diff --cc -- "${FILE}" | wc -l)"
			if [[ "${HUNKS}" == "1" ]]; then
				HUNK_LABEL="hunk"
			else
				HUNK_LABEL="hunks"
			fi
			printf "### \`%s\` - %s conflict %s\n\n" "${FILE}" "${HUNKS}" "${HUNK_LABEL}"
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
			DIFF_FILE="$(mktemp)"
			git diff --cc -- "${FILE}" | awk 'NR <= 200 { print }' >"${DIFF_FILE}"
			FENCE="$(markdown_fence <"${DIFF_FILE}")"
			printf '%sdiff\n' "${FENCE}"
			cat "${DIFF_FILE}"
			printf '%s\n</details>\n\n' "${FENCE}"
			rm -f "${DIFF_FILE}"
		} >"/tmp/report/details/$(printf '%06d' "${DETAIL_INDEX}").md"
	done </tmp/human.txt
	mapfile -t HUMAN_FILES </tmp/human.txt
	printf "## Resolve\n\nFollow \`.agents/skills/upstream-sync-conflict/SKILL.md\`.\n" >/tmp/report/resolve.md
	assemble_report /tmp/report "${REPORT_BUDGET}" >/tmp/conflict-report.md

	git add -A -- "${HUMAN_FILES[@]}"
	if [[ -s /tmp/lockfiles.txt ]]; then
		git add Cargo.lock
	fi
	write_merge_message 'Merge upstream with conflicts left for a human' >/tmp/merge-message.txt
	git commit --file /tmp/merge-message.txt
	echo "result=partial" >>"${GITHUB_OUTPUT}"
}
attempt_merge
result=$(sed -n "s/^result=//p" "$GITHUB_OUTPUT")
case "$result" in
	clean | resolved | partial)
		FORMAT_EXCLUDES=()
		if [[ "$result" == partial ]]; then
			mapfile -t HUMAN_FILES </tmp/human.txt
			FORMAT_EXCLUDES=(--excludes "${HUMAN_FILES[@]}")
		fi
		dprint fmt --allow-no-files "${FORMAT_EXCLUDES[@]}"
		# Formatter configuration merged from upstream must not reformat the
		# fork's automation either.
		if git cat-file -e HEAD:.github 2>/dev/null; then
			git checkout HEAD -- .github
		fi
		if ! git diff --quiet; then
			git commit --all --message "Apply this fork's formatting to the upstream merge"
		fi
		git bundle create /output/sync.bundle refs/heads/sync/upstream ^master
		cp /tmp/dropped-github.txt /output/
		;;
	*) exit 1 ;;
esac
case "$result" in
	resolved | partial) cp /tmp/formatting-only.txt /tmp/formatted-three-way.txt /tmp/structured.txt /tmp/lockfiles.txt /tmp/fork-deleted.txt /output/ ;;
esac
if [[ "$result" == partial ]]; then
	cp /tmp/conflict-report.md /tmp/human.txt /output/
fi
printf "%s\n" "$result" >/output/result
