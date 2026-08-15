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
    /// the video writer as a second stream.
    enc: Option<encoder::AudioEncoder>,
}

enum Session {
    None,
    Buffering {
        cap: capture::Capture,
        frames: capture::Frames,
        enc: encoder::Encoder,
        ring: Arc<replay::ReplayRing>,
        cfg: encoder::EncoderConfig,
        audio: Option<AudioSide>,
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
        audio: Option<AudioSide>,
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
                        Ok(new) => {
                            session = new;
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
                let audio_type = audio
                    .as_ref()
                    .and_then(|a| a.enc.as_ref())
                    .and_then(|ae| ae.negotiated_type.clone());
                match ring.save_mp4(&path.to_string_lossy(), cfg, audio_type.as_ref()) {
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

fn start_buffering(hwnd: windows::Win32::Foundation::HWND, config: &Config) -> windows::core::Result<Session> {
    let dev = d3d::Device::new()?;
    let (cap, frames) = capture::Capture::for_window(&dev, hwnd, config.fps, 6)?;
    let w = cap.size.Width as u32;
    let h = cap.size.Height as u32;
    let cfg = encoder::EncoderConfig {
        width: w,
        height: h,
        fps: config.fps,
        bitrate: config.bitrate_for(w, h),
    };
    // 512 MB cap: the window bounds the ring in time, this bounds it in bytes
    // if the bitrate estimate is ever badly wrong (ADR §6's RAM budget against
    // a 16 GB machine that is also running the game).
    let ring = Arc::new(replay::ReplayRing::new(config.window_secs, 2, 512 * 1024 * 1024));
    let enc = encoder::Encoder::to_replay(&dev, &cfg, Arc::clone(&ring))?;

    // Audio needs its own encoder here: the sample grabber sink carries exactly
    // one stream, so it cannot ride the video writer the way it does on the
    // file path. Losing sound must never lose the recording, so every failure
    // below degrades to video-only rather than propagating.
    let audio = if config.capture_audio {
        match recorder_core::audio::AudioCapture::start() {
            Ok((c, rx)) => match encoder::AudioEncoder::to_replay(&c.format, Arc::clone(&ring)) {
                Ok(ae) => Some(AudioSide { cap: c, rx, enc: Some(ae) }),
                Err(e) => {
                    eprintln!("audio encoder unavailable, buffering video only: {e}");
                    None
                }
            },
            Err(e) => {
                eprintln!("audio unavailable, buffering video only: {e}");
                None
            }
        }
    } else {
        None
    };

    cap.start()?;
    Ok(Session::Buffering { cap, frames, enc, ring, cfg, audio, video_base: None })
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
    };
    let path = timestamped_path(&config.output_dir, "recording");
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }

    let audio = if config.capture_audio {
        match recorder_core::audio::AudioCapture::start() {
            Ok((c, rx)) => Some(AudioSide { cap: c, rx, enc: None }),
            Err(e) => {
                eprintln!("audio unavailable, recording video only: {e}");
                None
            }
        }
    } else {
        None
    };

    // On the file path audio is a second stream on the same writer, so the
    // encoder needs the format up front.
    let enc = encoder::Encoder::to_file(
        &dev,
        &path.to_string_lossy(),
        &cfg,
        audio.as_ref().map(|a| &a.cap.format),
    )?;
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
            if let Some(a) = audio {
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
            if let Some(a) = audio {
                while let Ok(chunk) = a.rx.try_recv() {
                    if let Err(e) = enc.write_audio(&chunk.pcm, chunk.ts_100ns) {
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
            if let Some(mut a) = audio {
                a.cap.stop();
                if let Some(ae) = a.enc.take() {
                    let _ = ae.finish();
                }
            }
        }
        Session::Recording { cap, frames, mut enc, path, audio, .. } => {
            let _ = cap.stop();
            drain(&frames, &mut enc);
            // Stop audio before the final drain, or the loop chases a stream
            // that is still being fed and never ends.
            if let Some(mut a) = audio {
                a.cap.stop();
                while let Ok(chunk) = a.rx.try_recv() {
                    let _ = enc.write_audio(&chunk.pcm, chunk.ts_100ns);
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
