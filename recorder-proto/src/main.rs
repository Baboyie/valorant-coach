//! recorder-proto — a headless prototype for the Valorant recorder.
//!
//! This is deliberately NOT a UI. Per the implementation requirement, the first
//! thing to prove is "can we capture Valorant with extremely low overhead?", so
//! this binary exists only to answer that and to produce numbers.
//!
//!   recorder-proto probe                     what can this machine encode with
//!   recorder-proto capture [secs] [fps]      capture only, no encoding
//!   recorder-proto record  [secs] [fps] [out.mp4]   capture + hardware encode

mod capture;
mod d3d;
mod encoder;
mod encoders;

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use windows::core::Result;
use windows::Win32::Media::MediaFoundation::{MFShutdown, MFStartup, MFSTARTUP_NOSOCKET, MF_VERSION};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;

fn main() -> Result<()> {
    let cmd = std::env::args().nth(1).unwrap_or_else(|| "probe".into());

    // MTA, not STA: every part of this pipeline is free-threaded on purpose, so
    // that nothing in the capture or encode path can end up serialised behind a
    // UI thread (ADR §3).
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };
    unsafe { MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET)? };

    let result = match cmd.as_str() {
        "probe" => cmd_probe(),
        "capture" => cmd_capture(),
        "record" => cmd_record(),
        other => {
            eprintln!("unknown command: {other}\n\nusage: recorder-proto [probe|capture|record]");
            Ok(())
        }
    };

    unsafe { MFShutdown()? };
    unsafe { CoUninitialize() };
    result
}

/* ------------------------------------------------------------------- probe */

fn cmd_probe() -> Result<()> {
    println!("=== hardware encoder probe ===\n");
    let found = crate::encoders::probe()?;

    if found.is_empty() {
        // A real, reportable outcome, not an error: this machine would force a
        // CPU encode, which §2 says we must not silently do.
        println!("NO HARDWARE VIDEO ENCODER FOUND.");
        println!("Recording here would need a CPU encoder, which competes with");
        println!("the game for CPU. Refusing to recommend it.");
        return Ok(());
    }
    for e in &found {
        println!("  {:<6}  {:<20}  {}", e.codec.label(), e.vendor.label(), e.friendly_name);
    }
    if let Some(best) = crate::encoders::select_best(&found) {
        println!("\nselected: {} via {}", best.codec.label(), best.vendor.label());
    }
    Ok(())
}

/* ------------------------------------------------------------- shared setup */

struct Target {
    hwnd: windows::Win32::Foundation::HWND,
    what: &'static str,
}

fn pick_target() -> Option<Target> {
    match capture::find_valorant() {
        Some(h) => Some(Target { hwnd: h, what: "VALORANT" }),
        None => {
            let h = unsafe { GetForegroundWindow() };
            if h.0.is_null() {
                None
            } else {
                Some(Target { hwnd: h, what: "foreground window (Valorant not running)" })
            }
        }
    }
}

fn arg(n: usize) -> Option<String> {
    std::env::args().nth(n)
}

fn print_capture_stats(s: &capture::CaptureStats, secs: u64) {
    let arrived = s.arrived.load(Ordering::Relaxed);
    let kept = s.kept.load(Ordering::Relaxed);
    println!("frames arrived from compositor : {arrived}  ({:.1}/s)", arrived as f64 / secs as f64);
    println!("frames kept                    : {kept}  ({:.1}/s)", kept as f64 / secs as f64);
    println!("dropped by pacing              : {}", s.dropped_pacing.load(Ordering::Relaxed));
    println!("dropped, no free ring slot     : {}", s.dropped_ring_full.load(Ordering::Relaxed));
    println!();
    println!("capture callback  mean : {:>6.1} us", s.mean_callback_us());
    println!("                  p50  : {:>6} us", s.percentile_us(0.50));
    println!("                  p99  : {:>6} us", s.percentile_us(0.99));
    println!("                  p99.9: {:>6} us", s.percentile_us(0.999));
    println!("                  max  : {:>6.1} us",
             s.callback_ns_max.load(Ordering::Relaxed) as f64 / 1000.0);
}

/* ----------------------------------------------------------------- capture */

/// Capture only. Isolating this from encoding means a bad number later can be
/// attributed to the right stage instead of guessed at.
fn cmd_capture() -> Result<()> {
    let secs: u64 = arg(2).and_then(|s| s.parse().ok()).unwrap_or(10);
    let fps: u32 = arg(3).and_then(|s| s.parse().ok()).unwrap_or(60);

    let Some(t) = pick_target() else {
        eprintln!("no window to capture");
        return Ok(());
    };
    println!("target : {}", t.what);
    println!("pacing : {fps} fps target, {secs}s run\n");

    let dev = d3d::Device::new()?;
    let (cap, frames) = capture::Capture::for_window(&dev, t.hwnd, fps, 4)?;

    // Nothing consumes frames here, so recycle slots straight back or capture
    // would stall after four frames and report a misleading drop count.
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        match frames.full_rx.recv_timeout(Duration::from_millis(100)) {
            Ok((slot, _)) => { let _ = frames.free_tx.send(slot); }
            Err(_) => {}
        }
    }
    cap.stop()?;

    print_capture_stats(&cap.stats, secs);
    println!("\nNote: numbers from this machine are for correctness only.\n\
              Overhead figures are only meaningful on the RTX 2060 rig (ADR §6).");
    Ok(())
}

/* ------------------------------------------------------------------ record */

fn cmd_record() -> Result<()> {
    let secs: u64 = arg(2).and_then(|s| s.parse().ok()).unwrap_or(10);
    let fps: u32 = arg(3).and_then(|s| s.parse().ok()).unwrap_or(60);
    let out = arg(4).unwrap_or_else(|| "capture.mp4".into());

    let Some(t) = pick_target() else {
        eprintln!("no window to capture");
        return Ok(());
    };

    let dev = d3d::Device::new()?;
    // Six slots: enough that a brief encoder hiccup costs nothing, small enough
    // that VRAM stays trivial (6 x 1080p BGRA is ~50 MB).
    let (cap, frames) = capture::Capture::for_window(&dev, t.hwnd, fps, 6)?;

    let w = cap.size.Width as u32;
    let h = cap.size.Height as u32;
    if w % 2 == 1 || h % 2 == 1 {
        eprintln!("warning: {w}x{h} has an odd dimension; H.264 wants even sizes and \
                   the encoder may reject it.");
    }
    // ~0.1 bits per pixel per frame lands near 25 Mbps at 1080p60, which is in
    // the right neighbourhood for competitive footage without being wasteful.
    let bitrate = ((w as u64 * h as u64 * fps as u64) / 10).min(80_000_000) as u32;

    println!("target : {}", t.what);
    println!("output : {out}");
    println!("format : {w}x{h} @ {fps} fps, {:.1} Mbps H.264\n", bitrate as f64 / 1e6);

    let cfg = encoder::EncoderConfig { width: w, height: h, fps, bitrate };
    let mut enc = encoder::Encoder::new(&dev, &out, &cfg)?;

    let mut encode_ns_total: u64 = 0;
    let mut encode_ns_max: u64 = 0;
    let mut submitted: u64 = 0;

    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        match frames.full_rx.recv_timeout(Duration::from_millis(100)) {
            Ok((slot, ts)) => {
                let t0 = Instant::now();
                let r = enc.write_frame(&frames.ring.textures[slot], ts);
                let ns = t0.elapsed().as_nanos() as u64;
                encode_ns_total += ns;
                if ns > encode_ns_max { encode_ns_max = ns; }
                submitted += 1;
                // Return the slot even on failure, or the ring bleeds away.
                let _ = frames.free_tx.send(slot);
                r?;
            }
            Err(_) => {}
        }
    }

    cap.stop()?;
    // Drain whatever is still queued so the tail of the recording is not lost.
    while let Ok((slot, ts)) = frames.full_rx.try_recv() {
        let _ = enc.write_frame(&frames.ring.textures[slot], ts);
        let _ = frames.free_tx.send(slot);
        submitted += 1;
    }

    let written = enc.frames_written;
    enc.finish()?;

    print_capture_stats(&cap.stats, secs);
    println!();
    println!("frames submitted to encoder    : {submitted}");
    println!("frames accepted                : {written}");
    if submitted > 0 {
        println!("encode submit  mean : {:>6.1} us", encode_ns_total as f64 / submitted as f64 / 1000.0);
        println!("               max  : {:>6.1} us", encode_ns_max as f64 / 1000.0);
    }

    match std::fs::metadata(&out) {
        Ok(m) => println!("\nwrote {out} — {:.1} MB", m.len() as f64 / 1e6),
        Err(e) => println!("\ncould not stat {out}: {e}"),
    }
    println!("\nNote: numbers from this machine are for correctness only.\n\
              Overhead figures are only meaningful on the RTX 2060 rig (ADR §6).");
    Ok(())
}
