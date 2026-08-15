//! In-memory replay buffer: the last N seconds of *encoded* video, ready to be
//! muxed into a file the moment someone asks.
//!
//! The ring holds compressed H.264 samples, never textures. The arithmetic is
//! not close: ten seconds of 1080p60 BGRA is roughly 5 GB, on a card with 6 GB
//! total and a game to run (ADR §6's budget note). The same ten seconds encoded
//! at ~12 Mbps is ~15 MB of system RAM. Buffering raw and encoding on demand
//! was never an option; the encoder runs continuously and this ring stores its
//! output.
//!
//! Eviction recycles: an evicted frame's buffer goes into a pool and is handed
//! to a new frame, so once the ring has filled, steady state allocates nothing
//! (§19's rule applied to this stage — encoded frame sizes cluster around
//! bitrate/fps, so recycled capacities fit). The counters make this checkable
//! rather than assumed: `allocs` must stop growing after warmup.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Instant;

use windows::core::Result;
use windows::Win32::Media::MediaFoundation::*;

use crate::encoder::EncoderConfig;

/// How many recycled buffers to keep around. More than the ring will ever
/// evict per push (one or two), and small enough to never matter.
const POOL_MAX: usize = 64;

struct EncodedFrame {
    bytes: Vec<u8>,
    ts_100ns: i64,
    duration_100ns: i64,
    keyframe: bool,
}

struct Inner {
    frames: VecDeque<EncodedFrame>,
    pool: Vec<Vec<u8>>,
    bytes: usize,
    last_ts: i64,
    evicted: u64,
    allocs: u64,
    reuses: u64,
    /// Samples whose timestamp did not advance. This is the B-frame canary:
    /// encoders emit B-frames in decode order with reordered presentation
    /// times, which this ring stores faithfully and the muxer then garbles.
    /// We configure the encoder for zero B-frames (ADR §2); this counter is
    /// the check that the configuration actually took.
    non_monotonic: u64,
}

struct EncodedAudio {
    bytes: Vec<u8>,
    ts_100ns: i64,
    duration_100ns: i64,
}

struct AudioInner {
    packets: VecDeque<EncodedAudio>,
    pool: Vec<Vec<u8>>,
    bytes: usize,
}

pub struct ReplayRing {
    inner: Mutex<Inner>,
    /// Separate lock from video on purpose: the two grabber callbacks run on
    /// different Media Foundation worker threads, and sharing one lock would
    /// make the audio thread wait behind a video push for no reason.
    audio: Mutex<AudioInner>,
    window_100ns: i64,
    /// Retained beyond the window so that a keyframe exists at or before the
    /// window start — a clip must begin on an IDR or its first GOP is garbage.
    /// Two GOPs of margin.
    margin_100ns: i64,
    /// Hard safety cap. The window bounds memory in time; this bounds it in
    /// bytes if the bitrate estimate is ever badly wrong.
    byte_cap: usize,
}

pub struct RingReport {
    pub frames: usize,
    pub keyframes: usize,
    pub bytes: usize,
    pub span_secs: f64,
    pub mean_kf_interval_secs: f64,
    pub evicted: u64,
    pub allocs: u64,
    pub reuses: u64,
    pub non_monotonic: u64,
}

pub struct SaveReport {
    pub frames: usize,
    pub audio_packets: usize,
    pub bytes: usize,
    pub span_secs: f64,
    pub elapsed_ms: f64,
}

impl ReplayRing {
    /// `gop_secs` is the encoder's keyframe interval, in seconds. The ring
    /// retains two of them beyond the requested window so a keyframe always
    /// exists at or before the window start — so a shorter GOP directly buys
    /// both tighter clips and a smaller ring.
    pub fn new(window_secs: u64, gop_secs: f64, byte_cap: usize) -> ReplayRing {
        ReplayRing {
            inner: Mutex::new(Inner {
                frames: VecDeque::new(),
                pool: Vec::new(),
                bytes: 0,
                last_ts: i64::MIN,
                evicted: 0,
                allocs: 0,
                reuses: 0,
                non_monotonic: 0,
            }),
            audio: Mutex::new(AudioInner {
                packets: VecDeque::new(),
                pool: Vec::new(),
                bytes: 0,
            }),
            window_100ns: (window_secs as i64) * 10_000_000,
            margin_100ns: ((gop_secs * 2.0 * 10_000_000.0) as i64).max(10_000_000),
            byte_cap,
        }
    }

    /// Accept one encoded sample. Called from the sample grabber's thread —
    /// downstream of the encoder, never on the capture callback, so a short
    /// lock and a memcpy here cost a little encoder latency and nothing else.
    pub fn push(&self, data: &[u8], ts_100ns: i64, duration_100ns: i64) {
        let keyframe = contains_idr(data);
        let mut inner = self.inner.lock().unwrap();

        if !inner.frames.is_empty() && ts_100ns <= inner.last_ts {
            inner.non_monotonic += 1;
        }
        inner.last_ts = ts_100ns;

        let mut buf = match inner.pool.pop() {
            Some(v) => {
                inner.reuses += 1;
                v
            }
            None => {
                inner.allocs += 1;
                Vec::with_capacity(data.len())
            }
        };
        buf.clear();
        buf.extend_from_slice(data);
        inner.bytes += buf.len();
        inner.frames.push_back(EncodedFrame {
            bytes: buf,
            ts_100ns,
            duration_100ns,
            keyframe,
        });

        // Evict by age, with the byte cap as a backstop. Never evict down to
        // nothing: the newest frame always stays.
        loop {
            let evict = match inner.frames.front() {
                Some(front) if inner.frames.len() > 1 => {
                    ts_100ns - front.ts_100ns > self.window_100ns + self.margin_100ns
                        || inner.bytes > self.byte_cap
                }
                _ => false,
            };
            if !evict {
                break;
            }
            let f = inner.frames.pop_front().unwrap();
            inner.bytes -= f.bytes.len();
            inner.evicted += 1;
            if inner.pool.len() < POOL_MAX {
                inner.pool.push(f.bytes);
            }
        }
    }

    /// Accept one encoded audio packet, from the audio grabber's thread.
    ///
    /// Evicted by the same age window as video, so the two rings stay aligned
    /// and a clip never finds itself with video but no sound for its opening
    /// seconds.
    pub fn push_audio(&self, data: &[u8], ts_100ns: i64, duration_100ns: i64) {
        let mut inner = self.audio.lock().unwrap();
        let mut buf = inner.pool.pop().unwrap_or_default();
        buf.clear();
        buf.extend_from_slice(data);
        inner.bytes += buf.len();
        inner.packets.push_back(EncodedAudio {
            bytes: buf,
            ts_100ns,
            duration_100ns,
        });

        let horizon = self.window_100ns + self.margin_100ns;
        loop {
            let evict = match inner.packets.front() {
                Some(f) if inner.packets.len() > 1 => ts_100ns - f.ts_100ns > horizon,
                _ => false,
            };
            if !evict {
                break;
            }
            let p = inner.packets.pop_front().unwrap();
            inner.bytes -= p.bytes.len();
            if inner.pool.len() < POOL_MAX {
                inner.pool.push(p.bytes);
            }
        }
    }

    pub fn audio_report(&self) -> (usize, usize, f64) {
        let inner = self.audio.lock().unwrap();
        let span = match (inner.packets.front(), inner.packets.back()) {
            (Some(a), Some(b)) => (b.ts_100ns - a.ts_100ns) as f64 / 1e7,
            _ => 0.0,
        };
        (inner.packets.len(), inner.bytes, span)
    }

    pub fn report(&self) -> RingReport {
        let inner = self.inner.lock().unwrap();
        let keyframes: Vec<i64> = inner
            .frames
            .iter()
            .filter(|f| f.keyframe)
            .map(|f| f.ts_100ns)
            .collect();
        let span_secs = match (inner.frames.front(), inner.frames.back()) {
            (Some(a), Some(b)) => (b.ts_100ns - a.ts_100ns) as f64 / 1e7,
            _ => 0.0,
        };
        let mean_kf = if keyframes.len() >= 2 {
            (keyframes[keyframes.len() - 1] - keyframes[0]) as f64
                / 1e7
                / (keyframes.len() - 1) as f64
        } else {
            0.0
        };
        RingReport {
            frames: inner.frames.len(),
            keyframes: keyframes.len(),
            bytes: inner.bytes,
            span_secs,
            mean_kf_interval_secs: mean_kf,
            evicted: inner.evicted,
            allocs: inner.allocs,
            reuses: inner.reuses,
            non_monotonic: inner.non_monotonic,
        }
    }

    /// Mux the buffered window into an MP4. This is the "hotkey" path, so it is
    /// timed and reported: the product promise is that the last N seconds are
    /// already encoded and saving them is only container work, no encoder work.
    pub fn save_mp4(
        &self,
        path: &str,
        cfg: &EncoderConfig,
        audio_type: Option<&IMFMediaType>,
    ) -> Result<SaveReport> {
        let t0 = Instant::now();

        // Snapshot under the lock, mux outside it. Muxing takes tens of
        // milliseconds of Media Foundation work, and a grabber thread blocked
        // that long backs up the sink writer and eventually the capture ring —
        // §10 says drop-never-stall, and stalling here would push the stall
        // upstream. Copying ~20 MB under the lock is single-digit
        // milliseconds. The copy allocates, which is fine off steady state:
        // a save is a user action, not a per-frame event.
        let snapshot: Vec<(Vec<u8>, i64, i64, bool)> = {
            let inner = self.inner.lock().unwrap();
            let newest = match inner.frames.back() {
                Some(f) => f.ts_100ns,
                None => {
                    return Err(windows::core::Error::new(
                        windows::Win32::Foundation::E_FAIL,
                        "replay ring is empty — nothing has been recorded yet",
                    ))
                }
            };
            // The clip must start on a keyframe: the latest one at or before
            // the window start, or failing that the earliest one buffered.
            let cutoff = newest - self.window_100ns;
            let mut start = None;
            for (i, f) in inner.frames.iter().enumerate() {
                if f.keyframe && f.ts_100ns <= cutoff {
                    start = Some(i);
                }
            }
            let start = start
                .or_else(|| inner.frames.iter().position(|f| f.keyframe))
                .ok_or_else(|| {
                    windows::core::Error::new(
                        windows::Win32::Foundation::E_FAIL,
                        "no keyframe in the replay ring — cannot start a clip",
                    )
                })?;
            inner
                .frames
                .iter()
                .skip(start)
                .map(|f| (f.bytes.clone(), f.ts_100ns, f.duration_100ns, f.keyframe))
                .collect()
        };

        // Passthrough mux: one H.264 type used as both the stream's output and
        // input, so the sink writer inserts no transform and no re-encode
        // happens. The type is deliberately *minimal* — a muxer needs a stream
        // description, not encoder instructions, and encoder-side attributes
        // (GOP spacing, profile) on a passthrough type are exactly the kind of
        // mismatch that makes Media Foundation reject it as inconsistent.
        //
        // The MP4 sink needs SPS/PPS to write the avcC box; the first frame is
        // an IDR and NVENC emits parameter sets in-band ahead of each IDR, so
        // they are lifted from the bitstream and attached explicitly rather
        // than hoping the sink finds them.
        let mux_type = unsafe {
            let t: IMFMediaType = MFCreateMediaType()?;
            t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            t.SetUINT32(&MF_MT_AVG_BITRATE, cfg.bitrate)?;
            t.SetUINT64(
                &MF_MT_FRAME_SIZE,
                ((cfg.width as u64) << 32) | cfg.height as u64,
            )?;
            t.SetUINT64(&MF_MT_FRAME_RATE, ((cfg.fps.max(1) as u64) << 32) | 1)?;
            t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)?;
            let header = sequence_header(&snapshot[0].0);
            if !header.is_empty() {
                t.SetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &header)?;
            }
            t
        };

        let step = |what: &'static str| {
            move |e: windows::core::Error| {
                windows::core::Error::new(e.code(), format!("save_mp4/{what}: {}", e.message()))
            }
        };
        // Throttling off. With it on, the sink writer paces one stream against
        // the others and will block a caller that gets ahead — which a mux of
        // already-encoded samples always does, since there is nothing to wait
        // for. Leaving it on deadlocked the save outright.
        let mux_attrs = unsafe {
            let mut a: Option<IMFAttributes> = None;
            MFCreateAttributes(&mut a, 1)?;
            let a = a.expect("MFCreateAttributes returned nothing");
            a.SetUINT32(&MF_SINK_WRITER_DISABLE_THROTTLING, 1)?;
            a
        };
        let writer = unsafe {
            MFCreateSinkWriterFromURL(&windows::core::HSTRING::from(path), None, &mux_attrs)
                .map_err(step("create writer"))?
        };
        let stream = unsafe { writer.AddStream(&mux_type).map_err(step("add stream"))? };
        unsafe {
            writer
                .SetInputMediaType(stream, &mux_type, None)
                .map_err(step("set input type"))?
        };

        // Audio, trimmed to the video start.
        //
        // The clip's start is dictated by video — it must begin on a keyframe —
        // and audio has no keyframes to align to, so it is cut to fit rather
        // than the other way round. Packets before the chosen video keyframe
        // are dropped; without that the audio would lead the picture by however
        // far back the keyframe search had to reach.
        let base = snapshot[0].1;
        let audio_snapshot: Vec<(Vec<u8>, i64, i64)> = match audio_type {
            Some(_) => {
                let inner = self.audio.lock().unwrap();
                inner
                    .packets
                    .iter()
                    .filter(|p| p.ts_100ns >= base)
                    .map(|p| (p.bytes.clone(), p.ts_100ns, p.duration_100ns))
                    .collect()
            }
            None => Vec::new(),
        };

        let audio_stream = match (audio_type, audio_snapshot.is_empty()) {
            (Some(t), false) => {
                let s = unsafe { writer.AddStream(t).map_err(step("add audio stream"))? };
                unsafe {
                    writer
                        .SetInputMediaType(s, t, None)
                        .map_err(step("set audio input type"))?
                };
                Some(s)
            }
            _ => None,
        };

        unsafe { writer.BeginWriting().map_err(step("begin writing"))? };

        // Write both streams in timestamp order rather than one after the
        // other. A muxer expects roughly interleaved input; handing it an
        // entire video track followed by an entire audio track makes it buffer
        // the lot, and with throttling on it simply blocks. Interleaving is
        // also what keeps the resulting file seekable without a rewrite.
        let mut order: Vec<(i64, bool, usize)> = Vec::with_capacity(snapshot.len() + audio_snapshot.len());
        for (i, s) in snapshot.iter().enumerate() {
            order.push((s.1, false, i));
        }
        for (i, a) in audio_snapshot.iter().enumerate() {
            order.push((a.1, true, i));
        }
        order.sort_by_key(|(ts, is_audio, _)| (*ts, *is_audio));

        let mut bytes_written = 0usize;
        for (_, is_audio, idx) in &order {
            if *is_audio {
                let Some(a_stream) = audio_stream else { continue };
                let (data, ts, dur) = &audio_snapshot[*idx];
                let buffer: IMFMediaBuffer =
                    unsafe { MFCreateMemoryBuffer(data.len() as u32)? };
                unsafe {
                    let mut dst: *mut u8 = std::ptr::null_mut();
                    buffer.Lock(&mut dst, None, None)?;
                    std::ptr::copy_nonoverlapping(data.as_ptr(), dst, data.len());
                    buffer.Unlock()?;
                    buffer.SetCurrentLength(data.len() as u32)?;
                }
                let sample: IMFSample = unsafe { MFCreateSample()? };
                unsafe {
                    sample.AddBuffer(&buffer)?;
                    sample.SetSampleTime(ts - base)?;
                    sample.SetSampleDuration(*dur)?;
                    writer
                        .WriteSample(a_stream, &sample)
                        .map_err(step("write audio sample"))?;
                }
                bytes_written += data.len();
                continue;
            }

            let i = *idx;
            let (data, ts, dur, keyframe) = &snapshot[i];
            // Duration from the gap to the *next* sample, not the encoder's
            // nominal figure.
            //
            // WGC is change-driven, so a static scene genuinely produces frames
            // far apart while the encoder still reports a nominal 1/fps for
            // each. Trusting that nominal value makes the MP4 sink lay out the
            // timeline at a constant cadence, and the clip's duration collapses
            // to (frames / fps) — measured: 13.8 s of footage muxed as a 2 s
            // file, containing every frame but claiming a seventh of the time.
            // ADR §7 is explicit that the muxer must express real timing; dense
            // footage hides this because nominal and real spacing agree.
            let real_dur = if i + 1 < snapshot.len() {
                (snapshot[i + 1].1 - *ts).max(1)
            } else {
                *dur
            };
            let buffer: IMFMediaBuffer = unsafe { MFCreateMemoryBuffer(data.len() as u32)? };
            unsafe {
                let mut dst: *mut u8 = std::ptr::null_mut();
                buffer.Lock(&mut dst, None, None)?;
                std::ptr::copy_nonoverlapping(data.as_ptr(), dst, data.len());
                buffer.Unlock()?;
                buffer.SetCurrentLength(data.len() as u32)?;
            }
            let sample: IMFSample = unsafe { MFCreateSample()? };
            unsafe {
                sample.AddBuffer(&buffer)?;
                sample.SetSampleTime(ts - base)?;
                sample.SetSampleDuration(real_dur)?;
                if *keyframe {
                    sample.SetUINT32(&MFSampleExtension_CleanPoint, 1)?;
                }
                writer.WriteSample(stream, &sample)?;
            }
            bytes_written += data.len();
        }
        unsafe { writer.Finalize().map_err(step("finalize"))? };

        let span = (snapshot[snapshot.len() - 1].1 - base) as f64 / 1e7;
        Ok(SaveReport {
            frames: snapshot.len(),
            audio_packets: audio_snapshot.len(),
            bytes: bytes_written,
            span_secs: span,
            elapsed_ms: t0.elapsed().as_secs_f64() * 1000.0,
        })
    }
}

/* ------------------------------------------------------- annex B utilities */

/// Walk annex B NAL units, calling `f(nal_type, unit)` where `unit` starts at
/// the unit's start code. Single pass, no allocation — this runs per encoded
/// sample on the grabber thread.
fn for_each_nal(data: &[u8], mut f: impl FnMut(u8, &[u8])) {
    let n = data.len();
    let mut unit_start: Option<usize> = None;
    let mut i = 0;
    while i + 2 < n {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            let sc = if i > 0 && data[i - 1] == 0 { i - 1 } else { i };
            if let Some(us) = unit_start {
                f(nal_type_at(data, us), &data[us..sc]);
            }
            unit_start = Some(sc);
            i += 3;
        } else {
            i += 1;
        }
    }
    if let Some(us) = unit_start {
        f(nal_type_at(data, us), &data[us..n]);
    }
}

fn nal_type_at(data: &[u8], start_code: usize) -> u8 {
    let mut j = start_code;
    while j < data.len() && data[j] == 0 {
        j += 1;
    }
    // j is at the 0x01; the NAL header is the next byte.
    if j + 1 < data.len() {
        data[j + 1] & 0x1f
    } else {
        0
    }
}

/// Any IDR slice (NAL type 5) makes the sample a keyframe.
fn contains_idr(data: &[u8]) -> bool {
    let mut found = false;
    for_each_nal(data, |ty, _| {
        if ty == 5 {
            found = true;
        }
    });
    found
}

/// Collect SPS (7) and PPS (8) units, normalised to 4-byte start codes — the
/// blob format MF_MT_MPEG_SEQUENCE_HEADER expects for H.264.
fn sequence_header(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for_each_nal(data, |ty, unit| {
        if ty == 7 || ty == 8 {
            let mut j = 0;
            while j < unit.len() && unit[j] == 0 {
                j += 1;
            }
            // j at 0x01; payload starts after it.
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(&unit[j + 1..]);
        }
    });
    out
}
