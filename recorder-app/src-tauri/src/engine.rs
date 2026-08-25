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
    /// Title of the window being (or about to be) captured, so the UI can
    /// say "buffering: Notepad" instead of leaving the user to find out from
    /// the clip.
    pub target_title: Option<String>,
    /// A queued action waiting on the target — recording queued while the game
    /// is minimised, say. Shown as status, never as an error: nothing is
    /// wrong, something is waiting.
    pub pending: Option<String>,
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
            target_title: None,
            pending: None,
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
    /// The capture threads feeding this track. One for a plain source, two
    /// when the mixer is summing desktop and microphone into a single track.
    caps: Vec<recorder_core::audio::AudioCapture>,
    /// Present only when this track is a mix; owns the mixing thread.
    mixer: Option<recorder_core::mix::AudioMixer>,
    rx: Receiver<recorder_core::audio::AudioChunk>,
    /// The format actually being delivered — the mixer's master rate when
    /// mixing, the capture's otherwise.
    format: recorder_core::audio::AudioFormat,
    /// Only the replay path has its own encoder; on the file path audio rides
    /// the video writer as an extra stream.
    enc: Option<encoder::AudioEncoder>,
    track: replay::AudioTrack,
}

impl AudioSide {
    fn stop_capture(&mut self) {
        // Captures first, then the mixer: the mixer drains what is already in
        // flight and exits when its inputs close, so stopping it first would
        // strand the last packets.
        for c in self.caps.iter_mut() {
            c.stop();
        }
        if let Some(m) = self.mixer.as_mut() {
            m.stop();
        }
    }
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
    use recorder_core::mix::AudioMixer;

    // Open each capture the config asks for. A failure here is never fatal:
    // a machine with no microphone still records desktop audio.
    let mut opened = Vec::new();
    if config.capture_audio {
        match AudioCapture::start_on(
            AudioSource::Loopback,
            config.device_for(false),
            config.desktop_gain_clamped(),
        ) {
            Ok(pair) => opened.push((AudioSource::Loopback, pair)),
            Err(e) => eprintln!("desktop audio unavailable, continuing without it: {e}"),
        }
    }
    if config.capture_mic {
        match AudioCapture::start_on(
            AudioSource::Microphone,
            config.device_for(true),
            config.mic_gain_clamped(),
        ) {
            Ok(pair) => opened.push((AudioSource::Microphone, pair)),
            Err(e) => eprintln!("microphone unavailable, continuing without it: {e}"),
        }
    }

    // Mixing needs both. If one failed to open, fall through to whatever did —
    // a "mixed" track of one source is that source with a resampler in the way.
    let mix = config.mixing() && opened.len() == 2;
    let mut sides: Vec<AudioSide> = Vec::new();

    if mix {
        let mut it = opened.into_iter();
        let (_, (desktop_cap, desktop_rx)) = it.next().expect("two opened");
        let (_, (mic_cap, mic_rx)) = it.next().expect("two opened");
        // Desktop is the master clock: it is the stream that must not be
        // resampled, since it carries the game.
        //
        // The microphone's gain is applied in the mixer rather than at its
        // capture, because the mixer resamples it anyway — one scaling instead
        // of two, and the slider still lands on the next packet.
        mic_cap.set_gain(1.0);
        let (mixer, rx) = AudioMixer::start(
            (desktop_rx, desktop_cap.format),
            (mic_rx, mic_cap.format),
            config.mic_gain_clamped(),
        );
        let format = mixer.format;
        sides.push(AudioSide {
            caps: vec![desktop_cap, mic_cap],
            mixer: Some(mixer),
            rx,
            format,
            enc: None,
            track: replay::AudioTrack::Mixed,
        });
    } else {
        for (src, (cap, rx)) in opened {
            let track = match src {
                AudioSource::Loopback => replay::AudioTrack::Desktop,
                AudioSource::Microphone => replay::AudioTrack::Mic,
            };
            let format = cap.format;
            sides.push(AudioSide {
                caps: vec![cap],
                mixer: None,
                rx,
                format,
                enc: None,
                track,
            });
        }
    }

    // The replay path needs one encoder per track, writing into the ring. The
    // file path has none: audio rides the video writer as extra streams.
    if let Some(r) = ring {
        sides.retain_mut(|side| {
            match encoder::AudioEncoder::to_replay(side.track, &side.format, Arc::clone(r)) {
                Ok(ae) => {
                    side.enc = Some(ae);
                    true
                }
                Err(e) => {
                    eprintln!("{} encoder unavailable: {e}", side.track.label());
                    side.stop_capture();
                    false
                }
            }
        });
    }
    sides
}

/// Push live gain and device changes at running captures.
///
/// Gain is the reason this exists: restarting the session to change a slider
/// would drop the replay ring, so the user would lose their buffered footage
/// for turning the microphone down. Device changes cannot be applied this way
/// — a different endpoint is a different stream — so they still restart, which
/// is what `needs_audio_restart` decides.
fn apply_audio_settings(audio: &[AudioSide], config: &Config) {
    for side in audio {
        match side.track {
            replay::AudioTrack::Mixed => {
                // caps[0] is desktop, caps[1] the microphone; the mic's gain
                // lives in the mixer.
                if let Some(c) = side.caps.first() {
                    c.set_gain(config.desktop_gain_clamped());
                }
                if let Some(m) = &side.mixer {
                    m.set_other_gain(config.mic_gain_clamped());
                }
            }
            replay::AudioTrack::Desktop => {
                if let Some(c) = side.caps.first() {
                    c.set_gain(config.desktop_gain_clamped());
                }
            }
            replay::AudioTrack::Mic => {
                if let Some(c) = side.caps.first() {
                    c.set_gain(config.mic_gain_clamped());
                }
            }
        }
    }
}

/// Whether a settings change requires rebuilding the capture session.
///
/// Listed as what *does* force a rebuild rather than what does not, so a field
/// added later defaults to the safe answer only if someone remembers to add it
/// here — hence the exhaustive destructure below, which makes the compiler
/// raise the question instead.
fn needs_session_restart(old: &Config, new: &Config) -> bool {
    // Destructured so that adding a field to Config fails to compile until
    // someone decides which side of this line it falls on.
    let Config {
        window_secs,
        fps,
        bitrate_mbps,
        output_dir,
        save_hotkey: _,   // needs an app restart regardless
        auto_buffer: _,   // read on the next detection tick
        target,
        capture_audio,
        capture_mic,
        mix_audio: _,     // covered by mixing() below
        desktop_gain: _,  // pushed live
        mic_gain: _,      // pushed live
        desktop_device,
        mic_device,
        player: _,        // only ever read when writing a sidecar
    } = new;

    old.window_secs != *window_secs
        || old.fps != *fps
        || old.bitrate_mbps != *bitrate_mbps
        || old.output_dir != *output_dir
        || old.target != *target
        || old.capture_audio != *capture_audio
        || old.capture_mic != *capture_mic
        || old.mixing() != new.mixing()
        || old.desktop_device != *desktop_device
        || old.mic_device != *mic_device
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
        /// Wall clock at the moment capture began — the alignment key for
        /// multi-POV review. Captured here rather than derived from the file
        /// later: a file's mtime is the *end* of a recording, which would put
        /// every POV out by its own duration.
        started_utc: (String, i64),
        audio: Vec<AudioSide>,
        cfg: encoder::EncoderConfig,
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
    // What the live session captures. Lives on this thread only; handles never
    // cross to the UI, which sees a label.
    let mut locked: Option<capture::Source> = None;
    // A "start recording" the user asked for while the target could not be
    // captured — an exclusive-fullscreen game minimises the moment they click,
    // so honouring the click means remembering it.
    let mut pending_record = false;

    loop {
        // ---- commands -------------------------------------------------
        match rx.try_recv() {
            Ok(Cmd::Shutdown) | Err(TryRecvError::Disconnected) => {
                teardown(&mut session, &status, &config);
                break;
            }
            Ok(cmd) => handle_cmd(cmd, &mut session, &mut config, &status, &mut locked, &mut pending_record),
            Err(TryRecvError::Empty) => {}
        }

        // ---- detection ------------------------------------------------
        if Instant::now() >= next_detect {
            next_detect = Instant::now() + Duration::from_secs(2);
            // A session that ended by any route — stop button, reconfigure,
            // error — leaves no lock behind.
            if matches!(session, Session::None) {
                locked = None;
            }
            match locked {
                // Idle: resolve the configured target and act on it.
                None => {
                    let found = find_target(&config.target);
                    let iconic = found.map(source_iconic).unwrap_or(false);
                    {
                        let mut st = status.lock().unwrap();
                        st.game_running = found.is_some();
                        // What *would* be recorded, so the UI can name it
                        // before anything starts.
                        st.target_title = found.and_then(|f| f.label());
                        st.pending = match (found.is_some(), iconic, pending_record) {
                            // A minimised window reports the 160x28 iconic
                            // placeholder, which the capture core rightly
                            // refuses as too small. Nothing is wrong; something
                            // is waiting. Say that, instead of retrying into
                            // the same error every two seconds.
                            (true, true, true) => {
                                Some("window is minimised — recording starts when it comes back".into())
                            }
                            (true, true, false) => {
                                Some("window is minimised — capture resumes when it comes back".into())
                            }
                            (false, _, true) => {
                                Some("recording queued — waiting for the target to appear".into())
                            }
                            _ => None,
                        };
                    }
                    if let (Some(src), false) = (found, iconic) {
                        if pending_record {
                            match start_recording(src, &config) {
                                Ok(new) => {
                                    session = new;
                                    locked = Some(src);
                                    pending_record = false;
                                    let mut st = status.lock().unwrap();
                                    st.pending = None;
                                    st.last_error = None;
                                }
                                Err(e) => {
                                    // Transient — a display-mode switch mid-
                                    // restore reports odd sizes for a moment.
                                    // The queue survives the retry.
                                    set_error(&status, &format!("could not start recording: {e}"));
                                }
                            }
                        } else if config.auto_buffer {
                            match start_buffering(src, &config) {
                                Ok((new, adapter3)) => {
                                    session = new;
                                    adapter = adapter3;
                                    locked = Some(src);
                                    clear_error(&status);
                                }
                                Err(e) => {
                                    // Retry on the next tick rather than
                                    // giving up for good.
                                    set_error(&status, &format!("could not start buffering: {e}"));
                                }
                            }
                        }
                    }
                }
                // Live: the only question is whether the captured thing still
                // exists. Not whether it is focused, and not whether its title
                // still matches — a browser retitles itself on every tab
                // switch, and a session that died on that would be useless.
                // A recording loses nothing on teardown: finish() still writes
                // the moov atom.
                Some(src) => {
                    if !src.alive() {
                        teardown(&mut session, &status, &config);
                        locked = None;
                        let mut st = status.lock().unwrap();
                        st.game_running = false;
                        st.target_title = None;
                    }
                }
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

fn handle_cmd(
    cmd: Cmd,
    session: &mut Session,
    config: &mut Config,
    status: &Arc<Mutex<Status>>,
    locked: &mut Option<capture::Source>,
    pending_record: &mut bool,
) {
    match cmd {
        Cmd::SaveClip => match session {
            Session::Buffering { ring, cfg, audio, .. } => {
                let path =
                    timestamped_path(&config.output_dir.join(crate::media::CLIPS_DIR), "clip");
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
                        // Two enabled audio tracks make every player guess
                        // differently — the gallery played desktop while
                        // Windows played nothing but the mic. Say which one is
                        // meant. Never fatal: a clip with an ambiguous
                        // container still holds the footage.
                        if let Err(e) = recorder_core::mp4::mark_audio_alternates(&path) {
                            eprintln!("could not mark audio tracks in {}: {e}", path.display());
                        }
                        // A clip's start is derived, not observed: the ring
                        // holds the window *ending* now, so the footage began
                        // `span_secs` ago. Getting this wrong would shift this
                        // POV against everyone else's by the clip length.
                        let (_, now_ms) = crate::vod::now_utc();
                        let start_ms = now_ms - (r.span_secs * 1000.0) as i64;
                        write_sidecar(
                            &path,
                            &(crate::vod::rfc3339(start_ms), start_ms),
                            r.span_secs,
                            cfg,
                            audio,
                            config,
                            crate::vod::RecordingKind::Clip,
                            status,
                        );
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
            let Some(src) = find_target(&config.target) else {
                set_error(
                    status,
                    if config.target.is_valorant() {
                        "Valorant is not running"
                    } else {
                        "the chosen window or screen is not available — pick another"
                    },
                );
                return;
            };
            if source_iconic(src) {
                // The catch-22 this queue exists for: an exclusive-fullscreen
                // game minimises the moment the user tabs out, and tabbing out
                // is how this button gets clicked. Failing here with "too
                // small to record" made recording unstartable at any
                // resolution where the game goes truly fullscreen. Queue the
                // intent, hand focus straight back, and let the tick start the
                // recording once the window has a real size again.
                *pending_record = true;
                {
                    let mut st = status.lock().unwrap();
                    st.pending = Some("bringing the window back — recording will start".into());
                    st.last_error = None;
                }
                restore_window(src);
                return;
            }
            // Drop the buffering session first: one capture session per target,
            // and v1 does not run two encoders at once.
            teardown(session, status, config);
            match start_recording(src, config) {
                Ok(new) => {
                    *session = new;
                    *locked = Some(src);
                    *pending_record = false;
                    let mut st = status.lock().unwrap();
                    st.target_title = src.label();
                    st.pending = None;
                    st.last_error = None;
                }
                Err(e) => {
                    *locked = None;
                    set_error(status, &format!("could not start recording: {e}"));
                }
            }
        }

        Cmd::StopRecording => {
            *pending_record = false;
            status.lock().unwrap().pending = None;
            if let Session::Recording { .. } = session {
                teardown(session, status, config);
                *locked = None;
            }
        }

        Cmd::Reconfigure(new) => {
            // A volume slider must not cost the user their buffered footage.
            // Tearing down discards the replay ring, so settings that a live
            // session can absorb — the gains — are pushed at it instead, and
            // only structural changes rebuild. Compared before the swap, while
            // the old config is still here to compare against.
            let restart = needs_session_restart(config, &new);
            if !restart {
                if let Session::Buffering { audio, .. } | Session::Recording { audio, .. } = session
                {
                    apply_audio_settings(audio, &new);
                }
                *config = *new;
                return;
            }
            *config = *new;
            // Rebuild so window length, fps and bitrate take effect. Detection
            // restarts buffering on the next tick. The lock is cleared so a
            // target-mode change starts from a clean slate rather than
            // resuming on whatever the previous mode had chosen.
            teardown(session, status, config);
            *locked = None;
            *pending_record = false;
            status.lock().unwrap().pending = None;
        }

        Cmd::Shutdown => unreachable!("handled by the caller"),
    }
}

/// The window to record.
///
/// Whether a window source is currently minimised. Monitors never are.
fn source_iconic(src: capture::Source) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::IsIconic;
    match src {
        capture::Source::Window(h) => unsafe { IsIconic(h) }.as_bool(),
        capture::Source::Monitor(_) => false,
    }
}

/// Restore a minimised window and hand it the foreground. DEBRIEF is the
/// foreground window at the moment this runs — the user just clicked it —
/// which is exactly the condition under which Windows allows an app to give
/// focus away.
fn restore_window(src: capture::Source) {
    use windows::Win32::UI::WindowsAndMessaging::{SetForegroundWindow, ShowWindow, SW_RESTORE};
    if let capture::Source::Window(h) = src {
        unsafe {
            let _ = ShowWindow(h, SW_RESTORE);
            let _ = SetForegroundWindow(h);
        }
    }
}

/// Resolve the configured target to something capturable, right now.
///
/// `DEBRIEF_TEST_FOREGROUND=1` substitutes the foreground window regardless
/// of configuration, kept for scripted tests. Otherwise the saved identity is
/// looked up fresh: a window by title and class, a monitor by device name,
/// Valorant by its window class.
fn find_target(target: &crate::config::Target) -> Option<capture::Source> {
    use crate::config::Target;
    use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;

    if std::env::var("DEBRIEF_TEST_FOREGROUND").is_ok() {
        let h = unsafe { GetForegroundWindow() };
        return if h.0.is_null() { None } else { Some(capture::Source::Window(h)) };
    }
    match target {
        Target::Valorant => capture::find_valorant().map(capture::Source::Window),
        Target::Monitor { device } => capture::find_monitor(device).map(capture::Source::Monitor),
        Target::Window { title, class } => {
            capture::find_window_by_identity(title, class).map(capture::Source::Window)
        }
    }
}

/// Returns the session and the adapter it runs on, so the §17 monitor can read
/// VRAM from the same device the pipeline uses rather than guessing which GPU.
fn start_buffering(
    source: capture::Source,
    config: &Config,
) -> windows::core::Result<(Session, Option<windows::Win32::Graphics::Dxgi::IDXGIAdapter3>)> {
    let dev = d3d::Device::new()?;
    let adapter = dev.adapter3().ok();
    let (cap, frames) = capture::Capture::for_source(&dev, source, config.fps, 6)?;
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

fn start_recording(source: capture::Source, config: &Config) -> windows::core::Result<Session> {
    let dev = d3d::Device::new()?;
    let (cap, frames) = capture::Capture::for_source(&dev, source, config.fps, 6)?;
    let w = cap.size.Width as u32;
    let h = cap.size.Height as u32;
    let cfg = encoder::EncoderConfig {
        width: w,
        height: h,
        fps: config.fps,
        bitrate: config.bitrate_for(w, h),
        gop_frames: encoder::EncoderConfig::default_gop(config.fps),
    };
    let path =
        timestamped_path(&config.output_dir.join(crate::media::RECORDINGS_DIR), "recording");
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }

    // No ring on this path: audio rides the video writer as extra streams.
    let audio = start_audio_sources(config, None);
    let fmts: Vec<recorder_core::audio::AudioFormat> =
        audio.iter().map(|a| a.format).collect();
    let enc = encoder::Encoder::to_file(&dev, &path.to_string_lossy(), &cfg, &fmts)?;
    cap.start()?;
    Ok(Session::Recording {
        cap,
        frames,
        enc,
        path,
        started: Instant::now(),
        started_utc: crate::vod::now_utc(),
        audio,
        cfg,
    })
}

/// Write the sidecar that makes a recording reviewable alongside other POVs.
///
/// Failure here is reported but never fatal: a recording without metadata is
/// still a recording, and losing the video because its JSON could not be
/// written would be a bad trade.
fn write_sidecar(
    path: &Path,
    started_utc: &(String, i64),
    duration_secs: f64,
    cfg: &encoder::EncoderConfig,
    audio: &[AudioSide],
    config: &Config,
    kind: crate::vod::RecordingKind,
    status: &Arc<Mutex<Status>>,
) {
    let bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let meta = crate::vod::VodMeta {
        version: 1,
        started_utc: started_utc.0.clone(),
        started_epoch_ms: started_utc.1,
        duration_secs: (duration_secs * 100.0).round() / 100.0,
        width: cfg.width,
        height: cfg.height,
        fps: cfg.fps,
        video_codec: "h264".into(),
        audio_tracks: audio.iter().map(|a| a.track.label().to_string()).collect(),
        player: config.player.clone(),
        app_version: env!("CARGO_PKG_VERSION").to_string(),
        kind,
        file: path
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_default(),
        bytes,
    };
    if let Err(e) = meta.write(path) {
        set_error(status, &format!("could not write recording metadata: {e}"));
    }
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

fn teardown(session: &mut Session, status: &Arc<Mutex<Status>>, config: &Config) {
    let old = std::mem::replace(session, Session::None);
    match old {
        Session::None => {}
        Session::Buffering { cap, frames, mut enc, audio, .. } => {
            let _ = cap.stop();
            drain(&frames, &mut enc);
            let _ = enc.finish();
            for mut a in audio {
                a.stop_capture();
                if let Some(ae) = a.enc.take() {
                    let _ = ae.finish();
                }
            }
        }
        Session::Recording { cap, frames, mut enc, path, mut audio, started, started_utc, cfg, .. } => {
            let _ = cap.stop();
            drain(&frames, &mut enc);
            // Stop audio before the final drain, or the loop chases a stream
            // that is still being fed and never ends.
            for a in audio.iter_mut() {
                a.stop_capture();
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
            // Same disambiguation as the clip path, once the moov exists.
            if let Err(e) = recorder_core::mp4::mark_audio_alternates(&path) {
                eprintln!("could not mark audio tracks in {}: {e}", path.display());
            }
            // Sidecar after finish(), so the byte count is the finished file's.
            write_sidecar(
                &path,
                &started_utc,
                started.elapsed().as_secs_f64(),
                &cfg,
                &audio,
                config,
                crate::vod::RecordingKind::Recording,
                status,
            );
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
