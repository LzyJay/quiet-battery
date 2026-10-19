param(
    [ValidateRange(30, 7200)][int]$DurationSeconds = 30,
    [ValidateRange(1, 60)][int]$SampleSeconds = 3,
    [int]$TargetProcessId = 0,
    [string]$OutputDirectory = ''
)
$ErrorActionPreference = 'Stop'
$projectRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$statePath = Join-Path $projectRoot '.build/runtime-status.json'
$releasePath = Join-Path $projectRoot 'dist/quiet-battery.exe'
if ($TargetProcessId -eq 0) {
    $initialState = Get-Content -LiteralPath $statePath -Raw | ConvertFrom-Json
    $TargetProcessId = [int]$initialState.pid
}
$targetProcess = Get-Process -Id $TargetProcessId
if (-not [StringComparer]::OrdinalIgnoreCase.Equals([IO.Path]::GetFullPath($targetProcess.Path), $releasePath)) {
    throw 'Refusing to sample a process outside this project release.'
}
if (-not $OutputDirectory) {
    $OutputDirectory = Join-Path $projectRoot ('validation/resources-' + (Get-Date -Format 'yyyyMMdd-HHmmss'))
}
$resolvedOutput = [IO.Path]::GetFullPath($OutputDirectory)
if (-not $resolvedOutput.StartsWith($projectRoot + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
    throw 'OutputDirectory must stay inside the project.'
}
[IO.Directory]::CreateDirectory($resolvedOutput) | Out-Null
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class QuietBatterySampleResources {
    [DllImport("user32.dll")] public static extern uint GetGuiResources(IntPtr process, uint flags);
}
'@
$samples = [Collections.Generic.List[object]]::new()
$executableHash = (Get-FileHash -LiteralPath $releasePath -Algorithm SHA256).Hash
$targetProcess.Refresh()
$startCpu = $targetProcess.TotalProcessorTime.TotalSeconds
$startUtc = [DateTime]::UtcNow
$clock = [Diagnostics.Stopwatch]::StartNew()
$failure = $null
$invalidStatusSamples = 0
Write-Output "Sampling process $TargetProcessId for $DurationSeconds seconds. Evidence: $resolvedOutput"
try {
    do {
        $targetProcess.Refresh()
        if ($targetProcess.HasExited) { throw 'Application exited during resource sampling.' }
        $observedState = $null
        $statusProblem = $null
        $statusAge = $null
        try {
            $candidateState = Get-Content -LiteralPath $statePath -Raw | ConvertFrom-Json
            $statusAge = [DateTimeOffset]::UtcNow.ToUnixTimeSeconds() - [long]$candidateState.updated_unix
            if ([int]$candidateState.pid -ne $TargetProcessId) {
                $statusProblem = 'wrong_process'
            } elseif ($null -eq $candidateState.updated_unix -or $statusAge -lt -5 -or $statusAge -gt 60) {
                $statusProblem = 'stale_timestamp'
            } else { $observedState = $candidateState }
        } catch { $statusProblem = 'missing_or_invalid_snapshot' }
        if ($statusProblem) { $invalidStatusSamples++ }
        $samples.Add([pscustomobject]@{
            elapsed_seconds = [Math]::Round($clock.Elapsed.TotalSeconds, 3)
            utc = [DateTime]::UtcNow.ToString('o')
            working_set_bytes = $targetProcess.WorkingSet64
            private_bytes = $targetProcess.PrivateMemorySize64
            cpu_seconds = $targetProcess.TotalProcessorTime.TotalSeconds - $startCpu
            handles = $targetProcess.HandleCount
            threads = $targetProcess.Threads.Count
            gdi_objects = [QuietBatterySampleResources]::GetGuiResources($targetProcess.Handle, 0)
            user_objects = [QuietBatterySampleResources]::GetGuiResources($targetProcess.Handle, 1)
            status_valid = ($null -eq $statusProblem)
            status_problem = $statusProblem
            status_age_seconds = $statusAge
            panel_open = $observedState.panel_open
            busy = $observedState.busy
        })
        $samples | Export-Csv -LiteralPath (Join-Path $resolvedOutput 'resources.csv') -NoTypeInformation -Encoding utf8
        if ($clock.Elapsed.TotalSeconds -ge $DurationSeconds) { break }
        $remaining = $DurationSeconds - $clock.Elapsed.TotalSeconds
        Start-Sleep -Milliseconds ([int](1000 * [Math]::Min($SampleSeconds, $remaining)))
    } while ($true)
} catch {
    $failure = $_.Exception.Message
} finally {
    $clock.Stop()
    $cpuSeconds = $(if ($samples.Count) { $samples[-1].cpu_seconds } else { 0 })
    $appLeftRunning = $false
    try {
        $targetProcess.Refresh()
        $appLeftRunning = -not $targetProcess.HasExited
        if ($appLeftRunning) { $cpuSeconds = $targetProcess.TotalProcessorTime.TotalSeconds - $startCpu }
    } catch {
        if (-not $failure) { $failure = $_.Exception.Message }
    }
    if ($invalidStatusSamples -and -not $failure) {
        $failure = "Application status was missing, stale, or from another process in $invalidStatusSamples samples. Resource measurements remain in resources.csv."
    }
    $elapsed = [Math]::Max(0.001, $clock.Elapsed.TotalSeconds)
    $summary = [ordered]@{
        pid = $TargetProcessId
        executable = $releasePath
        executable_sha256 = $executableHash
        start_utc = $startUtc.ToString('o')
        requested_duration_seconds = $DurationSeconds
        duration_seconds = $clock.Elapsed.TotalSeconds
        completed = ($null -eq $failure -and $clock.Elapsed.TotalSeconds -ge $DurationSeconds)
        failure = $failure
        samples = $samples.Count
        valid_status_samples = $samples.Count - $invalidStatusSamples
        invalid_status_samples = $invalidStatusSamples
        logical_processors = [Environment]::ProcessorCount
        cpu_seconds = $cpuSeconds
        cpu_percent_one_core = 100 * $cpuSeconds / $elapsed
        cpu_percent_whole_machine = 100 * $cpuSeconds / $elapsed / [Environment]::ProcessorCount
        working_set_min_bytes = ($samples.working_set_bytes | Measure-Object -Minimum).Minimum
        working_set_max_bytes = ($samples.working_set_bytes | Measure-Object -Maximum).Maximum
        private_bytes_min = ($samples.private_bytes | Measure-Object -Minimum).Minimum
        private_bytes_max = ($samples.private_bytes | Measure-Object -Maximum).Maximum
        handles_first = $(if ($samples.Count) { $samples[0].handles })
        handles_last = $(if ($samples.Count) { $samples[-1].handles })
        gdi_objects_first = $(if ($samples.Count) { $samples[0].gdi_objects })
        gdi_objects_last = $(if ($samples.Count) { $samples[-1].gdi_objects })
        user_objects_first = $(if ($samples.Count) { $samples[0].user_objects })
        user_objects_last = $(if ($samples.Count) { $samples[-1].user_objects })
        panel_open_samples = @($samples | Where-Object { $_.panel_open }).Count
        app_left_running = $appLeftRunning
        physical_unplug_tested = $false
        real_reboot_tested = $false
    }
    $summary | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath (Join-Path $resolvedOutput 'summary.json') -Encoding utf8
    $summary | ConvertTo-Json -Depth 8
    $targetProcess.Dispose()
}
if ($failure) { throw $failure }
