param([string]$DriveRoot = 'C:\', [int]$MeasuredRuns = 1, [string]$FirstMethod = 'supersearch_scan', [switch]$Clean)
$ErrorActionPreference = 'Stop'
$repoRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..'))
$benchTemp = Join-Path $repoRoot 'target\readme-benchmark'
$runsDir = Join-Path $benchTemp 'runs'
if ($Clean -and (Test-Path $benchTemp)) { Remove-Item -Recurse -Force $benchTemp }
New-Item -ItemType Directory -Force -Path $runsDir | Out-Null
$runsLog = Join-Path $benchTemp 'runs.jsonl'
$benchExe = Join-Path $repoRoot 'target\release\supersearch.exe'
$windowsPsExe = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
$liveConfig = Join-Path $benchTemp 'full-drive-settings.json'
[IO.File]::WriteAllText($liveConfig, '{}')
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = [Security.Principal.WindowsPrincipal]::new($identity)
$isElevated = $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
$methods = @('supersearch_scan', 'windows_powershell_recursive')
if ($FirstMethod -eq 'windows_powershell_recursive') { $methods = @('windows_powershell_recursive', 'supersearch_scan') }

# Recursive baseline that skips directory junctions/symlinks (reparse
# points), includes hidden and system files (-Force), and matches the
# exact filename case-insensitively at the provider level. PS 5.1
# Get-ChildItem -Recurse would follow junctions, so the walk is manual.
# Emits matching FullNames on stdout and a WALK dirs/files line on stderr.
$walkTemplate = @'
[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)
$hits = New-Object Collections.Generic.List[string]
$stack = New-Object Collections.Generic.Stack[string]
$stack.Push('__ROOT__')
$dirs = 0; $files = 0
while ($stack.Count -gt 0) {
  $dir = $stack.Pop(); $dirs++
  try { $items = Get-ChildItem -LiteralPath $dir -Force -ErrorAction Stop } catch { continue }
  foreach ($it in $items) {
    try { $isDir = $it.PSIsContainer } catch { continue }
    if ($isDir) {
      if (($it.Attributes -band [IO.FileAttributes]::ReparsePoint) -eq 0) { $stack.Push($it.FullName) }
    } else {
      $files++
      if ($it.Name -eq 'notepad.exe') { $hits.Add($it.FullName) }
    }
  }
}
$hits | ForEach-Object { $_ }
[Console]::Error.WriteLine("WALK dirs=$dirs files=$files")
exit 0
'@

$runIndex = if (Test-Path $runsLog) { (Get-Content $runsLog | Measure-Object -Line).Lines } else { 0 }
$referencePaths = $null
for ($round = 0; $round -lt $MeasuredRuns; $round++) {
    for ($offset = 0; $offset -lt $methods.Count; $offset++) {
        $method = $methods[($offset + $round) % $methods.Count]
        $runIndex++
        Write-Output "Run ${runIndex}: $method, full $DriveRoot search started"
        $startInfo = [Diagnostics.ProcessStartInfo]::new()
        $startInfo.FileName = if ($method -eq 'supersearch_scan') { $benchExe } else { $windowsPsExe }
        $startInfo.UseShellExecute = $false
        $startInfo.CreateNoWindow = $true
        $startInfo.RedirectStandardOutput = $true
        $startInfo.RedirectStandardError = $true
        $startInfo.StandardOutputEncoding = [Text.Encoding]::UTF8
        if ($method -eq 'supersearch_scan') {
            $arguments = @('--config', $liveConfig, 'search', '--root', $DriveRoot, '--live', '--include-hidden', '--glob', '*\notepad.exe', '--ignore-case')
        } else {
            $escapedRoot = $DriveRoot.Replace("'", "''")
            $queryCommand = $walkTemplate.Replace('__ROOT__', $escapedRoot)
            $arguments = @('-NoProfile', '-NonInteractive', '-EncodedCommand', [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($queryCommand)))
        }
        # Windows PowerShell 5.1 (.NET Framework) has no ArgumentList
        # collection, so pass one quoted command line instead. A
        # trailing backslash must be doubled or it escapes the closing
        # quote (e.g. --root "C:\").
        $quoted = foreach ($argument in $arguments) {
            $e = $argument -replace '"', '\"'
            if ($e.EndsWith('\')) { $e += '\' }
            '"' + $e + '"'
        }
        $startInfo.Arguments = $quoted -join ' '
        $outFile = Join-Path $runsDir ("run-{0:00}-{1}.out.log" -f $runIndex, $method)
        $errFile = Join-Path $runsDir ("run-{0:00}-{1}.err.log" -f $runIndex, $method)
        $startedUtc = (Get-Date).ToUniversalTime().ToString('o')
        $timer = [Diagnostics.Stopwatch]::StartNew()
        $process = [Diagnostics.Process]::Start($startInfo)
        $stdoutTask = $process.StandardOutput.ReadToEndAsync()
        $stderrTask = $process.StandardError.ReadToEndAsync()
        $lastProgress = 0
        while (-not $process.WaitForExit(1000)) {
            if ($timer.Elapsed.TotalSeconds - $lastProgress -ge 30) {
                Write-Output "$method still searching: $([math]::Round($timer.Elapsed.TotalSeconds)) seconds"
                $lastProgress = $timer.Elapsed.TotalSeconds
            }
        }
        $stdout = $stdoutTask.GetAwaiter().GetResult()
        $stderr = $stderrTask.GetAwaiter().GetResult()
        $timer.Stop()
        [IO.File]::WriteAllText($outFile, $stdout)
        [IO.File]::WriteAllText($errFile, $stderr)
        $paths = @($stdout -split '\r?\n' | Where-Object { $_.Length -gt 0 } | ForEach-Object { $_.Trim('"').ToLowerInvariant() } | Sort-Object -Unique)
        $record = [ordered]@{
            run = $runIndex; method = $method; started_utc = $startedUtc
            elapsed_ms = [math]::Round($timer.Elapsed.TotalMilliseconds, 3)
            exit_code = $process.ExitCode; matches = $paths.Count
            backend = ''; scanned = $null; out_file = $outFile; err_file = $errFile
        }
        if ($process.ExitCode -ne 0) {
            $record['note'] = "unexpected failure, exit $($process.ExitCode); stderr tail saved"
            ($record | ConvertTo-Json -Compress) | Add-Content $runsLog
            throw "$method run $runIndex failed with exit code $($process.ExitCode); measurement preserved in $errFile"
        }
        if ($method -eq 'supersearch_scan') {
            if ($stderr -match '(\d+) scanned') { $record['scanned'] = [long]$Matches[1] }
            $record['backend'] = if ($stderr -match 'via MFT') { 'NTFS MFT' } else { 'parallel folder walk' }
        } else {
            if ($stderr -match 'WALK dirs=(\d+) files=(\d+)') { $record['scanned'] = [long]$Matches[2] }
            $record['backend'] = 'provider walk, junctions skipped'
        }
        if ($paths.Count -eq 0) {
            $record['note'] = 'no matches; recorded, not used for coverage reference'
            ($record | ConvertTo-Json -Compress) | Add-Content $runsLog
            throw "$method run $runIndex found no matching files; see $outFile"
        }
        if ($null -eq $referencePaths) { $referencePaths = $paths }
        $differences = @(Compare-Object $referencePaths $paths)
        $record['coverage_diff_vs_first'] = $differences.Count
        ($record | ConvertTo-Json -Compress) | Add-Content $runsLog
        Write-Output "$method run $runIndex finished in $([math]::Round($timer.Elapsed.TotalSeconds,3)) s; $($paths.Count) matches; backend: $($record['backend'])"
    }
}

# Aggregate every recorded run (this pair plus earlier pairs).
$all = Get-Content $runsLog | ForEach-Object { $_ | ConvertFrom-Json }
$measurements = foreach ($method in @('supersearch_scan', 'windows_powershell_recursive')) {
    $samples = @($all | Where-Object { $_.method -eq $method -and $_.exit_code -eq 0 } | Sort-Object run)
    if ($samples.Count -eq 0) { continue }
    $sorted = @($samples.elapsed_ms | Sort-Object)
    $middle = [int][math]::Floor($sorted.Count / 2)
    $median = if ($sorted.Count % 2) { $sorted[$middle] } else { ($sorted[$middle - 1] + $sorted[$middle]) / 2 }
    [ordered]@{
        method = $method; samples = $samples.Count
        median_ms = [math]::Round($median, 3); minimum_ms = [math]::Round($sorted[0], 3); maximum_ms = [math]::Round($sorted[-1], 3)
        samples_ms = @($samples.elapsed_ms)
    }
}
$ssBackend = @($all | Where-Object { $_.method -eq 'supersearch_scan' } | Select-Object -ExpandProperty backend -Unique)
$orderSeen = @($all | Sort-Object run | ForEach-Object { if ($_.method -eq 'supersearch_scan') { 'SS' } else { 'PS' } }) -join ','
$receipt = [ordered]@{
    date = (Get-Date).ToString('yyyy-MM-dd'); corpus = "Full $DriveRoot drive"
    root = $DriveRoot; query = 'Exact filename notepad.exe, recursive, case-insensitive, including hidden entries'
    elevated = $isElevated; supersearch_backend = @($ssBackend -join ' / ')
    windows_baseline = 'Windows PowerShell 5.1 manual recursion (Get-ChildItem -Force per directory, reparse-point directories skipped, -File semantics, exact name match); hidden and system files included; not the Windows Search index or Explorer UI'
    timing_scope = 'Both include process startup, complete drive search, and stdout/stderr retrieval'
    cache = 'OS caches not flushed; colder first passes included in every sample'
    run_order = $orderSeen; measured_runs_total = $all.Count
    result_paths_equal_across_runs = [bool]($all.coverage_diff_vs_first -notcontains $null -and (@($all.coverage_diff_vs_first | Where-Object { $_ -gt 0 }).Count -eq 0))
    excluded = @('index creation', 'Explorer window rendering')
    measurements = @($measurements)
}
$receipt | ConvertTo-Json -Depth 8 | Set-Content (Join-Path $repoRoot 'docs\benchmarks\search-comparison.json') -Encoding utf8
$measurements | ForEach-Object { [pscustomobject]$_ } | Select-Object method, samples, median_ms, minimum_ms, maximum_ms | Format-Table
Write-Output 'Aggregate receipt written to docs/benchmarks/search-comparison.json'
