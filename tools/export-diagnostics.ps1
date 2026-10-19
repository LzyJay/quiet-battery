param(
    [string]$Executable = (Join-Path $PSScriptRoot '../dist/quiet-battery.exe'),
    [string]$OutputPath = (Join-Path $PSScriptRoot '../validation/diagnostics.json'),
    [switch]$IncludeIdentifiers
)
$ErrorActionPreference = 'Stop'
$qbExecutable = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($Executable)
$qbOutput = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($OutputPath)
$qbInfo = [Diagnostics.ProcessStartInfo]::new()
$qbInfo.FileName = $qbExecutable
$qbInfo.Arguments = if ($IncludeIdentifiers) { '--diagnose --include-identifiers' } else { '--diagnose' }
$qbInfo.UseShellExecute = $false
$qbInfo.CreateNoWindow = $true
$qbInfo.RedirectStandardOutput = $true
$qbInfo.RedirectStandardError = $true
$qbInfo.StandardOutputEncoding = [Text.UTF8Encoding]::new($false)
$qbInfo.StandardErrorEncoding = [Text.UTF8Encoding]::new($false)
$qbProcess = [Diagnostics.Process]::new()
$qbProcess.StartInfo = $qbInfo
try {
    if (-not $qbProcess.Start()) { throw 'Could not start diagnostic process.' }
    $qbRead = $qbProcess.StandardOutput.ReadToEndAsync()
    $qbErrorRead = $qbProcess.StandardError.ReadToEndAsync()
    if (-not $qbProcess.WaitForExit(20000)) {
        $qbProcess.Kill()
        throw 'Diagnostic process exceeded 20 seconds; no complete report was saved.'
    }
    $qbJson = $qbRead.GetAwaiter().GetResult()
    $qbErrorText = $qbErrorRead.GetAwaiter().GetResult()
    if ($qbProcess.ExitCode -ne 0) { throw "Diagnostic process failed: $qbJson $qbErrorText" }
    $qbReport = $qbJson | ConvertFrom-Json
    if ($qbReport.mode -ne 'passive-hid-inventory') { throw 'Unexpected diagnostic output.' }
    [IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($qbOutput)) | Out-Null
    [IO.File]::WriteAllText($qbOutput, $qbJson, [Text.UTF8Encoding]::new($false))
    Write-Output "Saved $($qbReport.collections.Count) HID collections to $qbOutput; complete=$($qbReport.complete), truncated=$($qbReport.truncated)"
} finally { $qbProcess.Dispose() }
