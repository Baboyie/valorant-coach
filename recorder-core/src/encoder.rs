//! Hardware encode + MP4 muxing via Media Foundation's sink writer.
//!
//! The whole point of this file is what it does NOT do: no readback, no pixel
//! conversion on the CPU, no per-frame allocation. A captured frame is already an
//! `ID3D11Texture2D` in VRAM; we wrap it in an `IMFSample` that points at that
//! same texture and hand it to the encoder. The bytes never touch system memory
//! until they come back out compressed (ADR §8).
//!
//! `IMFSinkWriter` is used rather than driving an MFT by hand because it also
//! owns the muxer and the file writer, and — with hardware transforms enabled —
//! inserts the GPU colour converter (BGRA -> NV12) itself. That conversion has to
//! happen somewhere; letting the video processor do it on the GPU is strictly
//! cheaper than doing it on the cores Valorant wants.

use std::sync::Arc;

use windows::core::{implement, Interface, Result, HSTRING};
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::Foundation::VARIANT_BOOL;
use windows::Win32::System::Variant::{VARIANT, VT_BOOL, VT_UI4};

use crate::d3d;
use crate::replay::ReplayRing;

pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate: u32,
    /// Frames between keyframes.
    ///
    /// This is a clip-quality setting, not just a compression one: a replay
    /// clip must start on a keyframe, so the GOP is the worst-case error
    /// between the moment the player asked for and the moment the clip
    /// actually begins. It also sets how much extra the ring must retain
    /// beyond the requested window.
    ///
    /// Measured from ShadowPlay's own output on this machine: **0.5 s**, across
    /// clips from 2024 and 2026 alike. That is a deliberate choice on their
    /// part and the right one for a clipper — our original 2 s made a 10 s
    /// window produce a 13 s file.
    ///
    /// **The NVIDIA MFT does not honour this.** Requesting 30 frames yields ~56
    /// and requesting 15 yields ~52, through both `MF_MT_MAX_KEYFRAME_SPACING`
    /// and `CODECAPI_AVEncMPVGOPSize` — a floor around 0.9 s at 60 fps, not a
    /// setting we are failing to apply. The request is kept because it is
    /// correct, costs nothing, and a future driver may honour it. See ADR §9c
    /// for what this implies about the Media Foundation route.
    pub gop_frames: u32,
}

impl EncoderConfig {
    /// Bits per second for a frame size, at roughly the density ShadowPlay
    /// settles on for 1080p60 (~16 Mbps).
    ///
    /// Their older clips ran 50 Mbps and their newer ones ~16 at identical
    /// settings otherwise, so 16 is the figure they converged on rather than a
    /// number we invented. Bitrate is nearly free on NVENC — it costs disk and
    /// file size, not encode time — so §22's "quality comparable to ShadowPlay"
    /// is cheap to honour, while §14's disk concerns argue against copying the
    /// 50 Mbps setting.
    pub fn default_bitrate(width: u32, height: u32, fps: u32) -> u32 {
        let pixels_per_sec = width as u64 * height as u64 * fps as u64;
        // 1920*1080*60 = 124.4M pixel/s -> ~16 Mbps
        ((pixels_per_sec * 16_000_000) / 124_416_000).min(80_000_000) as u32
    }

    /// Half a second of frames, matching ShadowPlay.
    pub fn default_gop(fps: u32) -> u32 {
        (fps.max(1) / 2).max(1)
    }
}

/// The H.264 type the *encoder* is configured with. The replay muxer builds
/// its own, deliberately more minimal type: GOP spacing and profile are
/// instructions to an encoder, and a passthrough mux has no encoder to
/// instruct.
pub(crate) fn h264_output_type(cfg: &EncoderConfig) -> Result<IMFMediaType> {
    unsafe {
        let t: IMFMediaType = MFCreateMediaType()?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
        t.SetUINT32(&MF_MT_AVG_BITRATE, cfg.bitrate)?;
        t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        // High profile: better quality at the same bitrate than Main, and
        // universally supported by the hardware we care about.
        t.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)?;
        // Left to itself a driver picks a much longer GOP. See
        // EncoderConfig::gop_frames for why this is a clip-quality setting.
        t.SetUINT32(&MF_MT_MAX_KEYFRAME_SPACING, cfg.gop_frames.max(1))?;
        set_size(&t, &MF_MT_FRAME_SIZE, cfg.width, cfg.height)?;
        set_size(&t, &MF_MT_FRAME_RATE, cfg.fps, 1)?;
        set_size(&t, &MF_MT_PIXEL_ASPECT_RATIO, 1, 1)?;
        Ok(t)
    }
}

/// What lands in the file for audio: AAC-LC.
///
/// 128 kbps stereo. Voice comms and game audio for review do not benefit from
/// more, and the encoder cost is charged to the CPU, which §2 wants left alone.
pub(crate) fn aac_output_type(fmt: &crate::audio::AudioFormat) -> Result<IMFMediaType> {
    unsafe {
        let t: IMFMediaType = MFCreateMediaType()?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
        t.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
        t.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
        t.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, fmt.sample_rate)?;
        t.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, fmt.channels as u32)?;
        t.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, 16_000)?;
        // 0 = raw AAC in the container, which is what MP4 wants. The ADTS
        // variants are for streaming and would produce a file most players
        // handle badly.
        t.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0)?;
        Ok(t)
    }
}

/// What we hand the encoder: the interleaved 16-bit PCM `audio.rs` produces.
pub(crate) fn pcm_input_type(fmt: &crate::audio::AudioFormat) -> Result<IMFMediaType> {
    let block_align = fmt.channels as u32 * 2; // 16-bit
    unsafe {
        let t: IMFMediaType = MFCreateMediaType()?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
        t.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
        t.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
        t.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, fmt.sample_rate)?;
        t.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, fmt.channels as u32)?;
        t.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, block_align)?;
        t.SetUINT32(
            &MF_MT_AUDIO_AVG_BYTES_PER_SECOND,
            fmt.sample_rate * block_align,
        )?;
        t.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1)?;
        Ok(t)
    }
}

/// The input type: exactly what WGC produced. Declaring BGRA here rather than
/// converting ourselves is what lets the GPU video processor do the NV12
/// conversion on our behalf.
fn argb_input_type(cfg: &EncoderConfig) -> Result<IMFMediaType> {
    unsafe {
        let t: IMFMediaType = MFCreateMediaType()?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_ARGB32)?;
        t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        set_size(&t, &MF_MT_FRAME_SIZE, cfg.width, cfg.height)?;
        set_size(&t, &MF_MT_FRAME_RATE, cfg.fps, 1)?;
        set_size(&t, &MF_MT_PIXEL_ASPECT_RATIO, 1, 1)?;
        Ok(t)
    }
}

/// D3D device sharing plus the writer attributes both constructors need.
pub(crate) fn writer_attrs(dev: &d3d::Device) -> Result<(IMFDXGIDeviceManager, IMFAttributes)> {
    // Share our D3D11 device with Media Foundation so the encoder and the
    // colour converter run on the same device the frames already live on.
    // A second device would mean copying across devices — through RAM.
    let mut reset_token: u32 = 0;
    let mut manager: Option<IMFDXGIDeviceManager> = None;
    unsafe { MFCreateDXGIDeviceManager(&mut reset_token, &mut manager)? };
    let manager = manager.expect("MFCreateDXGIDeviceManager returned no manager");
    unsafe { manager.ResetDevice(&dev.device, reset_token)? };

    let mut a: Option<IMFAttributes> = None;
    unsafe { MFCreateAttributes(&mut a, 4)? };
    let attrs = a.expect("MFCreateAttributes returned nothing");
    unsafe {
        // Without this the sink writer silently picks the software encoder,
        // which is the one outcome §2 forbids.
        attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)?;
        // Do not pace writes to wall-clock: we are recording, not playing
        // back, and throttling here would push back on the capture thread.
        attrs.SetUINT32(&MF_SINK_WRITER_DISABLE_THROTTLING, 1)?;
        attrs.SetUnknown(&MF_SINK_WRITER_D3D_MANAGER, &manager)?;
    }
    Ok((manager, attrs))
}

pub struct Encoder {
    writer: IMFSinkWriter,
    stream: u32,
    // Kept alive for the writer's lifetime: it holds a raw reference to the
    // device manager, and dropping it early would take the D3D device with it.
    _dxgi_manager: IMFDXGIDeviceManager,
    // Present on the replay path: the grabber's media sink, kept so it can be
    // shut down explicitly at finish.
    media_sink: Option<IMFMediaSink>,
    /// (stream index, bytes/sec) per audio track, in the order given to
    /// `to_file`. Empty on the replay path, where the grabber sink carries one
    /// stream and audio runs its own encoders.
    audio_streams: Vec<(u32, u32)>,
    first_ts: Option<i64>,
    nominal_duration: i64,
    pub frames_written: u64,
    pub audio_written: u64,
}

impl Encoder {
    /// Encode straight into an MP4 file (the `record` command).
    ///
    /// `audio_fmt` adds a second AAC stream. Passing `None` keeps the file
    /// video-only, which is what the benchmark path wants: §9's overhead result
    /// was measured without an audio encode and stays comparable only if the
    /// benchmark keeps running the same way.
    /// `audio_fmts` adds one AAC stream per entry — desktop and microphone stay
    /// separate tracks rather than being mixed (§23). Mixing would also be the
    /// heavier option: the two devices frequently run at different sample
    /// rates, so combining them would need resampling.
    pub fn to_file(
        dev: &d3d::Device,
        path: &str,
        cfg: &EncoderConfig,
        audio_fmts: &[crate::audio::AudioFormat],
    ) -> Result<Encoder> {
        let (manager, attrs) = writer_attrs(dev)?;
        let writer = unsafe { MFCreateSinkWriterFromURL(&HSTRING::from(path), None, &attrs)? };
        let stream = unsafe { writer.AddStream(&h264_output_type(cfg)?)? };
        unsafe { writer.SetInputMediaType(stream, &argb_input_type(cfg)?, None)? };
        configure_codec(&writer, stream, cfg.gop_frames.max(1));

        let mut audio_streams = Vec::new();
        for fmt in audio_fmts {
            let a = unsafe { writer.AddStream(&aac_output_type(fmt)?)? };
            unsafe { writer.SetInputMediaType(a, &pcm_input_type(fmt)?, None)? };
            let block_align = fmt.channels as u32 * 2;
            audio_streams.push((a, fmt.sample_rate * block_align));
        }

        unsafe { writer.BeginWriting()? };

        Ok(Encoder {
            writer,
            stream,
            _dxgi_manager: manager,
            media_sink: None,
            audio_streams,
            first_ts: None,
            nominal_duration: if cfg.fps > 0 { 10_000_000 / cfg.fps as i64 } else { 166_666 },
            frames_written: 0,
            audio_written: 0,
        })
    }

    /// Encode into the replay ring instead of a file (the `replay` command).
    ///
    /// Same sink writer, same hardware encoder, same D3D sharing — the only
    /// difference is the sink: a sample grabber whose callback hands each
    /// compressed sample to the ring. Encoding and muxing are thereby
    /// decoupled, which is the property a replay buffer is made of: the
    /// encoder runs continuously, the muxer runs only when someone saves.
    pub fn to_replay(
        dev: &d3d::Device,
        cfg: &EncoderConfig,
        ring: Arc<ReplayRing>,
    ) -> Result<Encoder> {
        let (manager, attrs) = writer_attrs(dev)?;

        let callback: IMFSampleGrabberSinkCallback = GrabberCallback { ring }.into();
        let activate =
            unsafe { MFCreateSampleGrabberSinkActivate(&h264_output_type(cfg)?, &callback)? };
        // Deliver samples as they are produced. The grabber otherwise paces
        // delivery to a presentation clock, which is playback behaviour — the
        // same wall-clock coupling DISABLE_THROTTLING removes on the file path.
        unsafe { activate.SetUINT32(&MF_SAMPLEGRABBERSINK_IGNORE_CLOCK, 1)? };
        let sink: IMFMediaSink = unsafe { activate.ActivateObject()? };

        let writer = unsafe { MFCreateSinkWriterFromMediaSink(&sink, &attrs)? };
        // The grabber sink has exactly one stream sink; the writer maps it as
        // stream 0. AddStream would try to add a second and fail.
        let stream = 0u32;
        unsafe { writer.SetInputMediaType(stream, &argb_input_type(cfg)?, None)? };
        configure_codec(&writer, stream, cfg.gop_frames.max(1));
        unsafe { writer.BeginWriting()? };

        Ok(Encoder {
            writer,
            stream,
            _dxgi_manager: manager,
            media_sink: Some(sink),
            // The grabber sink carries exactly one stream, so audio cannot ride
            // along here. The replay path gets sound from parallel encoders
            // writing into the same ring — see `AudioEncoder`.
            audio_streams: Vec::new(),
            first_ts: None,
            nominal_duration: if cfg.fps > 0 { 10_000_000 / cfg.fps as i64 } else { 166_666 },
            frames_written: 0,
            audio_written: 0,
        })
    }

    /// Submit one packet of PCM.
    ///
    /// `ts_100ns` is WASAPI's QPC position, which shares its origin with WGC's
    /// `SystemRelativeTime` — both are QueryPerformanceCounter in 100 ns units.
    /// Rebasing both against the *video* first frame is what puts sound and
    /// picture on one timeline; using each stream's own first sample instead
    /// would silently shift audio by however long the encoder took to start.
    pub fn write_audio(&mut self, track: usize, pcm: &[i16], ts_100ns: i64) -> Result<()> {
        let Some(&(stream, bytes_per_sec)) = self.audio_streams.get(track) else {
            return Ok(());
        };
        if pcm.is_empty() {
            return Ok(());
        }
        // Audio arriving before the first video frame has nowhere to sit on the
        // timeline; dropping it costs a few milliseconds of lead-in and avoids
        // negative sample times, which the sink writer rejects.
        let Some(base) = self.first_ts else {
            return Ok(());
        };
        let rel = ts_100ns - base;
        if rel < 0 {
            return Ok(());
        }

        let bytes = pcm.len() * 2;
        let buffer: IMFMediaBuffer = unsafe { MFCreateMemoryBuffer(bytes as u32)? };
        unsafe {
            let mut dst: *mut u8 = std::ptr::null_mut();
            buffer.Lock(&mut dst, None, None)?;
            std::ptr::copy_nonoverlapping(pcm.as_ptr() as *const u8, dst, bytes);
            buffer.Unlock()?;
            buffer.SetCurrentLength(bytes as u32)?;
        }

        // Duration from the byte count and the format, not from a nominal
        // packet size — WASAPI packets vary in length.
        let duration = if bytes_per_sec > 0 {
            (bytes as i64 * 10_000_000) / bytes_per_sec as i64
        } else {
            0
        };

        let sample: IMFSample = unsafe { MFCreateSample()? };
        unsafe {
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(rel)?;
            sample.SetSampleDuration(duration)?;
            self.writer.WriteSample(stream, &sample)?;
        }
        self.audio_written += 1;
        Ok(())
    }

    /// Submit one captured frame.
    ///
    /// `ts_100ns` is the frame's `SystemRelativeTime` from WGC. We rebase it to
    /// the first frame and write real per-sample timestamps rather than assuming
    /// a constant cadence — WGC is change-driven, so a static scene genuinely
    /// produces fewer frames and the container should say so (ADR §7).
    pub fn write_frame(&mut self, tex: &ID3D11Texture2D, ts_100ns: i64) -> Result<()> {
        let base = *self.first_ts.get_or_insert(ts_100ns);
        let rel = (ts_100ns - base).max(0);

        // A buffer that *points at* the existing texture. No copy happens here.
        let buffer: IMFMediaBuffer = unsafe {
            MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, tex, 0, false)?
        };
        // The DXGI buffer knows its own size but reports zero current length
        // until told; the encoder rejects a zero-length sample.
        if let Ok(b2) = buffer.cast::<IMF2DBuffer>() {
            let len = unsafe { b2.GetContiguousLength()? };
            unsafe { buffer.SetCurrentLength(len)? };
        }

        let sample: IMFSample = unsafe { MFCreateSample()? };
        unsafe {
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(rel)?;
            sample.SetSampleDuration(self.nominal_duration)?;
            self.writer.WriteSample(self.stream, &sample)?;
        }
        self.frames_written += 1;
        Ok(())
    }

    /// Flush the encoder and close the container. Skipping this leaves an
    /// unplayable file — the moov atom never gets written. On the replay path
    /// it flushes the last samples into the ring instead.
    pub fn finish(self) -> Result<()> {
        unsafe { self.writer.Finalize()? };
        if let Some(sink) = &self.media_sink {
            // Release the grabber's worker threads promptly rather than at
            // process exit. Failure here costs nothing — the samples are
            // already in the ring.
            let _ = unsafe { sink.Shutdown() };
        }
        Ok(())
    }
}

/// Encodes audio into the replay ring, parallel to the video `Encoder`.
///
/// A separate object because the sample grabber sink carries exactly one
/// stream, so replay audio cannot ride the video writer the way it does on the
/// file path. Two writers, two grabbers, one ring.
pub struct AudioEncoder {
    writer: IMFSinkWriter,
    sink: IMFMediaSink,
    stream: u32,
    bytes_per_sec: u32,
    first_ts: Option<i64>,
    pub packets_written: u64,
    /// The AAC type Media Foundation actually negotiated, including the codec
    /// private data the MP4 sink needs to write `esds`. Asking MF for it beats
    /// hand-rolling an AudioSpecificConfig blob — the same problem SPS/PPS
    /// posed on the video side, solved by asking rather than constructing.
    pub negotiated_type: Option<IMFMediaType>,
}

impl AudioEncoder {
    pub fn to_replay(
        track: crate::replay::AudioTrack,
        fmt: &crate::audio::AudioFormat,
        ring: Arc<ReplayRing>,
    ) -> Result<AudioEncoder> {
        let callback: IMFSampleGrabberSinkCallback =
            AudioGrabberCallback { ring, track }.into();
        let activate =
            unsafe { MFCreateSampleGrabberSinkActivate(&aac_output_type(fmt)?, &callback)? };
        unsafe { activate.SetUINT32(&MF_SAMPLEGRABBERSINK_IGNORE_CLOCK, 1)? };
        let sink: IMFMediaSink = unsafe { activate.ActivateObject()? };

        let mut attrs: Option<IMFAttributes> = None;
        unsafe { MFCreateAttributes(&mut attrs, 2)? };
        let attrs = attrs.expect("MFCreateAttributes returned nothing");
        unsafe {
            attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)?;
            attrs.SetUINT32(&MF_SINK_WRITER_DISABLE_THROTTLING, 1)?;
        }

        let writer = unsafe { MFCreateSinkWriterFromMediaSink(&sink, &attrs)? };
        let stream = 0u32;
        unsafe { writer.SetInputMediaType(stream, &pcm_input_type(fmt)?, None)? };
        unsafe { writer.BeginWriting()? };

        // Only valid once writing has begun and the type is settled.
        let negotiated_type = unsafe {
            sink.GetStreamSinkByIndex(0)
                .and_then(|ss| ss.GetMediaTypeHandler())
                .and_then(|h| h.GetCurrentMediaType())
                .ok()
        };

        let block_align = fmt.channels as u32 * 2;
        Ok(AudioEncoder {
            writer,
            sink,
            stream,
            bytes_per_sec: fmt.sample_rate * block_align,
            first_ts: None,
            packets_written: 0,
            negotiated_type,
        })
    }

    /// `base_ts` is the *video* first-frame timestamp, so both streams share an
    /// origin. Passing None until video has started drops the lead-in rather
    /// than letting audio define a timeline video will disagree with.
    pub fn write(&mut self, pcm: &[i16], ts_100ns: i64, base_ts: Option<i64>) -> Result<()> {
        let Some(base) = base_ts.or(self.first_ts) else {
            return Ok(());
        };
        self.first_ts.get_or_insert(base);
        let rel = ts_100ns - base;
        if rel < 0 || pcm.is_empty() {
            return Ok(());
        }

        let bytes = pcm.len() * 2;
        let buffer: IMFMediaBuffer = unsafe { MFCreateMemoryBuffer(bytes as u32)? };
        unsafe {
            let mut dst: *mut u8 = std::ptr::null_mut();
            buffer.Lock(&mut dst, None, None)?;
            std::ptr::copy_nonoverlapping(pcm.as_ptr() as *const u8, dst, bytes);
            buffer.Unlock()?;
            buffer.SetCurrentLength(bytes as u32)?;
        }
        let duration = if self.bytes_per_sec > 0 {
            (bytes as i64 * 10_000_000) / self.bytes_per_sec as i64
        } else {
            0
        };
        let sample: IMFSample = unsafe { MFCreateSample()? };
        unsafe {
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(rel)?;
            sample.SetSampleDuration(duration)?;
            self.writer.WriteSample(self.stream, &sample)?;
        }
        self.packets_written += 1;
        Ok(())
    }

    pub fn finish(self) -> Result<()> {
        unsafe { self.writer.Finalize()? };
        let _ = unsafe { self.sink.Shutdown() };
        Ok(())
    }
}

#[implement(IMFSampleGrabberSinkCallback)]
struct AudioGrabberCallback {
    ring: Arc<ReplayRing>,
    track: crate::replay::AudioTrack,
}

impl IMFSampleGrabberSinkCallback_Impl for AudioGrabberCallback_Impl {
    fn OnSetPresentationClock(
        &self,
        _clock: windows::core::Ref<IMFPresentationClock>,
    ) -> Result<()> {
        Ok(())
    }

    fn OnProcessSample(
        &self,
        _major_type: *const windows::core::GUID,
        _sample_flags: u32,
        sample_time: i64,
        sample_duration: i64,
        buffer: *const u8,
        length: u32,
    ) -> Result<()> {
        if !buffer.is_null() && length > 0 {
            let data = unsafe { std::slice::from_raw_parts(buffer, length as usize) };
            self.ring
                .push_audio(self.track, data, sample_time, sample_duration);
        }
        Ok(())
    }

    fn OnShutdown(&self) -> Result<()> {
        Ok(())
    }
}

impl IMFClockStateSink_Impl for AudioGrabberCallback_Impl {
    fn OnClockStart(&self, _t: i64, _o: i64) -> Result<()> { Ok(()) }
    fn OnClockStop(&self, _t: i64) -> Result<()> { Ok(()) }
    fn OnClockPause(&self, _t: i64) -> Result<()> { Ok(()) }
    fn OnClockRestart(&self, _t: i64) -> Result<()> { Ok(()) }
    fn OnClockSetRate(&self, _t: i64, _r: f32) -> Result<()> { Ok(()) }
}

/// Ask the encoder MFT for zero B-frames and low-latency mode (ADR §2).
///
/// B-frames are not only a latency cost here: they are emitted in decode order
/// with reordered presentation times, and the replay ring stores samples in
/// arrival order — a clip muxed from reordered timestamps plays back garbled.
/// The ring counts non-monotonic timestamps precisely to catch this setting
/// not taking.
///
/// Best-effort by design: ICodecAPI properties vary by vendor, and a missing
/// one is reported rather than fatal — the recording is still valid, just
/// configured by driver defaults.
fn configure_codec(writer: &IMFSinkWriter, stream: u32, gop_frames: u32) {
    unsafe {
        let mut raw: *mut core::ffi::c_void = std::ptr::null_mut();
        let got = writer.GetServiceForStream(
            stream,
            &windows::core::GUID::zeroed(),
            &ICodecAPI::IID,
            &mut raw,
        );
        if got.is_err() || raw.is_null() {
            eprintln!("note: encoder exposes no ICodecAPI; using driver defaults");
            return;
        }
        let api = ICodecAPI::from_raw(raw);
        if let Err(e) = api.SetValue(&CODECAPI_AVEncMPVDefaultBPictureCount, &variant_u32(0)) {
            eprintln!("note: could not disable B-frames ({e}); watch the ring's non-monotonic counter");
        }
        // Not all encoders expose low-latency mode; silence is fine here
        // because nothing downstream depends on it the way it does on B=0.
        let _ = api.SetValue(&CODECAPI_AVLowLatencyMode, &variant_bool(true));

        // GOP again, through ICodecAPI this time.
        //
        // MF_MT_MAX_KEYFRAME_SPACING on the media type is honoured only
        // loosely by this NVIDIA MFT — measured 0.94 s of actual spacing from a
        // 0.5 s request, and 1.34 s from a 2 s one, which is neither the
        // requested value nor proportional to it. The codec property is the
        // direct control, so it is set as well and the media-type attribute
        // kept as the fallback for encoders that only read that.
        //
        // This matters for clips, not for file size: the GOP is the worst-case
        // gap between the moment the player asked for and the moment their clip
        // can actually start.
        if let Err(e) = api.SetValue(&CODECAPI_AVEncMPVGOPSize, &variant_u32(gop_frames)) {
            eprintln!("note: encoder would not take a {gop_frames}-frame GOP ({e}); clip start precision will be whatever the driver picks");
        }
    }
}

fn variant_u32(v: u32) -> VARIANT {
    let mut var: VARIANT = unsafe { std::mem::zeroed() };
    unsafe {
        (*var.Anonymous.Anonymous).vt = VT_UI4;
        (*var.Anonymous.Anonymous).Anonymous.ulVal = v;
    }
    var
}

fn variant_bool(v: bool) -> VARIANT {
    let mut var: VARIANT = unsafe { std::mem::zeroed() };
    unsafe {
        (*var.Anonymous.Anonymous).vt = VT_BOOL;
        (*var.Anonymous.Anonymous).Anonymous.boolVal =
            VARIANT_BOOL(if v { -1 } else { 0 });
    }
    var
}

/// COM callback the sample grabber sink invokes with each compressed sample.
/// Runs on a Media Foundation worker thread — downstream of the encoder, never
/// on the capture callback — so the ring's lock-and-memcpy is safe here.
#[implement(IMFSampleGrabberSinkCallback)]
struct GrabberCallback {
    ring: Arc<ReplayRing>,
}

impl IMFSampleGrabberSinkCallback_Impl for GrabberCallback_Impl {
    fn OnSetPresentationClock(
        &self,
        _clock: windows::core::Ref<IMFPresentationClock>,
    ) -> Result<()> {
        Ok(())
    }

    fn OnProcessSample(
        &self,
        _major_type: *const windows::core::GUID,
        _sample_flags: u32,
        sample_time: i64,
        sample_duration: i64,
        buffer: *const u8,
        length: u32,
    ) -> Result<()> {
        if !buffer.is_null() && length > 0 {
            // The buffer is only valid for the duration of this call; the ring
            // copies it into a (recycled) buffer of its own.
            let data = unsafe { std::slice::from_raw_parts(buffer, length as usize) };
            self.ring.push(data, sample_time, sample_duration);
        }
        Ok(())
    }

    fn OnShutdown(&self) -> Result<()> {
        Ok(())
    }
}

impl IMFClockStateSink_Impl for GrabberCallback_Impl {
    fn OnClockStart(&self, _hnssystemtime: i64, _llclockstartoffset: i64) -> Result<()> {
        Ok(())
    }
    fn OnClockStop(&self, _hnssystemtime: i64) -> Result<()> {
        Ok(())
    }
    fn OnClockPause(&self, _hnssystemtime: i64) -> Result<()> {
        Ok(())
    }
    fn OnClockRestart(&self, _hnssystemtime: i64) -> Result<()> {
        Ok(())
    }
    fn OnClockSetRate(&self, _hnssystemtime: i64, _flrate: f32) -> Result<()> {
        Ok(())
    }
}

/// MF packs paired 32-bit values (width/height, numerator/denominator) into one
/// 64-bit attribute, high word first.
fn set_size(t: &IMFMediaType, key: &windows::core::GUID, hi: u32, lo: u32) -> Result<()> {
    unsafe { t.SetUINT64(key, ((hi as u64) << 32) | lo as u64) }
}
