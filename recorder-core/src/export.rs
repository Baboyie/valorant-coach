//! Trimming and social-media export.
//!
//! Two jobs that look alike and are not:
//!
//! **Copy** trims by copying compressed packets between the cut points — no
//! decode, no encode, no quality loss, and near-instant. The catch is that a
//! copied stream can only *start* on a keyframe. Our recordings keep a
//! keyframe every half second (`EncoderConfig::gop_frames`), so the start
//! snaps at most half a second early; and because we encode with zero
//! B-frames, every frame references only the past, so the *end* can cut
//! anywhere.
//!
//! **Budget** re-encodes to fit a byte target — Discord's upload cap being
//! the one that matters. "Compression without losing quality" is not a thing
//! the arithmetic allows at these sizes, so the honest version is: NVENC at
//! the highest bitrate the budget affords, optionally scaled down, with one
//! audio track at 128 kbps. One track, because that is all any social
//! platform keeps anyway.
//!
//! Both run through one Media Foundation Source Reader → Sink Writer loop;
//! the modes differ only in whether the samples crossing it are compressed
//! or decoded.

use std::collections::HashMap;
use std::path::Path;

use windows::core::{Result, HSTRING};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};

use crate::encoder::EncoderConfig;

/// Stream index sentinels for `IMFSourceReader`, which the crate does not
/// export as plain u32s.
const ANY_STREAM: u32 = 0xFFFFFFFE;
const ALL_STREAMS: u32 = 0xFFFFFFFE;
/// `MF_SOURCE_READERF_*` bits.
const F_ENDOFSTREAM: u32 = 0x1;
const F_ERROR: u32 = 0x2;
const F_STREAMTICK: u32 = 0x100;

pub enum ExportMode {
    /// Packet copy between the cut points. Original quality; the start snaps
    /// to the previous keyframe.
    Copy,
    /// Re-encode to fit `target_bytes`, scaling down to at most `max_height`
    /// rows when the source is taller. Frame-accurate at both ends.
    Budget { target_bytes: u64, max_height: u32 },
}

#[derive(Debug)]
pub struct ExportReport {
    pub bytes: u64,
    pub duration_secs: f64,
    pub elapsed_ms: f64,
    /// Where the video actually starts, which for Copy mode can be up to one
    /// GOP before what was asked. Reported rather than hidden: the UI can say
    /// "starts 0.3s early" instead of the user wondering.
    pub actual_start_100ns: i64,
}

pub fn export(
    src: &Path,
    dst: &Path,
    start_100ns: i64,
    end_100ns: i64,
    mode: ExportMode,
) -> Result<ExportReport> {
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
    }
    let hr = unsafe { MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET) };
    let result = hr.and_then(|_| run(src, dst, start_100ns, end_100ns, mode));
    unsafe {
        let _ = MFShutdown();
        CoUninitialize();
    }
    result
}

fn run(
    src: &Path,
    dst: &Path,
    start_100ns: i64,
    end_100ns: i64,
    mode: ExportMode,
) -> Result<ExportReport> {
    let t0 = std::time::Instant::now();

    // ---- reader --------------------------------------------------------
    //
    // Budget mode hands the reader a D3D manager so decode and any scaling
    // run on the GPU. The pool hazard that creates — decoder and scaler
    // samples come from small D3D surface pools, and a sink writer with
    // throttling off will happily queue enough of them to starve the pool and
    // block ReadSample forever — is dealt with in the pump: every video
    // sample is copied to a plain memory sample before the writer sees it,
    // which releases the surface immediately. Copy mode needs none of this.
    let hw: Option<(crate::d3d::Device, IMFDXGIDeviceManager)> =
        if matches!(mode, ExportMode::Budget { .. }) {
            let dev = crate::d3d::Device::new()?;
            let (manager, _) = crate::encoder::writer_attrs(&dev)?;
            Some((dev, manager))
        } else {
            None
        };
    let reader: IMFSourceReader = unsafe {
        match &hw {
            None => MFCreateSourceReaderFromURL(
                &HSTRING::from(src.to_string_lossy().as_ref()),
                None,
            )?,
            Some((_, manager)) => {
                let mut attrs: Option<IMFAttributes> = None;
                MFCreateAttributes(&mut attrs, 3)?;
                let attrs = attrs.expect("MFCreateAttributes returned nothing");
                attrs.SetUnknown(&MF_SOURCE_READER_D3D_MANAGER, manager)?;
                attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)?;
                // The reader is the one component in Media Foundation that
                // scales; the sink writer converts formats but will reject a
                // frame whose size differs from its output — measured, not
                // read: WriteSample, E_INVALIDARG, first frame.
                attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1)?;
                MFCreateSourceReaderFromURL(&HSTRING::from(src.to_string_lossy().as_ref()), &attrs)?
            }
        }
    };

    unsafe { reader.SetStreamSelection(ALL_STREAMS, false)? };

    // The requested window, clamped to the source's real length. Budget mode
    // prices its bitrate as the byte target over the cut's duration, so an
    // open-ended "to the end of the clip" request taken at its literal end
    // point would price the budget over time that does not exist and starve
    // the encoder down to the floor bitrate.
    let end_100ns = unsafe {
        use windows::Win32::System::Variant::VT_UI8;
        reader
            .GetPresentationAttribute(MF_SOURCE_READER_MEDIASOURCE.0 as u32, &MF_PD_DURATION)
            .ok()
            .and_then(|pv| {
                let inner = &*pv.Anonymous.Anonymous;
                (inner.vt == VT_UI8).then(|| inner.Anonymous.uhVal as i64)
            })
            .map_or(end_100ns, |dur| end_100ns.min(dur))
    };

    // ---- discover streams ---------------------------------------------
    let mut video: Option<(u32, IMFMediaType)> = None;
    let mut audio: Vec<(u32, IMFMediaType)> = Vec::new();
    for i in 0.. {
        let native = match unsafe { reader.GetNativeMediaType(i, 0) } {
            Ok(t) => t,
            Err(_) => break, // past the last stream
        };
        let major = unsafe { native.GetGUID(&MF_MT_MAJOR_TYPE)? };
        if major == MFMediaType_Video && video.is_none() {
            video = Some((i, native));
        } else if major == MFMediaType_Audio {
            audio.push((i, native));
        }
    }
    let (video_idx, video_native) = video.ok_or_else(|| {
        windows::core::Error::new(
            windows::Win32::Foundation::E_FAIL,
            "the file has no video stream",
        )
    })?;

    // ---- writer, wired per mode ----------------------------------------
    //
    // Budget mode gets the same hardware attributes the live pipeline uses —
    // encoder.rs warns that without them the sink writer silently picks the
    // software encoder. Copy mode encodes nothing; a bare writer is correct.
    let writer: IMFSinkWriter = unsafe {
        match &hw {
            None => MFCreateSinkWriterFromURL(
                &HSTRING::from(dst.to_string_lossy().as_ref()),
                None,
                None,
            )?,
            Some((dev, _)) => {
                let (_manager2, attrs) = crate::encoder::writer_attrs(dev)?;
                MFCreateSinkWriterFromURL(
                    &HSTRING::from(dst.to_string_lossy().as_ref()),
                    None,
                    &attrs,
                )?
            }
        }
    };

    // reader stream index -> (writer stream index, is_video)
    let mut route: HashMap<u32, (u32, bool)> = HashMap::new();

    match &mode {
        ExportMode::Copy => {
            // Native types straight through: the writer just re-containers.
            unsafe {
                reader.SetStreamSelection(video_idx, true)?;
                let out = writer.AddStream(&video_native)?;
                writer.SetInputMediaType(out, &video_native, None)?;
                route.insert(video_idx, (out, true));
                // Every audio track survives a trim — a reviewer still wants
                // the mic track a social upload would not keep.
                for (idx, native) in &audio {
                    reader.SetStreamSelection(*idx, true)?;
                    let out = writer.AddStream(native)?;
                    writer.SetInputMediaType(out, native, None)?;
                    route.insert(*idx, (out, false));
                }
            }
        }
        ExportMode::Budget { target_bytes, max_height } => {
            unsafe {
                // Ask for NV12 by subtype alone and read back what the reader
                // agreed to. A fully specified type is an invitation to an
                // E_INVALIDARG over details nobody cares about — frame-rate
                // rounding was enough — and the negotiated type is exactly
                // what the writer must be told its input is.
                reader.SetStreamSelection(video_idx, true)?;
                let partial = partial_video_type(&MFVideoFormat_NV12)?;
                reader.SetCurrentMediaType(video_idx, None, &partial)?;
                let decoded = reader.GetCurrentMediaType(video_idx)?;

                let (src_w, src_h) = frame_size(&decoded)?;
                let fps = frame_rate(&decoded)?;
                let (out_w, out_h) = scaled(src_w, src_h, *max_height);

                // Scaling happens here or nowhere: override the negotiated
                // type's frame size and set it again, which makes the reader
                // build its processing chain to that size.
                if (out_w, out_h) != (src_w, src_h) {
                    decoded.SetUINT64(&MF_MT_FRAME_SIZE, ((out_w as u64) << 32) | out_h as u64)?;
                    reader.SetCurrentMediaType(video_idx, None, &decoded)?;
                }
                let decoded = reader.GetCurrentMediaType(video_idx)?;
                let duration_secs =
                    ((end_100ns - start_100ns).max(1) as f64 / 10_000_000.0).max(0.1);
                let audio_bps: u32 = if audio.is_empty() { 0 } else { 128_000 };
                let cfg = EncoderConfig {
                    width: out_w,
                    height: out_h,
                    fps,
                    bitrate: budget_bitrate(*target_bytes, duration_secs, audio_bps),
                    gop_frames: EncoderConfig::default_gop(fps),
                };

                // Input at the decoder size, output at the scaled size: the
                // size difference is what makes the writer insert its
                // (hardware) processor to scale on the way into NVENC.
                let out = writer.AddStream(&crate::encoder::h264_output_type(&cfg)?)?;                writer.SetInputMediaType(out, &decoded, None)?;
                route.insert(video_idx, (out, true));

                // First audio track only: every platform this exists for keeps
                // exactly one, so carrying the second would spend budget on
                // bytes the destination throws away.
                if let Some((idx, _)) = audio.first() {
                    reader.SetStreamSelection(*idx, true)?;
                    let partial = partial_audio_pcm()?;
                    reader.SetCurrentMediaType(*idx, None, &partial)?;
                    let pcm = reader.GetCurrentMediaType(*idx)?;
                    let rate = pcm.GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND).unwrap_or(48_000);
                    let channels = pcm.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS).unwrap_or(2) as u16;
                    let fmt = crate::audio::AudioFormat {
                        sample_rate: rate,
                        channels,
                        device_channels: channels,
                    };
                    let out = writer.AddStream(&crate::encoder::aac_output_type(&fmt)?)?;
                    writer.SetInputMediaType(out, &pcm, None)?;
                    route.insert(*idx, (out, false));
                }
            }
        }
    }

    unsafe { writer.BeginWriting()? };

    // ---- seek -----------------------------------------------------------
    // The MP4 source seeks video to the previous sync point but trims audio
    // to the seek target itself — and MP4, as this writer emits it, has no
    // way to say "this track starts late": players put every track's first
    // sample at time zero. Seek Copy mode to the requested start and audio
    // would begin most of a second after the keyframe video snapped to, then
    // play that much early. So Copy mode first asks the video stream where
    // its keyframe actually is, then seeks everything there, making the
    // keyframe the shared origin that audio is cut against. Budget mode is
    // frame-accurate — its origin is the cut itself — and needs none of this.
    let frame_accurate = matches!(mode, ExportMode::Budget { .. });
    let mut seek_to = start_100ns.max(0);
    let mut copy_base: Option<i64> = None;
    if !frame_accurate {
        if let Some(key_ts) = unsafe { probe_video_start(&reader, video_idx, seek_to, &route)? } {
            seek_to = key_ts;
            copy_base = Some(key_ts);
        }
    }
    unsafe {
        let pos = propvariant_i8(seek_to);
        reader.SetCurrentPosition(&windows::core::GUID::zeroed(), &pos)?;
    }

    // ---- pump -----------------------------------------------------------
    let mut base: Option<i64> = copy_base;
    let mut open = route.len();
    let mut done: HashMap<u32, bool> = route.keys().map(|k| (*k, false)).collect();
    let mut last_ts: i64 = start_100ns;
    // Belt to the flag-handling braces: no realistic export makes this many
    // reads without delivering a sample, so hitting it means the reader is
    // wedged and the honest outcome is an error, not a spin.
    let mut barren_reads: u32 = 0;

    while open > 0 {
        if barren_reads > 500_000 {
            return Err(windows::core::Error::new(
                windows::Win32::Foundation::E_FAIL,
                "the source stopped delivering samples before its streams ended",
            ));
        }
        barren_reads += 1;
        let mut stream_idx = 0u32;
        let mut flags = 0u32;
        let mut ts = 0i64;
        let mut sample: Option<IMFSample> = None;
        unsafe {
            if let Err(e) = reader.ReadSample(
                ANY_STREAM,
                0,
                Some(&mut stream_idx),
                Some(&mut flags),
                Some(&mut ts),
                Some(&mut sample),
            ) {
                eprintln!("[export] ReadSample failed: {e}");
                return Err(e);
            }
        }
        // An errored stream never reaches EOS. Left unhandled it turns the
        // loop into a hot spin: ReadSample returns instantly with the error
        // flag, forever, and the count of open streams never falls — observed
        // as a finished file with a process burning a core behind it.
        if flags & (F_ENDOFSTREAM | F_ERROR) != 0 {
            // The error flag fires routinely at the tail of an export that
            // runs to the file's end, so it is treated as end-of-stream and
            // not worth a log line — the stall guard catches a truly wedged
            // reader either way.
            if let Some(d) = done.get_mut(&stream_idx) {
                if !*d {
                    *d = true;
                    open -= 1;
                }
            }
            continue;
        }
        if flags & F_STREAMTICK != 0 {
            continue;
        }
        let Some(sample) = sample else { continue };
        barren_reads = 0;
        let Some(&(writer_idx, is_video)) = route.get(&stream_idx) else { continue };
        if done.get(&stream_idx).copied().unwrap_or(true) {
            continue;
        }

        let ts = unsafe { sample.GetSampleTime().unwrap_or(ts) };

        // Past the out point: this stream is finished. Stop asking for it so
        // the reader spends no more decode on frames nobody will keep.
        if ts >= end_100ns {
            if let Some(d) = done.get_mut(&stream_idx) {
                *d = true;
                open -= 1;
            }
            unsafe {
                let _ = reader.SetStreamSelection(stream_idx, false);
            }
            continue;
        }

        // Establish the timeline origin on the first *video* sample.
        //
        // Budget mode is frame-accurate, so decoded lead-in before the cut is
        // dropped and the origin is the cut itself. Copy mode cannot drop the
        // keyframe the seek landed on — everything after references it — so
        // the origin is that keyframe, and the report says where it landed.
        if base.is_none() {
            if is_video {
                if frame_accurate && ts < start_100ns {
                    continue; // decoded lead-in
                }
                base = Some(if frame_accurate { start_100ns } else { ts });
            } else {
                continue; // audio before video's origin has no timeline yet
            }
        }
        let base_ts = base.expect("set above");
        if ts < base_ts {
            continue;
        }

        unsafe {
            // Video frames in Budget mode may live in a decoder or scaler
            // surface pool; copy them to plain memory so the writer's queue
            // can never starve that pool (which blocks ReadSample, forever).
            let out_sample = if is_video && frame_accurate {
                to_system_sample(&sample)?
            } else {
                sample.clone()
            };
            if let Err(e) = out_sample.SetSampleTime(ts - base_ts) {
                eprintln!("[export] SetSampleTime({}) failed on stream {stream_idx}: {e}", ts - base_ts);
                return Err(e);
            }
            if let Err(e) = writer.WriteSample(writer_idx, &out_sample) {
                eprintln!("[export] WriteSample failed: writer stream {writer_idx} (video={is_video}) ts={ts}: {e}");
                return Err(e);
            }
        }
        if is_video {
            last_ts = ts;
        }
    }

    unsafe { writer.Finalize()? };

    // The finished file gets the same container corrections a recording gets.
    // The sink writer over-declares trimmed audio tracks' durations (10.2s of
    // samples declared as 13.1s — players show the longer, frozen timeline),
    // and a Copy of a two-audio-track clip needs its alternates re-marked.
    if let Err(e) = crate::mp4::normalize_durations(dst) {
        eprintln!("[export] could not normalize durations: {e}");
    }
    if let Err(e) = crate::mp4::mark_audio_alternates(dst) {
        eprintln!("[export] could not mark audio alternates: {e}");
    }

    let bytes = std::fs::metadata(dst).map(|m| m.len()).unwrap_or(0);
    let actual_start = base.unwrap_or(start_100ns);
    Ok(ExportReport {
        bytes,
        duration_secs: ((last_ts - actual_start).max(0) as f64 / 10_000_000.0),
        elapsed_ms: t0.elapsed().as_secs_f64() * 1000.0,
        actual_start_100ns: actual_start,
    })
}

/* ------------------------------------------------------------- helpers */

/// The bitrate a byte budget affords, once audio and container overhead take
/// their share. Floored well above zero: a budget too small for the duration
/// should produce a bad-looking video, not a broken one.
pub fn budget_bitrate(target_bytes: u64, duration_secs: f64, audio_bps: u32) -> u32 {
    let total_bps = (target_bytes as f64 * 8.0 * 0.94) / duration_secs.max(0.1);
    (total_bps - audio_bps as f64).max(500_000.0) as u32
}

/// Scale to fit `max_height`, preserving aspect, both dimensions even — H.264
/// 4:2:0 requires even, and an odd width is a rejected media type, not a
/// slightly narrow video.
pub fn scaled(w: u32, h: u32, max_height: u32) -> (u32, u32) {
    if h <= max_height || max_height == 0 {
        return (w & !1, h & !1);
    }
    let out_h = max_height & !1;
    let out_w = ((w as u64 * out_h as u64) / h as u64) as u32;
    (out_w.max(2) & !1, out_h.max(2))
}

unsafe fn frame_size(t: &IMFMediaType) -> Result<(u32, u32)> {
    let packed = unsafe { t.GetUINT64(&MF_MT_FRAME_SIZE)? };
    Ok(((packed >> 32) as u32, packed as u32))
}

unsafe fn frame_rate(t: &IMFMediaType) -> Result<u32> {
    let packed = unsafe { t.GetUINT64(&MF_MT_FRAME_RATE)? };
    let (num, den) = ((packed >> 32) as u32, packed as u32);
    Ok(if den == 0 { 60 } else { (num + den / 2) / den })
}

/// Where does video actually start for a seek to `start`?
///
/// The MP4 source snaps video to the previous keyframe; this asks it directly
/// and cheaply: every other routed stream deselected, one video sample read,
/// its timestamp returned. The reader is left with all routed streams selected
/// again but positioned mid-probe — the caller seeks afterwards regardless.
unsafe fn probe_video_start(
    reader: &IMFSourceReader,
    video_idx: u32,
    start: i64,
    route: &HashMap<u32, (u32, bool)>,
) -> Result<Option<i64>> {
    unsafe {
        for idx in route.keys() {
            if *idx != video_idx {
                reader.SetStreamSelection(*idx, false)?;
            }
        }
        let pos = propvariant_i8(start);
        reader.SetCurrentPosition(&windows::core::GUID::zeroed(), &pos)?;
        let mut found = None;
        // Bounded: a healthy source answers on the first read; a wedged one
        // must not turn a probe into a hang.
        for _ in 0..2048 {
            let mut flags = 0u32;
            let mut ts = 0i64;
            let mut sample: Option<IMFSample> = None;
            reader.ReadSample(
                video_idx,
                0,
                None,
                Some(&mut flags),
                Some(&mut ts),
                Some(&mut sample),
            )?;
            if flags & (F_ENDOFSTREAM | F_ERROR) != 0 {
                break;
            }
            if let Some(s) = sample {
                found = Some(s.GetSampleTime().unwrap_or(ts));
                break;
            }
        }
        for idx in route.keys() {
            reader.SetStreamSelection(*idx, true)?;
        }
        Ok(found)
    }
}

/// Copy a sample's payload into a plain memory sample, keeping its timing.
///
/// The bridge between GPU pools and the writer's queue: a D3D-backed sample
/// held by the writer is a surface the decoder cannot reuse, and pools are
/// small. ConvertToContiguousBuffer does the readback when the source is on
/// the GPU and is a cheap pass-through when it is not.
unsafe fn to_system_sample(sample: &IMFSample) -> Result<IMFSample> {
    unsafe {
        let src_buf = sample.ConvertToContiguousBuffer()?;
        let mut p: *mut u8 = std::ptr::null_mut();
        let mut len: u32 = 0;
        src_buf.Lock(&mut p, None, Some(&mut len))?;
        let dst_buf = MFCreateMemoryBuffer(len)?;
        let mut q: *mut u8 = std::ptr::null_mut();
        dst_buf.Lock(&mut q, None, None)?;
        std::ptr::copy_nonoverlapping(p, q, len as usize);
        dst_buf.Unlock()?;
        dst_buf.SetCurrentLength(len)?;
        src_buf.Unlock()?;

        let out = MFCreateSample()?;
        out.AddBuffer(&dst_buf)?;
        if let Ok(t) = sample.GetSampleTime() {
            out.SetSampleTime(t)?;
        }
        if let Ok(d) = sample.GetSampleDuration() {
            out.SetSampleDuration(d)?;
        }
        Ok(out)
    }
}

/// A partial video type: major and subtype only. The reader fills in size,
/// rate and the rest from the decoder, and GetCurrentMediaType reports what
/// was actually agreed.
fn partial_video_type(subtype: &windows::core::GUID) -> Result<IMFMediaType> {
    unsafe {
        let t: IMFMediaType = MFCreateMediaType()?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        t.SetGUID(&MF_MT_SUBTYPE, subtype)?;
        Ok(t)
    }
}

/// A partial PCM type: 16-bit pinned (the AAC encoder wants it); rate and
/// channel count are the decoder side of the negotiation.
fn partial_audio_pcm() -> Result<IMFMediaType> {
    unsafe {
        let t: IMFMediaType = MFCreateMediaType()?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
        t.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
        t.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
        Ok(t)
    }
}

/// A VT_I8 PROPVARIANT, which is how the source reader takes a seek position.
fn propvariant_i8(v: i64) -> windows::Win32::System::Com::StructuredStorage::PROPVARIANT {
    use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
    use windows::Win32::System::Variant::VT_I8;
    let mut pv: PROPVARIANT = Default::default();
    unsafe {
        (*pv.Anonymous.Anonymous).vt = VT_I8;
        (*pv.Anonymous.Anonymous).Anonymous.hVal = v;
    }
    pv
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_leaves_room_for_audio_and_container() {
        // 10 MB, 30 s, 128 kbps audio: roughly (10MB*8*0.94)/30 - 128k.
        let v = budget_bitrate(10 * 1024 * 1024, 30.0, 128_000);
        assert!((2_300_000..2_600_000).contains(&v), "got {v}");
    }

    #[test]
    fn budget_never_collapses_to_zero() {
        assert_eq!(budget_bitrate(100_000, 600.0, 128_000), 500_000);
        assert_eq!(budget_bitrate(0, 1.0, 128_000), 500_000);
    }

    #[test]
    fn scaling_preserves_aspect_and_evenness() {
        assert_eq!(scaled(1920, 1080, 720), (1280, 720));
        assert_eq!(scaled(1600, 900, 720), (1280, 720));
        // Already small enough: untouched apart from evenness.
        assert_eq!(scaled(1280, 720, 1080), (1280, 720));
        assert_eq!(scaled(1601, 901, 1080), (1600, 900));
        // Odd result forced even.
        let (w, h) = scaled(1602, 932, 720);
        assert_eq!(w % 2, 0);
        assert_eq!(h % 2, 0);
        assert_eq!(h, 720);
    }

    #[test]
    fn zero_max_height_means_no_scaling() {
        assert_eq!(scaled(1920, 1080, 0), (1920, 1080));
    }
}
