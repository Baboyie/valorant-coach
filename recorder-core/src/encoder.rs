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
}

/// The H.264 type the *encoder* is configured with. The replay muxer builds
/// its own, deliberately more minimal type: GOP spacing and profile are
/// instructions to an encoder, and a passthrough mux has no encoder to
/// instruct.
fn h264_output_type(cfg: &EncoderConfig) -> Result<IMFMediaType> {
    unsafe {
        let t: IMFMediaType = MFCreateMediaType()?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
        t.SetUINT32(&MF_MT_AVG_BITRATE, cfg.bitrate)?;
        t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        // High profile: better quality at the same bitrate than Main, and
        // universally supported by the hardware we care about.
        t.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)?;
        // Two-second GOP (ADR §2). Left to itself a driver may pick a much
        // longer one, and GOP length is not cosmetic here: a replay clip must
        // start on a keyframe, so the GOP bounds both how much extra the ring
        // retains and how far a clip's start can land from where the user
        // asked.
        t.SetUINT32(&MF_MT_MAX_KEYFRAME_SPACING, cfg.fps.max(1) * 2)?;
        set_size(&t, &MF_MT_FRAME_SIZE, cfg.width, cfg.height)?;
        set_size(&t, &MF_MT_FRAME_RATE, cfg.fps, 1)?;
        set_size(&t, &MF_MT_PIXEL_ASPECT_RATIO, 1, 1)?;
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
fn writer_attrs(dev: &d3d::Device) -> Result<(IMFDXGIDeviceManager, IMFAttributes)> {
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
    first_ts: Option<i64>,
    nominal_duration: i64,
    pub frames_written: u64,
}

impl Encoder {
    /// Encode straight into an MP4 file (the `record` command).
    pub fn to_file(dev: &d3d::Device, path: &str, cfg: &EncoderConfig) -> Result<Encoder> {
        let (manager, attrs) = writer_attrs(dev)?;
        let writer = unsafe { MFCreateSinkWriterFromURL(&HSTRING::from(path), None, &attrs)? };
        let stream = unsafe { writer.AddStream(&h264_output_type(cfg)?)? };
        unsafe { writer.SetInputMediaType(stream, &argb_input_type(cfg)?, None)? };
        configure_codec(&writer, stream);
        unsafe { writer.BeginWriting()? };

        Ok(Encoder {
            writer,
            stream,
            _dxgi_manager: manager,
            media_sink: None,
            first_ts: None,
            nominal_duration: if cfg.fps > 0 { 10_000_000 / cfg.fps as i64 } else { 166_666 },
            frames_written: 0,
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
        configure_codec(&writer, stream);
        unsafe { writer.BeginWriting()? };

        Ok(Encoder {
            writer,
            stream,
            _dxgi_manager: manager,
            media_sink: Some(sink),
            first_ts: None,
            nominal_duration: if cfg.fps > 0 { 10_000_000 / cfg.fps as i64 } else { 166_666 },
            frames_written: 0,
        })
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
fn configure_codec(writer: &IMFSinkWriter, stream: u32) {
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
