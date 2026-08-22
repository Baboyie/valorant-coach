//! Settings, persisted as JSON next to the app's data.
//!
//! Deliberately a plain file the user can read and edit: this is a tool for
//! people who cap their frame rate and care about 1% lows, and a settings file
//! they can inspect is a feature for that audience.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// What to record.
///
/// Persisted by *identity* rather than by handle: a window handle means nothing
/// after a restart, while a monitor's device name and a window's title and
/// class do. The engine resolves the identity to a live handle on each
/// detection tick.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Target {
    /// Valorant, found by window class. The default, and the only target the
    /// §29 performance claims speak for.
    Valorant,
    /// A whole monitor, by device name (`\\.\DISPLAY1`).
    Monitor { device: String },
    /// One window, by title and class. Titles move — a browser retitles itself
    /// on every tab switch — so the engine falls back to "the only window of
    /// this class" when the exact title is gone.
    Window { title: String, class: String },
}

impl Default for Target {
    fn default() -> Self {
        Target::Valorant
    }
}

impl Target {
    pub fn is_valorant(&self) -> bool {
        matches!(self, Target::Valorant)
    }
}

// Accepts the tagged form above and the first shipped form, which was a bare
// string — `"valorant"` or `"foreground"`. A config that fails to parse falls
// back to defaults *wholesale*, taking the player name and hotkey with it, so
// one stale field must not be allowed to do that.
impl<'de> Deserialize<'de> for Target {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(tag = "kind", rename_all = "lowercase")]
        enum Tagged {
            Valorant,
            Monitor { device: String },
            Window { title: String, class: String },
        }
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Tagged(Tagged),
            // Anything else at all: the first shipped form was a bare string,
            // and a hand-edited config can hold worse. Tried last, so it only
            // catches what the tagged form rejected.
            Legacy(serde::de::IgnoredAny),
        }
        Ok(match Repr::deserialize(d)? {
            // "foreground" carried no persistent identity to bring forward.
            Repr::Legacy(_) => Target::Valorant,
            Repr::Tagged(Tagged::Valorant) => Target::Valorant,
            Repr::Tagged(Tagged::Monitor { device }) => Target::Monitor { device },
            Repr::Tagged(Tagged::Window { title, class }) => Target::Window { title, class },
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Seconds of gameplay kept in the replay ring.
    pub window_secs: u64,
    /// Recording frame rate. 60 is the default because §22 prioritises
    /// compatibility and low encoder cost over matching a 240 Hz panel — the
    /// clip is for review, not for playing back at native refresh.
    pub fps: u32,
    /// Mbps. 0 means derive from resolution and frame rate.
    pub bitrate_mbps: u32,
    /// Where clips and recordings land.
    pub output_dir: PathBuf,
    /// Global hotkey for saving a clip, in Tauri accelerator syntax.
    pub save_hotkey: String,
    /// Whether to start buffering automatically when the target appears.
    pub auto_buffer: bool,
    /// What to record. `Valorant` is the product; windows and monitors exist
    /// because teammates asked to record other things, and the capture layer
    /// never cared — it wraps whatever Windows.Graphics.Capture can.
    pub target: Target,
    /// Record desktop audio (WASAPI loopback) alongside video.
    pub capture_audio: bool,
    /// Record the microphone as a **separate track** (§23), so a reviewer can
    /// isolate or mute the player's own voice. Off by default: many machines
    /// have no usable microphone, and a silent extra track is worse than none.
    pub capture_mic: bool,

    /// Who this machine's POV belongs to, for multi-POV review. Free text —
    /// a Riot `name#tag` is the obvious choice, but a team can use whatever
    /// they already call each other.
    pub player: String,
    //
    // `upload_url` and `auto_upload` used to live here, aimed at the review
    // server's own upload endpoint. Nothing ever read them, that endpoint is
    // gone, and full recordings now go to YouTube with only the link stored.
    // Whenever automatic upload is built it will need OAuth credentials and a
    // channel, not a URL, so these were removed rather than left as settings
    // that describe a plan the app no longer has.
}

impl Default for Config {
    fn default() -> Self {
        Config {
            window_secs: 30,
            fps: 60,
            bitrate_mbps: 0,
            output_dir: default_output_dir(),
            // Alt+F10 rather than F10 alone: Valorant uses bare function keys,
            // and a global hotkey that shadows a game binding is a bug the user
            // experiences as the game breaking.
            save_hotkey: "Alt+F10".into(),
            auto_buffer: true,
            target: Target::Valorant,
            capture_audio: true,
            capture_mic: false,
            player: String::new(),
        }
    }
}

fn default_output_dir() -> PathBuf {
    // Videos\DEBRIEF — where a user looks for recordings without being told.
    if let Some(p) = dirs_videos() {
        p.join("DEBRIEF")
    } else {
        PathBuf::from("clips")
    }
}

fn dirs_videos() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE").map(|u| PathBuf::from(u).join("Videos"))
}

pub fn config_path() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("DEBRIEF").join("config.json")
}

impl Config {
    pub fn load() -> Config {
        let path = config_path();
        match std::fs::read_to_string(&path) {
            // Strip a UTF-8 BOM before parsing. Notepad and PowerShell's
            // `-Encoding utf8` both write one, serde_json rejects it outright
            // ("expected value at line 1 column 1"), and the fallback is
            // silent to anyone running this as a tray app — a user who edited
            // their settings by hand would simply find them reverted, with the
            // explanation on a stderr they never see. This config is meant to
            // be hand-editable, so it has to survive the editors people
            // actually use.
            Ok(s) => match serde_json::from_str::<Config>(s.trim_start_matches('\u{feff}')) {
                Ok(c) => c,
                Err(e) => {
                    // A malformed config must not stop the app from recording.
                    // Defaults, and say so, rather than refusing to start.
                    eprintln!("config at {} is invalid ({e}); using defaults", path.display());
                    Config::default()
                }
            },
            Err(_) => Config::default(),
        }
    }

    pub fn save(&self) -> std::io::Result<()> {
        let path = config_path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(path, json)
    }

    /// Bitrate in bits/sec for a given frame size, honouring an explicit
    /// override.
    ///
    /// The automatic figure tracks what ShadowPlay settles on for 1080p60
    /// (~16 Mbps), measured from its own output on this machine rather than
    /// guessed — §22 asks for quality comparable to it, and bitrate is nearly
    /// free on NVENC, costing disk and file size rather than encode time.
    pub fn bitrate_for(&self, width: u32, height: u32) -> u32 {
        if self.bitrate_mbps > 0 {
            return self.bitrate_mbps.saturating_mul(1_000_000);
        }
        recorder_core::encoder::EncoderConfig::default_bitrate(width, height, self.fps)
    }
}
