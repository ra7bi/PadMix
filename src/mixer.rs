//! Real-time mixing core. Everything here runs inside audio callbacks, so it
//! never allocates (beyond pre-reserved capacity), never locks and never panics
//! on valid input. It is pure logic so it can be unit-tested without devices.
//!
//! The mix is out = limit(mic*gain + sound*volume), with a soft limiter so loud
//! sounds over your voice never clip harshly.

use std::sync::Arc;

/// Max sounds playing at the same time.
pub const MAX_VOICES: usize = 32;

/// A decoded sound, already converted to the engine rate, stereo interleaved f32.
pub struct Clip {
    pub id: u64,
    pub data: Vec<f32>,
}

impl Clip {
    pub fn frames(&self) -> usize {
        self.data.len() / 2
    }
}

struct Voice {
    clip: Arc<Clip>,
    pos: usize, // frame index
}

/// The set of currently playing sounds.
pub struct Voices {
    list: Vec<Voice>,
}

impl Default for Voices {
    fn default() -> Self {
        Self::new()
    }
}

impl Voices {
    pub fn new() -> Self {
        Self { list: Vec::with_capacity(MAX_VOICES) }
    }

    /// Start a clip. Pressing the same sound again restarts it (soundboard behaviour).
    pub fn play(&mut self, clip: Arc<Clip>) {
        if let Some(v) = self.list.iter_mut().find(|v| v.clip.id == clip.id) {
            v.pos = 0;
            return;
        }
        if self.list.len() < MAX_VOICES {
            self.list.push(Voice { clip, pos: 0 });
        }
    }

    pub fn stop(&mut self, id: u64) {
        self.list.retain(|v| v.clip.id != id);
    }

    pub fn stop_all(&mut self) {
        self.list.clear();
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    /// Sum of all voices for the next frame; advances them and drops finished ones.
    #[inline]
    pub fn next_frame(&mut self) -> (f32, f32) {
        let (mut l, mut r) = (0.0f32, 0.0f32);
        let mut i = 0;
        while i < self.list.len() {
            let v = &mut self.list[i];
            let idx = v.pos * 2;
            if let (Some(&a), Some(&b)) = (v.clip.data.get(idx), v.clip.data.get(idx + 1)) {
                l += a;
                r += b;
                v.pos += 1;
                i += 1;
            } else {
                self.list.swap_remove(i);
            }
        }
        (l, r)
    }

    /// (clip id, progress 0..1) for each playing sound.
    pub fn for_each_progress(&self, mut f: impl FnMut(u64, f32)) {
        for v in &self.list {
            let total = v.clip.frames().max(1);
            f(v.clip.id, v.pos as f32 / total as f32);
        }
    }
}

/// Soft limiter: linear up to 0.9, then smoothly approaches (never exceeds) 1.0.
#[inline]
pub fn limit(x: f32) -> f32 {
    const KNEE: f32 = 0.9;
    let a = x.abs();
    if a <= KNEE {
        x
    } else {
        let y = KNEE + (1.0 - KNEE) * ((a - KNEE) / (1.0 - KNEE)).tanh();
        y.copysign(x)
    }
}

/// One output frame: mic (mono) plus sounds (stereo), each with its own gain.
#[inline]
pub fn mix_frame(mic: f32, mic_gain: f32, snd: (f32, f32), snd_gain: f32) -> (f32, f32) {
    let m = mic * mic_gain;
    (limit(m + snd.0 * snd_gain), limit(m + snd.1 * snd_gain))
}

/// Streaming linear resampler for 1 or 2 channels, used to bridge devices that
/// run at different sample rates (mic -> cable, cable -> headphones).
pub struct Linear {
    ch: usize,
    step: f64, // source frames per output frame
    frac: f64,
    prev: [f32; 2],
    cur: [f32; 2],
    primed: bool,
}

impl Linear {
    pub fn new(channels: usize, src_rate: u32, dst_rate: u32) -> Self {
        Self {
            ch: channels.clamp(1, 2),
            step: src_rate as f64 / dst_rate.max(1) as f64,
            frac: 0.0,
            prev: [0.0; 2],
            cur: [0.0; 2],
            primed: false,
        }
    }

    /// Produce one output frame into `out`. `src` must fill the next source frame
    /// and return false when no data is available. On starvation the state is left
    /// unchanged (so nothing is lost) and false is returned.
    #[inline]
    pub fn next(&mut self, out: &mut [f32; 2], mut src: impl FnMut(&mut [f32; 2]) -> bool) -> bool {
        if !self.primed {
            let mut a = [0.0; 2];
            let mut b = [0.0; 2];
            if !src(&mut a) {
                return false;
            }
            if !src(&mut b) {
                // keep `a` as the starting point, try again next time
                self.prev = a;
                self.cur = a;
                self.primed = true;
                self.frac = 1.0;
                return false;
            }
            self.prev = a;
            self.cur = b;
            self.frac = 0.0;
            self.primed = true;
        }
        while self.frac >= 1.0 {
            let mut nxt = [0.0; 2];
            if !src(&mut nxt) {
                return false;
            }
            self.prev = self.cur;
            self.cur = nxt;
            self.frac -= 1.0;
        }
        let t = self.frac as f32;
        for c in 0..self.ch {
            out[c] = self.prev[c] + (self.cur[c] - self.prev[c]) * t;
        }
        self.frac += self.step;
        true
    }
}

/// Peak meter value with simple decay, stored as f32 bits in an AtomicU32 by callers.
#[inline]
pub fn peak_decay(old: f32, new_peak: f32) -> f32 {
    let decayed = old * 0.92;
    if new_peak > decayed {
        new_peak
    } else {
        decayed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clip(id: u64, frames: usize, v: f32) -> Arc<Clip> {
        Arc::new(Clip { id, data: vec![v; frames * 2] })
    }

    #[test]
    fn limiter_never_exceeds_one_and_is_transparent_below_knee() {
        for i in -400..=400 {
            let x = i as f32 / 100.0;
            let y = limit(x);
            assert!(y.abs() <= 1.0, "limit({x}) = {y}");
            if x.abs() <= 0.9 {
                assert_eq!(y, x);
            }
        }
    }

    #[test]
    fn mix_adds_mic_and_sound_with_gains() {
        let (l, r) = mix_frame(0.2, 0.5, (0.3, -0.1), 1.0);
        assert!((l - 0.4).abs() < 1e-6);
        assert!((r - 0.0).abs() < 1e-6);
    }

    #[test]
    fn voices_sum_and_finish() {
        let mut v = Voices::new();
        v.play(clip(1, 2, 0.25));
        v.play(clip(2, 3, 0.25));
        assert_eq!(v.next_frame(), (0.5, 0.5));
        assert_eq!(v.next_frame(), (0.5, 0.5));
        assert_eq!(v.next_frame(), (0.25, 0.25)); // clip 1 finished
        assert_eq!(v.next_frame(), (0.0, 0.0));
        assert!(v.is_empty());
    }

    #[test]
    fn same_clip_restarts_instead_of_stacking() {
        let mut v = Voices::new();
        let c = clip(7, 4, 0.1);
        v.play(c.clone());
        v.next_frame();
        v.next_frame();
        v.play(c);
        let mut n = 0;
        v.for_each_progress(|_, p| {
            n += 1;
            assert_eq!(p, 0.0);
        });
        assert_eq!(n, 1);
    }

    #[test]
    fn resampler_halves_rate() {
        // 96k -> 48k: 100 source frames give ~50 output frames
        let mut rs = Linear::new(1, 96_000, 48_000);
        let src: Vec<f32> = (0..100).map(|i| i as f32).collect();
        let mut it = src.into_iter();
        let mut produced = 0;
        let mut out = [0.0; 2];
        while rs.next(&mut out, |f| it.next().map(|s| f[0] = s).is_some()) {
            produced += 1;
        }
        assert!((49..=50).contains(&produced), "produced {produced}");
    }

    #[test]
    fn resampler_keeps_state_when_starved() {
        let mut rs = Linear::new(1, 48_000, 48_000);
        let mut out = [0.0; 2];
        let mut data = vec![1.0f32, 2.0, 3.0];
        let mut pop = |f: &mut [f32; 2]| {
            if data.is_empty() {
                false
            } else {
                f[0] = data.remove(0);
                true
            }
        };
        assert!(rs.next(&mut out, &mut pop));
        assert_eq!(out[0], 1.0);
        assert!(rs.next(&mut out, &mut pop));
        assert_eq!(out[0], 2.0);
        assert!(!rs.next(&mut out, &mut pop)); // starved
        assert!(!rs.next(&mut out, &mut pop));
    }
}
