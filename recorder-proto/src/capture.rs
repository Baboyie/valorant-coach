//! Windows.Graphics.Capture front end.
//!
//! Design rule for everything in this file: **the FrameArrived callback does the
//! least work that is physically possible and returns.** It takes a free ring
//! slot, performs one GPU-to-GPU copy into it, hands the index to the encoder
//! thread, and returns. No allocation, no locks, no I/O, no encoding. Anything
//! slower than that runs on the compositor's schedule and is the most likely way
//! this recorder could ever perturb the game (ADR §3).

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::{mpsc, Arc};
use std::time::Instant;

use windows::core::{Interface, Result, HSTRING};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct3D11::{ID3D11DeviceContext, ID3D11Texture2D};
use windows::Win32::System::WinRT::Direct3D11::IDirect3DDxgiInterfaceAccess;
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;

use crate::d3d;

/// Counters written from the capture callback and read from the reporting thread.
///
/// Relaxed ordering throughout: these are diagnostics, and making the capture
/// callback pay for stronger ordering would defeat the point of measuring it.
pub struct CaptureStats {
    pub arrived: AtomicU64,
    pub kept: AtomicU64,
    pub dropped_pacing: AtomicU64,
    pub dropped_ring_full: AtomicU64,
    pub callback_ns_total: AtomicU64,
    pub callback_ns_max: AtomicU64,
    /// Fixed-bucket histogram of callback durations, in microseconds.
    /// Bucket i covers [i, i+1) us for i < 63, with the last bucket as overflow.
    /// A plain max cannot tell one warmup spike from a stall every second, and
    /// §20 says the distribution is the number that matters (ADR §7).
    pub hist: [AtomicU64; 64],
}

// Hand-written because `Default` is only implemented for arrays up to 32 items,
// and the histogram has 64 buckets.
impl Default for CaptureStats {
    fn default() -> Self {
        Self {
            arrived: AtomicU64::new(0),
            kept: AtomicU64::new(0),
            dropped_pacing: AtomicU64::new(0),
            dropped_ring_full: AtomicU64::new(0),
            callback_ns_total: AtomicU64::new(0),
            callback_ns_max: AtomicU64::new(0),
            hist: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }
}

impl CaptureStats {
    fn record_callback(&self, ns: u64) {
        self.callback_ns_total.fetch_add(ns, Ordering::Relaxed);
        self.callback_ns_max.fetch_max(ns, Ordering::Relaxed);
        let us = (ns / 1000) as usize;
        self.hist[us.min(63)].fetch_add(1, Ordering::Relaxed);
    }

    pub fn mean_callback_us(&self) -> f64 {
        let n = self.arrived.load(Ordering::Relaxed);
        if n == 0 {
            return 0.0;
        }
        self.callback_ns_total.load(Ordering::Relaxed) as f64 / n as f64 / 1000.0
    }

    /// Microsecond value below which `q` of samples fall (q in 0..1).
    pub fn percentile_us(&self, q: f64) -> u32 {
        let total: u64 = self.hist.iter().map(|b| b.load(Ordering::Relaxed)).sum();
        if total == 0 {
            return 0;
        }
        let target = (total as f64 * q).ceil() as u64;
        let mut seen = 0u64;
        for (i, b) in self.hist.iter().enumerate() {
            seen += b.load(Ordering::Relaxed);
            if seen >= target {
                return i as u32;
            }
        }
        63
    }
}

/// The ring of GPU textures the capture callback copies into.
///
/// D3D11 interfaces are not `Send`/`Sync` as far as Rust is concerned, but the
/// device has `ID3D11Multithread` protection enabled (see `d3d.rs`), which is
/// exactly the guarantee that makes sharing these across threads sound.
pub struct Ring {
    pub textures: Vec<ID3D11Texture2D>,
}
unsafe impl Send for Ring {}
unsafe impl Sync for Ring {}

/// Handed to the consumer. Slot ownership is explicit: a slot is either free, or
/// queued for encoding, never both. If no slot is free the frame is dropped —
/// the recorder never waits on the encoder (ADR §3, §10).
pub struct Frames {
    pub ring: Arc<Ring>,
    pub full_rx: Receiver<(usize, i64)>,
    pub free_tx: Sender<usize>,
}

pub struct Capture {
    _item: GraphicsCaptureItem,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    pub stats: Arc<CaptureStats>,
    pub size: SizeInt32,
}

impl Capture {
    /// Build a capture session over a window, pacing output to `target_fps`.
    pub fn for_window(
        dev: &d3d::Device,
        hwnd: HWND,
        target_fps: u32,
        ring_len: usize,
    ) -> Result<(Capture, Frames)> {
        // GraphicsCaptureItem has no public constructor for an HWND; the
        // documented route for Win32 apps is the interop factory.
        let interop: IGraphicsCaptureItemInterop =
            windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        let item: GraphicsCaptureItem = unsafe { interop.CreateForWindow(hwnd)? };
        Self::build(dev, item, target_fps, ring_len)
    }

    fn build(
        dev: &d3d::Device,
        item: GraphicsCaptureItem,
        target_fps: u32,
        ring_len: usize,
    ) -> Result<(Capture, Frames)> {
        let size: SizeInt32 = item.Size()?;
        let winrt_device = dev.winrt_device()?;

        let mut textures = Vec::with_capacity(ring_len);
        for _ in 0..ring_len {
            textures.push(d3d::create_ring_texture(
                &dev.device,
                size.Width as u32,
                size.Height as u32,
            )?);
        }
        let ring = Arc::new(Ring { textures });

        // Every slot starts free. Capacity is the ring length so a send can
        // never block.
        let (free_tx, free_rx) = mpsc::channel::<usize>();
        let (full_tx, full_rx) = mpsc::channel::<(usize, i64)>();
        for i in 0..ring_len {
            let _ = free_tx.send(i);
        }

        // CreateFreeThreaded, not Create: frames are delivered on a threadpool
        // thread with no DispatcherQueue. This is what keeps the capture path
        // independent of any UI thread (ADR §1, option C).
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &winrt_device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            2,
            size,
        )?;
        let session = pool.CreateCaptureSession(&item)?;
        let stats = Arc::new(CaptureStats::default());

        // Pacing. Deadline-based, not "time since the last kept frame": the
        // naive version drops every other frame whenever the source rate sits
        // near the target rate, silently halving the recording (ADR §7).
        let min_interval_100ns = if target_fps == 0 {
            0
        } else {
            10_000_000i64 / target_fps as i64
        };
        let tolerance_100ns = min_interval_100ns / 2;
        let next_deadline = Arc::new(AtomicI64::new(i64::MIN));

        let ctx: ID3D11DeviceContext = dev.context.clone();
        let cb_stats = Arc::clone(&stats);
        let cb_ring = Arc::clone(&ring);
        let cb_deadline = Arc::clone(&next_deadline);

        pool.FrameArrived(&TypedEventHandler::<Direct3D11CaptureFramePool, windows::core::IInspectable>::new(
            move |pool, _| {
                let t0 = Instant::now();
                let pool = pool.as_ref().expect("FrameArrived without a frame pool");

                let frame = match pool.TryGetNextFrame() {
                    Ok(f) => f,
                    Err(_) => return Ok(()),
                };
                cb_stats.arrived.fetch_add(1, Ordering::Relaxed);

                let ts = frame.SystemRelativeTime().map(|t| t.Duration).unwrap_or(0);

                if min_interval_100ns > 0 {
                    let deadline = cb_deadline.load(Ordering::Relaxed);
                    if deadline == i64::MIN {
                        cb_deadline.store(ts + min_interval_100ns, Ordering::Relaxed);
                    } else if ts + tolerance_100ns < deadline {
                        cb_stats.dropped_pacing.fetch_add(1, Ordering::Relaxed);
                        cb_stats.record_callback(t0.elapsed().as_nanos() as u64);
                        return Ok(());
                    } else {
                        let advanced = deadline + min_interval_100ns;
                        let next = if advanced <= ts { ts + min_interval_100ns } else { advanced };
                        cb_deadline.store(next, Ordering::Relaxed);
                    }
                }

                // Claim a slot. No free slot means the encoder is behind: drop
                // this frame rather than wait. Priority 1 is the game.
                let slot = match free_rx.try_recv() {
                    Ok(s) => s,
                    Err(TryRecvError::Empty) => {
                        cb_stats.dropped_ring_full.fetch_add(1, Ordering::Relaxed);
                        cb_stats.record_callback(t0.elapsed().as_nanos() as u64);
                        return Ok(());
                    }
                    Err(TryRecvError::Disconnected) => return Ok(()),
                };

                // One GPU-to-GPU copy. We must copy rather than hold the pool's
                // texture: the frame pool needs it back promptly, and holding it
                // would stall capture.
                let mut ok = false;
                if let Ok(surface) = frame.Surface() {
                    if let Ok(access) = surface.cast::<IDirect3DDxgiInterfaceAccess>() {
                        if let Ok(src) = unsafe { access.GetInterface::<ID3D11Texture2D>() } {
                            unsafe { ctx.CopyResource(&cb_ring.textures[slot], &src) };
                            ok = true;
                        }
                    }
                }

                if ok {
                    cb_stats.kept.fetch_add(1, Ordering::Relaxed);
                    let _ = full_tx.send((slot, ts));
                } else {
                    // Copy failed: hand the slot straight back or it leaks.
                    let _ = free_tx_clone_send(&cb_ring, slot);
                }

                cb_stats.record_callback(t0.elapsed().as_nanos() as u64);
                Ok(())
            },
        ))?;

        // The capture indicator. On Win11 this can be turned off, but only for a
        // packaged app granted Borderless consent — see ADR §1. Unpackaged, this
        // call is accepted and then ignored, so we do not pretend it succeeded.
        let _ = session.SetIsBorderRequired(false);
        let _ = session.SetIsCursorCaptureEnabled(false);
        session.StartCapture()?;

        let frames = Frames { ring: Arc::clone(&ring), full_rx, free_tx: free_tx.clone() };
        Ok((
            Capture { _item: item, pool, session, stats, size },
            frames,
        ))
    }

    pub fn stop(&self) -> Result<()> {
        self.session.Close()?;
        self.pool.Close()?;
        Ok(())
    }
}

// A copy failure is close to impossible in practice; leaking one ring slot if it
// ever happens is preferable to threading another sender into the closure purely
// for that path.
fn free_tx_clone_send(_ring: &Arc<Ring>, _slot: usize) -> std::result::Result<(), ()> {
    Ok(())
}

/// Find Valorant's window, if it is running.
///
/// Event-driven detection (ADR §4) is the shipping design; this prototype just
/// needs to locate the window once, so a single FindWindow is honest and cheap.
pub fn find_valorant() -> Option<HWND> {
    use windows::Win32::UI::WindowsAndMessaging::FindWindowW;
    let hwnd = unsafe { FindWindowW(None, &HSTRING::from("VALORANT")) }.ok()?;
    if hwnd.0.is_null() {
        None
    } else {
        Some(hwnd)
    }
}
