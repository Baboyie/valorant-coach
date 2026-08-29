//! The gallery: what recordings exist, and serving their bytes to the webview.
//!
//! Playback goes through a custom `media:` URI scheme rather than Tauri's
//! asset protocol. The asset protocol needs a filesystem scope declared up
//! front, and the output directory is user-configurable — a scope wide enough
//! to cover "wherever they pointed it" is a scope wide enough to read
//! anything. This handler serves exactly one thing: `.mp4` files inside the
//! configured output directory, checked per request.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde::Serialize;

pub const CLIPS_DIR: &str = "clips";
pub const RECORDINGS_DIR: &str = "recordings";

/* ------------------------------------------------------------- layout */

/// Move loose `clip-*` / `recording-*` files into their subfolders.
///
/// One-time, at startup — nothing is being written then, so no open file can
/// be moved out from under its writer. `rename` on the same volume is instant
/// and never copies. A name that already exists in the destination is left
/// where it is: losing a recording to a migration would be absurd, so the rule
/// is *never overwrite, never delete*.
pub fn migrate_layout(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for entry in rd.flatten() {
        let p = entry.path();
        if !p.is_file() {
            continue;
        }
        let Some(name) = p.file_name().and_then(|n| n.to_str()) else { continue };
        if !(name.ends_with(".mp4") || name.ends_with(".json")) {
            continue;
        }
        let sub = if name.starts_with("clip-") {
            CLIPS_DIR
        } else if name.starts_with("recording-") {
            RECORDINGS_DIR
        } else {
            continue;
        };
        let dest_dir = dir.join(sub);
        if std::fs::create_dir_all(&dest_dir).is_err() {
            continue;
        }
        let dest = dest_dir.join(name);
        if dest.exists() {
            continue;
        }
        let _ = std::fs::rename(&p, &dest);
    }
}

/* ------------------------------------------------------------- listing */

/// One row in the gallery. Everything optional comes from the sidecar, which
/// may be missing — an mp4 without one still lists, with what the filesystem
/// alone can say about it.
#[derive(Debug, Clone, Serialize)]
pub struct MediaItem {
    /// "clip" or "recording".
    pub kind: String,
    /// Absolute path — what play and reveal-in-Explorer use.
    pub path: String,
    pub name: String,
    pub started_epoch_ms: Option<i64>,
    pub duration_secs: Option<f64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<u32>,
    pub player: Option<String>,
    pub audio_tracks: Vec<String>,
    /// From the filesystem, not the sidecar: the file is the truth about its
    /// own size, and a sidecar written at finalise time can be stale.
    pub bytes: u64,
    /// Sort fallback for files with no sidecar.
    pub modified_epoch_ms: i64,
}

pub fn list(dir: &Path) -> Vec<MediaItem> {
    let mut out = Vec::new();
    // The two subfolders, plus the top level for anything migration could not
    // move — a listing that silently hid those would look like lost footage.
    for sub in [Some(CLIPS_DIR), Some(RECORDINGS_DIR), None] {
        let d = match sub {
            Some(s) => dir.join(s),
            None => dir.to_path_buf(),
        };
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            let is_mp4 = p
                .extension()
                .map(|x| x.eq_ignore_ascii_case("mp4"))
                .unwrap_or(false);
            if p.is_file() && is_mp4 {
                out.push(item_for(&p, sub));
            }
        }
    }
    // Newest first, by when the footage *started* — the sidecar's field, and
    // the whole reason it exists. Modified time only for sidecar-less strays.
    out.sort_by_key(|m| std::cmp::Reverse(m.started_epoch_ms.unwrap_or(m.modified_epoch_ms)));
    out
}

fn item_for(p: &Path, sub: Option<&str>) -> MediaItem {
    let name = p
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let md = std::fs::metadata(p).ok();
    let bytes = md.as_ref().map(|m| m.len()).unwrap_or(0);
    let modified_epoch_ms = md
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    // Parsed loosely rather than through VodMeta: a sidecar from an older or
    // newer build should degrade to missing fields, not to a missing row.
    let side: Option<serde_json::Value> = std::fs::read_to_string(p.with_extension("json"))
        .ok()
        .and_then(|s| serde_json::from_str(s.trim_start_matches('\u{feff}')).ok());
    let g = |k: &str| side.as_ref().and_then(|v| v.get(k).cloned());

    let kind = if name.starts_with("clip-") {
        "clip"
    } else if name.starts_with("recording-") {
        "recording"
    } else {
        match (g("kind").and_then(|v| v.as_str().map(str::to_owned)), sub) {
            (Some(k), _) if k == "clip" => "clip",
            (Some(_), _) => "recording",
            (None, Some(CLIPS_DIR)) => "clip",
            _ => "recording",
        }
    }
    .to_string();

    MediaItem {
        kind,
        path: p.to_string_lossy().into_owned(),
        name,
        started_epoch_ms: g("started_epoch_ms").and_then(|v| v.as_i64()),
        duration_secs: g("duration_secs").and_then(|v| v.as_f64()),
        width: g("width").and_then(|v| v.as_u64()).map(|v| v as u32),
        height: g("height").and_then(|v| v.as_u64()).map(|v| v as u32),
        fps: g("fps").and_then(|v| v.as_u64()).map(|v| v as u32),
        player: g("player")
            .and_then(|v| v.as_str().map(str::to_owned))
            .filter(|s| !s.is_empty()),
        audio_tracks: g("audio_tracks")
            .and_then(|v| {
                v.as_array().map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(str::to_owned))
                        .collect()
                })
            })
            .unwrap_or_default(),
        bytes,
        modified_epoch_ms,
    }
}

/* ------------------------------------------------------------- serving */

/// Cap per response. The webview re-requests as it plays, so a smaller cap
/// costs an extra round-trip while a big one holds a multi-GB recording's
/// worth of RAM; 8 MB keeps seeks instant either way.
const CHUNK: u64 = 8 * 1024 * 1024;

struct Slice {
    satisfiable: bool,
    start: u64,
    end: u64,
    total: u64,
    body: Vec<u8>,
}

/// Append one line to `%APPDATA%\DEBRIEF\media.log`.
///
/// The release build has no console, and a video element that fails to load
/// shows nothing but 0:00 — no status, no reason. This is the only way to
/// learn whether a request reached the handler at all, and what it decided.
/// Truncated when it passes 256 KB so a long session of seeking cannot grow
/// it without bound.
fn log_line(msg: &str) {
    let Some(base) = std::env::var_os("APPDATA") else { return };
    let path = PathBuf::from(base).join("DEBRIEF").join("media.log");
    if let Ok(md) = std::fs::metadata(&path) {
        if md.len() > 256 * 1024 {
            let _ = std::fs::remove_file(&path);
        }
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{} {msg}", crate::vod::now_utc().0);
    }
}

/// Serve one gallery video with HTTP range semantics.
///
/// Always 206, never a whole-file 200: a 200 body would mean materialising a
/// multi-gigabyte recording in memory, and Chromium's media stack is happy to
/// be handed less than it asked for as long as Content-Range tells the truth.
pub fn serve(
    root: Option<PathBuf>,
    request: tauri::http::Request<Vec<u8>>,
) -> tauri::http::Response<Vec<u8>> {
    let plain = |code: u16, msg: &str| {
        tauri::http::Response::builder()
            .status(code)
            .header("Content-Type", "text/plain")
            .body(msg.as_bytes().to_vec())
            .unwrap()
    };

    let range = request
        .headers()
        .get("range")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    log_line(&format!(
        "{} {} range={}",
        request.method(),
        request.uri(),
        range.as_deref().unwrap_or("-")
    ));

    let Some(root) = root else {
        log_line("  -> 500 no state");
        return plain(500, "recorder state is not ready");
    };
    let path = PathBuf::from(percent_decode(request.uri().path().trim_start_matches('/')));
    if !is_servable(&root, &path) {
        // Wrong extension or outside the output directory. The distinction is
        // deliberately not reported to the caller: this is the only line of
        // defence between the webview and the filesystem.
        log_line(&format!("  -> 403 not servable: {} (root {})", path.display(), root.display()));
        return plain(403, "not a gallery file");
    }

    // CORS headers on everything. A <video> without `crossorigin` makes a
    // no-cors request and should not need them, but http://media.localhost is
    // a different origin from the page, and WebView2 has been known to want
    // them anyway. They cost nothing.
    let cors = |b: tauri::http::response::Builder| {
        b.header("Access-Control-Allow-Origin", "*")
            .header("Access-Control-Allow-Headers", "Range")
            .header("Access-Control-Expose-Headers", "Content-Range, Content-Length, Accept-Ranges")
    };
    if request.method() == tauri::http::Method::OPTIONS {
        return cors(tauri::http::Response::builder().status(204)).body(Vec::new()).unwrap();
    }

    match read_slice(&path, range.as_deref()) {
        Ok(s) if !s.satisfiable => {
            log_line(&format!("  -> 416 total={}", s.total));
            cors(tauri::http::Response::builder().status(416))
                .header("Content-Range", format!("bytes */{}", s.total))
                .body(Vec::new())
                .unwrap()
        }
        Ok(s) => {
            log_line(&format!("  -> 206 {}-{}/{} ({} bytes)", s.start, s.end, s.total, s.body.len()));
            cors(tauri::http::Response::builder().status(206))
                .header("Content-Type", "video/mp4")
                .header("Accept-Ranges", "bytes")
                .header("Content-Range", format!("bytes {}-{}/{}", s.start, s.end, s.total))
                .header("Content-Length", s.body.len().to_string())
                .body(s.body)
                .unwrap()
        }
        Err(e) => {
            log_line(&format!("  -> 404 {e}"));
            plain(404, "not found")
        }
    }
}

/// The only files this scheme will touch: `.mp4`, inside the output directory.
/// Canonicalised on both sides so `..` and symlink tricks resolve before the
/// containment check, not after.
pub(crate) fn is_servable(root: &Path, candidate: &Path) -> bool {
    let mp4 = candidate
        .extension()
        .map(|e| e.eq_ignore_ascii_case("mp4"))
        .unwrap_or(false);
    if !mp4 {
        return false;
    }
    match (std::fs::canonicalize(root), std::fs::canonicalize(candidate)) {
        (Ok(r), Ok(c)) => c.starts_with(&r),
        _ => false,
    }
}

fn read_slice(path: &Path, range: Option<&str>) -> std::io::Result<Slice> {
    let mut f = std::fs::File::open(path)?;
    let total = f.metadata()?.len();
    if total == 0 {
        return Ok(Slice { satisfiable: false, start: 0, end: 0, total, body: Vec::new() });
    }
    let (start, want_end) = parse_range(range, total);
    if start >= total {
        return Ok(Slice { satisfiable: false, start: 0, end: 0, total, body: Vec::new() });
    }
    let end = want_end.min(total - 1).min(start + CHUNK - 1);
    let len = (end - start + 1) as usize;
    f.seek(SeekFrom::Start(start))?;
    let mut body = vec![0u8; len];
    f.read_exact(&mut body)?;
    Ok(Slice { satisfiable: true, start, end, total, body })
}

/// `bytes=a-b`, `bytes=a-`, `bytes=-n`. Anything unparseable is treated as
/// "from the start", which the chunk cap makes safe.
fn parse_range(header: Option<&str>, total: u64) -> (u64, u64) {
    let whole = (0, total - 1);
    let Some(h) = header else { return whole };
    let Some(spec) = h.trim().strip_prefix("bytes=") else { return whole };
    // Multipart ranges are legal HTTP and pointless for one video element;
    // serve the first and let the client re-ask.
    let spec = spec.split(',').next().unwrap_or("");
    let mut halves = spec.splitn(2, '-');
    let a = halves.next().unwrap_or("").trim();
    let b = halves.next().unwrap_or("").trim();
    if a.is_empty() {
        // Suffix form: the last n bytes — how Chromium probes for the moov
        // atom in files that keep it at the end.
        match b.parse::<u64>() {
            Ok(n) if n > 0 => (total.saturating_sub(n), total - 1),
            _ => whole,
        }
    } else {
        let start = a.parse::<u64>().unwrap_or(0);
        let end = if b.is_empty() { total - 1 } else { b.parse::<u64>().unwrap_or(total - 1) };
        (start, end)
    }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hex = |c: u8| match c {
                b'0'..=b'9' => Some(c - b'0'),
                b'a'..=b'f' => Some(c - b'a' + 10),
                b'A'..=b'F' => Some(c - b'A' + 10),
                _ => None,
            };
            if let (Some(hi), Some(lo)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push(hi * 16 + lo);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/* ------------------------------------------------------------- deleting */

/// Send a recording and its sidecar to the Recycle Bin.
///
/// **The Recycle Bin, not `remove_file`.** This is footage that cannot be
/// recovered by re-running anything — one mis-click on a row would otherwise
/// destroy a round nobody can play again. `FOF_ALLOWUNDO` makes the mistake
/// cost a trip to the bin instead of the clip.
///
/// The path is re-validated here rather than trusted from the caller: this is
/// reachable from the webview, and the check that a path is an `.mp4` inside
/// the output directory is the same one that guards serving.
pub fn delete_to_recycle_bin(root: &Path, video: &Path) -> Result<(), String> {
    use windows::core::PCWSTR;
    use windows::Win32::UI::Shell::{
        SHFileOperationW, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT,
        FO_DELETE, SHFILEOPSTRUCTW,
    };

    if !is_servable(root, video) {
        return Err("that file is not in the output folder".into());
    }

    // pFrom is a double-null-terminated list, so both entries go in one call
    // and the sidecar cannot be orphaned by a failure between two calls.
    let mut list: Vec<u16> = Vec::new();
    let mut wide = |p: &Path| {
        list.extend(p.as_os_str().encode_wide());
        list.push(0);
    };
    use std::os::windows::ffi::OsStrExt;
    wide(video);
    let sidecar = video.with_extension("json");
    if sidecar.exists() {
        wide(&sidecar);
    }
    list.push(0); // terminates the list

    let mut op = SHFILEOPSTRUCTW {
        wFunc: FO_DELETE as u32,
        pFrom: PCWSTR(list.as_ptr()),
        fFlags: (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_SILENT | FOF_NOERRORUI).0 as u16,
        ..Default::default()
    };
    let rc = unsafe { SHFileOperationW(&mut op) };
    if rc != 0 {
        return Err(format!("Windows refused to delete it (code {rc})"));
    }
    if op.fAnyOperationsAborted.as_bool() {
        return Err("the delete was cancelled".into());
    }
    Ok(())
}

/* --------------------------------------------------------------- tests */

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_parse_the_forms_browsers_send() {
        assert_eq!(parse_range(None, 100), (0, 99));
        assert_eq!(parse_range(Some("bytes=0-"), 100), (0, 99));
        assert_eq!(parse_range(Some("bytes=10-19"), 100), (10, 19));
        assert_eq!(parse_range(Some("bytes=-20"), 100), (80, 99));
        assert_eq!(parse_range(Some("bytes=5-,30-40"), 100), (5, 99));
        assert_eq!(parse_range(Some("garbage"), 100), (0, 99));
    }

    #[test]
    fn percent_decoding_survives_windows_paths_and_junk() {
        assert_eq!(percent_decode("C%3A%5CUsers%5Cx%5Cclip.mp4"), r"C:\Users\x\clip.mp4");
        assert_eq!(percent_decode("plain"), "plain");
        assert_eq!(percent_decode("bad%2"), "bad%2");
        assert_eq!(percent_decode("%zz"), "%zz");
    }

    #[test]
    fn migration_sorts_files_and_the_listing_finds_them() {
        let dir = std::env::temp_dir().join(format!("debrief-media-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        std::fs::write(dir.join("clip-20260101-000000.mp4"), b"v").unwrap();
        std::fs::write(
            dir.join("clip-20260101-000000.json"),
            br#"{"kind":"clip","duration_secs":30.5,"started_epoch_ms":5,"width":1600,"height":900,"fps":60,"player":"babu","audio_tracks":["desktop"]}"#,
        )
        .unwrap();
        std::fs::write(dir.join("recording-20260101-000001.mp4"), b"vv").unwrap();
        std::fs::write(dir.join("unrelated.txt"), b"x").unwrap();

        migrate_layout(&dir);

        assert!(dir.join(CLIPS_DIR).join("clip-20260101-000000.mp4").exists());
        assert!(dir.join(CLIPS_DIR).join("clip-20260101-000000.json").exists());
        assert!(dir.join(RECORDINGS_DIR).join("recording-20260101-000001.mp4").exists());
        assert!(dir.join("unrelated.txt").exists(), "migration must not touch other files");

        // Idempotent: nothing left to move, nothing breaks.
        migrate_layout(&dir);

        let items = list(&dir);
        assert_eq!(items.len(), 2);
        let clip = items.iter().find(|m| m.kind == "clip").unwrap();
        assert_eq!(clip.duration_secs, Some(30.5));
        assert_eq!(clip.player.as_deref(), Some("babu"));
        assert_eq!(clip.width, Some(1600));
        let rec = items.iter().find(|m| m.kind == "recording").unwrap();
        assert_eq!(rec.duration_secs, None, "no sidecar still lists");
        assert_eq!(rec.bytes, 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn slices_are_capped_and_bounded() {
        let dir = std::env::temp_dir().join(format!("debrief-slice-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("t.mp4");
        std::fs::write(&f, vec![7u8; 100]).unwrap();

        let s = read_slice(&f, Some("bytes=90-500")).unwrap();
        assert!(s.satisfiable);
        assert_eq!((s.start, s.end, s.total), (90, 99, 100));
        assert_eq!(s.body.len(), 10);

        let s = read_slice(&f, Some("bytes=100-")).unwrap();
        assert!(!s.satisfiable, "start past the end is 416, not a panic");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn serving_is_confined_to_mp4_inside_the_root() {
        let dir = std::env::temp_dir().join(format!("debrief-confine-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("clips")).unwrap();
        let inside = dir.join("clips").join("a.mp4");
        std::fs::write(&inside, b"x").unwrap();
        let outside = std::env::temp_dir().join(format!("debrief-outside-{}.mp4", std::process::id()));
        std::fs::write(&outside, b"x").unwrap();

        assert!(is_servable(&dir, &inside));
        assert!(!is_servable(&dir, &outside));
        assert!(!is_servable(&dir, &dir.join("clips").join("a.json")));
        // Traversal resolves before the containment check.
        assert!(!is_servable(&dir, &dir.join("clips").join("..").join("..").join("x.mp4")));

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&outside);
    }
}
