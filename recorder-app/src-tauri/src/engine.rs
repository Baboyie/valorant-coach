//! The recorder engine: one thread that owns the whole pipeline.
//!
//! Everything Media Foundation and Direct3D touches lives on this thread, and
//! the UI never blocks on it. The UI sends commands down a channel and reads a
//! snapshot of status; it cannot stall capture no matter what it does. That is
//! ADR §6's "the capture path never depends on the React layer" made
//! structural rather than aspirational.
//!
//! **Buffering and manual recording are mutually exclusive in v1.** Running
//! both would mean two encoder instances fed the same textures — technically
//! easy, but it doubles encode-engine load, and the 15.3% figure §9 measured is
//! the one the product's overhead claim rests on. Recording already produces
//! the footage a clip would have contained, so the exclusion costs the user
//! nothing real.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use recorder_core::{capture, d3d, encoder, replay};
use windows::Win32::Media::MediaFoundation::{
    MFShutdown, MFStartup, MFSTARTUP_NOSOCKET, MF_VERSION,
};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};

use crate::config::Config;

#[derive(Debug)]
pub enum Cmd {
    /// Save the buffered window to a clip.
    SaveClip,
    StartRecording,
    StopRecording,
    /// Settings changed; rebuild the session so they take effect.
    Reconfigure(Box<Config>),
    Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    /// Valorant is not running.
    Idle,
    /// Capturing into the replay ring.
    Buffering,
    /// Capturing to a file.
    Recording,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Status {
    pub state: State,
    /// §17's monitor. None until the first interval has elapsed — every figure
    /// in it is a delta, and inventing a first value is exactly what §17
    /// forbids.
    pub perf: Option<crate::sysmon::PerfSample>,
    pub game_running: bool,
    pub buffered_secs: f64,
    pub ring_mb: f64,
    pub frames_kept: u64,
    pub dropped_ring_full: u64,
    pub dropped_resized: u64,
    pub callback_p99_us: u32,
    pub last_clip: Option<String>,
    pub last_save_ms: Option<f64>,
    pub last_error: Option<String>,
    pub recording_path: Option<String>,
    pub recording_secs: f64,
}

impl Default for Status {
    fn default() -> Self {
        Status {
            state: State::Idle,
            perf: None,
            game_running: false,
            buffered_secs: 0.0,
            ring_mb: 0.0,
            frames_kept: 0,
            dropped_ring_full: 0,
            dropped_resized: 0,
            callback_p99_us: 0,
            last_clip: None,
            last_save_ms: None,
            last_error: None,
            recording_path: None,
            recording_secs: 0.0,
        }
    }
}

pub struct Engine {
    tx: Sender<Cmd>,
    status: Arc<Mutex<Status>>,
}

impl Engine {
    pub fn spawn(config: Config) -> Engine {
        let (tx, rx) = mpsc::channel();
        let status = Arc::new(Mutex::new(Status::default()));
        let thread_status = Arc::clone(&status);
        std::thread::Builder::new()
            .name("recorder-engine".into())
            .spawn(move || run(config, rx, thread_status))
            .expect("failed to spawn recorder engine thread");
        Engine { tx, status }
    }

    pub fn send(&self, cmd: Cmd) {
        // A dead engine thread must not panic the UI. The status snapshot will
        // simply stop advancing, which is visible.
        let _ = self.tx.send(cmd);
    }

    pub fn status(&self) -> Status {
        self.status.lock().unwrap().clone()
    }
}

/* --------------------------------------------------------------- the thread */

/// What the engine is currently running, with the live objects.
/// Audio side of a session, absent when sound is off or unavailable.
///
/// Bundled rather than scattered so that "recording without audio" stays a
/// single `None` instead of three fields that could disagree.
struct AudioSide {
    cap: recorder_core::audio::AudioCapture,
    rx: Receiver<recorder_core::audio::AudioChunk>,
    /// Only the replay path has its own encoder; on the file path audio rides
    /// the video writer as an extra stream.
    enc: Option<encoder::AudioEncoder>,
    track: replay::AudioTrack,
}

/// Start the audio sources the config asks for, as separate tracks (§23).
///
/// Each source is independent: a machine with no microphone still records
/// desktop audio, and a failure on either never costs the recording.
fn start_audio_sources(
    config: &Config,
    ring: Option<&Arc<replay::ReplayRing>>,
) -> Vec<AudioSide> {
    use recorder_core::audio::{AudioCapture, AudioSource};

    let mut wanted = Vec::new();
    if config.capture_audio {
        wanted.push((AudioSource::Loopback, replay::AudioTrack::Desktop));
    }
    if config.capture_mic {
        wanted.push((AudioSource::Microphone, replay::AudioTrack::Mic));
    }

    let mut out = Vec::new();
    for (src, track) in wanted {
        match AudioCapture::start(src) {
            Ok((cap, rx)) => {
                let enc = match ring {
                    Some(r) => match encoder::AudioEncoder::to_replay(track, &cap.format, Arc::clone(r)) {
                        Ok(ae) => Some(ae),
                        Err(e) => {
                            eprintln!("{} encoder unavailable: {e}", src.label());
                            continue;
                        }
                    },
                    None => None,
                };
                out.push(AudioSide { cap, rx, enc, track });
            }
            Err(e) => eprintln!("{} unavailable, continuing without it: {e}", src.label()),
        }
    }
    out
}

enum Session {
    None,
    Buffering {
        cap: capture::Capture,
        frames: capture::Frames,
        enc: encoder::Encoder,
        ring: Arc<replay::ReplayRing>,
        cfg: encoder::EncoderConfig,
        audio: Vec<AudioSide>,
        /// Video's first timestamp — both streams rebase against it so sound
        /// and picture share an origin.
        video_base: Option<i64>,
    },
    Recording {
        cap: capture::Capture,
        frames: capture::Frames,
        enc: encoder::Encoder,
        path: PathBuf,
        started: Instant,
        audio: Vec<AudioSide>,
    },
}

fn run(mut config: Config, rx: Receiver<Cmd>, status: Arc<Mutex<Status>>) {
    // MTA, and owned by this thread for its whole life: every stage of this
    // pipeline is free-threaded on purpose (ADR §3).
    unsafe {
        if CoInitializeEx(None, COINIT_MULTITHREADED).is_err() {
            set_error(&status, "COM initialisation failed; recording unavailable");
            return;
        }
        if MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET).is_err() {
            set_error(&status, "Media Foundation startup failed; recording unavailable");
            CoUninitialize();
            return;
        }
    }

    let mut session = Session::None;
    // §17's monitor, sampled at 1 Hz off this same loop. Cheap process- and
    // adapter-level queries; a monitor that showed overhead by adding overhead
    // would defeat itself.
    let mut sysmon = crate::sysmon::SysMon::new();
    let mut adapter: Option<windows::Win32::Graphics::Dxgi::IDXGIAdapter3> = None;
    let mut next_perf = Instant::now() + Duration::from_secs(1);
    // Game detection is a window-class lookup, not a process-table scan, and it
    // only runs while we are not capturing — exactly ADR §4's fallback path.
    // The SetWinEventHook version §4 specifies as primary needs a message pump
    // on this thread, which would complicate the frame loop; the fallback alone
    // is cheap enough that the refinement can wait.
    let mut next_detect = Instant::now();

    loop {
        // ---- commands -------------------------------------------------
        match rx.try_recv() {
            Ok(Cmd::Shutdown) | Err(TryRecvError::Disconnected) => {
                teardown(&mut session, &status);
                break;
            }
            Ok(cmd) => handle_cmd(cmd, &mut session, &mut config, &status),
            Err(TryRecvError::Empty) => {}
        }

        // ---- detection ------------------------------------------------
        if Instant::now() >= next_detect {
            next_detect = Instant::now() + Duration::from_secs(2);
            let hwnd = find_target();
            {
                let mut s = status.lock().unwrap();
                s.game_running = hwnd.is_some();
            }
            match (&session, hwnd) {
                // Game appeared and we should be buffering.
                (Session::None, Some(h)) if config.auto_buffer => {
                    match start_buffering(h, &config) {
                        Ok((new, adapter3)) => {
                            session = new;
                            adapter = adapter3;
                            clear_error(&status);
                        }
                        Err(e) => {
                            // Most likely the window is minimised — the capture
                            // core refuses degenerate sizes (ADR §8). Retry on
                            // the next tick rather than giving up for good.
                            set_error(&status, &format!("could not start buffering: {e}"));
                        }
                    }
                }
                // Game vanished mid-session: stop cleanly. A recording loses
                // nothing — finish() still writes the moov atom.
                (Session::Buffering { .. }, None) | (Session::Recording { .. }, None) => {
                    teardown(&mut session, &status);
                }
                _ => {}
            }
        }

        // ---- performance monitor --------------------------------------
        if Instant::now() >= next_perf {
            next_perf = Instant::now() + Duration::from_secs(1);
            if let Some(p) = sysmon.sample(adapter.as_ref()) {
                status.lock().unwrap().perf = Some(p);
            }
        }

        // ---- pump frames ----------------------------------------------
        let idle = pump(&mut session, &status);
        if idle {
            // Nothing captured this tick. Sleep briefly so an idle engine does
            // not spin a core — §2 forbids competing with the game for CPU, and
            // that applies hardest when we are doing nothing.
            std::thread::sleep(Duration::from_millis(50));
        }
        publish(&session, &status);
    }

    unsafe {
        let _ = MFShutdown();
        CoUninitialize();
    }
}

fn handle_cmd(cmd: Cmd, session: &mut Session, config: &mut Config, status: &Arc<Mutex<Status>>) {
    match cmd {
        Cmd::SaveClip => match session {
            Session::Buffering { ring, cfg, audio, .. } => {
                let path = timestamped_path(&config.output_dir, "clip");
                // Create the folder here rather than assuming it exists. The
                // default is Videos\DEBRIEF, which will not exist on a first
                // run, and the failure lands at the worst possible moment —
                // the user has just pressed the hotkey to keep something.
                if let Some(dir) = path.parent() {
                    if let Err(e) = std::fs::create_dir_all(dir) {
                        set_error(status, &format!("could not create {}: {e}", dir.display()));
                        return;
                    }
                }
                // The AAC type Media Foundation negotiated, which the MP4 sink
                // needs to write the codec config. Absent when audio is off,
                // in which case the clip is silent rather than failing.
                let audio_types: Vec<(replay::AudioTrack, windows::Win32::Media::MediaFoundation::IMFMediaType)> =
                    audio
                        .iter()
                        .filter_map(|a| {
                            a.enc
                                .as_ref()
                                .and_then(|ae| ae.negotiated_type.clone())
                                .map(|ty| (a.track, ty))
                        })
                        .collect();
                match ring.save_mp4(&path.to_string_lossy(), cfg, &audio_types) {
                    Ok(r) => {
                        let mut s = status.lock().unwrap();
                        s.last_clip = Some(path.to_string_lossy().to_string());
                        s.last_save_ms = Some(r.elapsed_ms);
                        s.last_error = None;
                    }
                    Err(e) => set_error(status, &format!("save failed: {e}")),
                }
            }
            Session::Recording { .. } => {
                set_error(status, "already recording — stop the recording to use clips");
            }
            Session::None => set_error(status, "nothing buffered yet"),
        },

        Cmd::StartRecording => {
            if let Session::Recording { .. } = session {
                return;
            }
            let Some(hwnd) = capture::find_valorant() else {
                set_error(status, "Valorant is not running");
                return;
            };
            // Drop the buffering session first: one capture session per target,
            // and v1 does not run two encoders at once.
            teardown(session, status);
            match start_recording(hwnd, config) {
                Ok(new) => {
                    *session = new;
                    clear_error(status);
                }
                Err(e) => set_error(status, &format!("could not start recording: {e}")),
            }
        }

        Cmd::StopRecording => {
            if let Session::Recording { .. } = session {
                teardown(session, status);
            }
        }

        Cmd::Reconfigure(new) => {
            *config = *new;
            // Rebuild so window length, fps and bitrate take effect. Detection
            // restarts buffering on the next tick.
            teardown(session, status);
        }

        Cmd::Shutdown => unreachable!("handled by the caller"),
    }
}

/// The window to record.
///
/// `DEBRIEF_TEST_FOREGROUND=1` substitutes the foreground window for the game.
/// A test affordance, not a feature: the engine is otherwise only ever willing
/// to record Valorant, which makes the audio and clip paths unverifiable
/// whenever the game is not running. Mirrors `recorder-proto --foreground`.
fn find_target() -> Option<windows::Win32::Foundation::HWND> {
    if std::env::var("DEBRIEF_TEST_FOREGROUND").is_ok() {
        let h = unsafe { windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow() };
        return if h.0.is_null() { None } else { Some(h) };
    }
    capture::find_valorant()
}

/// Returns the session and the adapter it runs on, so the §17 monitor can read
/// VRAM from the same device the pipeline uses rather than guessing which GPU.
fn start_buffering(
    hwnd: windows::Win32::Foundation::HWND,
    config: &Config,
) -> windows::core::Result<(Session, Option<windows::Win32::Graphics::Dxgi::IDXGIAdapter3>)> {
    let dev = d3d::Device::new()?;
    let adapter = dev.adapter3().ok();
    let (cap, frames) = capture::Capture::for_window(&dev, hwnd, config.fps, 6)?;
    let w = cap.size.Width as u32;
    let h = cap.size.Height as u32;
    let gop_frames = encoder::EncoderConfig::default_gop(config.fps);
    let cfg = encoder::EncoderConfig {
        width: w,
        height: h,
        fps: config.fps,
        bitrate: config.bitrate_for(w, h),
        gop_frames,
    };
    // 512 MB cap: the window bounds the ring in time, this bounds it in bytes
    // if the bitrate estimate is ever badly wrong (ADR §6's RAM budget against
    // a 16 GB machine that is also running the game).
    let gop_secs = gop_frames as f64 / config.fps.max(1) as f64;
    let ring = Arc::new(replay::ReplayRing::new(config.window_secs, gop_secs, 512 * 1024 * 1024));
    let enc = encoder::Encoder::to_replay(&dev, &cfg, Arc::clone(&ring))?;

    // Audio needs its own encoder here: the sample grabber sink carries exactly
    // one stream, so it cannot ride the video writer the way it does on the
    // file path. Losing sound must never lose the recording, so every failure
    // below degrades to video-only rather than propagating.
    let audio = start_audio_sources(config, Some(&ring));

    cap.start()?;
    Ok((
        Session::Buffering { cap, frames, enc, ring, cfg, audio, video_base: None },
        adapter,
    ))
}

fn start_recording(hwnd: windows::Win32::Foundation::HWND, config: &Config) -> windows::core::Result<Session> {
    let dev = d3d::Device::new()?;
    let (cap, frames) = capture::Capture::for_window(&dev, hwnd, config.fps, 6)?;
    let w = cap.size.Width as u32;
    let h = cap.size.Height as u32;
    let cfg = encoder::EncoderConfig {
        width: w,
        height: h,
        fps: config.fps,
        bitrate: config.bitrate_for(w, h),
        gop_frames: encoder::EncoderConfig::default_gop(config.fps),
    };
    let path = timestamped_path(&config.output_dir, "recording");
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }

    // No ring on this path: audio rides the video writer as extra streams.
    let audio = start_audio_sources(config, None);
    let fmts: Vec<recorder_core::audio::AudioFormat> =
        audio.iter().map(|a| a.cap.format).collect();
    let enc = encoder::Encoder::to_file(&dev, &path.to_string_lossy(), &cfg, &fmts)?;
    cap.start()?;
    Ok(Session::Recording { cap, frames, enc, path, started: Instant::now(), audio })
}

/// Move whatever frames are waiting. Returns true if nothing was captured.
/// Move whatever frames are waiting. Returns true if nothing was captured.
///
/// The two session kinds route audio differently: buffering feeds a parallel
/// `AudioEncoder` and must be told video's base timestamp explicitly, while
/// recording feeds the shared video writer, which already knows its own base.
fn pump(session: &mut Session, status: &Arc<Mutex<Status>>) -> bool {
    // Bounded batch so command handling and detection still get a turn even at
    // 240 fps arrival rates.
    const BATCH: usize = 32;

    match session {
        Session::None => true,

        Session::Buffering { cap, frames, enc, audio, video_base, .. } => {
            let _ = cap.poll_resize();
            let mut moved = 0;
            while moved < BATCH {
                match frames.full_rx.try_recv() {
                    Ok((slot, ts)) => {
                        video_base.get_or_insert(ts);
                        let r = enc.write_frame(&frames.ring.textures[slot], ts);
                        // Return the slot even on failure, or the ring bleeds away.
                        let _ = frames.free_tx.send(slot);
                        if let Err(e) = r {
                            set_error(status, &format!("encode failed: {e}"));
                        }
                        moved += 1;
                    }
                    Err(_) => break,
                }
            }
            // Audio only once video has a base; packets before the first frame
            // have no timeline to sit on and are dropped by the encoder.
            for a in audio.iter_mut() {
                if let Some(ae) = &mut a.enc {
                    while let Ok(chunk) = a.rx.try_recv() {
                        let _ = ae.write(&chunk.pcm, chunk.ts_100ns, *video_base);
                    }
                }
            }
            moved == 0
        }

        Session::Recording { cap, frames, enc, audio, .. } => {
            let _ = cap.poll_resize();
            let mut moved = 0;
            while moved < BATCH {
                match frames.full_rx.try_recv() {
                    Ok((slot, ts)) => {
                        let r = enc.write_frame(&frames.ring.textures[slot], ts);
                        let _ = frames.free_tx.send(slot);
                        if let Err(e) = r {
                            set_error(status, &format!("encode failed: {e}"));
                        }
                        moved += 1;
                    }
                    Err(_) => break,
                }
            }
            for (i, a) in audio.iter().enumerate() {
                while let Ok(chunk) = a.rx.try_recv() {
                    if let Err(e) = enc.write_audio(i, &chunk.pcm, chunk.ts_100ns) {
                        set_error(status, &format!("audio encode failed: {e}"));
                    }
                }
            }
            moved == 0
        }
    }
}

fn teardown(session: &mut Session, status: &Arc<Mutex<Status>>) {
    let old = std::mem::replace(session, Session::None);
    match old {
        Session::None => {}
        Session::Buffering { cap, frames, mut enc, audio, .. } => {
            let _ = cap.stop();
            drain(&frames, &mut enc);
            let _ = enc.finish();
            for mut a in audio {
                a.cap.stop();
                if let Some(ae) = a.enc.take() {
                    let _ = ae.finish();
                }
            }
        }
        Session::Recording { cap, frames, mut enc, path, mut audio, .. } => {
            let _ = cap.stop();
            drain(&frames, &mut enc);
            // Stop audio before the final drain, or the loop chases a stream
            // that is still being fed and never ends.
            for a in audio.iter_mut() {
                a.cap.stop();
            }
            for (i, a) in audio.iter().enumerate() {
                while let Ok(chunk) = a.rx.try_recv() {
                    let _ = enc.write_audio(i, &chunk.pcm, chunk.ts_100ns);
                }
            }
            // finish() writes the moov atom; skipping it leaves a file that
            // looks like an encoder bug rather than an interrupted recording.
            if let Err(e) = enc.finish() {
                set_error(status, &format!("finalising recording failed: {e}"));
            }
            let mut s = status.lock().unwrap();
            s.last_clip = Some(path.to_string_lossy().to_string());
            s.recording_path = None;
        }
    }
    let mut s = status.lock().unwrap();
    s.state = State::Idle;
    s.buffered_secs = 0.0;
    s.ring_mb = 0.0;
    s.recording_secs = 0.0;
}

fn drain(frames: &capture::Frames, enc: &mut encoder::Encoder) {
    while let Ok((slot, ts)) = frames.full_rx.try_recv() {
        let _ = enc.write_frame(&frames.ring.textures[slot], ts);
        let _ = frames.free_tx.send(slot);
    }
}

fn publish(session: &Session, status: &Arc<Mutex<Status>>) {
    let mut s = status.lock().unwrap();
    match session {
        Session::None => {
            s.state = State::Idle;
        }
        Session::Buffering { cap, ring, .. } => {
            s.state = State::Buffering;
            let r = ring.report();
            s.buffered_secs = r.span_secs;
            s.ring_mb = r.bytes as f64 / 1e6;
            copy_capture_stats(&mut s, cap);
        }
        Session::Recording { cap, path, started, .. } => {
            s.state = State::Recording;
            s.recording_path = Some(path.to_string_lossy().to_string());
            s.recording_secs = started.elapsed().as_secs_f64();
            copy_capture_stats(&mut s, cap);
        }
    }
}

fn copy_capture_stats(s: &mut Status, cap: &capture::Capture) {
    use std::sync::atomic::Ordering;
    s.frames_kept = cap.stats.kept.load(Ordering::Relaxed);
    s.dropped_ring_full = cap.stats.dropped_ring_full.load(Ordering::Relaxed);
    s.dropped_resized = cap.stats.dropped_size_mismatch.load(Ordering::Relaxed);
    s.callback_p99_us = cap.stats.percentile_us(0.99);
}

fn set_error(status: &Arc<Mutex<Status>>, msg: &str) {
    status.lock().unwrap().last_error = Some(msg.to_string());
}

fn clear_error(status: &Arc<Mutex<Status>>) {
    status.lock().unwrap().last_error = None;
}

/// `<dir>/<kind>-YYYYMMDD-HHMMSS.mp4`, in local time.
fn timestamped_path(dir: &Path, kind: &str) -> PathBuf {
    use windows::Win32::System::SystemInformation::GetLocalTime;
    let t = unsafe { GetLocalTime() };
    let name = format!(
        "{kind}-{:04}{:02}{:02}-{:02}{:02}{:02}.mp4",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    );
    dir.join(name)
}
