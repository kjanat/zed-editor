#!/usr/bin/env bash

set -euo pipefail

OUTPUT_FILE="${1:-$(pwd)/assets/licenses.md}"
TEMPLATE_FILE="script/licenses/template.md.hbs"

fail_on_stderr() {
	local stderr_file
	stderr_file=$(mktemp)
	local rc=0
	"$@" 2>"${stderr_file}" || rc=$?
	cat "${stderr_file}" >&2
	if [[ "${rc}" -eq 0 && -s "${stderr_file}" ]]; then
		echo "error: $* wrote to stderr" >&2
		rc=1
	fi
	rm "${stderr_file}"
	return "${rc}"
}

{
	echo -e "# ###### THEME LICENSES ######\n"
	cat assets/themes/LICENSES

	echo -e "\n# ###### ICON LICENSES ######\n"
	cat assets/icons/LICENSES

	echo -e "\n# ###### CODE LICENSES ######\n"
} >"${OUTPUT_FILE}"

echo "Generating cargo licenses"
cargo about --version
set -x
if [[ -n "${ALLOW_MISSING_LICENSES-}" ]]; then
	cargo about generate -c script/licenses/zed-licenses.toml "${TEMPLATE_FILE}" >>"${OUTPUT_FILE}"
else
	fail_on_stderr cargo about generate --fail -c script/licenses/zed-licenses.toml "${TEMPLATE_FILE}" >>"${OUTPUT_FILE}"
fi
set +x

sed -i.bak \
	-e 's/&quot;/"/g' \
	-e "s/&#x27;/'/g" \
	-e 's/&#x3D;/=/g' \
	-e 's/&#x60;/`/g' \
	-e 's/&lt;/</g' \
	-e 's/&gt;/>/g' \
	"${OUTPUT_FILE}"
rm -f "${OUTPUT_FILE}.bak"

echo "generate-licenses completed. See ${OUTPUT_FILE}"
