//! Short audible confirmations.
//!
//! **This exists because a toast cannot reach you in game.** Windows turns on
//! Do Not Disturb by itself while an app is fullscreen, so the notification
//! confirming a saved clip is suppressed at exactly the moment it matters —
//! mid-round, right after the hotkey. An overlay would work and is the one
//! thing this project will not do: drawing into the game means hooking it,
//! which is what risks a Vanguard ban (ADR §1). Sound is what is left.
//!
//! **The honest cost:** desktop-audio capture records what the speakers play,
//! so this cue lands in the audio of whatever is recorded *next*. The ring
//! holds the preceding seconds, so the clip you just saved is unaffected —
//! but a second clip saved within the replay window will contain the first
//! one's chime. That is why it is a setting, and why the tone is short and
//! quiet rather than a fanfare.
//!
//! Synthesised rather than shipped as an asset: it is a few hundred samples,
//! and generating them avoids a binary file in the repo whose provenance and
//! licence someone would eventually have to answer for.

use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_MEMORY, SND_NODEFAULT};

const SAMPLE_RATE: u32 = 44_100;

/// A rising pair of tones: something was kept.
pub fn saved() {
    play(&chime(&[(880.0, 55), (1318.5, 90)], 0.16));
}

/// A single low tone: a recording started or stopped.
pub fn marker() {
    play(&chime(&[(587.3, 70)], 0.14));
}

/// A falling pair: something went wrong and there is nothing to review.
pub fn failed() {
    play(&chime(&[(440.0, 70), (329.6, 110)], 0.18));
}

/// Render tones to a 16-bit mono WAV in memory.
///
/// Each tone gets a raised-cosine envelope. Without one, starting and stopping
/// a sine at a non-zero sample is a step change — audible as a click on either
/// end, which sounds like a fault rather than a confirmation.
fn chime(tones: &[(f32, u32)], amplitude: f32) -> Vec<u8> {
    let mut pcm: Vec<i16> = Vec::new();
    for &(freq, ms) in tones {
        let n = (SAMPLE_RATE as u64 * ms as u64 / 1000) as usize;
        for i in 0..n {
            let t = i as f32 / SAMPLE_RATE as f32;
            let phase = (i as f32 / n as f32) * std::f32::consts::PI; // 0..pi
            let envelope = phase.sin(); // fades in and back out
            let v = (t * freq * std::f32::consts::TAU).sin() * envelope * amplitude;
            pcm.push((v.clamp(-1.0, 1.0) * i16::MAX as f32) as i16);
        }
    }
    wav(&pcm)
}

fn wav(pcm: &[i16]) -> Vec<u8> {
    let data_len = (pcm.len() * 2) as u32;
    let mut b = Vec::with_capacity(44 + data_len as usize);
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data_len).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes()); // PCM fmt chunk size
    b.extend_from_slice(&1u16.to_le_bytes()); // PCM
    b.extend_from_slice(&1u16.to_le_bytes()); // mono
    b.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    b.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes()); // byte rate
    b.extend_from_slice(&2u16.to_le_bytes()); // block align
    b.extend_from_slice(&16u16.to_le_bytes()); // bits
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data_len.to_le_bytes());
    for s in pcm {
        b.extend_from_slice(&s.to_le_bytes());
    }
    b
}

/// Hand the WAV to winmm.
///
/// `SND_ASYNC` so this never blocks the caller — it is called from the engine's
/// notification thread, and a blocking play would delay everything behind it.
/// `SND_NODEFAULT` so a failure is silence rather than the Windows default
/// "ding", which would be worse than no confirmation at all: it is the sound
/// of an error, and nothing went wrong.
///
/// The buffer must outlive the asynchronous playback, so it is leaked
/// deliberately — a few kilobytes, once per cue kind, and the alternative is
/// handing winmm a pointer into freed memory.
fn play(wav: &[u8]) {
    use std::sync::OnceLock;
    use std::sync::Mutex;

    // One retained copy per distinct cue, keyed by length, so repeated saves
    // do not leak repeatedly.
    static KEPT: OnceLock<Mutex<Vec<&'static [u8]>>> = OnceLock::new();
    let kept = KEPT.get_or_init(|| Mutex::new(Vec::new()));
    let ptr = {
        let mut kept = kept.lock().unwrap();
        match kept.iter().find(|k| **k == wav) {
            Some(existing) => *existing,
            None => {
                let leaked: &'static [u8] = Box::leak(wav.to_vec().into_boxed_slice());
                kept.push(leaked);
                leaked
            }
        }
    };

    unsafe {
        let _ = PlaySoundW(
            windows::core::PCWSTR(ptr.as_ptr() as *const u16),
            None,
            SND_MEMORY | SND_ASYNC | SND_NODEFAULT,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_len(w: &[u8]) -> (u32, u32) {
        let riff = u32::from_le_bytes([w[4], w[5], w[6], w[7]]);
        let data = u32::from_le_bytes([w[40], w[41], w[42], w[43]]);
        (riff, data)
    }

    #[test]
    fn the_header_describes_the_data_that_follows() {
        let w = chime(&[(880.0, 50)], 0.2);
        assert_eq!(&w[0..4], b"RIFF");
        assert_eq!(&w[8..12], b"WAVE");
        assert_eq!(&w[36..40], b"data");
        let (riff, data) = parse_len(&w);
        assert_eq!(data as usize, w.len() - 44, "data length must match the bytes present");
        assert_eq!(riff as usize, w.len() - 8, "RIFF length counts everything after itself");
        // 50 ms at 44.1 kHz, 16-bit mono.
        assert_eq!(data, 44_100 * 50 / 1000 * 2);
    }

    #[test]
    fn tones_are_concatenated() {
        let one = chime(&[(880.0, 50)], 0.2);
        let two = chime(&[(880.0, 50), (660.0, 50)], 0.2);
        assert_eq!(two.len(), one.len() * 2 - 44, "the second tone adds only samples");
    }

    #[test]
    fn the_envelope_starts_and_ends_at_silence() {
        // A click at either edge is the failure this envelope exists to avoid.
        let w = chime(&[(880.0, 60)], 0.5);
        let first = i16::from_le_bytes([w[44], w[45]]);
        let last = i16::from_le_bytes([w[w.len() - 2], w[w.len() - 1]]);
        assert_eq!(first, 0, "must start silent");
        assert!(last.abs() < 400, "must end near silent, got {last}");
    }

    #[test]
    fn amplitude_is_respected_and_never_clips() {
        let quiet = chime(&[(880.0, 60)], 0.1);
        let loud = chime(&[(880.0, 60)], 0.5);
        let peak = |w: &[u8]| {
            w[44..]
                .chunks_exact(2)
                .map(|c| i16::from_le_bytes([c[0], c[1]]).unsigned_abs())
                .max()
                .unwrap()
        };
        let (q, l) = (peak(&quiet), peak(&loud));
        assert!(l > q * 3, "0.5 should be markedly louder than 0.1 ({q} vs {l})");
        assert!(l < i16::MAX as u16, "must not reach full scale");
    }
}
