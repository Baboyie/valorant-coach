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

    [string]$PresentMon = 'D:\dev\tools\PresentMon.exe',

    [string]$RecorderExe = (Join-Path $PSScriptRoot '..\recorder-proto\target\release\recorder-proto.exe'),

    # The game is the measurement target, never something we attach to.
    [string]$GameProcess = 'VALORANT-Win64-Shipping.exe'
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

$sampler = Start-Job -ScriptBlock {
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
} -ArgumentList $Seconds, $counterCsv, $gameBase

# ------------------------------------------------------------- the recorder ---

$recorder = $null
if ($Condition -eq 'ours') {
    # Give the recorder a slightly longer window than the measurement so that
    # PresentMon never captures a stretch where the recorder is starting up or
    # tearing down — those frames would flatter us.
    $recSecs = $Seconds + 6
    $recOut = Join-Path $OutDir "$tag.mp4"
    Write-Host "starting recorder-proto for $recSecs s -> $recOut" -ForegroundColor Yellow
    $recorder = Start-Process -FilePath $RecorderExe `
        -ArgumentList @('record', $recSecs, $Fps, $recOut) `
        -PassThru -WindowStyle Minimized
    Start-Sleep -Seconds 3   # let capture reach steady state before measuring
}

# ---------------------------------------------------------------- PresentMon ---

Write-Host "measuring for $Seconds s — play the route now." -ForegroundColor Green

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
& $PresentMon @pmArgs
$pmExit = $LASTEXITCODE

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
}
$meta | ConvertTo-Json | Out-File -FilePath $metaJson -Encoding utf8

Write-Host ""
Write-Host "run complete: $tag" -ForegroundColor Cyan
Write-Host "analyse with: .\Measure-Frames.ps1" -ForegroundColor Cyan
