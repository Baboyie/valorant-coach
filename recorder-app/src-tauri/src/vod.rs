//! Recording metadata — the sidecar that makes multi-POV review possible.
//!
//! **Absolute time is the whole point of this file.** Everything else in the
//! recorder works in QueryPerformanceCounter units, which are monotonic and
//! precise but meaningless across machines: two teammates' QPC values have no
//! relationship at all. Aligning five players' POVs of the same round therefore
//! needs one thing the pipeline never had — the UTC instant each recording
//! started.
//!
//! Written as JSON rather than into the MP4 so it stays readable and editable
//! without a muxer, and so a recording whose upload failed can still be
//! matched up later by hand. But **not next to the video**: the output folder
//! is something people browse and share, and a JSON file shadowing every clip
//! reads as clutter (it was asked about twice). Metadata belongs to the app,
//! in its own data directory, keyed by the video's filename — which carries a
//! timestamp to the second and is therefore unique.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VodMeta {
    /// Schema version, so a server can accept sidecars from older app builds
    /// rather than rejecting a teammate who has not updated.
    pub version: u32,
    /// RFC 3339 UTC, e.g. "2026-08-15T12:34:56.789Z". **The alignment key.**
    pub started_utc: String,
    /// Milliseconds since the epoch — the same instant, in the form a
    /// JavaScript player can subtract without parsing.
    pub started_epoch_ms: i64,
    pub duration_secs: f64,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub video_codec: String,
    /// One entry per audio track, in container order.
    pub audio_tracks: Vec<String>,
    /// Who this POV belongs to. Free text so a team can use whatever names they
    /// already call each other; a Riot name#tag is the obvious choice.
    pub player: String,
    /// What produced this, for when a sidecar outlives the build that wrote it.
    pub app_version: String,
    pub kind: RecordingKind,
    pub file: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RecordingKind {
    /// A hotkey-saved replay clip.
    Clip,
    /// A manually started recording.
    Recording,
}

/// The app's metadata directory: `%APPDATA%\DEBRIEF\meta`.
///
/// `DEBRIEF_META_DIR` overrides it so tests can exercise migration without
/// writing into the real profile.
pub fn meta_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("DEBRIEF_META_DIR") {
        return PathBuf::from(d);
    }
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("DEBRIEF")
        .join("meta")
}

impl VodMeta {
    /// Metadata path for a recording: `...\clips\foo.mp4` -> `<meta>\foo.json`.
    pub fn sidecar_path(video: &Path) -> PathBuf {
        let mut name = video.file_stem().unwrap_or_default().to_os_string();
        name.push(".json");
        meta_dir().join(name)
    }

    pub fn write(&self, video: &Path) -> std::io::Result<PathBuf> {
        let path = Self::sidecar_path(video);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        // No BOM — the server and every JSON parser reject one, and this
        // project has already lost a config to exactly that (see config.rs).
        std::fs::write(&path, json)?;
        Ok(path)
    }
}

/// The instant a recording started, from the wall clock.
///
/// Taken at the moment capture begins rather than when the file is finalised:
/// the file's mtime is the *end* of a recording, and using it would put every
/// POV out by its own duration — the exact error this metadata exists to avoid.
pub fn now_utc() -> (String, i64) {
    use windows::Win32::System::SystemInformation::GetSystemTimeAsFileTime;

    let ft = unsafe { GetSystemTimeAsFileTime() };
    let ticks = ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64;
    // FILETIME counts 100 ns intervals from 1601-01-01; Unix time from
    // 1970-01-01. 11644473600 seconds separate them.
    const EPOCH_DIFF_100NS: u64 = 116_444_736_000_000_000;
    let unix_100ns = ticks.saturating_sub(EPOCH_DIFF_100NS);
    let epoch_ms = (unix_100ns / 10_000) as i64;
    (format_rfc3339(epoch_ms), epoch_ms)
}

/// RFC 3339 in UTC, to milliseconds.
///
/// Hand-rolled rather than pulling in a date crate: this is the only date
/// formatting the recorder does, and a dependency for it would be the kind of
/// weight §26 asks the desktop side to avoid.
pub fn rfc3339(epoch_ms: i64) -> String {
    format_rfc3339(epoch_ms)
}

fn format_rfc3339(epoch_ms: i64) -> String {
    let secs = epoch_ms.div_euclid(1000);
    let ms = epoch_ms.rem_euclid(1000);
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (h, m, s) = (tod / 3600, (tod % 3600) / 60, tod % 60);
    let (y, mo, d) = civil_from_days(days);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}.{ms:03}Z")
}

/// Howard Hinnant's days-from-civil, inverted. Handles leap years and centuries
/// correctly, which is the entire reason to use a known algorithm rather than
/// improvise one.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Expected values cross-checked against .NET's `DateTimeOffset`, not
    /// derived by hand — the first draft of this test asserted a wrong value
    /// and failed a correct implementation.
    #[test]
    fn formats_known_instants() {
        // Unix epoch, and a leap day in a century year divisible by 400 —
        // the case a naive leap-year rule gets wrong.
        assert_eq!(format_rfc3339(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(format_rfc3339(951_782_400_000), "2000-02-29T00:00:00.000Z");
        assert_eq!(format_rfc3339(1_780_000_000_000), "2026-05-28T20:26:40.000Z");
    }

    /// Alignment across machines is the entire point, so a clock difference has
    /// to survive the round trip as an exact millisecond offset.
    #[test]
    fn preserves_offsets_between_povs() {
        let a = 1_780_000_000_000i64;
        let b = a + 4_512; // a teammate who started 4.512s later
        assert_eq!(format_rfc3339(a), "2026-05-28T20:26:40.000Z");
        assert_eq!(format_rfc3339(b), "2026-05-28T20:26:44.512Z");
    }

    #[test]
    fn keeps_milliseconds() {
        assert_eq!(format_rfc3339(1_234), "1970-01-01T00:00:01.234Z");
    }
}
