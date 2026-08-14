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

use windows::core::{Interface, Result, HSTRING};
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Media::MediaFoundation::*;

use crate::d3d;

pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate: u32,
}

pub struct Encoder {
    writer: IMFSinkWriter,
    stream: u32,
    // Kept alive for the writer's lifetime: it holds a raw reference to the
    // device manager, and dropping it early would take the D3D device with it.
    _dxgi_manager: IMFDXGIDeviceManager,
    first_ts: Option<i64>,
    nominal_duration: i64,
    pub frames_written: u64,
}

impl Encoder {
    pub fn new(dev: &d3d::Device, path: &str, cfg: &EncoderConfig) -> Result<Encoder> {
        // Share our D3D11 device with Media Foundation so the encoder and the
        // colour converter run on the same device the frames already live on.
        // A second device would mean copying across devices — through RAM.
        let mut reset_token: u32 = 0;
        let mut manager: Option<IMFDXGIDeviceManager> = None;
        unsafe { MFCreateDXGIDeviceManager(&mut reset_token, &mut manager)? };
        let manager = manager.expect("MFCreateDXGIDeviceManager returned no manager");
        unsafe { manager.ResetDevice(&dev.device, reset_token)? };

        let attrs = {
            let mut a: Option<IMFAttributes> = None;
            unsafe { MFCreateAttributes(&mut a, 4)? };
            let a = a.expect("MFCreateAttributes returned nothing");
            unsafe {
                // Without this the sink writer silently picks the software
                // encoder, which is the one outcome §2 forbids.
                a.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)?;
                // Do not pace writes to wall-clock: we are recording, not
                // playing back, and throttling here would push back on the
                // capture thread.
                a.SetUINT32(&MF_SINK_WRITER_DISABLE_THROTTLING, 1)?;
                a.SetUnknown(&MF_SINK_WRITER_D3D_MANAGER, &manager)?;
            }
            a
        };

        let writer = unsafe { MFCreateSinkWriterFromURL(&HSTRING::from(path), None, &attrs)? };

        // ---- output: what lands in the file ----
        let out_type = unsafe {
            let t: IMFMediaType = MFCreateMediaType()?;
            t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            t.SetUINT32(&MF_MT_AVG_BITRATE, cfg.bitrate)?;
            t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            // High profile: better quality at the same bitrate than Main, and
            // universally supported by the hardware we care about.
            t.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)?;
            set_size(&t, &MF_MT_FRAME_SIZE, cfg.width, cfg.height)?;
            set_size(&t, &MF_MT_FRAME_RATE, cfg.fps, 1)?;
            set_size(&t, &MF_MT_PIXEL_ASPECT_RATIO, 1, 1)?;
            t
        };
        let stream = unsafe { writer.AddStream(&out_type)? };

        // ---- input: what we hand it, i.e. exactly what WGC produced ----
        let in_type = unsafe {
            let t: IMFMediaType = MFCreateMediaType()?;
            t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            // WGC hands back B8G8R8A8; MF calls that ARGB32. Declaring it here
            // rather than converting ourselves is what lets the GPU video
            // processor do the NV12 conversion on our behalf.
            t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_ARGB32)?;
            t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            set_size(&t, &MF_MT_FRAME_SIZE, cfg.width, cfg.height)?;
            set_size(&t, &MF_MT_FRAME_RATE, cfg.fps, 1)?;
            set_size(&t, &MF_MT_PIXEL_ASPECT_RATIO, 1, 1)?;
            t
        };
        unsafe { writer.SetInputMediaType(stream, &in_type, None)? };

        unsafe { writer.BeginWriting()? };

        Ok(Encoder {
            writer,
            stream,
            _dxgi_manager: manager,
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
    /// unplayable file — the moov atom never gets written.
    pub fn finish(self) -> Result<()> {
        unsafe { self.writer.Finalize()? };
        Ok(())
    }
}

/// MF packs paired 32-bit values (width/height, numerator/denominator) into one
/// 64-bit attribute, high word first.
fn set_size(t: &IMFMediaType, key: &windows::core::GUID, hi: u32, lo: u32) -> Result<()> {
    unsafe { t.SetUINT64(key, ((hi as u64) << 32) | lo as u64) }
}
