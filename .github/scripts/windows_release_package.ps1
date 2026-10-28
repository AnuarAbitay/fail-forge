param(
    [Parameter(Mandatory = $true)][string]$Installation,
    [Parameter(Mandatory = $true)][string]$DartSdk,
    [Parameter(Mandatory = $true)][string]$PackageDirectory,
    [Parameter(Mandatory = $true)][string]$SmokeOutput,
    [string]$ExpectedVersion = ''
)

# Shared by native CI and the manual release workflow; this script never uploads.
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true
$tool = Join-Path $Installation 'target/debug/failforge-tools.exe'
$arguments = @('package', '--installation', $Installation, '--output', $PackageDirectory)
if ($ExpectedVersion) { $arguments += @('--expected-version', $ExpectedVersion) }
& $tool @arguments

$manifest = Get-Content -Raw (Join-Path $PackageDirectory 'package-manifest.json') | ConvertFrom-Json
$target = 'x86_64-pc-windows-msvc'
if ($manifest.schemaVersion -ne 1 -or $manifest.target -ne $target) {
    throw 'Package manifest is not a native Windows x64 package'
}
if ($ExpectedVersion -and $manifest.version -ne $ExpectedVersion) {
    throw 'Package version differs from the requested release'
}
$archiveRoot = "failforge-$($manifest.version)-$target"
if ($manifest.archiveRoot -ne $archiveRoot -or $manifest.archive -ne "$archiveRoot.tar.gz") {
    throw 'Package archive names differ from the manifest identity'
}

$records = @{}
foreach ($line in [IO.File]::ReadAllLines((Join-Path $PackageDirectory 'SHA256SUMS'))) {
    if ($line -notmatch '^([0-9a-f]{64})  ([A-Za-z0-9._-]+)$') { throw 'Invalid checksum record' }
    $name = $Matches[2]
    if ($records.ContainsKey($name)) { throw 'Duplicate checksum record' }
    $records[$name] = $Matches[1]
    $actual = (Get-FileHash -Algorithm SHA256 (Join-Path $PackageDirectory $name)).Hash.ToLowerInvariant()
    if ($actual -ne $records[$name]) { throw 'Package checksum mismatch' }
}
if ($records.Count -ne 2 -or -not $records.ContainsKey($manifest.archive) -or -not $records.ContainsKey('package-manifest.json')) {
    throw 'Checksums must identify the archive and its manifest exactly'
}

$extracted = "$PackageDirectory extracted"
$moved = "$PackageDirectory moved installation"
New-Item -ItemType Directory $extracted | Out-Null
tar.exe -xzf (Join-Path $PackageDirectory $manifest.archive) -C $extracted
Move-Item (Join-Path $extracted $manifest.archiveRoot) $moved
& $tool package-smoke --installation $moved --dart-sdk $DartSdk --output $SmokeOutput

# Exercise the actual target-specific uploader validation without credentials.
$sourceSha = git -C $Installation rev-parse HEAD
python (Join-Path $Installation '.github/scripts/release_upload.py') --version $manifest.version --source-sha $sourceSha --target $target --package-dir $PackageDirectory --smoke-summary (Join-Path $SmokeOutput 'summary.json') --validate-only
