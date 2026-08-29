//! Post-finalise container fixes.
//!
//! Media Foundation's MP4 sink writes every track it is given as **enabled**,
//! with `alternate_group = 0`. For one video and one audio track that is
//! correct. For one video and *two* audio tracks it is a lie: the file says
//! "play both" while giving no hint that the two are alternatives, and no
//! player can play two audio tracks at once — so each one guesses differently.
//! Chromium takes the first and plays desktop; Windows' own players take the
//! other and play nothing but the microphone. The footage was always fine; the
//! container never said which track it meant.
//!
//! ISO/IEC 14496-12 has exactly the right fields for this. `alternate_group`
//! puts tracks in a mutually-exclusive set — a player picks one member and
//! offers the rest as switchable alternates — and the `tkhd` enabled flag says
//! which one it picks. So: every audio track joins one alternate group, and
//! only the first stays enabled.
//!
//! Done by patching the finished file rather than at write time because MF
//! exposes neither field. It is a handful of in-place byte writes with no size
//! change, so nothing downstream needs its offsets recomputed.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

/// `tkhd` flag bit 0: the track is played when the file is played.
const TRACK_ENABLED: u32 = 0x1;
/// Arbitrary but non-zero: zero means "not in any alternate group", which is
/// the state being corrected.
const AUDIO_ALTERNATE_GROUP: i16 = 1;

/// Where in the file one track's mutable `tkhd` fields live.
struct TrackFields {
    /// Absolute offset of the 3 flag bytes (the byte after `version`).
    flags_at: u64,
    flags: u32,
    /// Absolute offset of the 2-byte `alternate_group`.
    alt_group_at: u64,
    alt_group: i16,
    /// `soun`, `vide`, …
    handler: [u8; 4],
}

/// Declare a file's audio tracks alternatives of one another, leaving the
/// first enabled.
///
/// A no-op for a file with fewer than two audio tracks — there is nothing to
/// disambiguate — and for a file that is already correct, so it is safe to run
/// unconditionally after every save.
pub fn mark_audio_alternates(path: &Path) -> std::io::Result<bool> {
    let mut f = File::options().read(true).write(true).open(path)?;
    let len = f.metadata()?.len();

    let Some((moov_at, moov_len)) = find_top_level(&mut f, len, b"moov")? else {
        return Ok(false);
    };
    // The moov is the index, not the media: a couple of hundred KB for a
    // multi-gigabyte recording. Reading it whole is fine; reading the file
    // whole would not be.
    let mut moov = vec![0u8; moov_len as usize];
    f.seek(SeekFrom::Start(moov_at))?;
    f.read_exact(&mut moov)?;

    let tracks = parse_tracks(&moov, moov_at);
    let audio: Vec<&TrackFields> = tracks.iter().filter(|t| &t.handler == b"soun").collect();
    if audio.len() < 2 {
        return Ok(false);
    }

    let mut changed = false;
    for (i, t) in audio.iter().enumerate() {
        // First audio track plays by default; the rest are alternates a viewer
        // can switch to.
        let want_flags = if i == 0 {
            t.flags | TRACK_ENABLED
        } else {
            t.flags & !TRACK_ENABLED
        };
        if want_flags != t.flags {
            f.seek(SeekFrom::Start(t.flags_at))?;
            f.write_all(&[
                (want_flags >> 16) as u8,
                (want_flags >> 8) as u8,
                want_flags as u8,
            ])?;
            changed = true;
        }
        if t.alt_group != AUDIO_ALTERNATE_GROUP {
            f.seek(SeekFrom::Start(t.alt_group_at))?;
            f.write_all(&AUDIO_ALTERNATE_GROUP.to_be_bytes())?;
            changed = true;
        }
    }
    if changed {
        f.flush()?;
    }
    Ok(changed)
}

/// Walk the top-level boxes for one of them, without reading their contents.
fn find_top_level(f: &mut File, len: u64, want: &[u8; 4]) -> std::io::Result<Option<(u64, u64)>> {
    let mut at = 0u64;
    while at + 8 <= len {
        f.seek(SeekFrom::Start(at))?;
        let mut hdr = [0u8; 8];
        f.read_exact(&mut hdr)?;
        let mut size = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) as u64;
        let kind = &hdr[4..8];
        let mut header_len = 8u64;
        if size == 1 {
            let mut ext = [0u8; 8];
            f.read_exact(&mut ext)?;
            size = u64::from_be_bytes(ext);
            header_len = 16;
        } else if size == 0 {
            // "To the end of the file" — legal, and the last box either way.
            size = len - at;
        }
        if size < header_len || at + size > len {
            return Ok(None); // truncated or not an MP4; leave it alone
        }
        if kind == want {
            return Ok(Some((at + header_len, size - header_len)));
        }
        at += size;
    }
    Ok(None)
}

/// Read every `trak`'s patchable fields out of an in-memory `moov`.
///
/// `base` is the moov contents' absolute file offset, so the offsets returned
/// can be written to directly.
fn parse_tracks(moov: &[u8], base: u64) -> Vec<TrackFields> {
    let mut out = Vec::new();
    for trak in children(moov, 0, moov.len()) {
        if &trak.kind != b"trak" {
            continue;
        }
        let kids = children(moov, trak.body, trak.end);
        let Some(tkhd) = kids.iter().find(|c| &c.kind == b"tkhd") else { continue };
        let Some(mdia) = kids.iter().find(|c| &c.kind == b"mdia") else { continue };
        let hdlr = children(moov, mdia.body, mdia.end)
            .into_iter()
            .find(|c| &c.kind == b"hdlr");
        let Some(hdlr) = hdlr else { continue };
        // hdlr: version+flags(4), pre_defined(4), handler_type(4)
        if hdlr.body + 12 > moov.len() {
            continue;
        }
        let mut handler = [0u8; 4];
        handler.copy_from_slice(&moov[hdlr.body + 8..hdlr.body + 12]);

        let p = tkhd.body;
        if p + 4 > moov.len() {
            continue;
        }
        let version = moov[p];
        let flags = ((moov[p + 1] as u32) << 16) | ((moov[p + 2] as u32) << 8) | moov[p + 3] as u32;

        // Past creation_time, modification_time, track_ID, reserved, duration.
        let mut q = p + 4;
        q += if version == 1 { 8 + 8 } else { 4 + 4 };
        q += 4 + 4; // track_ID, reserved
        q += if version == 1 { 8 } else { 4 }; // duration
        q += 8; // reserved[2]
        q += 2; // layer
        if q + 2 > moov.len() {
            continue;
        }
        let alt_group = i16::from_be_bytes([moov[q], moov[q + 1]]);

        out.push(TrackFields {
            flags_at: base + p as u64 + 1,
            flags,
            alt_group_at: base + q as u64,
            alt_group,
            handler,
        });
    }
    out
}

/// Make every declared duration agree with the samples actually present.
///
/// Media Foundation's sink writer can declare an `mdhd` duration well past the
/// track's own `stts` sum — observed on passthrough-trimmed audio, where a
/// 10.2s track declared 13.1s. The sample tables were correct; only the
/// declarations lied, and players believe declarations, so an 11s clip showed
/// a 13s timeline with a frozen tail. This rewrites each track's `mdhd` and
/// `tkhd` duration from its `stts` sum and `mvhd` from the longest track —
/// in-place byte writes, no size change, safe to run on any finished file.
pub fn normalize_durations(path: &Path) -> std::io::Result<bool> {
    let mut f = File::options().read(true).write(true).open(path)?;
    let len = f.metadata()?.len();
    let Some((moov_at, moov_len)) = find_top_level(&mut f, len, b"moov")? else {
        return Ok(false);
    };
    let mut moov = vec![0u8; moov_len as usize];
    f.seek(SeekFrom::Start(moov_at))?;
    f.read_exact(&mut moov)?;

    // Movie header: the timescale track durations are declared against, and
    // the whole-movie duration to be recomputed.
    let Some(mvhd) = children(&moov, 0, moov.len())
        .into_iter()
        .find(|c| &c.kind == b"mvhd")
    else {
        return Ok(false);
    };
    if mvhd.body + 32 > moov.len() {
        return Ok(false);
    }
    let mvhd_v1 = moov[mvhd.body] == 1;
    let (movie_ts, mvhd_dur_at) = if mvhd_v1 {
        (be32(&moov, mvhd.body + 20), mvhd.body + 24)
    } else {
        (be32(&moov, mvhd.body + 12), mvhd.body + 16)
    };
    if movie_ts == 0 {
        return Ok(false);
    }

    let mut changed = false;
    let mut longest_movie_ticks: u64 = 0;

    for trak in children(&moov, 0, moov.len()) {
        if &trak.kind != b"trak" {
            continue;
        }
        let kids = children(&moov, trak.body, trak.end);
        let Some(tkhd) = kids.iter().find(|c| &c.kind == b"tkhd") else { continue };
        let Some(mdia) = kids.iter().find(|c| &c.kind == b"mdia") else { continue };
        let mk = children(&moov, mdia.body, mdia.end);
        let Some(mdhd) = mk.iter().find(|c| &c.kind == b"mdhd") else { continue };
        let Some(minf) = mk.iter().find(|c| &c.kind == b"minf") else { continue };
        let Some(stts) = children(&moov, minf.body, minf.end)
            .into_iter()
            .find(|c| &c.kind == b"stbl")
            .and_then(|stbl| {
                children(&moov, stbl.body, stbl.end)
                    .into_iter()
                    .find(|c| &c.kind == b"stts")
            })
        else {
            continue;
        };
        if mdhd.body + 32 > moov.len() || stts.body + 8 > moov.len() {
            continue;
        }

        // The truth: the sum of the sample deltas.
        let n = be32(&moov, stts.body + 4) as usize;
        let mut media_ticks: u64 = 0;
        for i in 0..n {
            let at = stts.body + 8 + i * 8;
            if at + 8 > stts.end {
                break;
            }
            media_ticks += be32(&moov, at) as u64 * be32(&moov, at + 4) as u64;
        }
        if media_ticks == 0 {
            continue;
        }

        let mdhd_v1 = moov[mdhd.body] == 1;
        let (media_ts, mdhd_dur_at) = if mdhd_v1 {
            (be32(&moov, mdhd.body + 20), mdhd.body + 24)
        } else {
            (be32(&moov, mdhd.body + 12), mdhd.body + 16)
        };
        if media_ts == 0 {
            continue;
        }
        let movie_ticks =
            (media_ticks as u128 * movie_ts as u128 / media_ts as u128) as u64;
        longest_movie_ticks = longest_movie_ticks.max(movie_ticks);

        if declared(&moov, mdhd_dur_at, mdhd_v1) != media_ticks {
            write_duration(&mut f, moov_at + mdhd_dur_at as u64, media_ticks, mdhd_v1)?;
            changed = true;
        }

        // tkhd's duration is in *movie* timescale, and its version is its own.
        let tkhd_v1 = moov[tkhd.body] == 1;
        let tkhd_dur_at = tkhd.body + if tkhd_v1 { 4 + 8 + 8 + 4 + 4 } else { 4 + 4 + 4 + 4 + 4 };
        if tkhd_dur_at + 8 > moov.len() {
            continue;
        }
        if declared(&moov, tkhd_dur_at, tkhd_v1) != movie_ticks {
            write_duration(&mut f, moov_at + tkhd_dur_at as u64, movie_ticks, tkhd_v1)?;
            changed = true;
        }
    }

    if longest_movie_ticks > 0 && declared(&moov, mvhd_dur_at, mvhd_v1) != longest_movie_ticks {
        write_duration(&mut f, moov_at + mvhd_dur_at as u64, longest_movie_ticks, mvhd_v1)?;
        changed = true;
    }
    if changed {
        f.flush()?;
    }
    Ok(changed)
}

fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn declared(b: &[u8], at: usize, v1: bool) -> u64 {
    if v1 {
        let mut x = [0u8; 8];
        x.copy_from_slice(&b[at..at + 8]);
        u64::from_be_bytes(x)
    } else {
        be32(b, at) as u64
    }
}

fn write_duration(f: &mut File, at: u64, value: u64, v1: bool) -> std::io::Result<()> {
    f.seek(SeekFrom::Start(at))?;
    if v1 {
        f.write_all(&value.to_be_bytes())
    } else {
        f.write_all(&(value.min(u32::MAX as u64) as u32).to_be_bytes())
    }
}

struct Child {
    kind: [u8; 4],
    /// Index of the box's contents within the buffer.
    body: usize,
    /// One past the box's last byte.
    end: usize,
}

fn children(buf: &[u8], from: usize, to: usize) -> Vec<Child> {
    let mut out = Vec::new();
    let mut at = from;
    while at + 8 <= to {
        let size = u32::from_be_bytes([buf[at], buf[at + 1], buf[at + 2], buf[at + 3]]) as usize;
        let mut kind = [0u8; 4];
        kind.copy_from_slice(&buf[at + 4..at + 8]);
        let (size, header) = if size == 1 {
            if at + 16 > to {
                break;
            }
            let mut ext = [0u8; 8];
            ext.copy_from_slice(&buf[at + 8..at + 16]);
            (u64::from_be_bytes(ext) as usize, 16)
        } else if size == 0 {
            (to - at, 8)
        } else {
            (size, 8)
        };
        if size < header || at + size > to {
            break;
        }
        out.push(Child { kind, body: at + header, end: at + size });
        at += size;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    /// Build a minimal but structurally real MP4: `ftyp`, then a `moov`
    /// holding one video and `audio_tracks` audio traks, each with a `tkhd`
    /// and an `mdia`/`hdlr`. Enough for the parser to be exercised against the
    /// same box nesting a real file has.
    fn synth(audio_tracks: usize, version: u8) -> Vec<u8> {
        fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
            let mut v = ((body.len() + 8) as u32).to_be_bytes().to_vec();
            v.extend_from_slice(kind);
            v.extend_from_slice(body);
            v
        }
        fn tkhd(version: u8, alt_group: i16, flags: u32) -> Vec<u8> {
            let mut b = vec![version, (flags >> 16) as u8, (flags >> 8) as u8, flags as u8];
            if version == 1 {
                b.extend_from_slice(&[0u8; 8]); // creation
                b.extend_from_slice(&[0u8; 8]); // modification
                b.extend_from_slice(&1u32.to_be_bytes()); // track_ID
                b.extend_from_slice(&[0u8; 4]); // reserved
                b.extend_from_slice(&[0u8; 8]); // duration
            } else {
                b.extend_from_slice(&[0u8; 4]);
                b.extend_from_slice(&[0u8; 4]);
                b.extend_from_slice(&1u32.to_be_bytes());
                b.extend_from_slice(&[0u8; 4]);
                b.extend_from_slice(&[0u8; 4]);
            }
            b.extend_from_slice(&[0u8; 8]); // reserved[2]
            b.extend_from_slice(&0i16.to_be_bytes()); // layer
            b.extend_from_slice(&alt_group.to_be_bytes());
            b.extend_from_slice(&0x0100u16.to_be_bytes()); // volume
            b.extend_from_slice(&[0u8; 2]); // reserved
            b.extend_from_slice(&[0u8; 36]); // matrix
            b.extend_from_slice(&[0u8; 8]); // width, height
            boxed(b"tkhd", &b)
        }
        fn mdia(handler: &[u8; 4]) -> Vec<u8> {
            let mut h = vec![0u8; 8]; // version+flags, pre_defined
            h.extend_from_slice(handler);
            h.extend_from_slice(&[0u8; 12]); // reserved
            h.push(0); // name
            boxed(b"mdia", &boxed(b"hdlr", &h))
        }
        fn trak(version: u8, handler: &[u8; 4]) -> Vec<u8> {
            let mut body = tkhd(version, 0, 0x3);
            body.extend_from_slice(&mdia(handler));
            boxed(b"trak", &body)
        }

        let mut moov_body = trak(version, b"vide");
        for _ in 0..audio_tracks {
            moov_body.extend_from_slice(&trak(version, b"soun"));
        }
        let mut out = boxed(b"ftyp", b"isom\0\0\0\0isom");
        out.extend_from_slice(&boxed(b"moov", &moov_body));
        out
    }

    fn write_temp(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("debrief-mp4-{}-{name}.mp4", std::process::id()));
        let mut f = File::create(&p).unwrap();
        f.write_all(bytes).unwrap();
        p
    }

    fn read_tracks(path: &Path) -> Vec<(String, u32, i16)> {
        let mut f = File::options().read(true).open(path).unwrap();
        let len = f.metadata().unwrap().len();
        let (at, l) = find_top_level(&mut f, len, b"moov").unwrap().unwrap();
        let mut moov = vec![0u8; l as usize];
        f.seek(SeekFrom::Start(at)).unwrap();
        f.read_exact(&mut moov).unwrap();
        parse_tracks(&moov, at)
            .into_iter()
            .map(|t| (String::from_utf8_lossy(&t.handler).into_owned(), t.flags, t.alt_group))
            .collect()
    }

    #[test]
    fn two_audio_tracks_become_alternates_with_only_the_first_enabled() {
        let p = write_temp("two", &synth(2, 0));
        assert!(mark_audio_alternates(&p).unwrap(), "should have changed something");

        let t = read_tracks(&p);
        assert_eq!(t.len(), 3);
        assert_eq!(t[0].0, "vide");
        // Video is untouched: still enabled, still in no alternate group.
        assert_eq!(t[0].1 & TRACK_ENABLED, TRACK_ENABLED);
        assert_eq!(t[0].2, 0);

        assert_eq!(t[1].1 & TRACK_ENABLED, TRACK_ENABLED, "desktop should still play");
        assert_eq!(t[2].1 & TRACK_ENABLED, 0, "mic should be an alternate, not a second default");
        assert_eq!(t[1].2, AUDIO_ALTERNATE_GROUP);
        assert_eq!(t[2].2, AUDIO_ALTERNATE_GROUP);
        // The other flag bits survive.
        assert_eq!(t[1].1 & 0x2, 0x2);
        assert_eq!(t[2].1 & 0x2, 0x2);

        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn a_single_audio_track_is_left_alone() {
        let p = write_temp("one", &synth(1, 0));
        assert!(!mark_audio_alternates(&p).unwrap(), "nothing to disambiguate");
        let t = read_tracks(&p);
        assert_eq!(t[1].1 & TRACK_ENABLED, TRACK_ENABLED);
        assert_eq!(t[1].2, 0, "one track is not an alternative to anything");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn running_it_twice_changes_nothing_the_second_time() {
        let p = write_temp("idem", &synth(2, 0));
        assert!(mark_audio_alternates(&p).unwrap());
        let after_first = std::fs::read(&p).unwrap();
        assert!(!mark_audio_alternates(&p).unwrap(), "already correct");
        assert_eq!(std::fs::read(&p).unwrap(), after_first, "bytes must be identical");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn version_1_track_headers_are_parsed_at_the_right_offsets() {
        // A v1 tkhd has 64-bit times and duration; getting the skip wrong
        // writes alternate_group into the middle of the matrix.
        let p = write_temp("v1", &synth(2, 1));
        assert!(mark_audio_alternates(&p).unwrap());
        let t = read_tracks(&p);
        assert_eq!(t[1].2, AUDIO_ALTERNATE_GROUP);
        assert_eq!(t[2].2, AUDIO_ALTERNATE_GROUP);
        assert_eq!(t[2].1 & TRACK_ENABLED, 0);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn the_file_keeps_its_exact_size() {
        let src = synth(2, 0);
        let p = write_temp("size", &src);
        mark_audio_alternates(&p).unwrap();
        assert_eq!(std::fs::metadata(&p).unwrap().len(), src.len() as u64);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn a_file_with_no_moov_is_refused_quietly() {
        let p = write_temp("nomoov", b"\0\0\0\x10ftypisom\0\0\0\0isom");
        assert!(!mark_audio_alternates(&p).unwrap());
        let _ = std::fs::remove_file(&p);
    }

    /// A movie (mvhd timescale 1000) whose single audio trak carries real
    /// timing — an mdhd with a timescale and an stts whose sum is the truth —
    /// plus whatever declared durations the caller wants to lie with.
    fn synth_timed(
        media_ts: u32,
        declared_media: u32,
        declared_movie: u32,
        stts_entries: &[(u32, u32)],
    ) -> Vec<u8> {
        fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
            let mut v = ((body.len() + 8) as u32).to_be_bytes().to_vec();
            v.extend_from_slice(kind);
            v.extend_from_slice(body);
            v
        }

        let mut tk = vec![0u8, 0, 0, 3]; // v0, enabled+in-movie
        tk.extend_from_slice(&[0u8; 8]); // creation, modification
        tk.extend_from_slice(&1u32.to_be_bytes()); // track_ID
        tk.extend_from_slice(&[0u8; 4]); // reserved
        tk.extend_from_slice(&declared_movie.to_be_bytes()); // duration
        tk.extend_from_slice(&[0u8; 8]); // reserved[2]
        tk.extend_from_slice(&[0u8; 4]); // layer, alternate_group
        tk.extend_from_slice(&0x0100u16.to_be_bytes()); // volume
        tk.extend_from_slice(&[0u8; 2]);
        tk.extend_from_slice(&[0u8; 36]); // matrix
        tk.extend_from_slice(&[0u8; 8]); // width, height
        let tkhd = boxed(b"tkhd", &tk);

        let mut mh = vec![0u8; 4]; // v0
        mh.extend_from_slice(&[0u8; 8]); // creation, modification
        mh.extend_from_slice(&media_ts.to_be_bytes());
        mh.extend_from_slice(&declared_media.to_be_bytes());
        mh.extend_from_slice(&[0u8; 4]); // language, quality
        let mdhd = boxed(b"mdhd", &mh);

        let mut h = vec![0u8; 8];
        h.extend_from_slice(b"soun");
        h.extend_from_slice(&[0u8; 12]);
        h.push(0);
        let hdlr = boxed(b"hdlr", &h);

        let mut st = vec![0u8; 4];
        st.extend_from_slice(&(stts_entries.len() as u32).to_be_bytes());
        for (count, delta) in stts_entries {
            st.extend_from_slice(&count.to_be_bytes());
            st.extend_from_slice(&delta.to_be_bytes());
        }
        let minf = boxed(b"minf", &boxed(b"stbl", &boxed(b"stts", &st)));

        let mut mdia_body = mdhd;
        mdia_body.extend_from_slice(&hdlr);
        mdia_body.extend_from_slice(&minf);
        let mut trak_body = tkhd;
        trak_body.extend_from_slice(&boxed(b"mdia", &mdia_body));
        let trak = boxed(b"trak", &trak_body);

        let mut mv = vec![0u8; 4]; // v0
        mv.extend_from_slice(&[0u8; 8]); // creation, modification
        mv.extend_from_slice(&1000u32.to_be_bytes()); // movie timescale
        mv.extend_from_slice(&declared_movie.to_be_bytes());
        mv.extend_from_slice(&[0u8; 80]); // rate .. next_track_ID
        let mut moov_body = boxed(b"mvhd", &mv);
        moov_body.extend_from_slice(&trak);

        let mut out = boxed(b"ftyp", b"isom\0\0\0\0isom");
        out.extend_from_slice(&boxed(b"moov", &moov_body));
        out
    }

    #[test]
    fn overdeclared_durations_are_corrected_from_stts() {
        // 480 packets of 1024 ticks at 48 kHz: truly 10.24s of samples, but
        // declared as 13.1s in track and movie — the sink writer's exact lie.
        let p = write_temp(
            "overdecl",
            &synth_timed(48_000, 628_800, 13_100, &[(480, 1024)]),
        );
        assert!(normalize_durations(&p).unwrap(), "should correct something");

        let b = std::fs::read(&p).unwrap();
        let body_of = |kind: &[u8; 4]| {
            b.windows(4).position(|w| w == kind).unwrap() + 4
        };
        let mdhd = body_of(b"mdhd");
        assert_eq!(be32(&b, mdhd + 16), 480 * 1024, "mdhd duration");
        let movie_true = 480u32 * 1024 * 1000 / 48_000;
        let tkhd = body_of(b"tkhd");
        assert_eq!(be32(&b, tkhd + 20), movie_true, "tkhd duration");
        let mvhd = body_of(b"mvhd");
        assert_eq!(be32(&b, mvhd + 16), movie_true, "mvhd duration");

        assert!(!normalize_durations(&p).unwrap(), "second pass changes nothing");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn correct_durations_are_left_untouched() {
        let media: u32 = 100 * 512;
        let movie = (media as u64 * 1000 / 48_000) as u32;
        let p = write_temp(
            "gooddecl",
            &synth_timed(48_000, media, movie, &[(100, 512)]),
        );
        assert!(!normalize_durations(&p).unwrap());
        let _ = std::fs::remove_file(&p);
    }
}
