<#
.SYNOPSIS
    Runs one measured pass of the ADR-001 §29 acceptance benchmark.

.DESCRIPTION
    One invocation = one cell of the matrix = one run. The matrix is
    (condition x run index), and the ADR asks for three runs per condition over
    an identical route, so this script is deliberately small and repeatable
    rather than clever: it does not loop over conditions, because each run needs
    a human driving the same route in the Range.

    Measurement is Intel PresentMon (ADR §5). PresentMon reads ETW and never
    touches the game process, which is both the honest way to measure and the
    only Vanguard-safe way. Nothing here injects, hooks, or opens a handle to
    VALORANT — see ADR §1, which forbids that outright.

    Note we use the PresentMon *console* build, not the 2.x MSI. The MSI ships an
    in-game overlay that hooks the target process to draw itself; that is exactly
    the pattern §1 rules out, and pointing it at a Vanguard-protected game would
    be reckless. Console build only.

.PARAMETER Condition
    'baseline' — no recorder running. The control.
    'ours'     — recorder-proto recording concurrently.

.EXAMPLE
    .\Invoke-Benchmark.ps1 -Condition baseline -Run 1
    .\Invoke-Benchmark.ps1 -Condition ours -Run 1
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [ValidateSet('baseline', 'ours')]
    [string]$Condition,

    [int]$Run = 1,

    # 60 s is long enough that a single stutter cannot dominate the percentiles,
    # and short enough that a human can hold an identical route across three runs.
    [int]$Seconds = 60,

    [int]$Fps = 60,

    [string]$OutDir = (Join-Path $PSScriptRoot 'results'),

    # Recorded video goes off the synced drive. The CSVs are small and belong
    # with the repo; a 90 MB MP4 per run does not want to be in OneDrive.
    [string]$VideoDir = 'D:\dev\bench-video',

    [string]$PresentMon = 'D:\dev\tools\PresentMon.exe',

    [string]$RecorderExe = (Join-Path $PSScriptRoot '..\recorder-proto\target\release\recorder-proto.exe'),

    # The game is the measurement target, never something we attach to.
    [string]$GameProcess = 'VALORANT-Win64-Shipping.exe',

    # Proceed even when another process is already using the video-encode engine.
    [switch]$AllowEncoderContention
)

$ErrorActionPreference = 'Stop'

function Fail($msg) { Write-Host "ERROR: $msg" -ForegroundColor Red; exit 1 }

# --------------------------------------------------------------- preflight ---

if (-not (Test-Path $PresentMon)) { Fail "PresentMon not found at $PresentMon" }

$gameBase = [IO.Path]::GetFileNameWithoutExtension($GameProcess)
$game = Get-Process -Name $gameBase -ErrorAction SilentlyContinue
if (-not $game) {
    Fail "$GameProcess is not running. Start Valorant and get into the Range first."
}

if ($Condition -eq 'ours' -and -not (Test-Path $RecorderExe)) {
    Fail "recorder-proto not built. Run: cargo build --release"
}

# A minimised game is the one failure that produces a full set of plausible CSVs
# and means nothing. WGC composites nothing for an iconic window, so the recorder
# would capture zero frames while PresentMon happily recorded the game's own
# presents — a "recording" condition that never recorded, indistinguishable from a
# spectacularly good result. The capture ring is also sized once at session start
# from the iconic placeholder (160x28), so restoring the window mid-run does not
# rescue it (ADR §8, "Known gap").
Add-Type -Namespace Bench -Name Win -MemberDefinition @'
[DllImport("user32.dll")] public static extern bool IsIconic(IntPtr hWnd);
'@ -ErrorAction SilentlyContinue
if ($game.MainWindowHandle -ne [IntPtr]::Zero -and [Bench.Win]::IsIconic($game.MainWindowHandle)) {
    Fail "Valorant is minimised. Bring it to the foreground, at the resolution you intend to measure, before starting a run."
}

# PresentMon needs an ETW session, which needs elevation. Better to say so now
# than to fail 60 seconds into a run the user has already played.
$isAdmin = ([Security.Principal.WindowsPrincipal] `
    [Security.Principal.WindowsIdentity]::GetCurrent()
    ).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $isAdmin) {
    Fail "Not elevated. PresentMon needs admin to open an ETW session. Re-run this script from an elevated PowerShell."
}

# Another process already on the encode engine invalidates the comparison twice
# over: the "baseline" is not recorder-free, and our NVENC path then competes with
# it for the same silicon. Measured on this rig: Discord's clips helper sat at a
# constant 9.4% through all three baseline runs, which is not noise.
$encoderHogs = @()
try {
    $encCounters = Get-Counter -Counter "\GPU Engine(*engtype_VideoEncode)\Utilization Percentage" -ErrorAction Stop
    foreach ($s in $encCounters.CounterSamples) {
        if ($s.CookedValue -gt 1.0 -and $s.InstanceName -match 'pid_(\d+)_') {
            $hogId = [int]$Matches[1]
            $hp = Get-Process -Id $hogId -ErrorAction SilentlyContinue
            if ($hp -and $hp.ProcessName -ne 'recorder-proto') {
                $encoderHogs += "$($hp.ProcessName) (pid $hogId) at $([math]::Round($s.CookedValue,1))%"
            }
        }
    }
} catch { }
if ($encoderHogs.Count -gt 0) {
    Write-Host "Another process is using the GPU video-encode engine:" -ForegroundColor Yellow
    $encoderHogs | ForEach-Object { Write-Host "    $_" -ForegroundColor Yellow }
    if (-not $AllowEncoderContention) {
        Fail "Close it and re-run, or pass -AllowEncoderContention to measure anyway (and say so in the writeup)."
    }
    Write-Host "  proceeding anyway (-AllowEncoderContention)" -ForegroundColor Yellow
}

New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$tag = "$Condition-run$Run-$stamp"
$frameCsv = Join-Path $OutDir "$tag.frames.csv"
$counterCsv = Join-Path $OutDir "$tag.counters.csv"
$metaJson = Join-Path $OutDir "$tag.meta.json"

Write-Host ""
Write-Host "=== ADR-001 §29 benchmark run ===" -ForegroundColor Cyan
Write-Host "  condition : $Condition"
Write-Host "  run       : $Run"
Write-Host "  duration  : $Seconds s"
Write-Host "  target    : $GameProcess (pid $($game.Id))"
Write-Host "  frames -> $frameCsv"
Write-Host ""

# ------------------------------------------------- process/GPU counter sampler ---
#
# PresentMon gives frame timing but not CPU%, RAM, or GPU engine split. Those
# come from perf counters, sampled from a background job at 1 Hz. GPU Engine
# counters are per-process and per-engine, so summing the engtype_VideoEncode
# instances is what produces the "encoder %" column §5 asks for.

$samplerBlock = {
    param($seconds, $outCsv, $gameBase)

    $rows = @()
    $deadline = (Get-Date).AddSeconds($seconds)
    while ((Get-Date) -lt $deadline) {
        $t = Get-Date
        $row = [ordered]@{ Time = $t.ToString('o') }

        foreach ($p in @($gameBase, 'recorder-proto')) {
            $proc = Get-Process -Name $p -ErrorAction SilentlyContinue | Select-Object -First 1
            if ($proc) {
                $row["$p.WorkingSetMB"] = [math]::Round($proc.WorkingSet64 / 1MB, 1)
                $row["$p.CPUSeconds"] = [math]::Round($proc.TotalProcessorTime.TotalSeconds, 3)
            } else {
                $row["$p.WorkingSetMB"] = $null
                $row["$p.CPUSeconds"] = $null
            }
        }

        foreach ($eng in @('3D', 'VideoEncode')) {
            $sum = 0.0
            try {
                $c = Get-Counter -Counter "\GPU Engine(*engtype_$eng)\Utilization Percentage" -ErrorAction Stop
                $sum = ($c.CounterSamples | Measure-Object -Property CookedValue -Sum).Sum
            } catch { $sum = $null }
            $row["GPU.$eng.Pct"] = if ($null -eq $sum) { $null } else { [math]::Round($sum, 2) }
        }

        $rows += [pscustomobject]$row
        Start-Sleep -Milliseconds 1000
    }
    $rows | Export-Csv -Path $outCsv -NoTypeInformation -Encoding utf8
}

# ------------------------------------------------------------- the recorder ---

$recorder = $null
if ($Condition -eq 'ours') {
    # Longer window than the measurement so PresentMon never sees the recorder
    # starting up or tearing down — those frames would flatter us. The margin is
    # generous because the recorder starts *before* the focus wait, and the
    # operator may take a while to click into the game; a recorder that expired
    # mid-measurement would look like a clean run that quietly stopped recording.
    $recSecs = $Seconds + 90
    New-Item -ItemType Directory -Force -Path $VideoDir | Out-Null
    $recOut = Join-Path $VideoDir "$tag.mp4"
    Write-Host "starting recorder-proto for $recSecs s -> $recOut" -ForegroundColor Yellow
    $recorder = Start-Process -FilePath $RecorderExe `
        -ArgumentList @('record', $recSecs, $Fps, $recOut) `
        -PassThru -WindowStyle Minimized
    Start-Sleep -Seconds 3   # let capture reach steady state before measuring
}

# ------------------------------------------------------------ wait for focus ---
#
# Nothing is measured until Valorant is genuinely the foreground window.
#
# This is not politeness, it is the difference between a valid run and a wasted
# one. Valorant caps itself to 30 fps in the background, so every frame recorded
# while the game is unfocused lands at ~33 ms and poisons exactly the percentile
# metrics §20 calls the headline. The first version of this harness started
# measuring immediately, which meant it always caught the operator alt-tabbing
# in from the console: measured contamination was 1.7-3.9% of wall time in the
# baseline condition and 3.4-9.2% in the recording condition.
#
# That asymmetry is the dangerous part. Starting the recorder steals focus, so
# the bias landed on `ours` and looked exactly like recorder overhead. A harness
# that damages one arm of its own comparison is worse than no harness.

Add-Type -Namespace Bench -Name Fg -MemberDefinition @'
[DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
'@ -ErrorAction SilentlyContinue

$gameHwnd = $game.MainWindowHandle
function Test-GameFocused { return ([Bench.Fg]::GetForegroundWindow() -eq $gameHwnd) }

if (-not (Test-GameFocused)) {
    Write-Host ""
    Write-Host "  >>> Click into Valorant now. Measurement starts once it has focus." -ForegroundColor Cyan
    $waitUntil = (Get-Date).AddSeconds(120)
    while (-not (Test-GameFocused)) {
        if ((Get-Date) -gt $waitUntil) { Fail "Valorant never came to the foreground within 120 s." }
        Start-Sleep -Milliseconds 200
    }
}
# Let the game settle back to its unthrottled frame rate before measuring.
Start-Sleep -Seconds 2

# ---------------------------------------------------------------- PresentMon ---

$sampler = Start-Job -ScriptBlock $samplerBlock -ArgumentList $Seconds, $counterCsv, $gameBase

Write-Host "measuring for $Seconds s — play the route now. DO NOT ALT-TAB." -ForegroundColor Green

$pmArgs = @(
    '--process_name', $GameProcess
    '--output_file', $frameCsv
    '--timed', $Seconds
    '--terminate_after_timed'
    '--track_gpu_video'      # split encode work from the rest of the GPU (§5 "encoder %")
    '--v2_metrics'
    '--stop_existing_session'
    '--no_console_stats'
)

# PresentMon runs detached so this thread can watch focus at 10 Hz for the whole
# measurement. A run that loses focus is not silently averaged in — it is
# reported, because the operator is the only one who can decide to redo it.
$pm = Start-Process -FilePath $PresentMon -ArgumentList $pmArgs -PassThru -NoNewWindow
$focusChecks = 0
$focusLost = 0
while (-not $pm.HasExited) {
    $focusChecks++
    if (-not (Test-GameFocused)) { $focusLost++ }
    Start-Sleep -Milliseconds 100
}
$pmExit = $pm.ExitCode

$focusLostPct = 0.0
if ($focusChecks -gt 0) { $focusLostPct = [math]::Round(($focusLost / $focusChecks) * 100, 2) }
if ($focusLost -gt 0) {
    Write-Host ""
    Write-Host "  WARNING: Valorant lost focus for $focusLostPct% of this run." -ForegroundColor Red
    Write-Host "  The game throttles to 30 fps in the background, so this run's 1% and" -ForegroundColor Red
    Write-Host "  0.1% lows are contaminated. Re-run it before using the numbers." -ForegroundColor Red
} else {
    Write-Host "  focus held for the whole run." -ForegroundColor DarkGray
}

# ------------------------------------------------------------------ teardown ---

if ($recorder -and -not $recorder.HasExited) {
    Write-Host "waiting for recorder to finalise the mp4..." -ForegroundColor Yellow
    # Do not kill it: finish() writes the moov atom, and a killed recorder
    # leaves an unplayable file that looks like an encoder bug.
    $null = $recorder.WaitForExit(30000)
    if (-not $recorder.HasExited) {
        Write-Host "recorder still running after 30 s; leaving it alone." -ForegroundColor Yellow
    }
}

Wait-Job $sampler -Timeout 30 | Out-Null
Receive-Job $sampler -ErrorAction SilentlyContinue | Out-Null
Remove-Job $sampler -Force -ErrorAction SilentlyContinue

if ($pmExit -ne 0) { Fail "PresentMon exited $pmExit" }
if (-not (Test-Path $frameCsv)) { Fail "PresentMon produced no CSV" }

# Record the machine with the numbers. A benchmark figure without the rig it came
# from is exactly the kind of claim ADR §6 says invalidates a report.
$cpu = (Get-CimInstance Win32_Processor | Select-Object -First 1).Name
$gpu = (Get-CimInstance Win32_VideoController | Select-Object -First 1).Name
$meta = [ordered]@{
    condition   = $Condition
    run         = $Run
    seconds     = $Seconds
    targetFps   = $Fps
    timestamp   = $stamp
    cpu         = $cpu.Trim()
    gpu         = $gpu.Trim()
    ramGB       = [math]::Round((Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory / 1GB, 0)
    presentMon  = '2.5.1 (console)'
    frameCsv    = [IO.Path]::GetFileName($frameCsv)
    counterCsv  = [IO.Path]::GetFileName($counterCsv)
    # Validity, stored with the run rather than remembered. A run whose focus
    # slipped is not a slightly worse run, it is a different measurement.
    focusLostPct     = $focusLostPct
    encoderContention = $encoderHogs
}
$meta | ConvertTo-Json | Out-File -FilePath $metaJson -Encoding utf8

Write-Host ""
Write-Host "run complete: $tag" -ForegroundColor Cyan
Write-Host "analyse with: .\Measure-Frames.ps1" -ForegroundColor Cyan
