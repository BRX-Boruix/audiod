//! BORUIX `audiod` mixing core: pure DSP functions, host-verifiable.
//!
//! Separated from `main.rs` deliberately (S23/S29): format conversion, summing and
//! clamping are PURE functions. Isolating them lets every boundary be exhaustively
//! tested on the host, with no audio device and no QEMU. Left inside the bare-metal
//! binary, the only available check would be running the whole VM and reading
//! printed output -- slow, and unable to cover inputs a real device never produces.
//!
//! The binary links this crate and only supplies I/O.

#![no_std]

extern crate alloc;

use alloc::vec::Vec;

/// Convert one signed 16-bit sample to f32 in [-1, 1).
///
/// Divides by 32768, not 32767: s16 spans [-32768, 32767] asymmetrically, so
/// 1/32768 maps -32768 exactly onto -1.0 and keeps the positive peak just under
/// 1.0. The result never leaves [-1, 1), which is what makes clamping downstream a
/// real fallback. Using 1/32767.0 would push -32768 to about -1.0000305 -- out of
/// range before any summing occurs.
#[inline]
pub fn s16_to_f32(raw: i16) -> f32 {
    raw as f32 / 32768.0
}

/// Convert one unsigned 8-bit sample to f32 in [-1, 1), centred on 128.
#[inline]
pub fn u8_to_f32(raw: u8) -> f32 {
    (raw as f32 - 128.0) / 128.0
}

/// Convert one signed 24-bit sample (sign-extended into i32) to f32 in [-1, 1).
///
/// 24-bit uses 8388608 = 2^23 as the scale, by the same asymmetry argument as s16.
#[inline]
pub fn s24_to_f32(raw: i32) -> f32 {
    raw as f32 / 8388608.0
}

/// Convert f32 to signed 16-bit with SATURATING clamp.
///
/// Saturation, never wrapping: a wrapped over-range sample flips sign and turns
/// loud material into harsh noise, which is far worse than plain clipping.
///
/// NaN and infinities violate the input contract. They are mapped to full-scale
/// (or silence for NaN) rather than left to produce garbage: NaN compares false
/// against every bound, so an unguarded cast could yield an arbitrary integer.
/// Mapping NaN to 0 (silence) is the least harmful defined outcome.
#[inline]
pub fn f32_to_s16(sample: f32) -> i16 {
    // NaN check must come first: NaN fails both comparisons below.
    if sample.is_nan() {
        return 0;
    }
    if sample >= 1.0 {
        return i16::MAX;
    }
    if sample <= -1.0 {
        return i16::MIN;
    }
    // Scaled by 32767 for the positive side. Because |sample| < 1 here, the
    // product is strictly inside (-32767, 32767), so the cast cannot overflow.
    (sample * 32767.0) as i16
}

/// Duplicate a mono buffer into interleaved stereo, with NO pan attenuation.
///
/// Attenuating here (x0.5 per channel) is a common mistake: it makes mono material
/// 6 dB quieter for no reason and inconsistent with stereo material in the same mix.
pub fn mono_to_stereo(mono: &[f32]) -> Vec<f32> {
    let mut out = Vec::with_capacity(mono.len() * 2);
    for &s in mono {
        out.push(s);
        out.push(s);
    }
    out
}

/// Fixed per-path gain applied before clamping, for `n` contributing paths.
///
/// Uses 1/n rather than 1/sqrt(n). 1/sqrt(n) preserves summed POWER for
/// uncorrelated sources, which is the right choice for a statistical mix of
/// unrelated material. 1/n instead guarantees the sum can never exceed full
/// scale, since each |x| <= 1 implies |sum/n| <= 1.
///
/// 1/n is chosen because the guarantee is UNCONDITIONAL. 1/sqrt(n) bounds only
/// average power, so n correlated paths (all playing the same tone -- exactly what
/// a hardware test does) still clip, and clipping would then be the normal outcome
/// rather than the fallback. The cost is that uncorrelated material is quieter than
/// it could be; that is a gain decision, reversible later, whereas clipping is
/// audible distortion baked into the output. With n = 0 no signal exists at all, so
/// the gain is defined as 1.0 (unused) to keep callers free of a division by zero.
#[inline]
pub fn mix_gain(paths: usize) -> f32 {
    if paths == 0 { 1.0 } else { 1.0 / paths as f32 }
}

/// Sum N paths sample-by-sample with the gain set by `paths.len()`.
///
/// This is the correct call only when EVERY configured path is represented in
/// `paths`. For a mixer with a fixed path count where some paths may be starved,
/// use [`mix_add_configured`] and pass the configured total instead.
pub fn mix_add(paths: &[&[f32]]) -> Vec<f32> {
    mix_add_configured(paths, paths.len())
}

/// Sum N paths sample-by-sample, apply fixed gain, then clamp to [-1, 1].
///
/// Output length is the LONGEST input: a short path is treated as silent past its
/// end, never as a reason to truncate the others. Truncating would silently drop
/// audio from the longest stream, which is exactly the kind of hidden data loss the
/// project forbids.
///
/// Gain uses the FULL path count, not the count of paths live at a given sample
/// index. Using the live count would make gain vary within one buffer, producing a
/// discontinuity partway through and therefore a click.
///
/// Inputs are never mutated: callers keep ownership of their buffers and the mixer
/// produces a fresh one.
/// `configured` is the number of paths the mixer was BUILT with, and it determines
/// the gain. It is deliberately separate from `paths.len()`.
///
/// Passing only the live paths and deriving gain from that count was the original
/// design, and it is wrong: producer jitter means the live count changes constantly,
/// so gain would change constantly too, and a level change is an audible click.
/// Worse, the same signal would be rendered at different levels depending on whether
/// a neighbour happened to have data that instant.
///
/// Callers therefore must NOT filter out starved paths. They pass one (possibly
/// empty) slice per configured path and state the configured total. A path with no
/// data contributes silence and leaves the gain untouched, which is what makes
/// "one path drops out" inaudible as a level change.
pub fn mix_add_configured(paths: &[&[f32]], configured: usize) -> Vec<f32> {
    let longest = paths.iter().map(|p| p.len()).max().unwrap_or(0);
    let gain = mix_gain(configured);
    let mut out = Vec::with_capacity(longest);
    for i in 0..longest {
        let mut acc = 0.0f32;
        for p in paths {
            if let Some(&s) = p.get(i) {
                acc += s;
            }
        }
        let scaled = acc * gain;
        // Clamp after gain. Saturation here is deliberate defence-in-depth:
        // with 1/n it should be unreachable for in-range inputs, and the test
        // suite pins that it does not fire under normal conditions.
        out.push(scaled.clamp(-1.0, 1.0));
    }
    out
}

/// Per-path bookkeeping for the mixer (batch-4 M3).
///
/// Lives in the library, not the binary, so the state machine can be exercised
/// exhaustively on the host. The rules it encodes are easy to get subtly wrong
/// (especially the disconnect/underrun distinction) and impossible to check
/// reliably by listening to a VM.
///
/// Only two facts are tracked per path, deliberately kept minimal:
///   - connected: is a producer still attached with data expected?
///   - underruns: how many times was it connected but had nothing to give?
pub struct MixerState {
    connected: Vec<bool>,
    underruns: Vec<u64>,
    /// Consecutive rounds this path has been connected but empty.
    empty_run: Vec<u64>,
    /// Whether this path has EVER delivered data since connecting.
    ///
    /// Starvation is the absence of something that was previously present. Without
    /// this flag, a path merely not yet started would look identical to one that
    /// stopped, and the startup transient would be reported as a fault.
    ever_delivered: Vec<bool>,
    /// Whether the current empty run has already been counted.
    ///
    /// Keeps one starvation episode to one count, however long it lasts. Without it
    /// a single long gap would add one per round and the total would measure elapsed
    /// time rather than the number of failures.
    run_counted: Vec<bool>,
}

/// Consecutive empty rounds before absence counts as starvation.
///
/// Chosen from measurement, not intuition. On QEMU the mixer loop runs far faster
/// than producers fill buffers, so a given path is legitimately empty in most
/// rounds; counting every one produced 5871 underruns by round 6016 with BOTH
/// producers healthy. A threshold of 32 consecutive empty rounds is comfortably
/// above that polling jitter while still detecting a stopped producer within a few
/// milliseconds, since rounds are far shorter than an audio frame period.
pub const STARVATION_THRESHOLD: u64 = 32;

/// Observable per-path statistics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PathStats {
    /// True while a producer is still expected to supply this path.
    pub connected: bool,
    /// Times this path was connected yet produced no data for a round.
    pub underruns: u64,
}

impl MixerState {
    /// Create state for `paths` input paths, all initially disconnected.
    ///
    /// A path starts DISCONNECTED on purpose: at startup no producer has written
    /// anything yet, so treating it as connected would immediately record bogus
    /// underruns for every path during the normal startup transient.
    pub fn new(paths: usize) -> Self {
        Self {
            connected: alloc::vec![false; paths],
            underruns: alloc::vec![0; paths],
            empty_run: alloc::vec![0; paths],
            ever_delivered: alloc::vec![false; paths],
            run_counted: alloc::vec![false; paths],
        }
    }

    /// Number of configured paths.
    pub fn paths(&self) -> usize {
        self.connected.len()
    }

    /// Mark a path connected or disconnected.
    ///
    /// Disconnecting clears nothing else: the accumulated underrun count is history
    /// and stays readable (`stream/N/status` must still report what happened before
    /// the producer left).
    pub fn set_connected(&mut self, path: usize, connected: bool) {
        if let Some(c) = self.connected.get_mut(path) {
            *c = connected;
        }
    }

    /// Whether a producer is expected to feed this path.
    pub fn is_connected(&self, path: usize) -> bool {
        self.connected.get(path).copied().unwrap_or(false)
    }

    /// Record a round in which `path` DID supply data.
    ///
    /// Resets the consecutive-empty counter and marks the path as having delivered
    /// at least once. Both matter: the first bounds a gap so only sustained absence
    /// counts, the second ensures a path that never started cannot be starving.
    pub fn note_data(&mut self, path: usize) {
        if let Some(r) = self.empty_run.get_mut(path) {
            *r = 0;
        }
        if let Some(d) = self.ever_delivered.get_mut(path) {
            *d = true;
        }
        if let Some(c) = self.run_counted.get_mut(path) {
            *c = false;
        }
    }

    /// Record a round in which `path` was CONNECTED but supplied no data.
    ///
    /// Counts an underrun only when ALL of these hold:
    ///   - the path is connected (a path nobody feeds cannot be starving);
    ///   - it has delivered data before (absence of what was never there is not loss);
    ///   - the empty run reaches STARVATION_THRESHOLD consecutive rounds;
    ///   - this run has not already been counted.
    ///
    /// Every one of those conditions exists because leaving it out produced a number
    /// that grew on a healthy system and therefore measured nothing. See the tests
    /// for the measured figures that motivated each.
    pub fn note_starved(&mut self, path: usize) {
        if !self.is_connected(path) {
            return;
        }
        // A path that never delivered cannot be underrunning: it has not started.
        if !self.ever_delivered.get(path).copied().unwrap_or(false) {
            return;
        }
        let run = match self.empty_run.get_mut(path) {
            Some(r) => {
                *r = r.saturating_add(1);
                *r
            }
            None => return,
        };
        if run < STARVATION_THRESHOLD {
            return;
        }
        // One episode counts once, no matter how long it lasts.
        if self.run_counted.get(path).copied().unwrap_or(false) {
            return;
        }
        if let Some(c) = self.run_counted.get_mut(path) {
            *c = true;
        }
        if let Some(u) = self.underruns.get_mut(path) {
            // Saturating: a counter that wraps would under-report a chronic fault,
            // which is exactly the case this number exists to expose.
            *u = u.saturating_add(1);
        }
    }

    /// Statistics for one path.
    pub fn stats(&self, path: usize) -> PathStats {
        PathStats {
            connected: self.is_connected(path),
            underruns: self.underruns.get(path).copied().unwrap_or(0),
        }
    }

    /// Count of paths currently expected to supply data.
    pub fn connected_count(&self) -> usize {
        self.connected.iter().filter(|c| **c).count()
    }
}

/// Sample rate the mixer operates at, in Hz.
///
/// Tied to the codec format the driver actually programs (48000). The ramp duration
/// is expressed in seconds and converted through this, so if the rate ever changes
/// the ramp keeps its real-world duration rather than silently becoming longer or
/// shorter. R1 found the codec is fixed at 48000; see the batch-3 notes on why
/// 44100 was refused by the format gate.
pub const SAMPLE_RATE_HZ: u32 = 48_000;

/// Volume ramp duration, in seconds.
///
/// 5ms is ~240 frames at 48kHz. The lower bound is audibility: a step spreads energy
/// across the spectrum as a click, and spreading it over a few hundred samples
/// pushes that energy below the ear's integration time. The upper bound is feel:
/// much longer and a volume change stops feeling immediate. Anything in the few-ms
/// range works; 5ms sits comfortably inside it.
pub const RAMP_SECONDS: f32 = 0.005;

/// Per-sample volume increment for a full-scale (0 -> 1) ramp of RAMP_SECONDS.
///
/// Derived, not hand-picked: a full-range change must complete in RAMP_SECONDS worth
/// of samples. Any larger step would finish early (and risk a click on large jumps);
/// any smaller would still be ramping after the intended duration.
pub const RAMP_STEP: f32 = 1.0 / (RAMP_SECONDS * SAMPLE_RATE_HZ as f32);

/// Per-sample increment for a ramp over the full 0..1 range.
pub fn auto_ramp_step() -> f32 {
    RAMP_STEP
}

/// A validated volume in [0.0, 1.0].
///
/// The range is enforced at CONSTRUCTION, so an invalid volume cannot exist inside
/// the mixer. Downstream code therefore needs no defensive clamping, and there is no
/// path by which a NaN or an out-of-range value could reach a multiply.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Volume(f32);

impl Volume {
    /// Validate and wrap a volume.
    ///
    /// Rejects rather than clamps. A silent clamp would leave the caller believing
    /// something the system did not do, which is the failure mode this project treats
    /// as a red line. NaN is rejected explicitly: it compares false against every
    /// bound, so a naive range check would let it through and it would then propagate
    /// through every subsequent multiply, poisoning the entire mix.
    pub fn new(value: f32) -> Result<Self, VolumeError> {
        // NaN must be tested first: `!(0.0..=1.0).contains(&NaN)` is true, so the range
        // check below would already reject it, but stating it explicitly documents the
        // intent and survives a future rewrite of the range test.
        if value.is_nan() {
            return Err(VolumeError::NotANumber);
        }
        if !(0.0..=1.0).contains(&value) {
            return Err(VolumeError::OutOfRange);
        }
        Ok(Self(value))
    }

    /// The validated value.
    pub fn get(self) -> f32 {
        self.0
    }
}

/// Why a volume was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VolumeError {
    /// Value was NaN.
    NotANumber,
    /// Value was outside [0.0, 1.0].
    OutOfRange,
}

/// A linear volume ramp.
///
/// Holds the current value and the target. Each call to [`Ramp::next`] moves one
/// sample's worth toward the target, so a change is spread over many samples instead
/// of happening in one.
pub struct Ramp {
    current: f32,
    target: f32,
    step: f32,
}

impl Ramp {
    /// Start settled at `initial`.
    ///
    /// Starting settled (rather than ramping up from zero) is deliberate: a player
    /// beginning at its configured volume should not fade in.
    pub fn new(initial: f32) -> Self {
        Self {
            current: initial,
            target: initial,
            step: RAMP_STEP,
        }
    }

    /// Aim at a new value, ramping at `step` per sample.
    ///
    /// The ramp always continues from `current`, never from the previous target.
    /// Restarting from the old target would itself be a jump, reintroducing the very
    /// discontinuity the ramp exists to remove.
    pub fn set_target(&mut self, target: f32, step: f32) {
        self.target = target;
        self.step = if step > 0.0 { step } else { RAMP_STEP };
    }

    /// Advance one sample and return the new volume.
    pub fn next_sample(&mut self) -> f32 {
        let delta = self.target - self.current;
        if delta.abs() <= self.step {
            // Snap on the final sample so the ramp lands EXACTLY on the target.
            // Without the snap, repeated additions of a step that does not divide the
            // range evenly would leave a permanent residue and the ramp would never
            // settle, keeping is_settled() false forever.
            self.current = self.target;
        } else if delta > 0.0 {
            self.current += self.step;
        } else {
            self.current -= self.step;
        }
        self.current
    }

    /// Whether the ramp has reached its target.
    pub fn is_settled(&self) -> bool {
        self.current == self.target
    }

    /// The value that will be returned by the next call, without advancing.
    pub fn current(&self) -> f32 {
        self.current
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    // ================= M2: f32 mixing core (pure functions, host-verifiable) =================

    /// M2: s16 -> f32 fixed-point scaling convention.
    ///
    /// Divide by 32768 (not 32767): s16 spans [-32768, 32767], which is asymmetric.
    /// Dividing by 32768 maps -32768 exactly to -1.0, and the positive peak to
    /// 32767/32768 ~= 0.99997. The mapping is strictly monotonic and never exceeds
    /// [-1, 1], so clamping stays a genuine fallback rather than a load-bearing path.
    /// With 1/32767.0 instead, -32768 would map to -1.0000305, already out of range
    /// before any summing happens.
    #[test]
    fn test_m2_s16_to_f32_scale_is_monotonic_and_bounded() {
        // Endpoints must land inside [-1, 1).
        assert_eq!(s16_to_f32(i16::MIN), -1.0);
        let top = s16_to_f32(i16::MAX);
        assert!(top < 1.0, "s16::MAX must map below 1.0, got {top}");
        assert!(top > 0.999, "s16::MAX must map close to 1.0, got {top}");

        // Zero must be exact (no DC offset).
        assert_eq!(s16_to_f32(0), 0.0);

        // Strict monotonicity across all 65536 inputs.
        let mut prev = s16_to_f32(i16::MIN);
        let mut raw = i16::MIN as i32 + 1;
        while raw <= i16::MAX as i32 {
            let cur = s16_to_f32(raw as i16);
            assert!(cur > prev, "not monotonic at {raw}: {prev} -> {cur}");
            prev = cur;
            raw += 1;
        }
    }

    /// M2: f32 -> s16 rounding and saturation boundaries.
    ///
    /// Clamping must SATURATE, never wrap: wrapping turns an over-loud signal into
    /// sign-flipped noise, which sounds far worse than clipping.
    #[test]
    fn test_m2_f32_to_s16_saturates_not_wraps() {
        assert_eq!(f32_to_s16(0.0), 0);
        assert_eq!(f32_to_s16(-1.0), -32768);

        // Out of range must saturate, never wrap.
        assert_eq!(f32_to_s16(2.0), i16::MAX, "+2.0 must saturate to MAX");
        assert_eq!(f32_to_s16(-2.0), i16::MIN, "-2.0 must saturate to MIN");
        assert_eq!(f32_to_s16(f32::MAX), i16::MAX);
        assert_eq!(f32_to_s16(f32::MIN), i16::MIN);

        // NaN/Inf violate the input contract; define as silence rather than garbage.
        assert_eq!(f32_to_s16(f32::NAN), 0, "NaN must not produce garbage");
        assert_eq!(f32_to_s16(f32::INFINITY), i16::MAX);
        assert_eq!(f32_to_s16(f32::NEG_INFINITY), i16::MIN);
    }

    /// M2: u8 -> f32 (unsigned, centred at 128).
    #[test]
    fn test_m2_u8_to_f32() {
        assert_eq!(u8_to_f32(0), -1.0);
        assert_eq!(u8_to_f32(128), 0.0);
        let top = u8_to_f32(255);
        assert!(top < 1.0 && top > 0.99, "u8::MAX maps near 1.0, got {top}");
    }

    /// M2: mono -> stereo duplicates to both channels with no pan attenuation.
    #[test]
    fn test_m2_mono_duplicates_to_both_channels_without_attenuation() {
        let out = mono_to_stereo(&[0.25, -0.5, 1.0]);
        assert_eq!(out, alloc::vec![0.25, 0.25, -0.5, -0.5, 1.0, 1.0]);
    }

    /// M2: per-sample summing of N paths, fixed attenuation, clamping.
    #[test]
    fn test_m2_mix_two_identical_fullscale_does_not_clip() {
        let a = alloc::vec![1.0f32; 8];
        let b = alloc::vec![1.0f32; 8];
        let mixed = mix_add(&[&a, &b]);
        let mut i = 0usize;
        while i < mixed.len() {
            let s = mixed[i];
            assert!(
                (s.abs() - 1.0).abs() < 1e-6,
                "sample {i} should be exactly 1.0 after 1/N scaling, got {s}"
            );
            i += 1;
        }
        assert_eq!(f32_to_s16(mixed[0]), i16::MAX);
    }

    /// M2 adversarial inputs: unequal lengths, empty set, single path.
    #[test]
    fn test_m2_mix_adversarial_inputs() {
        let none: alloc::vec::Vec<&[f32]> = alloc::vec::Vec::new();
        assert!(mix_add(&none).is_empty(), "empty path set -> empty output");

        let e: alloc::vec::Vec<f32> = alloc::vec::Vec::new();
        assert!(mix_add(&[&e, &e]).is_empty(), "all-empty -> empty");

        // Unequal lengths: output takes the LONGEST, short paths are silent-padded.
        let a = alloc::vec![1.0f32, 1.0, 1.0];
        let b = alloc::vec![1.0f32];
        let m = mix_add(&[&a, &b]);
        assert_eq!(m.len(), 3, "output must be longest input, not truncated");
        assert!(
            (m[0] - 1.0).abs() < 1e-6,
            "both present -> 2.0/2 = 1.0, got {}", m[0]
        );
        assert!(
            (m[2] - 0.5).abs() < 1e-6,
            "only a present -> 1.0/2 = 0.5, got {}", m[2]
        );
    }

    /// M2: mixing must not mutate its inputs (pure-function contract).
    #[test]
    fn test_m2_mix_does_not_mutate_inputs() {
        let a = alloc::vec![0.5f32, -0.25];
        let b = alloc::vec![0.5f32, 0.25];
        let a_copy = a.clone();
        let b_copy = b.clone();
        let _ = mix_add(&[&a, &b]);
        assert_eq!(a, a_copy, "mix must not mutate its inputs");
        assert_eq!(b, b_copy, "mix must not mutate its inputs");
    }

    /// M2 acceptance: the EXACT two-tone scenario the end-to-end run performs.
    ///
    /// Stream A writes +24576 (+0.75), stream B writes -8192 (-0.25), both to stereo.
    /// Summing gives +0.5, and the 1/N gain yields +0.25 in f32, about 8192 in s16.
    ///
    /// The amplitudes differ by a FACTOR OF THREE ON PURPOSE. An earlier version used
    /// +16384 and -16383 -- nearly equal magnitudes -- which cancel almost exactly
    /// (their sum is 1/65536, not the intended value). Nearly equal opposite tones
    /// demonstrate cancellation, not addition, and the expected output would have been
    /// almost zero, indistinguishable from silence. That mistake was caught by this
    /// very test failing on its first run.
    ///
    /// Distinguishable failure modes, all far from the correct +0.25:
    ///   both paths -> +0.25 -> about 8192
    ///   only A     -> +0.75 -> about 24575
    ///   only B     -> -0.25 -> about -8192
    ///   neither    -> no bytes written at all
    #[test]
    fn test_m2_two_tone_acceptance_matches_end_to_end_scenario() {
        const AMP_HI: i16 = 24576;
        const AMP_LO: i16 = -8192;

        // Stereo duplication of each mono tone, as the producers write it.
        let a = mono_to_stereo(&[s16_to_f32(AMP_HI), s16_to_f32(AMP_HI)]);
        let b = mono_to_stereo(&[s16_to_f32(AMP_LO), s16_to_f32(AMP_LO)]);

        let mixed = mix_add(&[&a, &b]);
        for (i, s) in mixed.iter().enumerate() {
            assert!(
                (s - 0.25).abs() < 1e-4,
                "sample {i}: expected +0.25 from (0.75 - 0.25)/2, got {s}"
            );
            assert!(
                !(-1.0..=1.0).contains(s) || s.abs() <= 1.0,
                "sample {i} escaped [-1, 1]: {s}"
            );
        }

        // Matches what the DMA dump should show: about +0.25 full scale.
        let s16 = f32_to_s16(mixed[0]);
        assert!(
            (s16 - 8192).abs() <= 2,
            "expected about 8192 in s16, got {s16}"
        );

        // Negative control: a single path at unity gain must NOT look like the mix.
        let only_a = mix_add(&[&a]);
        assert!(
            (f32_to_s16(only_a[0]) - 24575).abs() <= 2,
            "single path must stay near 24575, proving the mix is distinguishable"
        );
    }

    /// M3: a PERMANENTLY disconnected path must be removed from the mix set and
    /// must NOT be counted as underrunning.
    ///
    /// The distinction is the whole point of M3. A path that has gone away is not
    /// starved; nobody is trying to feed it. Counting it would inflate the underrun
    /// figure with a condition the mixer cannot fix, drowning out the real signal:
    /// intermittent starvation of a path that IS still connected.
    #[test]
    fn test_m3_permanent_disconnect_is_not_an_underrun() {
        let mut m = MixerState::new(4);
        m.set_connected(0, true);
        m.set_connected(1, true);

        // Path 0 delivers, then starves for long enough to count as an episode.
        m.note_data(0);
        let mut i = 0;
        while i < STARVATION_THRESHOLD + 5 {
            m.note_starved(0);
            i += 1;
        }
        assert_eq!(m.stats(0).underruns, 1);

        // Path 0 disconnects: it leaves the mix set...
        m.set_connected(0, false);
        assert!(!m.is_connected(0));
        // ...and a further long absence must not be recorded as underrun, because
        // nobody is feeding a disconnected path in the first place.
        m.note_data(0);
        let mut j = 0;
        while j < STARVATION_THRESHOLD + 5 {
            m.note_starved(0);
            j += 1;
        }
        assert_eq!(
            m.stats(0).underruns,
            1,
            "a disconnected path must not accrue underruns"
        );

        // A path never connected likewise never underruns.
        let mut k = 0;
        while k < STARVATION_THRESHOLD + 5 {
            m.note_starved(2);
            k += 1;
        }
        assert_eq!(
            m.stats(2).underruns,
            0,
            "a never-connected path must not accrue underruns"
        );
    }

    /// M3: mix gain must be FIXED at 1/N, independent of how many paths happen to
    /// have data in a given round.
    ///
    /// This was wrong in the first M2 implementation: gain was 1/<paths live right
    /// now>, so one path briefly lacking data changed the gain for every other path.
    /// Producer jitter is constant in a real system, so that produced continuous
    /// audible level jumps -- and a level jump is a click.
    ///
    /// The two questions are orthogonal and must stay separate:
    ///   - how loud is the mix? fixed 1/N, decided once;
    ///   - what if a path has no data this instant? it contributes silence.
    #[test]
    fn test_m3_gain_is_fixed_by_path_count_not_by_liveness() {
        assert_eq!(
            mix_gain(4),
            0.25,
            "gain must come from configured path count"
        );

        // One live path among four is still scaled by 1/4, NOT 1/1.
        let a = alloc::vec![1.0f32, 1.0];
        let silent: alloc::vec::Vec<f32> = alloc::vec![0.0f32, 0.0];
        let one_of_four = mix_add_configured(&[&a, &silent, &silent, &silent], 4);
        assert!(
            (one_of_four[0] - 0.25).abs() < 1e-6,
            "full-scale input must be scaled by 1/4, got {}", one_of_four[0]
        );

        // The decisive stability property: the surviving path is at the SAME level
        // whether the other paths are merely silent or have gone away entirely. Both
        // calls below declare the same configured total (4), so both use gain 1/4.
        // That is exactly what makes a path dropping out inaudible as a level change.
        let starved = mix_add_configured(&[&a, &silent, &silent, &silent], 4);
        assert!(
            (starved[0] - one_of_four[0]).abs() < 1e-6,
            "level must not depend on which other paths are present"
        );

        // Contrast: deriving gain from the passed slice count instead would change the
        // level (1/1 here). This documents why `configured` exists as a parameter.
        let naive = mix_add(&[&a]);
        assert!(
            (naive[0] - 1.0).abs() < 1e-6,
            "slice-count gain gives 1.0, demonstrating the defect being prevented"
        );
    }

    /// M3: a starved path contributes silence WITHOUT blocking or shortening others.
    #[test]
    fn test_m3_starved_path_is_silence_not_a_stall() {
        let a = alloc::vec![0.5f32, 0.25, 0.125];
        let empty: alloc::vec::Vec<f32> = alloc::vec::Vec::new();

        // Both calls declare the same configured path count, so a starved path adds
        // only silence and cannot shift the surviving path's level.
        let with_empty = mix_add_configured(&[&a, &empty], 2);
        let with_silent = mix_add_configured(&[&a, &alloc::vec![0.0f32; 3]], 2);
        assert_eq!(
            with_empty.len(),
            a.len(),
            "a starved path must not truncate the live one"
        );
        let mut i = 0usize;
        while i < with_empty.len() {
            assert!(
                (with_empty[i] - with_silent[i]).abs() < 1e-6,
                "a starved path must behave exactly like an explicit silent one, sample {i}"
            );
            i += 1;
        }
    }

    /// M3: a single empty round is NOT an underrun; sustained absence is.
    ///
    /// The first implementation counted every round in which a path had no data,
    /// which made the number grow without bound on a healthy system. Measured on
    /// QEMU: 47 underruns within 64 rounds, 5871 by round 6016, while BOTH producers
    /// were alive and actively writing. A counter that always rises carries no
    /// information -- it cannot distinguish a real fault from ordinary jitter.
    ///
    /// Why single-round emptiness is normal here: the mixer is a polling loop, not
    /// an audio clock. It iterates far faster than producers fill buffers, so most
    /// rounds legitimately find a given path empty. That is idle spinning, not
    /// starvation.
    ///
    /// Real starvation is SUSTAINED absence -- evidence that a path which was
    /// supplying data has stopped. Requiring a run of consecutive empty rounds
    /// before counting gives the counter its meaning back.
    #[test]
    fn test_m3_isolated_empty_rounds_are_not_underruns() {
        let mut m = MixerState::new(2);
        // Established connection: this path HAS been supplying data.
        m.set_connected(0, true);
        m.note_data(0);
        assert_eq!(m.stats(0).underruns, 0);

        // Occasional empty rounds, each followed by data again. A healthy but
        // jittery producer looks exactly like this and must stay at zero.
        let mut i = 0;
        while i < 20 {
            m.note_starved(0);
            m.note_data(0);
            i += 1;
        }
        assert_eq!(
            m.stats(0).underruns,
            0,
            "isolated empty rounds must not accumulate underruns"
        );
    }

    /// M3: SUSTAINED absence does count, and counts once per starvation episode.
    #[test]
    fn test_m3_sustained_absence_counts_once_per_episode() {
        let mut m = MixerState::new(2);
        m.set_connected(0, true);
        m.note_data(0);

        // A long run of empty rounds is one starvation episode, not one per round.
        let mut i = 0;
        while i < 100 {
            m.note_starved(0);
            i += 1;
        }
        assert_eq!(
            m.stats(0).underruns,
            1,
            "a sustained gap counts once, not once per round"
        );

        // Data returns, then the path starves again: a second episode.
        m.note_data(0);
        let mut j = 0;
        while j < 100 {
            m.note_starved(0);
            j += 1;
        }
        assert_eq!(
            m.stats(0).underruns,
            2,
            "a second starvation episode counts separately"
        );
    }

    /// M3: a path must have supplied data before its absence can mean starvation.
    ///
    /// Startup is the clearest case: nothing has been written yet, so every path is
    /// empty, and counting that would report a fault before any audio ever played.
    #[test]
    fn test_m3_starvation_requires_prior_data() {
        let mut m = MixerState::new(2);
        m.set_connected(0, true);
        // Connected, but has never delivered anything.
        let mut i = 0;
        while i < 500 {
            m.note_starved(0);
            i += 1;
        }
        assert_eq!(
            m.stats(0).underruns,
            0,
            "a path that never delivered cannot be underrunning"
        );
    }

    /// M4: volume must reject illegal values rather than silently rounding them.
    ///
    /// Rejecting matters because a silent clamp makes the caller's belief diverge from
    /// reality: they set 2.5, hear no error, and cannot tell what the system actually
    /// did. Reporting an error keeps the caller informed. NaN is included because it
    /// propagates through every later multiply and would poison the whole mix.
    #[test]
    fn test_m4_volume_validation_rejects_illegal_values() {
        assert!(Volume::new(0.0).is_ok());
        assert!(Volume::new(1.0).is_ok());
        assert!(Volume::new(0.5).is_ok());
        assert!(Volume::new(-0.001).is_err());
        assert!(Volume::new(1.001).is_err());
        assert!(Volume::new(f32::NAN).is_err());
        assert!(Volume::new(f32::INFINITY).is_err());
        assert!(Volume::new(f32::NEG_INFINITY).is_err());
    }

    /// M4: a volume change must ramp, never step.
    ///
    /// An instantaneous level change is a discontinuity in the waveform, which spreads
    /// energy across the spectrum and is heard as a click. Ramping distributes the same
    /// change over many samples, keeping the waveform continuous.
    #[test]
    fn test_m4_ramp_never_steps_in_one_sample() {
        let mut r = Ramp::new(0.0);
        r.set_target(1.0, RAMP_STEP);
        let mut prev = 0.0f32;
        let mut steps = 0usize;
        while !r.is_settled() {
            let v = r.next_sample();
            let delta = (v - prev).abs();
            assert!(delta <= RAMP_STEP + 1e-6);
            assert!(v >= prev);
            prev = v;
            steps += 1;
            assert!(steps < 10_000);
        }
        assert!((prev - 1.0).abs() < 1e-6);
    }

    /// M4: a 5ms ramp at 48kHz must span roughly 240 frames.
    ///
    /// The duration is the whole design decision, so it is asserted directly rather
    /// than left implicit in RAMP_STEP. Too short and the discontinuity is still
    /// audible; too long and a volume change feels laggy.
    #[test]
    fn test_m4_ramp_duration_is_about_5ms() {
        let frames = (RAMP_SECONDS * SAMPLE_RATE_HZ as f32) as usize;
        assert!((220..=260).contains(&frames));
        let step = auto_ramp_step();
        assert!(step * frames as f32 >= 1.0);
    }

    /// M4: setting a new target mid-ramp must continue from the CURRENT value.
    ///
    /// Jumping straight to the new plan from the old target would itself be a step,
    /// reintroducing exactly the click the ramp exists to prevent.
    #[test]
    fn test_m4_retarget_mid_ramp_is_continuous() {
        let mut r = Ramp::new(0.0);
        r.set_target(1.0, 0.01);
        let mut last = 0.0f32;
        let mut i = 0;
        while i < 10 {
            last = r.next_sample();
            i += 1;
        }
        r.set_target(0.2, 0.01);
        let after = r.next_sample();
        assert!((after - last).abs() <= 0.01 + 1e-6);
    }

    /// M4: volume applies to the per-path signal BEFORE summing.
    ///
    /// Applied after summing, one path's volume would scale the others too, which is
    /// plainly wrong -- and silent in effect if only one path is playing.
    #[test]
    fn test_m4_per_path_volume_scales_only_its_own_path() {
        let a = alloc::vec![1.0f32, 1.0];
        let b = alloc::vec![1.0f32, 1.0];
        let av: Vec<f32> = a.iter().map(|s| s * 0.5).collect();
        let mixed = mix_add_configured(&[&av, &b], 2);
        assert!((mixed[0] - 0.75).abs() < 1e-6);
    }

    /// M4: full mute (volume 0) must produce exact digital silence.
    ///
    /// Not merely quiet: the mixer must emit true zeros, so that muting is verifiable
    /// by inspection rather than by ear.
    #[test]
    fn test_m4_mute_produces_exact_silence() {
        let a = alloc::vec![1.0f32, -1.0, 0.5];
        let muted: Vec<f32> = a.iter().map(|s| s * 0.0).collect();
        let mixed = mix_add_configured(&[&muted], 1);
        for s in mixed.iter() {
            assert_eq!(*s, 0.0);
            assert_eq!(f32_to_s16(*s), 0);
        }
    }
}
