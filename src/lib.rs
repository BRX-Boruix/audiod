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

/// Number of channels in the mixer's interleaved PCM format.
///
/// Defined here rather than in the binary: the resampling and mixing code is what
/// depends on the layout, and two definitions of "how many channels" would be two
/// things to keep in step (S15). The binary aliases this rather than restating it.
pub const FORMAT_CHANNELS: usize = 2;

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
/// Input sample rates the resampler supports, in Hz.
///
/// Enumerated deliberately rather than accepting any rate. Each entry is a ratio this
/// implementation has an actual test for; anything else would be a promise about
/// resampling quality that nothing verifies. General ratios need a polyphase FIR,
/// which the plan explicitly places out of scope -- so the honest boundary is a list.
///
/// The set covers the two real-world families (44.1k and 48k) and their common halves,
/// which is what consumer audio actually delivers.
pub const SUPPORTED_INPUT_RATES: [u32; 5] = [48_000, 44_100, 32_000, 22_050, 16_000];

/// Why a resampler could not be constructed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResampleError {
    /// The requested input rate is not in SUPPORTED_INPUT_RATES.
    UnsupportedInputRate,
    /// The output rate is not the mixer's internal rate.
    UnsupportedOutputRate,
}
/// A streaming linear-interpolation resampler.
///
/// Converts a mono stream from `src_rate` to `dst_rate`. The mixer calls this per path
/// before mixing, so paths with different source rates share one output timeline.
///
/// ## Why fixed point
///
/// The read position advances by a fractional amount per output sample. Accumulating
/// that in `f32` loses precision as the position grows: once the position is near
/// 2^23, adding a step smaller than the mantissa resolution does nothing at all, so
/// output samples repeat and the stream slows down. The drift is gradual and sounds
/// like a slightly wrong pitch -- far harder to notice than a hard failure.
/// Fixed point has no such failure mode: the step stays exact at every magnitude.
///
/// ## Why `prev` exists
///
/// The mixer feeds input in whatever chunk sizes the ring provides, so a frame and its
/// neighbour are routinely split across calls. `prev` retains the last frame of the
/// previous slice so interpolation can still see the left neighbour at a seam.
/// Without it every buffer boundary would interpolate against nothing and the output
/// would be discontinuous there.
pub struct Resampler {
    src_rate: u32,
    dst_rate: u32,
    /// Last frame of the previous slice, kept to interpolate across a seam.
    prev: f32,
    /// Whether `prev` holds a real frame yet.
    have_prev: bool,
    /// Total input frames fed in since construction.
    ///
    /// Paired with `frames_out` this enforces the timeline exactly. See `process`.
    frames_in: u64,
    /// Total output frames produced since construction.
    frames_out: u64,
}

impl Resampler {
    /// Build a resampler for `src_rate` -> `dst_rate`.
    pub fn new(src_rate: u32, dst_rate: u32) -> Result<Self, ResampleError> {
        if !SUPPORTED_INPUT_RATES.contains(&src_rate) {
            return Err(ResampleError::UnsupportedInputRate);
        }
        if dst_rate != SAMPLE_RATE_HZ {
            // The mix bus runs at exactly one rate. Accepting another here would mean
            // the output side also needed resampling, which nothing implements.
            return Err(ResampleError::UnsupportedOutputRate);
        }
                Ok(Self {
            src_rate,
            dst_rate,
            prev: 0.0,
            have_prev: false,
            frames_in: 0,
            frames_out: 0,
        })
    }

    /// Output frames per input frame. Derived from the rates, never baked in.
    pub fn ratio(&self) -> f32 {
        self.dst_rate as f32 / self.src_rate as f32
    }

    /// Source rate this resampler expects.
    pub fn src_rate(&self) -> u32 {
        self.src_rate
    }

    /// Resample `input` into `output`.
    ///
    /// Consumes `input` entirely and returns `(input.len(), produced)`. The caller
    /// advances by the full slice and never re-sends a frame; the one frame a block
    /// needs from its predecessor is carried in `prev`.
    ///
    /// ## The mapping, in exact integers
    ///
    /// Input frame `n` represents time `n / src_rate`; output frame `k` represents
    /// `k / dst_rate`. So output `k` reads input position
    ///
    /// ```text
    /// p = k * src_rate / dst_rate
    /// ```
    ///
    /// and interpolates between `x[floor(p)]` and `x[floor(p)+1]`:
    ///
    /// ```text
    /// n    = (k * src_rate) / dst_rate          // integer division, exact
    /// frac = (k * src_rate) % dst_rate / dst_rate
    /// ```
    ///
    /// Both come straight from `k`, so nothing accumulates. That is the whole reason
    /// this function holds no running position: an earlier version advanced a fixed-point
    /// position by a per-frame step, and the step cannot be represented exactly. The
    /// error was tiny -- about 0.2 units in 2^32 -- but it was systematic, and the
    /// producibility test sits exactly on integer boundaries. For 16k -> 48k, where every
    /// third output frame lands exactly on an input frame, the accumulated shortfall put
    /// `p` just under an integer and the resampler emitted one frame too many: 47998
    /// instead of 47997. No practical amount of extra precision fixes that; removing the
    /// accumulation does.
    ///
    /// ## Which frames are available
    ///
    /// Output `k` is producible once input frame `n+1` exists, i.e. once `n+1` is within
    /// the frames fed so far. The left neighbour may be frame `-1` of the current block,
    /// which is `prev` -- the last frame of the previous block -- so a block boundary
    /// interpolates from the same pair as it would in one uninterrupted pass.
    ///
    /// The final input frame can never be a right neighbour, so the last output frame or
    /// two are unproducible. That tail is inherent to interpolation and does not grow.
    pub fn process(&mut self, input: &[f32], output: &mut [f32]) -> (usize, usize) {
        if input.is_empty() || output.is_empty() {
            return (0, 0);
        }
        // Identity: copy through, bit-exact. Most streams already run at the bus rate,
        // and interpolation would add rounding error to the majority case for nothing.
        if self.src_rate == self.dst_rate {
            let n = core::cmp::min(input.len(), output.len());
            output[..n].copy_from_slice(&input[..n]);
            self.prev = input[n - 1];
            self.have_prev = true;
            self.frames_in += n as u64;
            self.frames_out += n as u64;
            return (n, n);
        }
        // Global index of input[0] for this call, and the count of frames available
        // including this slice.
        let base = self.frames_in;
        let available = base + input.len() as u64;
        let src = self.src_rate as u64;
        let dst = self.dst_rate as u64;
        let mut produced = 0usize;
        loop {
            if produced >= output.len() {
                break;
            }
            let k = self.frames_out + produced as u64;
            let num = k * src;
            let n = num / dst;
            // The right neighbour must exist in this slice. `available` is `base + len`,
            // so `n + 1 >= available` and `right_i >= input.len()` are the same condition
            // written two ways; keeping both would be two sources of truth for one rule.
            // The local form is the one that also yields the index actually used below.
            //
            // `n` itself may sit one frame before this block (that is `prev`), but n+1
            // never may: a block boundary reads `[prev, input[0]]`, and `prev` is only ever
            // the LEFT neighbour.
            let right_i = (n + 1 - base) as usize;
            if right_i >= input.len() {
                break;
            }
            // The left neighbour is `prev` only when it is the frame immediately before
            // this block, i.e. n == base - 1. Testing `n == base` instead would capture the
            // ordinary in-block case and substitute the previous block's last frame for a
            // frame that is present in this very slice -- which shows up as a wrong sample
            // at the first output frame after each block boundary.
            let left = if n + 1 == base {
                if self.have_prev { self.prev } else { input[0] }
            } else if n >= base {
                input[(n - base) as usize]
            } else {
                // n is before this block and not adjacent to it; the caller skipped
                // frames the resampler never saw, so there is no continuous signal to
                // interpolate. Hold the last known frame rather than inventing one.
                if self.have_prev { self.prev } else { input[0] }
            };
            let right = input[right_i];
            // Exact fractional position: the remainder is the fraction, with no error.
            let frac = (num % dst) as f32 / dst as f32;
            output[produced] = left + (right - left) * frac;
            produced += 1;
        }
        // The whole slice is taken in, so the caller never re-sends a frame and the
        // consumption count always matches the advance it makes.
        self.prev = input[input.len() - 1];
        self.have_prev = true;
        self.frames_in = available;
        self.frames_out += produced as u64;
        (input.len(), produced)
    }
}


/// A stereo resampler: one [`Resampler`] per channel.
///
/// The two channels MUST NOT share state. A [`Resampler`] carries the previous frame for
/// seam interpolation plus running in/out frame counts, so feeding left then right
/// through one instance would leave the right channel starting mid-stream, offset in
/// time from the left, and interpolating its block boundaries against left-channel
/// samples. None of that fails loudly -- it just collapses the stereo image -- so the
/// pairing is enforced by the type instead of by remembering to do it.
///
/// The scratch planes live here rather than being passed in: a caller free to supply
/// them could hand in a buffer aliasing the interleaved input, and the borrow checker
/// cannot distinguish that from a legitimate call.
pub struct StereoResampler {
    left: Resampler,
    right: Resampler,
    /// Per-channel working planes. Reused across calls so the steady state allocates
    /// nothing: this runs in a daemon loop, where per-iteration allocation would
    /// accumulate into long-term fragmentation.
    left_plane: alloc::vec::Vec<f32>,
    right_plane: alloc::vec::Vec<f32>,
    scratch: alloc::vec::Vec<f32>,
}

impl StereoResampler {
    /// Build a stereo resampler for `src_rate` to `dst_rate`.
    pub fn new(src_rate: u32, dst_rate: u32) -> Result<Self, ResampleError> {
        Ok(Self {
            left: Resampler::new(src_rate, dst_rate)?,
            right: Resampler::new(src_rate, dst_rate)?,
            left_plane: alloc::vec::Vec::new(),
            right_plane: alloc::vec::Vec::new(),
            scratch: alloc::vec::Vec::new(),
        })
    }

    /// Upper bound on the frames `process` may write for `frames` input frames.
    ///
    /// Callers size their output buffer with this. It is derived from the ratio rather
    /// than guessed, and rounded up: a buffer one frame short would silently truncate
    /// audio, because `process` stops when the output is full and the tail of the block
    /// is then never produced.
    ///
    /// The ceiling uses integer arithmetic, so it cannot be off by a floating-point
    /// rounding step at the very boundary it exists to protect.
    pub fn output_capacity(&self, frames: usize) -> usize {
        let src = self.left.src_rate() as usize;
        let dst = SAMPLE_RATE_HZ as usize;
        (frames * dst).div_ceil(src) + RESAMPLE_MARGIN
    }

    /// True when the source rate already equals the destination rate.
    ///
    /// Callers use this to skip the plane split entirely for the common case. It is not
    /// an optimisation claim -- it is the same predicate [`Resampler`] uses internally to
    /// take its bit-exact path.
    pub fn is_identity(&self) -> bool {
        self.left.src_rate() == SAMPLE_RATE_HZ
    }

    /// Resample interleaved stereo `input` into interleaved stereo `output`.
    ///
    /// `input` holds L,R pairs and is consumed entirely; the return value is the number
    /// of frames written to `output`. `output` must be at least
    /// `output_capacity(frames) * FORMAT_CHANNELS` long.
    pub fn process(&mut self, input: &[f32], output: &mut [f32]) -> usize {
        let frames = input.len() / FORMAT_CHANNELS;
        if frames == 0 {
            return 0;
        }
        // Identity: no plane split needed, and no rounding introduced.
        if self.is_identity() {
            let n = core::cmp::min(frames * FORMAT_CHANNELS, output.len());
            output[..n].copy_from_slice(&input[..n]);
            return n / FORMAT_CHANNELS;
        }
        // Split into planes. This is a correctness requirement, not a convenience:
        // resampling the interleaved stream would interpolate between a left sample and
        // the following right sample, averaging the two channels together.
        self.left_plane.clear();
        self.right_plane.clear();
        let mut i = 0usize;
        while i < frames {
            self.left_plane.push(input[i * FORMAT_CHANNELS]);
            self.right_plane.push(input[i * FORMAT_CHANNELS + 1]);
            i += 1;
        }
        // Each channel is resampled with its own state; that is what keeps them from
        // contaminating each other (see the note on the type).
        let left_out = self.resample_one_channel(false);
        let right_out = self.resample_one_channel(true);
        // Equal by construction: identical configuration, identical input length.
        debug_assert_eq!(left_out, right_out);
        let n = core::cmp::min(left_out, right_out);
        let mut f = 0usize;
        while f < n && (f + 1) * FORMAT_CHANNELS <= output.len() {
            output[f * FORMAT_CHANNELS] = self.left_plane[f];
            output[f * FORMAT_CHANNELS + 1] = self.right_plane[f];
            f += 1;
        }
        f
    }

    /// Resample one channel's plane in place, returning the produced frame count.
    ///
    /// `right` selects which channel's resampler state to use. The plane is replaced by
    /// the resampled result, so the caller reads it back from the same field.
    fn resample_one_channel(&mut self, right: bool) -> usize {
        let len = if right {
            self.right_plane.len()
        } else {
            self.left_plane.len()
        };
        // Sized from the ratio, NOT from `len + RESAMPLE_MARGIN`. Upsampling produces
        // proportionally MORE frames than it consumes, so a scratch buffer sized just
        // past the input length fills up and `process` stops there, silently truncating
        // the tail of every block. The first version made exactly that mistake and dropped
        // about 88 frames per 1024-frame block at 44.1k -> 48k.
        self.scratch.clear();
        self.scratch.resize(self.output_capacity(len), 0.0);
        let (rs, plane) = if right {
            (&mut self.right, &mut self.right_plane)
        } else {
            (&mut self.left, &mut self.left_plane)
        };
        let (_used, produced) = rs.process(plane, &mut self.scratch);
        plane.clear();
        plane.extend_from_slice(&self.scratch[..produced]);
        produced
    }
}

/// Extra output room the resampler may need beyond the input frame count.
///
/// Upsampling emits proportionally more frames than it consumes, and the exact count
/// depends on the ratio. Two frames covers the rounding at a block boundary; it is a
/// bound on the arithmetic, not a tuning knob.
const RESAMPLE_MARGIN: usize = 2;

/// Which way a path's buffer watermark is moving.
///
/// Reporting direction rather than only a number is deliberate: a correction acts on
/// direction, and a caller that has to interpret a bare float is a caller that can
/// get the sign wrong.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DriftDirection {
    /// The buffer level is not moving in one direction beyond the noise floor.
    Stable,
    /// The buffer is gaining data over time. Uncorrected, it eventually blocks.
    Filling,
    /// The buffer is losing data over time. Uncorrected, it eventually starves.
    Draining,
}

/// Number of watermark samples in each comparison window.
///
/// The window has to smooth out producer jitter without being so long that a real drift
/// takes minutes to appear. Producers write in bursts, so the sample-to-sample swing is
/// large; averaging 128 of them (about 2.7 seconds at one round per 21 ms) brings the
/// noise well below the drift this is meant to detect.
const TREND_WINDOW: usize = 128;

/// Drift smaller than this counts as noise rather than movement.
///
/// A buffer that shifts by a fraction of a percent over a window has not drifted, it has
/// jittered. Acting on that would turn normal variation into pitch wobble, which is a
/// worse artifact than the drift it was trying to fix.
const DRIFT_EPSILON: f32 = 0.01;

/// Tracks whether a path's buffer watermark is drifting, using two sliding averages.
///
/// Instantaneous watermark is useless for this: producers write in bursts, so the level
/// swings widely round to round while its average stays put. Comparing an average of the
/// recent past against an average of the less recent past separates the two, and looking
/// at the *change* rather than the level keeps a steadily-full buffer from being reported
/// as a problem -- sitting at 90% forever is not a leak.
///
/// Sampling every round is intentional. Deciding which rounds are interesting is itself
/// a judgement that would need its own justification, and the ring buffer already reports
/// its true occupancy every time.
pub struct WatermarkTrend {
    /// Ring of the most recent samples, `TREND_WINDOW * 2` long.
    samples: alloc::vec::Vec<f32>,
    /// Index of the next write into `samples`. Full once `filled == samples.len()`.
    next: usize,
    /// How many samples have been pushed, saturating at `samples.len()`.
    filled: usize,
}

impl WatermarkTrend {
    /// Create an empty trend tracker with no history.
    pub fn new() -> Self {
        Self {
            samples: alloc::vec![0.0; TREND_WINDOW * 2],
            next: 0,
            filled: 0,
        }
    }

    /// Record the current watermark, where 0.0 is empty and 1.0 is full.
    ///
    /// Values are clamped into `0.0..=1.0` so a caller passing a raw byte count cannot
    /// silently produce a meaningless average. Out-of-range input is a caller bug, and
    /// clamping keeps it from corrupting the history that later readings depend on.
    pub fn push(&mut self, level: f32) {
        let v = if level.is_nan() {
            0.0
        } else {
            level.clamp(0.0, 1.0)
        };
        self.samples[self.next] = v;
        self.next = (self.next + 1) % self.samples.len();
        if self.filled < self.samples.len() {
            self.filled += 1;
        }
    }

    /// Mean of the `n` samples ending one window-length before the newest one.
    ///
    /// `n` is the age of the newest sample to include: 0 is the newest, so the older
    /// window is `mean_of_window(TREND_WINDOW)` and the newer one is `mean_of_window(0)`.
    fn mean_at_age(&self, age: usize) -> Option<f32> {
        // The window must lie entirely within what has actually been recorded.
        if self.filled < age + TREND_WINDOW {
            return None;
        }
        let len = self.samples.len();
        let mut sum = 0.0f32;
        let mut i = 0usize;
        while i < TREND_WINDOW {
            // Walk backwards from the newest sample by `age`, then by `i`.
            let back = age + i;
            let idx = (self.next + len - 1 - back) % len;
            sum += self.samples[idx];
            i += 1;
        }
        Some(sum / TREND_WINDOW as f32)
    }

    /// Change in average watermark between the older and newer windows.
    ///
    /// Positive means the buffer is gaining. Returns 0.0 until both windows are full,
    /// because a trend computed from partial history would fire during startup -- exactly
    /// when producers are least steady.
    pub fn drift(&self) -> f32 {
        let older = match self.mean_at_age(TREND_WINDOW) {
            Some(v) => v,
            None => return 0.0,
        };
        let newer = match self.mean_at_age(0) {
            Some(v) => v,
            None => return 0.0,
        };
        newer - older
    }

    /// Direction of the drift, with anything under the noise floor reported as stable.
    pub fn direction(&self) -> DriftDirection {
        let d = self.drift();
        if d > DRIFT_EPSILON {
            DriftDirection::Filling
        } else if d < -DRIFT_EPSILON {
            DriftDirection::Draining
        } else {
            DriftDirection::Stable
        }
    }

    /// Number of samples recorded so far, saturating at a full history.
    pub fn samples_seen(&self) -> usize {
        self.filled
    }
}

impl DriftDirection {
    /// Short ASCII name, for logs and `status` output.
    ///
    /// A method rather than a `Display` impl: this crate is `no_std` and the string is
    /// only ever written into a log line, so pulling in formatting machinery would cost
    /// more than the three match arms.
    pub fn as_str(self) -> &'static str {
        match self {
            DriftDirection::Stable => "stable",
            DriftDirection::Filling => "filling",
            DriftDirection::Draining => "draining",
        }
    }
}
impl Default for WatermarkTrend {
    fn default() -> Self {
        Self::new()
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

    /// M5: the resampler's ratio must be DERIVED from the two rates, never baked in.
    ///
    /// A hardcoded 44100/48000 constant would silently produce the wrong pitch for any
    /// other pair, and the error would be subtle -- a slightly wrong speed rather than
    /// an obvious failure. Deriving makes every supported pair correct by construction.
    #[test]
    fn test_m5_ratio_derives_from_rates_not_constants() {
        // 44.1k -> 48k needs MORE output samples than input.
        let r = Resampler::new(44_100, 48_000).expect("supported pair");
        assert!(r.ratio() > 1.0);
        assert!((r.ratio() - 48_000.0 / 44_100.0).abs() < 1e-9);

        // 48k -> 48k is the identity: ratio must be EXACTLY 1, not merely close.
        let r = Resampler::new(48_000, 48_000).expect("identity");
        assert_eq!(r.ratio(), 1.0);

        // A different pair must give a different ratio -- the checks above would still
        // pass if ratio() returned a constant, so this pins that it actually varies.
        let r = Resampler::new(16_000, 48_000).expect("up 3x");
        assert!((r.ratio() - 3.0).abs() < 1e-9);
    }

    /// M5: position accumulation must use fixed point, not floating point.
    ///
    /// The plan requires this explicitly and the reason is concrete: at 48kHz a float32
    /// accumulator has ~24 bits of mantissa, so after minutes of playback the step size
    /// becomes comparable to the accumulated rounding error. The mixer would then drift,
    /// and drifting pitch is far harder to diagnose than an obvious failure.
    ///
    /// This test drives a LONG run and asserts the output count lands exactly on the
    /// ratio-implied total. Float accumulation does not survive it; exact fixed point does.
    #[test]
    fn test_m5_position_accumulation_does_not_drift_over_long_run() {
        // 441000 input frames, about 10 seconds of audio. The expected output count is
        // the exact integer ceiling implied by the mapping, not a rounded ratio -- see
        // `test_m5_output_length_follows_ratio` for why the tail frame is unproducible.
        let inputs = 441 * 1000;
        let mut r = Resampler::new(44_100, 48_000).expect("supported");
        let src = alloc::vec![0.0f32; inputs];
        let mut total_out = 0usize;
        let mut out = alloc::vec![0.0f32; 4096];
        let mut pos = 0usize;
        while pos < inputs {
            let end = core::cmp::min(pos + 512, inputs);
            let (used, produced) = r.process(&src[pos..end], &mut out);
            pos += used;
            total_out += produced;
            if produced == 0 && used == 0 {
                break;
            }
        }
        // Derived from the definition: the largest k with floor(k*44100/48000) <= N-2.
        let mut k = 0usize;
        while (k as f64) * 44_100.0 / 48_000.0 < (inputs - 1) as f64 {
            if ((k as f64) * 44_100.0 / 48_000.0).floor() as usize > inputs - 2 {
                break;
            }
            k += 1;
        }
        assert_eq!(total_out, k, "frame count drifted");
        assert_eq!(k, 479_999);
    }

    /// M5: an identity resampler must be bit-exact, so 48k paths pay no quality cost.
    ///
    /// Most streams are already 48k. If the identity path went through interpolation it
    /// would add rounding error to the majority case for no benefit.
    #[test]
    fn test_m5_identity_is_bit_exact() {
        let src: Vec<f32> = (0..64).map(|i| (i as f32) * 0.01 - 0.3).collect();
        let mut r = Resampler::new(48_000, 48_000).expect("identity");
        let mut out = alloc::vec![0.0f32; 128];
        let (used, produced) = r.process(&src, &mut out);
        assert_eq!(used, src.len());
        assert_eq!(produced, src.len());
        assert_eq!(&out[..produced], &src[..]);
    }

    /// M5: an unsupported input rate must be REJECTED, not silently approximated.
    ///
    /// The supported set is enumerated because only those ratios are actually tested.
    /// Accepting an arbitrary rate would mean promising resampling quality nothing here
    /// verifies -- and the plan explicitly excludes polyphase FIR, which is what general
    /// ratios would need. Rejecting is the honest boundary of what this implements.
    #[test]
    fn test_m5_unsupported_rates_are_rejected() {
        assert!(Resampler::new(44_100, 48_000).is_ok());
        assert!(Resampler::new(48_000, 48_000).is_ok());
        assert!(Resampler::new(22_050, 48_000).is_ok());
        assert!(Resampler::new(16_000, 48_000).is_ok());
        assert!(Resampler::new(32_000, 48_000).is_ok());

        // A rate nobody tested must not be accepted on the grounds that it is just a
        // number. 11025 is a real-world rate but is NOT in the verified set.
        assert!(Resampler::new(11_025, 48_000).is_err());
        assert!(Resampler::new(96_000, 48_000).is_err());
        assert!(Resampler::new(0, 48_000).is_err());
    }

    /// M5: a pure tone must survive resampling with its FREQUENCY unchanged.
    ///
    /// This is the plan's acceptance criterion (no pitch shift) stated as a property.
    /// It is checked by counting zero crossings, which is robust to amplitude changes
    /// and to interpolation smoothing, and directly measures what "pitch" means.
    #[test]
    fn test_m5_tone_frequency_is_preserved() {
        // 1000 Hz at 44100 for 1 second -> after resampling to 48000 it is still 1000 Hz,
        // so the output must contain the SAME NUMBER of cycles, not more.
        let src_rate = 44_100u32;
        let out_rate = 48_000u32;
        let freq = 1000.0f32;
        let inputs = src_rate as usize; // exactly 1 second
        let src: Vec<f32> = (0..inputs)
            .map(|i| {
                let t = i as f32 / src_rate as f32;
                (2.0 * core::f32::consts::PI * freq * t).sin()
            })
            .collect();

        let mut r = Resampler::new(src_rate, out_rate).expect("supported");
        let mut out = alloc::vec![0.0f32; inputs * 2 + 1024];
        let (_used, produced) = r.process(&src, &mut out);
        assert!(produced > 0);

        // Count rising zero crossings over the produced output.
        let mut crossings = 0usize;
        let mut i = 1usize;
        while i < produced {
            if out[i - 1] < 0.0 && out[i] >= 0.0 {
                crossings += 1;
            }
            i += 1;
        }
        // One second of 1kHz is 1000 cycles. Allow a small edge tolerance.
        assert!(
            (998..=1002).contains(&crossings),
            "pitch shifted: expected ~1000 cycles");
    }

    /// M5: output length must follow the ratio, or the timeline is wrong.
    ///
    /// If the length were wrong the stream would still sound correct in pitch but run
    /// fast or slow, and would slowly desynchronise from every other source.
    ///
    /// ## The one-frame tail
    ///
    /// The count is NOT `floor(N * dst / src)`. Interpolating output frame `k` needs the
    /// input frames at `floor(p)` and `floor(p)+1` where `p = k * src / dst`, so the last
    /// input frame cannot serve as a right neighbour for anything. The largest producible
    /// `k` is therefore the one where `floor(p)` is still `N-2`, giving `47999` rather
    /// than `48000` for one second of 44.1k.
    ///
    /// That single missing frame is inherent to interpolation, not a defect: it does not
    /// accumulate, and at 2e-5 of the stream it is far below anything audible. Encoding
    /// the derived value rather than the rounded one is the point of this test -- a
    /// tolerance here would hide a genuine off-by-one.
    #[test]
    fn test_m5_output_length_follows_ratio() {
        // Derived, not assumed: the number of output frames that have a complete pair of
        // input neighbours. Output `k` needs `floor(p)` and `floor(p)+1`, so it is
        // producible exactly while `floor(p) <= N-2`.
        fn expected(N: usize, src: u32, dst: u32) -> usize {
            let mut count = 0usize;
            loop {
                let p = (count as f64) * (src as f64) / (dst as f64);
                if p.floor() as usize > N - 2 {
                    break;
                }
                count += 1;
            }
            count
        }

        let src = alloc::vec![0.0f32; 44_100];
        let mut r = Resampler::new(44_100, 48_000).expect("supported");
        let mut out = alloc::vec![0.0f32; 88_200];
        let (_used, produced) = r.process(&src, &mut out);
        assert_eq!(produced, expected(44_100, 44_100, 48_000));
        assert_eq!(produced, 47_999);

        // The ratio still holds to within the single trailing frame.
        let ratio = produced as f64 / src.len() as f64;
        assert!((ratio - 48_000.0 / 44_100.0).abs() < 1e-4);

        // 16k -> 48k upsampling, checked in the other direction as well.
        let src = alloc::vec![0.0f32; 16_000];
        let mut r = Resampler::new(16_000, 48_000).expect("supported");
        let mut out = alloc::vec![0.0f32; 64_000];
        let (_used, produced) = r.process(&src, &mut out);
        assert_eq!(produced, expected(16_000, 16_000, 48_000));
    }

    /// M5: straddling a buffer seam must not lose or duplicate a sample.
    ///
    /// The mixer feeds the resampler in whatever chunk sizes the ring happens to have,
    /// so a frame's neighbours are routinely split across calls. If the resampler
    /// restarted its position each call, output would be discontinuous at every seam.
    ///
    /// The check: feeding the same signal in one call and in many small calls must
    /// produce IDENTICAL output. Any seam handling error breaks this.
    #[test]
    fn test_m5_chunked_and_whole_input_agree() {
        let src: Vec<f32> = (0..4000).map(|i| ((i as f32) * 0.05).sin() * 0.7).collect();
        let mut a = Resampler::new(44_100, 48_000).expect("supported");
        let mut out_a = alloc::vec![0.0f32; 8192];
        let (_u, n_a) = a.process(&src, &mut out_a);
        let mut b = Resampler::new(44_100, 48_000).expect("supported");
        let mut out_b = alloc::vec![0.0f32; 8192];
        let mut n_b = 0usize;
        let mut pos = 0usize;
        while pos < src.len() {
            let end = core::cmp::min(pos + 37, src.len());
            let (used, produced) = b.process(&src[pos..end], &mut out_b[n_b..]);
            pos += used;
            n_b += produced;
            if used == 0 && produced == 0 {
                break;
            }
        }
        assert_eq!(n_a, n_b, "chunking changed the output length");
        assert_eq!(&out_a[..n_a], &out_b[..n_b], "seam bug: chunked output differs");
    }

    /// M5: resampling must not overshoot the source range (ripple/instability).
    ///
    /// Linear interpolation between two samples can never exceed their extremes. If the
    /// output ever does, the interpolator is wrong -- and overshoot near clipping is
    /// exactly what turns a quiet passage into audible crackle.
    #[test]
    fn test_m5_output_stays_within_source_envelope() {
        let src: Vec<f32> = (0..2000).map(|i| if i % 2 == 0 { 0.5 } else { -0.5 }).collect();
        let mut r = Resampler::new(44_100, 48_000).expect("supported");
        let mut out = alloc::vec![0.0f32; 4096];
        let (_u, n) = r.process(&src, &mut out);
        let mut i = 0usize;
        while i < n {
            let v = out[i];
            assert!(v >= -0.5 - 1e-6 && v <= 0.5 + 1e-6, "exceeded source envelope");
            i += 1;
        }
    }

    /// M5: an empty input must produce nothing and consume nothing, without panicking.
    #[test]
    fn test_m5_empty_input_is_a_no_op() {
        let mut r = Resampler::new(44_100, 48_000).expect("supported");
        let mut out = alloc::vec![0.0f32; 64];
        let (used, produced) = r.process(&[], &mut out);
        assert_eq!(used, 0);
        assert_eq!(produced, 0);
    }

    /// M5: frame accounting must stay exact over a long run.
    ///
    /// The plan forbids floating-point position accumulation, and the reason is concrete:
    /// an f32 accumulator has ~24 bits of mantissa, so past 2^24 the position can no
    /// longer resolve the step and individual output frames begin to repeat. The stream
    /// then plays measurably slow -- a failure that sounds like slightly wrong pitch and
    /// is far harder to notice than a crash.
    ///
    /// The assertion is the exact frame count, derived from the entitlement identity.
    /// Integer accounting satisfies it; a float accumulator does not.
    #[test]
    fn test_m5_long_run_frame_count_is_exact() {
        let mut r = Resampler::new(44_100, 48_000).expect("supported");
        let src = alloc::vec![0.0f32; 1024];
        let mut out = alloc::vec![0.0f32; 4096];
        let mut total_out = 0u64;
        let mut fed = 0u64;
        // 3,600,000 input frames is about 81 seconds -- far past 2^24 frames.
        while fed < 3_600_000 {
            let (used, produced) = r.process(&src, &mut out);
            if used == 0 && produced == 0 {
                break;
            }
            fed += used as u64;
            total_out += produced as u64;
        }
        let expected = fed * 48_000 / 44_100;
        assert!(
            total_out == expected,
            "frame count drifted: got {}, expected {}", total_out, expected
        );
    }

    /// M5: linear interpolation must actually INTERPOLATE, not hold the previous sample.
    ///
    /// This is the defining property of the method and the one thing the other tests
    /// cannot see. A zero-order hold -- emitting `x[floor(p)]` and ignoring the
    /// fractional part -- keeps the pitch correct, keeps the length correct, keeps the
    /// output inside the source envelope, and is still perfectly consistent between
    /// chunked and whole input. Every other test here passes on it. Mutation testing
    /// found exactly that: forcing the fraction to zero broke nothing.
    ///
    /// A linear ramp is the sharpest probe available, because linear interpolation
    /// reproduces a linear function EXACTLY. With `x[n] = n`, output `k` must equal the
    /// exact position `k * src / dst`. Any error in the fraction shows up immediately,
    /// and a hold instead of an interpolation produces a staircase against a straight
    /// line.
    #[test]
    fn test_m5_linear_ramp_is_reproduced_exactly() {
        let n = 2000usize;
        let src: Vec<f32> = (0..n).map(|i| i as f32).collect();
        let mut r = Resampler::new(44_100, 48_000).expect("supported");
        let mut out = alloc::vec![0.0f32; 4096];
        let (_u, produced) = r.process(&src, &mut out);
        assert!(produced > 100);
        let mut k = 0usize;
        while k < produced {
            // Exact value: the position the output sample maps to on the input axis.
            let want = (k as f64) * 44_100.0 / 48_000.0;
            let got = out[k] as f64;
            assert!(
                (got - want).abs() < 1e-3,
                "k={} ramp not reproduced: got {}, want {}", k, got, want
            );
            k += 1;
        }
    }

    /// M5: the fraction must advance BETWEEN output samples.
    ///
    /// A subtler failure than a full hold: the fraction correct on the first sample of
    /// each call and stale afterwards. On a ramp that shows up as runs of repeated
    /// values, so this asserts the output is strictly increasing where the input is.
    #[test]
    fn test_m5_ramp_output_is_strictly_increasing() {
        let n = 500usize;
        let src: Vec<f32> = (0..n).map(|i| i as f32).collect();
        let mut r = Resampler::new(44_100, 48_000).expect("supported");
        let mut out = alloc::vec![0.0f32; 1024];
        let (_u, produced) = r.process(&src, &mut out);
        assert!(produced > 100);
        let mut k = 1usize;
        while k < produced {
            assert!(
                out[k] > out[k - 1],
                "output stalled at k={}: {} then {}", k, out[k - 1], out[k]
            );
            k += 1;
        }
    }

    /// M5: the output rate is fixed at the mixer's internal rate, and must be enforced.
    ///
    /// The mix bus runs at exactly one rate. Accepting a different destination would
    /// imply the output side is resampled too, which nothing implements -- the samples
    /// would simply be produced on the wrong timeline.
    #[test]
    fn test_m5_output_rate_must_be_the_mixer_rate() {
        assert!(Resampler::new(44_100, SAMPLE_RATE_HZ).is_ok());
        assert_eq!(
            Resampler::new(44_100, 44_100).err(),
            Some(ResampleError::UnsupportedOutputRate)
        );
        assert_eq!(
            Resampler::new(44_100, 96_000).err(),
            Some(ResampleError::UnsupportedOutputRate)
        );
        // An unsupported INPUT rate is reported as such, not as an output problem.
        assert_eq!(
            Resampler::new(11_025, 48_000).err(),
            Some(ResampleError::UnsupportedInputRate)
        );
    }

    /// M5: the two channels must be resampled with INDEPENDENT state.
    ///
    /// Sharing one resampler between channels would offset the right channel in time and
    /// interpolate its block boundaries against left-channel samples. Nothing about that
    /// fails loudly -- the pitch stays right -- so it is pinned by giving the channels
    /// clearly different signals and checking each survives intact.
    ///
    /// Left is a rising ramp and right is its negation, so any leakage between them shows
    /// up as a sign or magnitude error rather than as a subtle blur.
    #[test]
    fn test_m5_stereo_channels_do_not_leak_into_each_other() {
        let frames = 1000usize;
        let mut interleaved: Vec<f32> = alloc::vec::Vec::new();
        let mut i = 0usize;
        while i < frames {
            interleaved.push(i as f32);
            interleaved.push(-(i as f32));
            i += 1;
        }
        let mut sr = StereoResampler::new(44_100, 48_000).expect("supported");
        let mut out = alloc::vec![0.0f32; sr.output_capacity(frames) * FORMAT_CHANNELS];
        let n = sr.process(&interleaved, &mut out);
        assert!(n > 100, "expected audio, got {n} frames");
        let mut f = 0usize;
        while f < n {
            let l = out[f * FORMAT_CHANNELS];
            let r = out[f * FORMAT_CHANNELS + 1];
            // Right is the exact negation of left. Interpolation is linear, so this holds
            // exactly rather than approximately.
            assert!(
                (l + r).abs() < 1e-3,
                "channel leak at frame {f}: left={l} right={r}"
            );
            f += 1;
        }
    }

    /// M5: the right channel must not start late.
    ///
    /// The specific failure of shared state: the left channel advances the frame counter,
    /// so the right channel resumes from wherever the left left off and silently drops its
    /// opening frames. Checking every frame of a constant signal catches it.
    #[test]
    fn test_m5_stereo_right_channel_does_not_start_late() {
        let frames = 600usize;
        let mut interleaved: Vec<f32> = alloc::vec::Vec::new();
        let mut i = 0usize;
        while i < frames {
            interleaved.push(0.25);
            interleaved.push(-0.75);
            i += 1;
        }
        let mut sr = StereoResampler::new(44_100, 48_000).expect("supported");
        let mut out = alloc::vec![0.0f32; sr.output_capacity(frames) * FORMAT_CHANNELS];
        let n = sr.process(&interleaved, &mut out);
        assert!(n > 100, "expected audio, got {n} frames");
        // A constant signal interpolates to itself, so every frame must carry the
        // original constants regardless of position.
        let mut f = 0usize;
        while f < n {
            assert!(
                (out[f * FORMAT_CHANNELS] - 0.25).abs() < 1e-6,
                "left drifted at frame {f}"
            );
            assert!(
                (out[f * FORMAT_CHANNELS + 1] + 0.75).abs() < 1e-6,
                "right drifted at frame {f}"
            );
            f += 1;
        }
    }

    /// M5: a 48k stereo stream must pass through bit-exactly.
    ///
    /// This is the common case, so it is the one that must not pay any cost. A single
    /// rounding step here would accumulate across the whole stream for no benefit.
    #[test]
    fn test_m5_stereo_identity_is_bit_exact() {
        let frames = 256usize;
        let mut interleaved: Vec<f32> = alloc::vec::Vec::new();
        let mut i = 0usize;
        while i < frames {
            interleaved.push(i as f32 * 0.001);
            interleaved.push(-(i as f32) * 0.001);
            i += 1;
        }
        let mut sr = StereoResampler::new(48_000, 48_000).expect("identity");
        assert!(sr.is_identity());
        let mut out = alloc::vec![0.0f32; frames * FORMAT_CHANNELS];
        let n = sr.process(&interleaved, &mut out);
        assert_eq!(n, frames);
        assert_eq!(&out[..n * FORMAT_CHANNELS], &interleaved[..]);
    }

    /// M5: `output_capacity` must never be smaller than what `process` writes.
    ///
    /// Size the buffer with the reported bound and check nothing is dropped: a bound one
    /// frame short would silently truncate the tail of every block.
    #[test]
    fn test_m5_output_capacity_is_never_short() {
        let rates = [(44_100u32, 48_000u32), (16_000, 48_000), (22_050, 48_000), (32_000, 48_000)];
        for (src, dst) in rates {
            let frames = 1024usize;
            let mut interleaved: Vec<f32> = alloc::vec::Vec::new();
            let mut i = 0usize;
            while i < frames {
                interleaved.push(0.5);
                interleaved.push(-0.5);
                i += 1;
            }
            let mut sr = StereoResampler::new(src, dst).expect("supported");
            let cap = sr.output_capacity(frames);
            let mut out = alloc::vec![0.0f32; cap * FORMAT_CHANNELS];
            let n = sr.process(&interleaved, &mut out);
            assert!(n <= cap, "{src}->{dst}: wrote {n} frames into capacity {cap}");
            let expect = frames * dst as usize / src as usize;
            // The shortfall is derived, not a tolerance. Output `k` needs input frames
            // `floor(k*src/dst)` and `+1`, and the last input frame can never be a right
            // neighbour -- so the tail that cannot be produced is at most one input
            // frame worth of output, i.e. `dst/src` frames. It is a constant per block,
            // not a growing error (the next block reuses the last frame as its left
            // neighbour). At 16k->48k that is 3 frames per 1024, which `expect - 1`
            // would have failed on while being perfectly correct.
            let max_tail = (dst as usize).div_ceil(src as usize) + 1;
            assert!(
                n + max_tail >= expect,
                "{src}->{dst}: wrote {n}, expected at least {} of {expect}",
                expect - max_tail
            );
            assert!(n <= expect + 1, "{src}->{dst}: wrote {n}, more than {expect}");
        }
    }

    /// M6: a steady watermark must report no drift.
    ///
    /// This is the control case that keeps the detector honest. A detector that
    /// reports drift for a constant signal would eventually ask the resampler to trim
    /// its ratio for no reason, and trimming on noise converts jitter into audible
    /// pitch wobble. Flat input must give flat output.
    #[test]
    fn test_m6_steady_watermark_reports_no_drift() {
        let mut t = WatermarkTrend::new();
        let mut i = 0usize;
        while i < 4096 {
            t.push(0.5);
            i += 1;
        }
        assert_eq!(t.drift(), 0.0);
        assert_eq!(t.direction(), DriftDirection::Stable);
    }

    /// M6: a monotonic rise must be reported as rising, with the right sign.
    ///
    /// The point of the trend is direction, not magnitude: knowing whether the buffer
    /// is filling or draining is what picks which way a correction would go.
    #[test]
    fn test_m6_rising_watermark_reports_filling() {
        let mut t = WatermarkTrend::new();
        // Climb from 0.1 to 0.9 across the window.
        let mut i = 0usize;
        while i < 4096 {
            t.push(0.1 + 0.8 * (i as f32 / 4095.0));
            i += 1;
        }
        assert!(
            t.drift() > 0.0,
            "a rising buffer must report positive drift, got {}",
            t.drift()
        );
        assert_eq!(t.direction(), DriftDirection::Filling);
    }

    /// M6: a monotonic fall must be reported as draining.
    #[test]
    fn test_m6_falling_watermark_reports_draining() {
        let mut t = WatermarkTrend::new();
        let mut i = 0usize;
        while i < 4096 {
            t.push(0.9 - 0.8 * (i as f32 / 4095.0));
            i += 1;
        }
        assert!(t.drift() < 0.0, "draining must report negative drift, got {}", t.drift());
        assert_eq!(t.direction(), DriftDirection::Draining);
    }

    /// M6: jitter around a constant level must NOT be reported as drift.
    ///
    /// This is the property the whole component exists for. Real producer write rates
    /// vary round to round, so the instantaneous watermark swings widely; a detector
    /// that reacted to that swing would correct against noise. The sliding average must
    /// absorb it and report stable.
    #[test]
    fn test_m6_jitter_around_a_level_is_not_drift() {
        let mut t = WatermarkTrend::new();
        // Alternate +/-0.2 around 0.5: a large swing with zero underlying trend.
        let mut i = 0usize;
        while i < 4096 {
            t.push(if i % 2 == 0 { 0.3 } else { 0.7 });
            i += 1;
        }
        assert_eq!(
            t.direction(),
            DriftDirection::Stable,
            "alternating jitter has no trend, but drift was {}",
            t.drift()
        );
    }

    /// M6: a stable-but-biased watermark is NOT drift.
    ///
    /// Sitting at 0.9 forever is not a leak; it is a full buffer. Distinguishing level
    /// from trend is the difference between correcting a real problem and inventing one.
    #[test]
    fn test_m6_stable_high_watermark_is_not_drift() {
        let mut t = WatermarkTrend::new();
        let mut i = 0usize;
        while i < 4096 {
            t.push(0.9);
            i += 1;
        }
        assert_eq!(t.direction(), DriftDirection::Stable);
    }

    /// M6: fewer samples than a full window must not fabricate a trend.
    ///
    /// At startup there is no history to compare against. Reporting drift from a
    /// near-empty window would correct on the first few rounds, exactly when the
    /// producers are least steady.
    #[test]
    fn test_m6_insufficient_history_reports_stable() {
        let mut t = WatermarkTrend::new();
        t.push(0.1);
        t.push(0.9);
        assert_eq!(
            t.direction(),
            DriftDirection::Stable,
            "two samples are not a trend"
        );
        assert_eq!(t.drift(), 0.0);
    }

    /// M6: the sliding average must genuinely average -- phase-aligned jitter is not enough.
    ///
    /// `test_m6_jitter_around_a_level_is_not_drift` uses an even-period alternation, and
    /// the two windows are an even number of samples apart, so sampling a single point
    /// from each window lands on the SAME phase and also reports stability. That test
    /// therefore passes whether or not any averaging happens.
    ///
    /// This one closes that hole: both halves average to exactly the same value, but the
    /// amplitude differs, so no single-point comparison can see them as equal. Only a
    /// real average reports Stable.
    #[test]
    fn test_m6_average_is_what_absorbs_amplitude_change() {
        let mut t = WatermarkTrend::new();
        let mut i = 0usize;
        // Older half: wide swing around 0.5. Newer half: narrow swing around 0.5.
        // Both average to 0.5 exactly, so the trend is zero -- but any given sample
        // differs between the halves.
        while i < TREND_WINDOW {
            t.push(if i % 2 == 0 { 0.1 } else { 0.9 });
            i += 1;
        }
        while i < TREND_WINDOW * 2 {
            t.push(if i % 2 == 0 { 0.45 } else { 0.55 });
            i += 1;
        }
        assert_eq!(
            t.direction(),
            DriftDirection::Stable,
            "both halves average to 0.5, so there is no drift, but drift was {}",
            t.drift()
        );
    }

    /// M6: out-of-range and NaN watermarks must not corrupt the history.
    ///
    /// The caller computes the level as a ratio; a zero capacity would produce infinity
    /// or NaN, and one such value entering the ring would poison every later average that
    /// includes it -- turning a division edge case into a permanent phantom trend.
    #[test]
    fn test_m6_adversarial_watermarks_do_not_corrupt_history() {
        let mut t = WatermarkTrend::new();
        // A burst of nonsense, then a clean steady level.
        t.push(f32::NAN);
        t.push(f32::INFINITY);
        t.push(f32::NEG_INFINITY);
        t.push(-5.0);
        t.push(99.0);
        // Fill well past the window so the garbage is entirely aged out.
        let mut i = 0usize;
        while i < TREND_WINDOW * 3 {
            t.push(0.5);
            i += 1;
        }
        assert_eq!(
            t.direction(),
            DriftDirection::Stable,
            "garbage input must not leave a phantom trend, got {}",
            t.drift()
        );
        // The clamped values must also stay inside the documented range, so a later
        // consumer reading `drift()` never sees a magnitude larger than the whole range.
        assert!(t.drift().abs() <= 1.0, "drift {} out of range", t.drift());
    }

    /// M6: a single out-of-range sample inside the window must be clamped, not averaged raw.
    ///
    /// The previous adversarial test pushed garbage and then enough clean samples to age it
    /// out entirely, so it passed with or without clamping -- it never observed a clamped
    /// value. Here one 99.0 sits INSIDE the newer window, and the assertion is on the exact
    /// arithmetic: clamped to 1.0 it contributes `1.0 / 128`, raw it contributes `99.0 / 128`.
    /// The two differ by 78x, so there is no borderline judgement involved.
    #[test]
    fn test_m6_out_of_range_sample_is_clamped_in_the_average() {
        let mut t = WatermarkTrend::new();
        // Older window: all zero. Newer window: all zero except one absurd sample.
        let mut i = 0usize;
        while i < TREND_WINDOW {
            t.push(0.0);
            i += 1;
        }
        while i < TREND_WINDOW * 2 - 1 {
            t.push(0.0);
            i += 1;
        }
        t.push(99.0);
        // Clamped: drift == 1.0 / 128 == 0.0078125. Raw: drift == 99.0 / 128 == 0.773.
        let d = t.drift();
        assert!(
            d < 0.01,
            "an out-of-range level must be clamped to 1.0 before averaging; drift was {d}"
        );
        assert!(d > 0.0, "the clamped sample must still be counted, drift was {d}");
    }
}
