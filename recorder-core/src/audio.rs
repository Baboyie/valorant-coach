//! Desktop audio capture via WASAPI loopback.
//!
//! Loopback records what the default output device is *playing* — game audio and
//! voice comms as the player hears them. It is entirely out-of-process: we open
//! the render endpoint and read the mix, never touching the game. That keeps it
//! inside the same ADR §1 rule that chose WGC over injection, and it needs no
//! special privileges.
//!
//! **Timestamps are the whole reason this file is careful.** WASAPI reports a
//! QPC position for each packet, and WGC reports `SystemRelativeTime` for each
//! frame — both derived from QueryPerformanceCounter. Converted to the same
//! 100 ns unit they share an origin, so audio and video land on one timeline
//! without a calibration fudge. Anything that assumed a constant packet cadence
//! instead would drift exactly the way §7's muxing bug did.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{mpsc, Arc};

use windows::core::Result;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED,
};
use windows::Win32::System::Performance::QueryPerformanceFrequency;
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

/// One packet of captured audio, already converted to the encoder's format.
pub struct AudioChunk {
    /// Interleaved 16-bit PCM, `channels` samples per frame.
    pub pcm: Vec<i16>,
    /// QPC-derived, in 100 ns units — the same timebase as WGC's
    /// `SystemRelativeTime`.
    pub ts_100ns: i64,
}

/// The format we hand downstream.
///
/// Fixed at 16-bit PCM because that is what the Media Foundation AAC encoder
/// accepts, and **always stereo**: the AAC encoder takes mono or stereo only,
/// while a 5.1/7.1 output device mixes at 6 or 8 channels. Downmixing here
/// keeps every consumer simple and means the encoder never has to refuse a
/// format the user's speakers happened to be in.
#[derive(Debug, Clone, Copy)]
pub struct AudioFormat {
    pub sample_rate: u32,
    /// Channels we emit — always 2.
    pub channels: u16,
    /// What the device actually mixes at, for reporting. Anything above 2 was
    /// downmixed.
    pub device_channels: u16,
}

pub struct AudioStats {
    pub packets: AtomicU64,
    pub frames: AtomicU64,
    /// Packets WASAPI flagged as discontinuous — the capture fell behind and
    /// audio was lost. Non-zero means A/V sync is being held together by the
    /// timestamps rather than by continuity, which is exactly why we carry
    /// real timestamps instead of counting samples.
    pub discontinuities: AtomicU64,
    /// Packets the device reported as silent. Written as zeros rather than
    /// skipped: a gap in the timeline would desync everything after it.
    pub silent: AtomicU64,
}

impl Default for AudioStats {
    fn default() -> Self {
        AudioStats {
            packets: AtomicU64::new(0),
            frames: AtomicU64::new(0),
            discontinuities: AtomicU64::new(0),
            silent: AtomicU64::new(0),
        }
    }
}

pub struct AudioCapture {
    stop: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
    pub format: AudioFormat,
    pub stats: Arc<AudioStats>,
}

impl AudioCapture {
    /// Start capturing the default render endpoint's mix.
    ///
    /// Returns the receiver alongside the capture, so the consumer owns the
    /// queue and this struct owns only the thread.
    pub fn start() -> Result<(AudioCapture, Receiver<AudioChunk>)> {
        // Probe the device format on this thread so failure is reported to the
        // caller, rather than disappearing into a thread that silently exits.
        let format = probe_format()?;

        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(AudioStats::default());

        let thread_stop = Arc::clone(&stop);
        let thread_stats = Arc::clone(&stats);
        let join = std::thread::Builder::new()
            .name("audio-loopback".into())
            .spawn(move || {
                if let Err(e) = capture_loop(tx, thread_stop, thread_stats) {
                    eprintln!("audio capture stopped: {e}");
                }
            })
            .expect("failed to spawn audio thread");

        Ok((
            AudioCapture { stop, join: Some(join), format, stats },
            rx,
        ))
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl Drop for AudioCapture {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Read the mix format without starting capture.
fn probe_format() -> Result<AudioFormat> {
    // This runs on the caller's thread, which may or may not have COM up.
    // Initialising and uninitialising around the probe keeps it self-contained;
    // a nested init on an already-MTA thread is a no-op refcount bump.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let result = (|| -> Result<AudioFormat> {
        unsafe {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
            let mix = client.GetMixFormat()?;
            let fmt = AudioFormat {
                sample_rate: (*mix).nSamplesPerSec,
                channels: 2,
                device_channels: (*mix).nChannels,
            };
            CoTaskMemFree(Some(mix as *const _));
            Ok(fmt)
        }
    })();
    unsafe { CoUninitialize() };
    result
}

fn capture_loop(
    tx: Sender<AudioChunk>,
    stop: Arc<AtomicBool>,
    stats: Arc<AudioStats>,
) -> Result<()> {
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
    }
    let result = capture_loop_inner(tx, stop, stats);
    unsafe { CoUninitialize() };
    result
}

fn capture_loop_inner(
    tx: Sender<AudioChunk>,
    stop: Arc<AtomicBool>,
    stats: Arc<AudioStats>,
) -> Result<()> {
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        // eRender, not eCapture: loopback taps the *output* device.
        let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;

        let mix = client.GetMixFormat()?;
        let sample_rate = (*mix).nSamplesPerSec;
        let channels = (*mix).nChannels;
        let bits = (*mix).wBitsPerSample;
        let is_float = mix_is_float(mix);

        // Loopback is shared-mode only and the format is not negotiable — it
        // must be the device mix format exactly. Conversion happens on our side.
        //
        // 200 ms buffer: large enough that a scheduling hiccup on this thread
        // does not drop audio, small enough to be irrelevant against the replay
        // ring's own memory.
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
            2_000_000,
            0,
            mix,
            None,
        )?;

        let event: HANDLE = CreateEventW(None, false, false, None)?;
        client.SetEventHandle(event)?;
        let capture: IAudioCaptureClient = client.GetService()?;
        client.Start()?;

        // QPC ticks -> 100 ns units. WASAPI documents pu64QPCPosition as already
        // being in 100 ns units, but the frequency is read anyway as the
        // fallback path when a device reports 0 (some do) and we have to derive
        // a timestamp from the raw counter.
        let mut qpf: i64 = 0;
        let _ = QueryPerformanceFrequency(&mut qpf);

        let mut pcm: Vec<i16> = Vec::new();

        while !stop.load(Ordering::Relaxed) {
            // 200 ms timeout rather than INFINITE: a device that stops
            // signalling must not wedge the thread past a stop request.
            if WaitForSingleObject(event, 200) != WAIT_OBJECT_0 {
                continue;
            }

            loop {
                let avail = capture.GetNextPacketSize()?;
                if avail == 0 {
                    break;
                }

                let mut data: *mut u8 = std::ptr::null_mut();
                let mut frames: u32 = 0;
                let mut flags: u32 = 0;
                let mut qpc: u64 = 0;
                capture.GetBuffer(
                    &mut data,
                    &mut frames,
                    &mut flags,
                    None,
                    Some(&mut qpc),
                )?;

                if frames > 0 {
                    let src_n = frames as usize * channels as usize;
                    // We always emit stereo — see AudioFormat.
                    let out_n = frames as usize * 2;
                    pcm.clear();
                    pcm.reserve(out_n);

                    let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
                    if silent {
                        // Zeros, not a skip: dropping silent packets would
                        // shorten the audio timeline and desync everything
                        // after the first quiet moment.
                        stats.silent.fetch_add(1, Ordering::Relaxed);
                        pcm.resize(out_n, 0);
                    } else if is_float {
                        let src = std::slice::from_raw_parts(data as *const f32, src_n);
                        for f in 0..frames as usize {
                            let (l, r) = downmix_f32(src, f, channels as usize);
                            pcm.push(to_i16(l));
                            pcm.push(to_i16(r));
                        }
                    } else if bits == 16 {
                        let src = std::slice::from_raw_parts(data as *const i16, src_n);
                        for f in 0..frames as usize {
                            let base = f * channels as usize;
                            if channels >= 2 {
                                pcm.push(src[base]);
                                pcm.push(src[base + 1]);
                            } else {
                                pcm.push(src[base]);
                                pcm.push(src[base]);
                            }
                        }
                    } else {
                        // Unexpected mix format; emit silence of the right
                        // length so the timeline stays intact and the problem
                        // shows up as silence rather than as drift.
                        pcm.resize(out_n, 0);
                    }

                    if flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0 {
                        stats.discontinuities.fetch_add(1, Ordering::Relaxed);
                    }

                    let ts = if qpc > 0 {
                        qpc as i64
                    } else {
                        // Device did not report a position. Fall back to reading
                        // the counter now — slightly late, but a real timestamp
                        // beats an assumed cadence.
                        now_100ns(qpf)
                    };

                    stats.packets.fetch_add(1, Ordering::Relaxed);
                    stats.frames.fetch_add(frames as u64, Ordering::Relaxed);

                    // Send takes the buffer; a fresh one is allocated next
                    // round. Audio packets are ~10 ms and a few KB, so this is
                    // orders of magnitude below the video path's cost — not
                    // worth a pool, unlike the replay ring.
                    let chunk = AudioChunk {
                        pcm: std::mem::take(&mut pcm),
                        ts_100ns: ts,
                    };
                    if tx.send(chunk).is_err() {
                        // Consumer went away.
                        capture.ReleaseBuffer(frames)?;
                        client.Stop()?;
                        let _ = CloseHandle(event);
                        CoTaskMemFree(Some(mix as *const _));
                        return Ok(());
                    }
                }

                capture.ReleaseBuffer(frames)?;
            }
        }

        client.Stop()?;
        let _ = CloseHandle(event);
        CoTaskMemFree(Some(mix as *const _));
        let _ = sample_rate;
        Ok(())
    }
}

/// Clamp before scaling: a shared-mode mix can exceed ±1.0 when several loud
/// sources sum, and letting that wrap would be an audible click rather than a
/// clip.
fn to_i16(v: f32) -> i16 {
    (v.clamp(-1.0, 1.0) * 32767.0) as i16
}

/// Fold one frame down to stereo.
///
/// Standard ITU-style coefficients for the 5.1 case: centre and LFE go to both
/// sides at -3 dB, surrounds follow their own side. Anything beyond 6 channels
/// falls back to taking the front pair, which is wrong in principle but
/// inaudible in the cases that actually reach it — and far better than refusing
/// to record because someone has a 7.1 headset.
fn downmix_f32(src: &[f32], frame: usize, channels: usize) -> (f32, f32) {
    let b = frame * channels;
    match channels {
        0 => (0.0, 0.0),
        1 => (src[b], src[b]),
        2 => (src[b], src[b + 1]),
        // L R C LFE Ls Rs
        6 => {
            const K: f32 = 0.707;
            let (l, r, c, lfe, ls, rs) = (
                src[b], src[b + 1], src[b + 2], src[b + 3], src[b + 4], src[b + 5],
            );
            (l + K * (c + lfe + ls), r + K * (c + lfe + rs))
        }
        _ => (src[b], src[b + 1]),
    }
}

fn now_100ns(qpf: i64) -> i64 {
    use windows::Win32::System::Performance::QueryPerformanceCounter;
    let mut c: i64 = 0;
    unsafe {
        let _ = QueryPerformanceCounter(&mut c);
    }
    if qpf > 0 {
        ((c as i128 * 10_000_000) / qpf as i128) as i64
    } else {
        0
    }
}

/// Whether the mix format is IEEE float. Shared-mode mixes are float32 in
/// practice, but the extensible header has to be inspected to know.
unsafe fn mix_is_float(mix: *const WAVEFORMATEX) -> bool {
    const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
    const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
    // Declared here rather than pulling in the whole Win32_Media_Multimedia
    // feature for a single well-known GUID.
    const SUBTYPE_IEEE_FLOAT: windows::core::GUID =
        windows::core::GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);
    unsafe {
        match (*mix).wFormatTag {
            WAVE_FORMAT_IEEE_FLOAT => true,
            WAVE_FORMAT_EXTENSIBLE => {
                // WAVEFORMATEXTENSIBLE is packed, so the field cannot be
                // borrowed — read it out unaligned and compare the copy.
                let ext = mix as *const WAVEFORMATEXTENSIBLE;
                let sub = std::ptr::read_unaligned(std::ptr::addr_of!((*ext).SubFormat));
                sub == SUBTYPE_IEEE_FLOAT
            }
            _ => false,
        }
    }
}
