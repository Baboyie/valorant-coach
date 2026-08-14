<#
.SYNOPSIS
    Turns PresentMon CSVs from Invoke-Benchmark.ps1 into the ADR-001 §29 table.

.DESCRIPTION
    Reads every run in the results directory, groups by condition, and reports
    the columns §5 asks for: average FPS, 1% low, 0.1% low, frame-time standard
    deviation, GPU %, encoder %, CPU % and RAM.

    §20 makes frame-time standard deviation the headline number, not average FPS.
    A recorder with a good average and periodic hitches is worse competitively
    than a slightly slower one that is consistent, so the table is ordered to put
    consistency in front of throughput.

    On the definition of "1% low": there are two in circulation — the mean of the
    slowest 1% of frames, and the frame rate implied by the 99th-percentile frame
    time. This uses the percentile form (as FrameView and CapFrameX do) and says
    so in the output, because quoting a low without its definition is how
    benchmark numbers stop being comparable.

.EXAMPLE
    .\Measure-Frames.ps1
    .\Measure-Frames.ps1 -ResultsDir .\results -Markdown
#>
[CmdletBinding()]
param(
    [string]$ResultsDir = (Join-Path $PSScriptRoot 'results'),
    [switch]$Markdown
)

$ErrorActionPreference = 'Stop'

if (-not (Test-Path $ResultsDir)) {
    Write-Host "No results directory at $ResultsDir — run Invoke-Benchmark.ps1 first." -ForegroundColor Yellow
    exit 1
}

# --------------------------------------------------------------- helpers ---

# Percentile over an ascending-sorted array, linear rank. Frame-time samples
# number in the thousands, so interpolation between neighbours is noise; nearest
# rank is honest and easier to reason about.
function Get-Percentile {
    param([double[]]$Sorted, [double]$Q)
    if ($Sorted.Count -eq 0) { return [double]::NaN }
    $idx = [math]::Ceiling($Q * $Sorted.Count) - 1
    if ($idx -lt 0) { $idx = 0 }
    if ($idx -ge $Sorted.Count) { $idx = $Sorted.Count - 1 }
    return $Sorted[$idx]
}

# Averages a property that may be absent from every object in the group — which
# happens whenever a run lost its counter sidecar (sampler job died, or the run
# predates the sampler). Measure-Object throws outright in that case, and one
# missing column should not take down the whole summary table.
function Get-SafeAverage {
    param($Group, [string]$Property, [int]$Digits = 1)
    $vals = @()
    foreach ($o in $Group) {
        if ($o.PSObject.Properties.Name -contains $Property) {
            $v = 0.0
            if ([double]::TryParse([string]$o.$Property, [ref]$v)) { $vals += $v }
        }
    }
    if ($vals.Count -eq 0) { return $null }
    return [math]::Round((($vals | Measure-Object -Average).Average), $Digits)
}

function Get-StdDev {
    param([double[]]$Values)
    if ($Values.Count -lt 2) { return [double]::NaN }
    $mean = ($Values | Measure-Object -Average).Average
    $sum = 0.0
    foreach ($v in $Values) { $sum += [math]::Pow($v - $mean, 2) }
    return [math]::Sqrt($sum / ($Values.Count - 1))
}

# PresentMon renamed its columns between v1 and v2 metrics. Resolve by trying
# the names we know rather than assuming a schema, so a PresentMon upgrade
# degrades to a clear error instead of silently reading the wrong column.
function Resolve-Column {
    param($Row, [string[]]$Candidates)
    foreach ($c in $Candidates) {
        if ($Row.PSObject.Properties.Name -contains $c) { return $c }
    }
    return $null
}

# ----------------------------------------------------------- per-run stats ---

function Measure-Run {
    param([string]$FrameCsv)

    $rows = Import-Csv $FrameCsv
    if ($rows.Count -eq 0) { return $null }

    $first = $rows[0]
    $ftCol = Resolve-Column $first @('FrameTime', 'msBetweenPresents', 'MsBetweenPresents')
    if (-not $ftCol) {
        throw "No frame-time column in $FrameCsv (looked for FrameTime / msBetweenPresents). Columns: $($first.PSObject.Properties.Name -join ', ')"
    }
    $gpuCol   = Resolve-Column $first @('GPUBusy', 'msGPUActive', 'GPUTime')
    $videoCol = Resolve-Column $first @('VideoBusy', 'msVideoActive')

    $ft = @()
    foreach ($r in $rows) {
        $v = 0.0
        if ([double]::TryParse($r.$ftCol, [ref]$v)) {
            # Zero/negative frame times are dropped-frame artefacts, not frames.
            if ($v -gt 0) { $ft += $v }
        }
    }
    if ($ft.Count -lt 2) { return $null }

    $sorted = [double[]]($ft | Sort-Object)
    $meanFt = ($ft | Measure-Object -Average).Average

    $stat = [ordered]@{
        File        = [IO.Path]::GetFileName($FrameCsv)
        Frames      = $ft.Count
        AvgFps      = [math]::Round(1000.0 / $meanFt, 1)
        # p99 frame time -> the FPS a 1%-worst frame corresponds to.
        Low1Fps     = [math]::Round(1000.0 / (Get-Percentile $sorted 0.99), 1)
        Low01Fps    = [math]::Round(1000.0 / (Get-Percentile $sorted 0.999), 1)
        FtMeanMs    = [math]::Round($meanFt, 3)
        FtStdDevMs  = [math]::Round((Get-StdDev $ft), 3)
        FtMaxMs     = [math]::Round(($ft | Measure-Object -Maximum).Maximum, 2)
    }

    # Per-frame GPU and video-encode busy time, averaged. VideoBusy is what
    # --track_gpu_video buys us: the encode engine split out from the rest.
    if ($gpuCol) {
        $g = @(); $v2 = 0.0
        foreach ($r in $rows) { if ([double]::TryParse($r.$gpuCol, [ref]$v2)) { $g += $v2 } }
        if ($g.Count) { $stat['GpuBusyMs'] = [math]::Round(($g | Measure-Object -Average).Average, 3) }
    }
    if ($videoCol) {
        $vv = @(); $v3 = 0.0
        foreach ($r in $rows) { if ([double]::TryParse($r.$videoCol, [ref]$v3)) { $vv += $v3 } }
        if ($vv.Count) { $stat['VideoBusyMs'] = [math]::Round(($vv | Measure-Object -Average).Average, 3) }
    }

    # Counter sidecar, if the run produced one.
    $counterCsv = $FrameCsv -replace '\.frames\.csv$', '.counters.csv'
    if (Test-Path $counterCsv) {
        $c = Import-Csv $counterCsv
        if ($c.Count -ge 2) {
            $logical = (Get-CimInstance Win32_ComputerSystem).NumberOfLogicalProcessors
            $elapsed = ([datetime]$c[-1].Time - [datetime]$c[0].Time).TotalSeconds
            foreach ($p in @('VALORANT-Win64-Shipping', 'recorder-proto')) {
                $col = "$p.CPUSeconds"
                if ($c[0].PSObject.Properties.Name -contains $col) {
                    $a = 0.0; $b = 0.0
                    $okA = [double]::TryParse($c[0].$col, [ref]$a)
                    $okB = [double]::TryParse($c[-1].$col, [ref]$b)
                    if ($okA -and $okB -and $elapsed -gt 0) {
                        $stat["$p.CpuPct"] = [math]::Round((($b - $a) / $elapsed / $logical) * 100, 2)
                    }
                }
                $wsCol = "$p.WorkingSetMB"
                if ($c[0].PSObject.Properties.Name -contains $wsCol) {
                    $ws = @(); $v4 = 0.0
                    foreach ($row in $c) { if ([double]::TryParse($row.$wsCol, [ref]$v4)) { $ws += $v4 } }
                    if ($ws.Count) { $stat["$p.RamMB"] = [math]::Round(($ws | Measure-Object -Average).Average, 0) }
                }
            }
            foreach ($eng in @('3D', 'VideoEncode')) {
                $col = "GPU.$eng.Pct"
                if ($c[0].PSObject.Properties.Name -contains $col) {
                    $g2 = @(); $v5 = 0.0
                    foreach ($row in $c) { if ([double]::TryParse($row.$col, [ref]$v5)) { $g2 += $v5 } }
                    if ($g2.Count) { $stat["GPU.$eng.Pct"] = [math]::Round(($g2 | Measure-Object -Average).Average, 1) }
                }
            }
        }
    }

    return [pscustomobject]$stat
}

# ------------------------------------------------------------------ report ---

$runs = @()
foreach ($f in Get-ChildItem $ResultsDir -Filter '*.frames.csv' | Sort-Object Name) {
    $cond = 'unknown'
    if ($f.Name -match '^(baseline|ours|shadowplay|obs)-run(\d+)') {
        $cond = $Matches[1]
    }
    $s = Measure-Run $f.FullName
    if ($s) {
        $s | Add-Member -NotePropertyName Condition -NotePropertyValue $cond -Force
        $runs += $s
    }
}

if ($runs.Count -eq 0) {
    Write-Host "No usable runs found in $ResultsDir." -ForegroundColor Yellow
    exit 1
}

Write-Host ""
Write-Host "=== per-run ===" -ForegroundColor Cyan
$runs | Format-Table Condition, Frames, AvgFps, Low1Fps, Low01Fps, FtStdDevMs, FtMaxMs -AutoSize

Write-Host "=== by condition (mean of runs) ===" -ForegroundColor Cyan
$summary = $runs | Group-Object Condition | ForEach-Object {
    $g = $_.Group
    [pscustomobject][ordered]@{
        Condition  = $_.Name
        Runs       = $g.Count
        AvgFps     = Get-SafeAverage $g 'AvgFps' 1
        Low1Fps    = Get-SafeAverage $g 'Low1Fps' 1
        Low01Fps   = Get-SafeAverage $g 'Low01Fps' 1
        FtStdDevMs = Get-SafeAverage $g 'FtStdDevMs' 3
        EncodePct  = Get-SafeAverage $g 'GPU.VideoEncode.Pct' 1
        Gpu3DPct   = Get-SafeAverage $g 'GPU.3D.Pct' 1
    }
}
$summary | Format-Table -AutoSize

# The comparison the ADR actually cares about: what did the recorder cost?
$base = $summary | Where-Object Condition -eq 'baseline'
$ours = $summary | Where-Object Condition -eq 'ours'
if ($base -and $ours) {
    Write-Host "=== recorder cost vs baseline ===" -ForegroundColor Cyan
    $dFps = [math]::Round((($ours.AvgFps - $base.AvgFps) / $base.AvgFps) * 100, 2)
    $dLow = [math]::Round((($ours.Low1Fps - $base.Low1Fps) / $base.Low1Fps) * 100, 2)
    $dStd = [math]::Round($ours.FtStdDevMs - $base.FtStdDevMs, 3)
    # The three-section format (positive;negative;zero) is what forces an explicit
    # '+' on a gain. A regression and an improvement must be visually distinct at
    # a glance, or a table like this invites reading the sign wrong.
    Write-Host ("  average FPS       : {0,8:+0.00;-0.00;0.00} %" -f $dFps)
    Write-Host ("  1% low FPS        : {0,8:+0.00;-0.00;0.00} %" -f $dLow)
    Write-Host ("  frame-time stddev : {0,8:+0.000;-0.000;0.000} ms  <- §20 headline" -f $dStd)
    Write-Host ""
    Write-Host "  Per ADR §1, none of this is ever marketed as '0% FPS loss'." -ForegroundColor DarkGray
}

if ($Markdown) {
    $md = Join-Path $ResultsDir 'summary.md'
    $lines = @()
    $lines += "| Condition | Runs | Avg FPS | 1% low | 0.1% low | Frame-time stddev (ms) | Encode % | GPU 3D % |"
    $lines += "|---|---|---|---|---|---|---|---|"
    foreach ($s in $summary) {
        $lines += "| $($s.Condition) | $($s.Runs) | $($s.AvgFps) | $($s.Low1Fps) | $($s.Low01Fps) | **$($s.FtStdDevMs)** | $($s.EncodePct) | $($s.Gpu3DPct) |"
    }
    $lines += ""
    $lines += "_1% low = FPS implied by the 99th-percentile frame time. Measured with Intel PresentMon 2.5.1 (console build, ETW only)._"
    $lines | Out-File -FilePath $md -Encoding utf8
    Write-Host "wrote $md" -ForegroundColor Cyan
}
