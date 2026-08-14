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
    [switch]$Markdown,
    # Average contaminated runs into the summary anyway. For inspecting damage,
    # never for reporting.
    [switch]$IncludeContaminated
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
        # Share of wall time spent on frames slower than 25 ms (below 40 fps).
        # Valorant caps itself to 30 fps when it is not the foreground window, so
        # a run with a meaningful figure here spent part of its life backgrounded
        # and its percentile metrics are measuring alt-tab, not the recorder.
        # Checked from the frame data itself so an old or hand-run CSV cannot
        # smuggle a contaminated run into a table on the harness's say-so.
        ThrottlePct = [math]::Round((($ft | Where-Object { $_ -gt 25 } | Measure-Object -Sum).Sum / ($ft | Measure-Object -Sum).Sum) * 100, 2)
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

    # Focus metadata, if the run produced it. This exists because the
    # frame-based throttle check has a blind spot: an alt-tab under ~2 s barely
    # engages the background frame cap, so ThrottlePct stays near zero while
    # the transition hitches still poison the 0.1% low (measured: 58.8 vs ~125
    # on otherwise identical runs). The harness's 10 Hz focus poll sees what
    # the frame times hide.
    $metaJson = $FrameCsv -replace '\.frames\.csv$', '.meta.json'
    if (Test-Path $metaJson) {
        $meta = Get-Content $metaJson -Raw | ConvertFrom-Json
        if ($null -ne $meta.focusLostPct) {
            $stat['FocusLostPct'] = [double]$meta.focusLostPct
        }
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
$runs | Format-Table Condition, Frames, AvgFps, Low1Fps, Low01Fps, FtStdDevMs, FtMaxMs, ThrottlePct -AutoSize

# Refuse to let a contaminated run pass quietly into a summary. 1% is a generous
# threshold: a clean run on a focused game should be 0.00.
$dirty = @($runs | Where-Object { $_.ThrottlePct -gt 1.0 })

# Any recorded focus loss disqualifies independently of frame times — see the
# note where FocusLostPct is read.
$focusDirty = @($runs | Where-Object {
    $_.PSObject.Properties.Name -contains 'FocusLostPct' -and $_.FocusLostPct -gt 0.5
})
if ($focusDirty.Count -gt 0) {
    Write-Host "WARNING: $($focusDirty.Count) run(s) lost window focus during measurement:" -ForegroundColor Red
    foreach ($d in $focusDirty) {
        Write-Host ("  {0,-10} focus lost {1,5:F2}% of the run  <- percentile lows are poisoned" -f $d.Condition, $d.FocusLostPct) -ForegroundColor Red
    }
    Write-Host "  Excluded from the summary. Re-run them." -ForegroundColor Red
    Write-Host ""
    if (-not $IncludeContaminated) {
        $runs = @($runs | Where-Object {
            -not ($_.PSObject.Properties.Name -contains 'FocusLostPct' -and $_.FocusLostPct -gt 0.5)
        })
    }
}
if ($dirty.Count -gt 0) {
    Write-Host "WARNING: $($dirty.Count) run(s) spent >1% of wall time below 40 fps." -ForegroundColor Red
    foreach ($d in $dirty) {
        Write-Host ("  {0,-10} {1,5:F2}%  <- backgrounded during the run" -f $d.Condition, $d.ThrottlePct) -ForegroundColor Red
    }
    Write-Host "  Valorant throttles to 30 fps unfocused. These runs measure alt-tab," -ForegroundColor Red
    Write-Host "  not recorder overhead, and their 1%/0.1% lows should not be reported." -ForegroundColor Red
    if ($IncludeContaminated) {
        Write-Host "  INCLUDING them anyway (-IncludeContaminated). The summary below is not reportable." -ForegroundColor Red
    } else {
        Write-Host "  Excluded from the summary below. Re-run them." -ForegroundColor Red
    }
    Write-Host ""
}

# In a baseline run nothing of ours is encoding, so *any* sustained encode-engine
# activity is another application recording over the top of the control. Preflight
# refuses to start when that is already true, but it cannot catch something that
# begins mid-run — which happened twice here, at 4.0% and 10.4%. A depressed
# baseline understates the recorder's cost, so this flatters us if left in.
$foreignEncode = @($runs | Where-Object {
    $_.Condition -eq 'baseline' -and
    $_.PSObject.Properties.Name -contains 'GPU.VideoEncode.Pct' -and
    [double]$_.'GPU.VideoEncode.Pct' -gt 1.0
})
if ($foreignEncode.Count -gt 0) {
    Write-Host "WARNING: $($foreignEncode.Count) baseline run(s) had another process on the encode engine:" -ForegroundColor Red
    foreach ($fe in $foreignEncode) {
        Write-Host ("  baseline  encode={0,5:F1}%  <- not a recorder-free control" -f $fe.'GPU.VideoEncode.Pct') -ForegroundColor Red
    }
    Write-Host "  A contended baseline is slower than a true baseline, which makes the" -ForegroundColor Red
    Write-Host "  recorder look cheaper than it is. Excluded from the summary." -ForegroundColor Red
    Write-Host ""
}

# Warning and then averaging them in anyway is the worst of both worlds: it puts a
# number on screen that looks like a result. A contaminated run is not a noisy
# sample of the right thing, it is a clean sample of the wrong thing, so the
# default is to drop it.
if (-not $IncludeContaminated) {
    $runs = @($runs | Where-Object { $_.ThrottlePct -le 1.0 })
    $runs = @($runs | Where-Object {
        -not ($_.Condition -eq 'baseline' -and
              $_.PSObject.Properties.Name -contains 'GPU.VideoEncode.Pct' -and
              [double]$_.'GPU.VideoEncode.Pct' -gt 1.0)
    })
    if ($runs.Count -eq 0) {
        Write-Host "No clean runs left to summarise." -ForegroundColor Red
        exit 1
    }
}

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
