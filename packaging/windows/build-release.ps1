#requires -Version 7.0
param([ValidatePattern('^\d+\.\d+\.\d+$')][string]$Version = '1.1.0')
$ErrorActionPreference = 'Stop'
if (-not $IsWindows) { throw 'Build this Windows archive on Windows with Rust installed.' }
$projectRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..'))
$target = 'x86_64-pc-windows-msvc'
$archiveBase = "SuperSearch-v$Version-windows-x64"
$outputRoot = Join-Path $projectRoot "target\releases\$Version"
$packageRoot = Join-Path $outputRoot $archiveBase
$archivePath = Join-Path $outputRoot "$archiveBase.zip"
if ((Test-Path -LiteralPath $packageRoot) -or (Test-Path -LiteralPath $archivePath)) {
    throw "Release output already exists in $outputRoot. Review and remove the generated package before rebuilding."
}
$oldRustFlags = $env:CARGO_ENCODED_RUSTFLAGS
Push-Location $projectRoot
try {
    $sourceCommit = (& git rev-parse HEAD).Trim()
    if ($LASTEXITCODE -ne 0) { throw 'Cannot identify the source commit.' }
    if (& git status --porcelain --untracked-files=no) { throw 'Commit source changes before packaging a release.' }
    $env:CARGO_ENCODED_RUSTFLAGS = @(
        '-C', 'target-feature=+crt-static',
        "--remap-path-prefix=$projectRoot=.",
        "--remap-path-prefix=$env:USERPROFILE=.build-user"
    ) -join [char]31
    & cargo build --release --locked --target $target -p supersearch-cli -p supersearch-gui -p supersearch-tui
    if ($LASTEXITCODE -ne 0) { throw 'Release build failed.' }
    $metadataText = & cargo metadata --locked --offline --format-version 1 --filter-platform $target
    if ($LASTEXITCODE -ne 0) { throw 'Cannot read locked dependency metadata.' }
    $metadata = ($metadataText -join "`n") | ConvertFrom-Json -AsHashtable
    $packageMap = @{}
    foreach ($package in $metadata.packages) { $packageMap[$package.id] = $package }
    foreach ($member in $metadata.workspace_members) {
        if ($packageMap[$member].version -ne $Version) { throw 'Requested version differs from the workspace packages.' }
    }
    New-Item -ItemType Directory -Force -Path $packageRoot | Out-Null
    foreach ($binary in @('supersearch.exe', 'supersearch-gui.exe', 'supersearch-tui.exe')) {
        Copy-Item -LiteralPath (Join-Path $projectRoot "target\$target\release\$binary") -Destination $packageRoot
    }
    $quickStart = @"
SuperSearch $Version - Windows x64

1. Extract this entire folder to a permanent location.
2. Open supersearch-gui.exe for the desktop app.
3. Choose a folder, enter a filter, and select Scan & search.

For direct NTFS Master File Table searches, use Run as administrator.
Normal folder searches work without administrator privileges.

Command-line example (PowerShell, from this folder):
  .\supersearch.exe search --root "`$HOME\Documents" --live --name report --ignore-case
  .\supersearch.exe --help

Interactive terminal interface (run inside PowerShell or Windows Terminal):
  .\supersearch-tui.exe --folder "`$HOME\Documents"
Use Tab to edit filters, F5 to search, Esc for results, and ? for help.
Press q from the results or Ctrl+Q anywhere to quit. Terminal font size
is controlled by your terminal application.

To run the commands from anywhere, add this folder to your user PATH and
restart your terminal application. Add the folder, not the executable.

No Rust installation is required. The Microsoft C runtime is linked into
the executables. App settings are saved under %APPDATA%\supersearch.

Source, full instructions, and benchmark conditions:
https://github.com/RazvanManolache/SuperSearch

Third-party dependency license files are included under licenses/.
"@
    [IO.File]::WriteAllText((Join-Path $packageRoot 'README.txt'), $quickStart + "`n")
    $nodeMap = @{}
    foreach ($node in $metadata.resolve.nodes) { $nodeMap[$node.id] = $node }
    $queue = [Collections.Generic.Stack[string]]::new()
    foreach ($member in $metadata.workspace_members) { $queue.Push($member) }
    $included = [Collections.Generic.HashSet[string]]::new()
    while ($queue.Count) {
        $packageId = $queue.Pop()
        if (-not $included.Add($packageId)) { continue }
        foreach ($dependency in $nodeMap[$packageId].deps) {
            if (@($dependency.dep_kinds | Where-Object { $_.kind -ne 'dev' }).Count) { $queue.Push($dependency.pkg) }
        }
    }
    $notices = [Collections.Generic.List[string]]::new()
    $notices.Add("SuperSearch $Version - third-party dependency notices")
    $notices.Add('License texts are preserved in the accompanying licenses directory.')
    foreach ($packageId in ($included | Sort-Object)) {
        if ($packageId -in $metadata.workspace_members) { continue }
        $package = $packageMap[$packageId]
        $notices.Add("`n$($package.name) $($package.version)`nLicense: $($package.license)`nSource: $($package.repository)")
        $dependencyRoot = Split-Path -Parent $package.manifest_path
        $licenseFiles = @(Get-ChildItem -LiteralPath $dependencyRoot -File | Where-Object { $_.Name -match '^(LICENSE|LICENCE|COPYING|NOTICE|UNLICENSE)([-._].*)?$' })
        if ($package.license_file) {
            $declaredLicense = Join-Path $dependencyRoot $package.license_file
            if (Test-Path -LiteralPath $declaredLicense) { $licenseFiles += Get-Item -LiteralPath $declaredLicense }
        }
        if ($licenseFiles.Count) {
            $licenseDirectory = Join-Path $packageRoot "licenses\$($package.name)-$($package.version)"
            New-Item -ItemType Directory -Force -Path $licenseDirectory | Out-Null
            foreach ($licenseFile in ($licenseFiles | Sort-Object FullName -Unique)) { Copy-Item -LiteralPath $licenseFile.FullName -Destination $licenseDirectory }
        }
    }
    $rustRoot = (& rustc --print sysroot).Trim()
    $rustLicenseDirectory = Join-Path $packageRoot 'licenses\rust-standard-library'
    New-Item -ItemType Directory -Force -Path $rustLicenseDirectory | Out-Null
    Copy-Item -LiteralPath (Join-Path $rustRoot 'share\doc\rust\COPYRIGHT-library.html') -Destination $rustLicenseDirectory
    Get-ChildItem -LiteralPath (Join-Path $rustRoot 'share\doc\rust\licenses') -File | ForEach-Object { Copy-Item -LiteralPath $_.FullName -Destination $rustLicenseDirectory }
    $notices.Add("`nRust standard library`nSource: https://github.com/rust-lang/rust`nCopyright and license texts: licenses/rust-standard-library/")
    [IO.File]::WriteAllText((Join-Path $packageRoot 'THIRD-PARTY-NOTICES.txt'), ($notices -join "`n") + "`n")
    $binaryHashes = [ordered]@{}
    foreach ($binary in @('supersearch.exe', 'supersearch-gui.exe', 'supersearch-tui.exe')) { $binaryHashes[$binary] = (Get-FileHash -Algorithm SHA256 -LiteralPath (Join-Path $packageRoot $binary)).Hash.ToLowerInvariant() }
    $buildInfo = [ordered]@{
        version = $Version; target = $target; source_commit = $sourceCommit
        source_repository = 'https://github.com/RazvanManolache/SuperSearch'
        rustc = (& rustc --version).Trim(); c_runtime = 'statically linked'
        binary_sha256 = $binaryHashes
    }
    $buildInfo | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath (Join-Path $packageRoot 'BUILDINFO.json') -Encoding utf8
    [IO.Compression.ZipFile]::CreateFromDirectory($packageRoot, $archivePath, [IO.Compression.CompressionLevel]::Optimal, $true)
    $archiveHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $archivePath).Hash.ToLowerInvariant()
    [IO.File]::WriteAllText((Join-Path $outputRoot 'SHA256SUMS.txt'), "$archiveHash  $archiveBase.zip`n")
    Write-Output "Created $archivePath"
    Write-Output "SHA-256: $archiveHash"
} finally {
    if ($null -eq $oldRustFlags) { Remove-Item Env:\CARGO_ENCODED_RUSTFLAGS -ErrorAction SilentlyContinue } else { $env:CARGO_ENCODED_RUSTFLAGS = $oldRustFlags }
    Pop-Location
}
