//! The §17 performance monitor: what the recorder is actually costing.
//!
//! Every figure here is read from the OS, never estimated. §17 is explicit —
//! *"Never fake these values"* — so anything unavailable is reported as absent
//! rather than filled in with something plausible. That is why several fields
//! are `Option`.
//!
//! Deliberately cheap: these are process- and adapter-level queries costing
//! microseconds, sampled at 1 Hz from the engine's own loop. A monitor that
//! showed the recorder's overhead by adding overhead would be self-defeating
//! (§26 — the desktop client must stay light).
//!
//! **Game FPS and frame time are not here.** Getting them honestly requires an
//! ETW consumer of the kind PresentMon implements, which is a subsystem rather
//! than a call; the benchmark harness in `bench/` already does it properly and
//! out-of-process. Showing a guessed number would be worse than showing none.

use std::time::Instant;

use windows::Win32::Foundation::FILETIME;
use windows::Win32::Graphics::Dxgi::{
    DXGI_MEMORY_SEGMENT_GROUP_LOCAL, DXGI_QUERY_VIDEO_MEMORY_INFO,
};
use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetProcessIoCounters, GetProcessTimes, IO_COUNTERS,
};

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct PerfSample {
    /// Our own CPU time as a share of one core's worth of wall time, then
    /// normalised across all logical processors — the figure §2 cares about.
    pub cpu_pct: f64,
    pub ram_mb: f64,
    /// GPU memory the process has resident, and the driver's budget for it.
    pub vram_mb: Option<f64>,
    pub vram_budget_mb: Option<f64>,
    /// Bytes/sec written, averaged over the sample interval. Covers the whole
    /// process, which for this app is essentially the recording.
    pub disk_write_mbps: f64,
}

pub struct SysMon {
    logical_cpus: f64,
    last: Option<Snapshot>,
}

struct Snapshot {
    at: Instant,
    cpu_100ns: u64,
    write_bytes: u64,
}

impl SysMon {
    pub fn new() -> SysMon {
        let logical = std::thread::available_parallelism()
            .map(|n| n.get() as f64)
            .unwrap_or(1.0);
        SysMon { logical_cpus: logical, last: None }
    }

    /// Sample. Returns None on the first call, because every rate here is a
    /// delta and a first sample has nothing to subtract from — reporting 0%
    /// would be a lie, and §17 forbids invented values.
    pub fn sample(&mut self, adapter: Option<&windows::Win32::Graphics::Dxgi::IDXGIAdapter3>) -> Option<PerfSample> {
        let now = Instant::now();
        let cpu_100ns = process_cpu_100ns()?;
        let write_bytes = process_write_bytes().unwrap_or(0);

        let prev = self.last.replace(Snapshot { at: now, cpu_100ns, write_bytes })?;
        let elapsed = now.duration_since(prev.at).as_secs_f64();
        if elapsed <= 0.0 {
            return None;
        }

        let cpu_delta = cpu_100ns.saturating_sub(prev.cpu_100ns) as f64 / 1e7;
        let cpu_pct = (cpu_delta / elapsed / self.logical_cpus) * 100.0;

        let write_delta = write_bytes.saturating_sub(prev.write_bytes) as f64;
        let disk_write_mbps = write_delta / elapsed / 1e6;

        let (vram_mb, vram_budget_mb) = match adapter.and_then(query_vram) {
            Some((used, budget)) => (Some(used), Some(budget)),
            None => (None, None),
        };

        Some(PerfSample {
            cpu_pct: (cpu_pct * 100.0).round() / 100.0,
            ram_mb: (process_ram_bytes().unwrap_or(0) as f64 / 1e6 * 10.0).round() / 10.0,
            vram_mb,
            vram_budget_mb,
            disk_write_mbps: (disk_write_mbps * 100.0).round() / 100.0,
        })
    }
}

fn filetime_to_100ns(ft: FILETIME) -> u64 {
    ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64
}

/// Kernel + user time for this process, in 100 ns units.
fn process_cpu_100ns() -> Option<u64> {
    let mut c = FILETIME::default();
    let mut e = FILETIME::default();
    let mut k = FILETIME::default();
    let mut u = FILETIME::default();
    unsafe { GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u).ok()? };
    Some(filetime_to_100ns(k) + filetime_to_100ns(u))
}

fn process_ram_bytes() -> Option<u64> {
    let mut pmc = PROCESS_MEMORY_COUNTERS::default();
    let size = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
    unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut pmc, size).ok()? };
    Some(pmc.WorkingSetSize as u64)
}

fn process_write_bytes() -> Option<u64> {
    let mut io = IO_COUNTERS::default();
    unsafe { GetProcessIoCounters(GetCurrentProcess(), &mut io).ok()? };
    Some(io.WriteTransferCount)
}

/// Video memory this process has resident on the local (dedicated) segment,
/// and the budget the driver is willing to give it, in MB.
fn query_vram(adapter: &windows::Win32::Graphics::Dxgi::IDXGIAdapter3) -> Option<(f64, f64)> {
    let mut info = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
    unsafe {
        adapter
            .QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut info)
            .ok()?
    };
    Some((
        (info.CurrentUsage as f64 / 1e6 * 10.0).round() / 10.0,
        (info.Budget as f64 / 1e6 * 10.0).round() / 10.0,
    ))
}
