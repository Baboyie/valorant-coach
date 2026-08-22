//! Mixing two capture sources into one track.
//!
//! **Separate tracks are still the default** (§23), and for review they are the
//! better answer: a reviewer can mute either side. This exists because of where
//! the footage goes — YouTube keeps only the *first* audio track, so an
//! uploaded POV loses the player's voice entirely. Mixing is the only way both
//! survive the upload.
//!
//! §23 warns that mixing is the heavier option, and it is right about why: the
//! microphone commonly runs at 44.1 kHz while the output device mixes at
//! 48 kHz, so combining them needs resampling. That cost is paid here, on a
//! thread of its own, and never on the capture threads or the engine thread —
//! the capture path stays exactly as measured.
//!
//! The master clock is the desktop stream. Its packets define the output
//! timeline; the microphone is resampled onto it by timestamp, never by
//! counting samples. Both sources carry QPC-derived timestamps in the same
//! 100 ns unit, which is what makes alignment possible at all — counting
//! samples would drift the moment either device glitched.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::audio::{AudioChunk, AudioFormat};

/// How long a master packet waits for the microphone to catch up.
///
/// The two capture threads are independent, so the mic packet covering a given
/// instant may not have arrived when the desktop packet for it does. Holding
/// briefly lets it. This delays *encoding*, never the game, and never playback:
/// timestamps are preserved, so the only visible cost is that the last ~120 ms
/// of microphone audio may be missing from a clip saved this instant — against
/// a 30-second window, invisible.
const HOLD: Duration = Duration::from_millis(120);

/// Discard mic packets older than this behind the mix point, so a stalled
/// master stream cannot grow the buffer without bound.
const MIC_BACKLOG: usize = 512;

pub struct AudioMixer {
    stop: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
    /// Gain for the *second* source, live. The master's gain is applied at its
    /// own capture; this one is applied here because the mic is resampled
    /// here anyway.
    other_gain: Arc<AtomicU32>,
    pub format: AudioFormat,
}

impl AudioMixer {
    /// Start mixing `other` into `master`, emitting on the master's timeline.
    ///
    /// The output format is the master's: same sample rate, stereo, 16-bit.
    /// Nothing downstream can tell a mixed track from a captured one.
    pub fn start(
        master: (Receiver<AudioChunk>, AudioFormat),
        other: (Receiver<AudioChunk>, AudioFormat),
        other_gain: f32,
    ) -> (AudioMixer, Receiver<AudioChunk>) {
        let (master_rx, master_fmt) = master;
        let (other_rx, other_fmt) = other;
        let (tx, rx) = mpsc::channel();

        let stop = Arc::new(AtomicBool::new(false));
        let gain = Arc::new(AtomicU32::new(other_gain.to_bits()));

        let thread_stop = Arc::clone(&stop);
        let thread_gain = Arc::clone(&gain);
        let join = std::thread::Builder::new()
            .name("audio-mix".into())
            .spawn(move || {
                mix_loop(
                    master_rx,
                    master_fmt,
                    other_rx,
                    other_fmt,
                    tx,
                    thread_stop,
                    thread_gain,
                );
            })
            .expect("failed to spawn mixer thread");

        (
            AudioMixer {
                stop,
                join: Some(join),
                other_gain: gain,
                format: master_fmt,
            },
            rx,
        )
    }

    pub fn set_other_gain(&self, g: f32) {
        self.other_gain
            .store(g.clamp(0.0, 4.0).to_bits(), Ordering::Relaxed);
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl Drop for AudioMixer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// A mic packet, kept until the mix point passes it.
struct Held {
    pcm: Vec<i16>,
    ts: i64,
    /// Exclusive end, in the same 100 ns unit.
    end: i64,
}

fn duration_100ns(frames: usize, rate: u32) -> i64 {
    if rate == 0 {
        return 0;
    }
    (frames as i64 * 10_000_000) / rate as i64
}

fn mix_loop(
    master_rx: Receiver<AudioChunk>,
    master_fmt: AudioFormat,
    other_rx: Receiver<AudioChunk>,
    other_fmt: AudioFormat,
    tx: Sender<AudioChunk>,
    stop: Arc<AtomicBool>,
    other_gain: Arc<AtomicU32>,
) {
    let mut pending: VecDeque<(Instant, AudioChunk)> = VecDeque::new();
    let mut held: VecDeque<Held> = VecDeque::new();
    let mut master_open = true;
    let mut other_open = true;

    while !stop.load(Ordering::Relaxed) {
        // Take everything waiting on both sides before deciding anything.
        loop {
            match master_rx.try_recv() {
                Ok(c) => pending.push_back((Instant::now(), c)),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    master_open = false;
                    break;
                }
            }
        }
        loop {
            match other_rx.try_recv() {
                Ok(c) => {
                    let frames = c.pcm.len() / 2;
                    let end = c.ts_100ns + duration_100ns(frames, other_fmt.sample_rate);
                    held.push_back(Held { pcm: c.pcm, ts: c.ts_100ns, end });
                    while held.len() > MIC_BACKLOG {
                        held.pop_front();
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    other_open = false;
                    break;
                }
            }
        }

        // Emit every master packet the mic has caught up with — or that has
        // waited long enough that it never will.
        let g = f32::from_bits(other_gain.load(Ordering::Relaxed));
        let mic_covers_to = held.back().map(|h| h.end).unwrap_or(i64::MIN);
        let mut emitted_any = false;

        while let Some((arrived, chunk)) = pending.front() {
            let frames = chunk.pcm.len() / 2;
            let win_end = chunk.ts_100ns + duration_100ns(frames, master_fmt.sample_rate);
            let ready = mic_covers_to >= win_end || arrived.elapsed() >= HOLD || !other_open;
            if !ready {
                break;
            }
            let (_, chunk) = pending.pop_front().expect("front checked");
            let mixed = mix_one(&chunk, master_fmt, &held, other_fmt, g);
            if tx.send(mixed).is_err() {
                return; // consumer gone
            }
            emitted_any = true;

            // Drop mic packets entirely behind the point just consumed.
            while held.len() > 1 && held.front().map(|h| h.end).unwrap_or(0) < chunk.ts_100ns {
                held.pop_front();
            }
        }

        if !master_open && pending.is_empty() {
            return;
        }
        if !emitted_any {
            // Nothing to do. Sleeping beats spinning — this thread must never
            // compete with the game for a core (§2).
            std::thread::sleep(Duration::from_millis(4));
        }
    }
}

/// Mix one master packet with whatever microphone audio covers its window.
fn mix_one(
    master: &AudioChunk,
    master_fmt: AudioFormat,
    held: &VecDeque<Held>,
    other_fmt: AudioFormat,
    gain: f32,
) -> AudioChunk {
    let frames = master.pcm.len() / 2;
    let mut out = Vec::with_capacity(master.pcm.len());
    let step = if master_fmt.sample_rate > 0 {
        10_000_000.0 / master_fmt.sample_rate as f64
    } else {
        0.0
    };

    for f in 0..frames {
        let t = master.ts_100ns + (f as f64 * step) as i64;
        let (ml, mr) = (master.pcm[f * 2], master.pcm[f * 2 + 1]);
        let (ol, or) = sample_at(held, other_fmt, t);
        // Sum and saturate. Two full-scale sources cannot both fit, and
        // clipping is the honest outcome — the gain sliders are what a user
        // reaches for, and halving both to guarantee headroom would make a
        // quiet mic inaudible to protect against a case that rarely happens.
        out.push(sat(ml as i32 + (ol as f32 * gain) as i32));
        out.push(sat(mr as i32 + (or as f32 * gain) as i32));
    }

    AudioChunk {
        pcm: out,
        ts_100ns: master.ts_100ns,
    }
}

fn sat(v: i32) -> i16 {
    v.clamp(i16::MIN as i32, i16::MAX as i32) as i16
}

/// The microphone's value at an instant, linearly interpolated.
///
/// Linear interpolation rather than a windowed resampler: the input is speech
/// at 44.1 or 48 kHz being nudged onto a neighbouring rate, where the audible
/// difference is nil and the cost difference is not. Returns silence for an
/// instant no packet covers, which is what a gap in microphone capture
/// actually sounds like.
fn sample_at(held: &VecDeque<Held>, fmt: AudioFormat, t: i64) -> (i16, i16) {
    let Some(h) = held.iter().find(|h| t >= h.ts && t < h.end) else {
        return (0, 0);
    };
    let frames = h.pcm.len() / 2;
    if frames == 0 || fmt.sample_rate == 0 {
        return (0, 0);
    }
    let pos = (t - h.ts) as f64 * fmt.sample_rate as f64 / 10_000_000.0;
    let i = pos.floor() as usize;
    if i >= frames {
        return (0, 0);
    }
    let frac = (pos - i as f64) as f32;
    let j = (i + 1).min(frames - 1);
    let lerp = |a: i16, b: i16| (a as f32 + (b as f32 - a as f32) * frac) as i16;
    (
        lerp(h.pcm[i * 2], h.pcm[j * 2]),
        lerp(h.pcm[i * 2 + 1], h.pcm[j * 2 + 1]),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The 100 ns grid does not divide evenly by any real sample rate —
    /// 10_000_000 / 48_000 is 208.33, truncated to 208 — so a sample read one
    /// nominal frame later lands a fraction short and interpolation returns a
    /// value a hair off. That is correct behaviour, not drift: the error is
    /// bounded at one frame and never accumulates, because every lookup is
    /// absolute rather than counted from the last.
    fn near(got: (i16, i16), want: (i16, i16), tol: i16) {
        assert!(
            (got.0 - want.0).abs() <= tol && (got.1 - want.1).abs() <= tol,
            "got {got:?}, wanted {want:?} within {tol}"
        );
    }

    fn fmt(rate: u32) -> AudioFormat {
        AudioFormat { sample_rate: rate, channels: 2, device_channels: 2 }
    }

    fn held_of(ts: i64, rate: u32, frames: &[(i16, i16)]) -> VecDeque<Held> {
        let mut pcm = Vec::new();
        for (l, r) in frames {
            pcm.push(*l);
            pcm.push(*r);
        }
        let mut d = VecDeque::new();
        d.push_back(Held { pcm, ts, end: ts + duration_100ns(frames.len(), rate) });
        d
    }

    #[test]
    fn durations_are_computed_from_the_rate_not_assumed() {
        assert_eq!(duration_100ns(48_000, 48_000), 10_000_000); // one second
        assert_eq!(duration_100ns(480, 48_000), 100_000); // 10 ms
        assert_eq!(duration_100ns(100, 0), 0, "a zero rate must not divide by zero");
    }

    #[test]
    fn an_instant_no_packet_covers_is_silence() {
        let held = held_of(1000, 48_000, &[(100, 200)]);
        assert_eq!(sample_at(&held, fmt(48_000), 0), (0, 0), "before");
        assert_eq!(sample_at(&held, fmt(48_000), 9_999_999), (0, 0), "after");
        assert_eq!(sample_at(&VecDeque::new(), fmt(48_000), 1000), (0, 0), "no packets at all");
    }

    #[test]
    fn a_sample_is_read_at_its_own_timestamp() {
        // Three frames at 48 kHz starting at t=0: each is 208.33 (100 ns) long.
        let held = held_of(0, 48_000, &[(1000, -1000), (2000, -2000), (3000, -3000)]);
        assert_eq!(sample_at(&held, fmt(48_000), 0), (1000, -1000), "frame 0 is exact");
        let one_frame = duration_100ns(1, 48_000);
        near(sample_at(&held, fmt(48_000), one_frame), (2000, -2000), 4);
        near(sample_at(&held, fmt(48_000), one_frame * 2), (3000, -3000), 8);
    }

    #[test]
    fn between_two_samples_the_value_is_between_them() {
        let held = held_of(0, 48_000, &[(0, 0), (1000, 2000)]);
        let half = duration_100ns(1, 48_000) / 2;
        let (l, r) = sample_at(&held, fmt(48_000), half);
        assert!((400..=600).contains(&l), "l = {l}");
        assert!((900..=1100).contains(&r), "r = {r}");
    }

    #[test]
    fn a_different_mic_rate_still_lands_on_the_right_instant() {
        // 44.1 kHz mic, 48 kHz master — the case §23 warns about. Sample 0
        // holds 5000; one mic frame later holds -5000. Reading at those
        // instants must give those values despite neither being a master
        // frame boundary.
        let held = held_of(0, 44_100, &[(5000, 5000), (-5000, -5000)]);
        assert_eq!(sample_at(&held, fmt(44_100), 0), (5000, 5000), "frame 0 is exact");
        let one = duration_100ns(1, 44_100);
        // A full-scale swing between adjacent frames magnifies the sub-frame
        // offset; the tolerance is that offset, not slop.
        near(sample_at(&held, fmt(44_100), one), (-5000, -5000), 40);
    }

    #[test]
    fn mixing_sums_and_saturates_rather_than_wrapping() {
        let master = AudioChunk { pcm: vec![30_000, -30_000], ts_100ns: 0 };
        let held = held_of(0, 48_000, &[(30_000, -30_000)]);
        let out = mix_one(&master, fmt(48_000), &held, fmt(48_000), 1.0);
        assert_eq!(out.pcm, vec![i16::MAX, i16::MIN], "must clip, not wrap around");
        assert_eq!(out.ts_100ns, 0, "the master's timestamp is the output's");
    }

    #[test]
    fn zero_gain_on_the_mic_leaves_the_master_untouched() {
        let master = AudioChunk { pcm: vec![1234, -4321], ts_100ns: 77 };
        let held = held_of(77, 48_000, &[(9999, 9999)]);
        let out = mix_one(&master, fmt(48_000), &held, fmt(48_000), 0.0);
        assert_eq!(out.pcm, vec![1234, -4321]);
    }

    #[test]
    fn with_no_mic_audio_the_master_passes_through_whole() {
        let master = AudioChunk { pcm: vec![1, 2, 3, 4, 5, 6], ts_100ns: 500 };
        let out = mix_one(&master, fmt(48_000), &VecDeque::new(), fmt(48_000), 1.0);
        assert_eq!(out.pcm, master.pcm, "silence must not shorten the timeline");
    }
}
