$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true

$outputFile = $args[0] ? $args[0] : "$(Get-Location)/assets/licenses.md"
$templateFile = "script/licenses/template.md.hbs"

New-Item -Path "$outputFile" -ItemType File -Value "" -Force

@(
	"# ###### THEME LICENSES ######\n"
	Get-Content assets/themes/LICENSES
	"\n# ###### ICON LICENSES ######\n"
	Get-Content assets/icons/LICENSES
	"\n# ###### CODE LICENSES ######\n"
) | Add-Content -Path $outputFile

Write-Host "Generating cargo licenses"
cargo about --version

$failFlag = $env:ALLOW_MISSING_LICENSES ? "" : "--fail"
$args = @('about', 'generate', $failFlag, '-c', 'script/licenses/zed-licenses.toml', $templateFile, '-o', $outputFile) |
Where-Object { $_ }
$stderrFile = New-TemporaryFile
try {
	cargo @args 2> $stderrFile
}
finally {
	Get-Content $stderrFile | ForEach-Object { [Console]::Error.WriteLine($_) }
	$wroteStderr = (Get-Item $stderrFile).Length -gt 0
	Remove-Item $stderrFile
}
if ($wroteStderr -and -not $env:ALLOW_MISSING_LICENSES) {
	throw "cargo about wrote to stderr"
}

Write-Host "Applying replacements"
$replacements = @{
	'&quot;' = '"'
	'&#x27;' = "'"
	'&#x3D;' = '='
	'&#x60;' = '`'
	'&lt;'   = '<'
	'&gt;'   = '>'
}
$content = Get-Content $outputFile
foreach ($find in $replacements.keys) {
	$content = $content -replace $find, $replacements[$find]
}
$content | Set-Content $outputFile

Write-Host "generate-licenses completed. See $outputFile"
