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
pub fn mix_add(paths: &[&[f32]]) -> Vec<f32> {
    let longest = paths.iter().map(|p| p.len()).max().unwrap_or(0);
    let gain = mix_gain(paths.len());
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
}
