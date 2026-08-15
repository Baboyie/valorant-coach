//! recorder-proto — a headless prototype for the Valorant recorder.
//!
//! This is deliberately NOT a UI. Per the implementation requirement, the first
//! thing to prove is "can we capture Valorant with extremely low overhead?", so
//! this binary exists only to answer that and to produce numbers.
//!
//!   recorder-proto probe                     what can this machine encode with
//!   recorder-proto capture [secs] [fps]      capture only, no encoding
//!   recorder-proto record  [secs] [fps] [out.mp4]   capture + hardware encode
//!   recorder-proto replay  [window] [fps] [out.mp4] encode into a memory ring,
//!                                            then save the last [window] secs

use recorder_core::{audio, capture, d3d, encoder, encoders, replay};

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
        "replay" => cmd_replay(),
        "audio" => cmd_audio(),
        other => {
            eprintln!(
                "unknown command: {other}\n\nusage: recorder-proto [probe|capture|record|replay|audio]"
            );
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
    let found = encoders::probe()?;

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
    if let Some(best) = encoders::select_best(&found) {
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
    // `--foreground` forces the fallback target. This exists because a minimised
    // Valorant is found correctly and then yields *zero* frames — WGC composites
    // nothing for an iconic window (measured on the 12400F rig: 0 frames in 15 s,
    // window parked at -32000,-32000 at 160x28). That makes an idle background
    // Valorant useless as a smoke test for the encoder, so this flag lets the
    // encode path be exercised against any live window without disturbing the
    // game — or requiring the player to be sitting at it.
    // `--hwnd <handle>` aims at one specific window. This is a test affordance,
    // not a product feature: paths like resize handling are otherwise untestable
    // from a script, because Windows refuses SetForegroundWindow to a background
    // process, so a harness cannot put its chosen window in front to be picked up
    // by --foreground.
    let args: Vec<String> = std::env::args().collect();
    if let Some(i) = args.iter().position(|a| a == "--hwnd") {
        if let Some(raw) = args.get(i + 1) {
            let trimmed = raw.trim_start_matches("0x").trim_start_matches("0X");
            let parsed = if raw.starts_with("0x") || raw.starts_with("0X") {
                usize::from_str_radix(trimmed, 16).ok()
            } else {
                raw.parse::<usize>().ok()
            };
            if let Some(v) = parsed {
                return Some(Target {
                    hwnd: windows::Win32::Foundation::HWND(v as *mut _),
                    what: "explicit --hwnd",
                });
            }
            eprintln!("could not parse --hwnd {raw}");
        }
    }

    let force_foreground = args.iter().any(|a| a == "--foreground");

    if !force_foreground {
        if let Some(h) = capture::find_valorant() {
            return Some(Target { hwnd: h, what: "VALORANT" });
        }
    }

    let h = unsafe { GetForegroundWindow() };
    if h.0.is_null() {
        None
    } else if force_foreground {
        Some(Target { hwnd: h, what: "foreground window (--foreground)" })
    } else {
        Some(Target { hwnd: h, what: "foreground window (Valorant not running)" })
    }
}

fn arg(n: usize) -> Option<String> {
    std::env::args().nth(n)
}

/// State the hardware a measurement came from, rather than asserting which
/// machine the reader is on.
///
/// This replaced a hardcoded "numbers from this machine are for correctness
/// only" footer. That footer was written on the dev laptop and became actively
/// misleading the moment the prototype ran on the benchmark rig, where it argued
/// against the only numbers ADR §6 considers reportable. Printing the adapter
/// lets the output be read correctly on either machine.
fn print_provenance(dev: &d3d::Device) {
    let adapter = dev
        .adapter_name()
        .unwrap_or_else(|_| "unknown adapter".to_string());
    println!();
    println!("measured on : {adapter}");
    println!("ADR §6: overhead figures are reportable only from the i5-12400F / RTX 2060");
    println!("rig. Figures from the Iris Xe dev laptop are correctness signals only.");
}

fn print_capture_stats(s: &capture::CaptureStats, secs: u64) {
    let arrived = s.arrived.load(Ordering::Relaxed);
    let kept = s.kept.load(Ordering::Relaxed);
    println!("frames arrived from compositor : {arrived}  ({:.1}/s)", arrived as f64 / secs as f64);
    println!("frames kept                    : {kept}  ({:.1}/s)", kept as f64 / secs as f64);
    println!("dropped by pacing              : {}", s.dropped_pacing.load(Ordering::Relaxed));
    println!("dropped, no free ring slot     : {}", s.dropped_ring_full.load(Ordering::Relaxed));
    let mismatched = s.dropped_size_mismatch.load(Ordering::Relaxed);
    println!("dropped, target resized        : {mismatched}");
    if mismatched > 0 {
        // Worth spelling out: this is the target changing shape, not the recorder
        // falling behind, and the two would otherwise look identical in a table.
        println!("  (target was minimised or resized; frame pool rebuilt {} time(s).",
                 s.pool_recreations.load(Ordering::Relaxed));
        println!("   recording resumes by itself once it returns to its original size)");
    }
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

    cap.start()?;

    // Nothing consumes frames here, so recycle slots straight back or capture
    // would stall after four frames and report a misleading drop count.
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        // The recv timeout guarantees this runs ~10x/s even when no frames are
        // arriving, which is exactly the case a resize can leave us in.
        if let Ok(true) = cap.poll_resize() {
            println!("  (capture target resized; frame pool rebuilt)");
        }
        match frames.full_rx.recv_timeout(Duration::from_millis(100)) {
            Ok((slot, _)) => { let _ = frames.free_tx.send(slot); }
            Err(_) => {}
        }
    }
    cap.stop()?;

    print_capture_stats(&cap.stats, secs);
    print_provenance(&dev);
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
    let bitrate = encoder::EncoderConfig::default_bitrate(w, h, fps);

    println!("target : {}", t.what);
    println!("output : {out}");
    println!("format : {w}x{h} @ {fps} fps, {:.1} Mbps H.264\n", bitrate as f64 / 1e6);

    // `--no-audio` keeps the benchmark path exactly as §9 measured it: that
    // result was video-only, and an audio encode would make new runs
    // incomparable with the recorded table.
    // Desktop and mic as separate tracks (§23). `--mic` opts in; a machine with
    // no microphone is the common case, so it is not on by default.
    let want_audio = !std::env::args().any(|a| a == "--no-audio");
    let want_mic = std::env::args().any(|a| a == "--mic");
    let mut sources = Vec::new();
    if want_audio {
        sources.push(audio::AudioSource::Loopback);
    }
    if want_mic {
        sources.push(audio::AudioSource::Microphone);
    }

    let mut caps: Vec<audio::AudioCapture> = Vec::new();
    let mut rxs = Vec::new();
    for src in sources {
        match audio::AudioCapture::start(src) {
            Ok((c, rx)) => {
                println!("audio  : {:<10} {} Hz, {} ch", c.source.label(), c.format.sample_rate, c.format.channels);
                caps.push(c);
                rxs.push(rx);
            }
            // Losing one source must not lose the recording, or the other source.
            Err(e) => eprintln!("{} unavailable, continuing without it: {e}", src.label()),
        }
    }

    let cfg = encoder::EncoderConfig {
        width: w,
        height: h,
        fps,
        bitrate,
        gop_frames: encoder::EncoderConfig::default_gop(fps),
    };
    let fmts: Vec<audio::AudioFormat> = caps.iter().map(|c| c.format).collect();
    let mut enc = encoder::Encoder::to_file(&dev, &out, &cfg, &fmts)?;

    // Only now: the encoder exists, so the first captured frame has somewhere to
    // go. Starting capture any earlier spends the ring on sink-writer init.
    cap.start()?;

    let mut encode_ns_total: u64 = 0;
    let mut encode_ns_max: u64 = 0;
    let mut submitted: u64 = 0;

    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if let Ok(true) = cap.poll_resize() {
            println!("  (capture target resized; frame pool rebuilt, recording continues)");
        }
        for (i, rx) in rxs.iter().enumerate() {
            while let Ok(chunk) = rx.try_recv() {
                enc.write_audio(i, &chunk.pcm, chunk.ts_100ns)?;
            }
        }
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
    // Stop audio before draining it, or the loop chases a stream that is still
    // being fed and never ends.
    for c in &mut caps {
        c.stop();
    }
    for (i, rx) in rxs.iter().enumerate() {
        while let Ok(chunk) = rx.try_recv() {
            let _ = enc.write_audio(i, &chunk.pcm, chunk.ts_100ns);
        }
    }

    let written = enc.frames_written;
    let audio_written = enc.audio_written;
    enc.finish()?;

    print_capture_stats(&cap.stats, secs);
    println!();
    println!("frames submitted to encoder    : {submitted}");
    println!("frames accepted                : {written}");
    if !caps.is_empty() {
        println!("audio packets encoded          : {audio_written}");
        for c in &caps {
            println!("  {:<10} discontinuities    : {}", c.source.label(),
                     c.stats.discontinuities.load(Ordering::Relaxed));
        }
    }
    if submitted > 0 {
        println!("encode submit  mean : {:>6.1} us", encode_ns_total as f64 / submitted as f64 / 1000.0);
        println!("               max  : {:>6.1} us", encode_ns_max as f64 / 1000.0);
    }

    match std::fs::metadata(&out) {
        Ok(m) => println!("\nwrote {out} — {:.1} MB", m.len() as f64 / 1e6),
        Err(e) => println!("\ncould not stat {out}: {e}"),
    }
    print_provenance(&dev);
    Ok(())
}

/* ------------------------------------------------------------------- audio */

/// Desktop audio capture on its own, before it is wired to an encoder.
///
/// Proving a stage in isolation is the same discipline capture got: a bad
/// number later can then be attributed rather than guessed at. The figure that
/// matters here is **peak level** — packet counts prove the plumbing runs,
/// but only a non-zero peak proves we are capturing what the speakers are
/// playing rather than a well-formed stream of silence.
fn cmd_audio() -> Result<()> {
    let secs: u64 = arg(2).and_then(|s| s.parse().ok()).unwrap_or(10);
    let source = match arg(3).as_deref() {
        Some("mic") | Some("microphone") => audio::AudioSource::Microphone,
        _ => audio::AudioSource::Loopback,
    };

    let (cap, rx) = audio::AudioCapture::start(source)?;
    println!("source : {}", source.label());
    println!(
        "device : {} Hz, {} channels (emitting {} — downmixed if needed)",
        cap.format.sample_rate, cap.format.device_channels, cap.format.channels
    );
    println!("run    : {secs}s — play something so there is audio to capture\n");

    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut peak: i32 = 0;
    let mut sum_sq: f64 = 0.0;
    let mut samples: u64 = 0;
    let mut first_ts: Option<i64> = None;
    let mut last_ts: i64 = 0;

    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(chunk) => {
                if first_ts.is_none() {
                    first_ts = Some(chunk.ts_100ns);
                }
                last_ts = chunk.ts_100ns;
                for s in &chunk.pcm {
                    let a = (*s as i32).abs();
                    if a > peak {
                        peak = a;
                    }
                    sum_sq += (*s as f64) * (*s as f64);
                    samples += 1;
                }
            }
            Err(_) => {}
        }
    }
    let packets = cap.stats.packets.load(Ordering::Relaxed);
    let frames = cap.stats.frames.load(Ordering::Relaxed);
    let discontinuities = cap.stats.discontinuities.load(Ordering::Relaxed);
    let silent = cap.stats.silent.load(Ordering::Relaxed);
    drop(cap);

    let span = match first_ts {
        Some(f) => (last_ts - f) as f64 / 1e7,
        None => 0.0,
    };
    let rms = if samples > 0 { (sum_sq / samples as f64).sqrt() } else { 0.0 };

    println!("packets          : {packets}  ({frames} frames)");
    println!("discontinuities  : {discontinuities}");
    println!("silent packets   : {silent}");
    println!("samples captured : {samples}");
    println!("timestamp span   : {span:.2}s over a {secs}s run");
    println!("peak level       : {peak} / 32767  ({:.1} dBFS)", dbfs(peak as f64));
    println!("rms level        : {rms:.0} / 32767  ({:.1} dBFS)", dbfs(rms));
    if peak == 0 {
        println!("\nSILENT — the stream is well-formed but carries nothing.");
        match source {
            audio::AudioSource::Loopback => {
                println!("Loopback captures what the default output device renders, so play");
                println!("audio and re-run before concluding anything about the capture path.");
            }
            audio::AudioSource::Microphone => {
                println!("Nothing reached the microphone. Check it is not muted, that Windows");
                println!("microphone privacy allows desktop apps, and that the communications");
                println!("default is the device you actually speak into.");
            }
        }
    }
    Ok(())
}

fn dbfs(v: f64) -> f64 {
    if v <= 0.0 {
        return f64::NEG_INFINITY;
    }
    20.0 * (v / 32767.0).log10()
}

/* ------------------------------------------------------------------ replay */

/// Replay buffer: encode continuously into a memory ring, then save the last
/// `window` seconds — the ShadowPlay-style "that just happened, keep it" path,
/// which is the reason this product records at all.
///
/// The run deliberately lasts longer than the window so the ring wraps: a test
/// where eviction never fired would prove nothing about the part of this that
/// is actually new.
fn cmd_replay() -> Result<()> {
    let window: u64 = arg(2).and_then(|s| s.parse().ok()).unwrap_or(15);
    let fps: u32 = arg(3).and_then(|s| s.parse().ok()).unwrap_or(60);
    let out = arg(4).unwrap_or_else(|| "replay.mp4".into());
    let run_secs = window + 15;

    let Some(t) = pick_target() else {
        eprintln!("no window to capture");
        return Ok(());
    };

    let dev = d3d::Device::new()?;
    let (cap, frames) = capture::Capture::for_window(&dev, t.hwnd, fps, 6)?;

    let w = cap.size.Width as u32;
    let h = cap.size.Height as u32;
    if w % 2 == 1 || h % 2 == 1 {
        eprintln!("warning: {w}x{h} has an odd dimension; H.264 wants even sizes and \
                   the encoder may reject it.");
    }
    let bitrate = encoder::EncoderConfig::default_bitrate(w, h, fps);
    let gop_frames = encoder::EncoderConfig::default_gop(fps);
    let gop_secs = gop_frames as f64 / fps.max(1) as f64;

    println!("target : {}", t.what);
    println!("window : last {window}s kept, running {run_secs}s so the ring wraps");
    println!("format : {w}x{h} @ {fps} fps, {:.1} Mbps H.264, keyframe every {gop_secs:.2}s\n",
             bitrate as f64 / 1e6);

    // 256 MB hard cap: the window bounds memory in time, this bounds it in
    // bytes if the bitrate estimate is ever badly wrong (ADR §6's RAM budget).
    let ring = std::sync::Arc::new(replay::ReplayRing::new(window, gop_secs, 256 * 1024 * 1024));
    let cfg = encoder::EncoderConfig {
        width: w,
        height: h,
        fps,
        bitrate,
        gop_frames,
    };
    let mut enc = encoder::Encoder::to_replay(&dev, &cfg, std::sync::Arc::clone(&ring))?;

    // Audio runs as a second encoder into the same ring: the grabber sink
    // carries one stream, so it cannot ride the video writer here.
    let want_audio = !std::env::args().any(|a| a == "--no-audio");
    let want_mic = std::env::args().any(|a| a == "--mic");
    let mut wanted = Vec::new();
    if want_audio {
        wanted.push((audio::AudioSource::Loopback, replay::AudioTrack::Desktop));
    }
    if want_mic {
        wanted.push((audio::AudioSource::Microphone, replay::AudioTrack::Mic));
    }

    let mut caps: Vec<audio::AudioCapture> = Vec::new();
    let mut rxs = Vec::new();
    let mut aencs: Vec<(replay::AudioTrack, encoder::AudioEncoder)> = Vec::new();
    for (src, track) in wanted {
        match audio::AudioCapture::start(src) {
            Ok((c, rx)) => {
                match encoder::AudioEncoder::to_replay(track, &c.format, std::sync::Arc::clone(&ring)) {
                    Ok(ae) => {
                        println!("audio  : {:<10} {} Hz, {} ch", c.source.label(), c.format.sample_rate, c.format.channels);
                        aencs.push((track, ae));
                        caps.push(c);
                        rxs.push(rx);
                    }
                    Err(e) => eprintln!("{} encoder unavailable: {e}", src.label()),
                }
            }
            Err(e) => eprintln!("{} unavailable: {e}", src.label()),
        }
    }

    cap.start()?;
    let mut video_base: Option<i64> = None;

    let deadline = Instant::now() + Duration::from_secs(run_secs);
    while Instant::now() < deadline {
        if let Ok(true) = cap.poll_resize() {
            println!("  (capture target resized; frame pool rebuilt, recording continues)");
        }
        // Audio is only submitted once video has a base timestamp, so both
        // streams are rebased against the same origin.
        for (i, rx) in rxs.iter().enumerate() {
            while let Ok(chunk) = rx.try_recv() {
                let _ = aencs[i].1.write(&chunk.pcm, chunk.ts_100ns, video_base);
            }
        }
        match frames.full_rx.recv_timeout(Duration::from_millis(100)) {
            Ok((slot, ts)) => {
                video_base.get_or_insert(ts);
                let r = enc.write_frame(&frames.ring.textures[slot], ts);
                let _ = frames.free_tx.send(slot);
                r?;
            }
            Err(_) => {}
        }
    }

    cap.stop()?;
    while let Ok((slot, ts)) = frames.full_rx.try_recv() {
        let _ = enc.write_frame(&frames.ring.textures[slot], ts);
        let _ = frames.free_tx.send(slot);
    }
    for c in &mut caps {
        c.stop();
    }
    for (i, rx) in rxs.iter().enumerate() {
        while let Ok(chunk) = rx.try_recv() {
            let _ = aencs[i].1.write(&chunk.pcm, chunk.ts_100ns, video_base);
        }
    }
    enc.finish()?;
    // Keep each negotiated AAC type for the mux before the encoders are consumed.
    let audio_types: Vec<(replay::AudioTrack, windows::Win32::Media::MediaFoundation::IMFMediaType)> =
        aencs
            .iter()
            .filter_map(|(t, ae)| ae.negotiated_type.clone().map(|ty| (*t, ty)))
            .collect();
    for (_, ae) in aencs {
        let _ = ae.finish();
    }

    print_capture_stats(&cap.stats, run_secs);

    let r = ring.report();
    println!();
    println!("replay ring : {} frames ({} keyframes), {:.1} MB, spanning {:.1}s",
             r.frames, r.keyframes, r.bytes as f64 / 1e6, r.span_secs);
    println!("              evicted {} frames; buffers: {} allocated, {} reused",
             r.evicted, r.allocs, r.reuses);
    println!("              keyframe interval {:.2}s (requested {:.2}s — the NVIDIA MFT",
             r.mean_kf_interval_secs, gop_secs);
    println!("              clamps to ~0.9s regardless; see ADR §9c)");
    if r.non_monotonic > 0 {
        // B-frames got through despite configure_codec — the muxed clip's
        // timestamps are suspect and this needs investigating, loudly.
        println!("              WARNING: {} non-monotonic timestamps — B-frames?",
                 r.non_monotonic);
    }

    for (track, _) in &audio_types {
        let (a_packets, a_bytes, a_span) = ring.audio_report(*track);
        println!("{:<11} : {a_packets} packets, {:.1} MB, spanning {a_span:.1}s",
                 format!("{} ring", track.label()), a_bytes as f64 / 1e6);
    }

    match ring.save_mp4(&out, &cfg, &audio_types) {
        Ok(s) => {
            println!();
            println!("saved  : last {:.1}s ({} frames, {} audio packets across {} track(s), {:.1} MB) -> {out}",
                     s.span_secs, s.frames, s.audio_packets, s.audio_tracks, s.bytes as f64 / 1e6);
            // The number that matters for the product: a save must feel
            // instant, because the moment it protects has already happened.
            println!("save cost : {:.0} ms (mux only — no encoder work)", s.elapsed_ms);
        }
        Err(e) => println!("\nsave failed: {e}"),
    }

    print_provenance(&dev);
    Ok(())
}
