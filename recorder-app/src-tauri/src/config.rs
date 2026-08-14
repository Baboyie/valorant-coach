//! Settings, persisted as JSON next to the app's data.
//!
//! Deliberately a plain file the user can read and edit: this is a tool for
//! people who cap their frame rate and care about 1% lows, and a settings file
//! they can inspect is a feature for that audience.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

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
    /// Whether to start buffering automatically when Valorant appears.
    pub auto_buffer: bool,
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
            Ok(s) => match serde_json::from_str::<Config>(&s) {
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
    /// override. The automatic figure is ~0.1 bits per pixel per frame, which
    /// lands near 12 Mbps at 1080p60 — enough for competitive footage without
    /// making the encoder work for picture quality nobody reviews.
    pub fn bitrate_for(&self, width: u32, height: u32) -> u32 {
        if self.bitrate_mbps > 0 {
            return self.bitrate_mbps.saturating_mul(1_000_000);
        }
        ((width as u64 * height as u64 * self.fps as u64) / 10).min(80_000_000) as u32
    }
}
