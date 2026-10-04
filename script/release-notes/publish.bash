#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 2 ]]; then
	echo "usage: $0 TAG NOTES.md" >&2
	exit 2
fi

tag="$1"
notes="$2"
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

"${script_dir}/linkify.py" "${notes}"
gh release edit "${tag}" --notes-file "${notes}"

asset_dir="$(mktemp -d)"
trap 'rm -r "${asset_dir}"' EXIT
gh release view "${tag}" --json name,body --jq '{title: .name, release_notes: .body}' >"${asset_dir}/notes.json"
gh release upload "${tag}" --clobber "${asset_dir}/notes.json#Release notes (JSON)"
