//! Pure, allocation-free event generation: note values, beat ranges and the
//! per-lane scan.
//!
//! Like `model`, this module is deliberately free of nice-plug types so it
//! can be unit-tested without the framework.

use std::fmt;

use crate::model::{Lane, LaneData, SeqData, Step, clamp_u8};

/// A base note value, from a whole note down to a 1/64th.
///
/// The discriminant is the number of halvings from a whole note, so
/// `base_beats = 4.0 / 2^d` — exact in binary floating point for every
/// variant.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
#[repr(u8)]
pub enum NoteValue {
    Whole = 0,
    Half = 1,
    Quarter = 2,
    Eighth = 3,
    #[default]
    Sixteenth = 4,
    ThirtySecond = 5,
    SixtyFourth = 6,
}

impl NoteValue {
    /// Longest first — the order the GUI's rate dropdown lists them in.
    pub const ALL: [NoteValue; 7] = [
        NoteValue::Whole,
        NoteValue::Half,
        NoteValue::Quarter,
        NoteValue::Eighth,
        NoteValue::Sixteenth,
        NoteValue::ThirtySecond,
        NoteValue::SixtyFourth,
    ];

    /// The unmodified length in beats: 1/1 = 4.0 beats (one 4/4 bar), halving
    /// down to 1/64 = 0.0625.
    pub const fn beats(self) -> f64 {
        // Scaling the power of two rather than dividing keeps this exact and
        // const-evaluable.
        4.0 / (1u32 << self as u32) as f64
    }

    /// The denominator in the "1/n" name: 1, 2, 4 … 64.
    pub const fn denominator(self) -> u32 {
        1u32 << self as u32
    }
}

impl fmt::Display for NoteValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "1/{}", self.denominator())
    }
}

/// What a note value is multiplied by: nothing, a triplet (×2/3) or a dot
/// (×3/2).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
#[repr(u8)]
pub enum Modifier {
    #[default]
    Straight = 0,
    Triplet = 1,
    Dotted = 2,
}

impl Modifier {
    pub const ALL: [Modifier; 3] = [Modifier::Straight, Modifier::Triplet, Modifier::Dotted];

    /// The suffix the GUI appends to the note value: "", "T", "D".
    pub const fn suffix(self) -> &'static str {
        match self {
            Modifier::Straight => "",
            Modifier::Triplet => "T",
            Modifier::Dotted => "D",
        }
    }
}

impl fmt::Display for Modifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.suffix())
    }
}

/// One lane's step length: a note value crossed with a modifier, 21 variants
/// in all.
///
/// This is stored as a single `u8` discriminant in `LaneData.rate` and in the
/// state snapshot, so the layout is part of the wire format: the discriminant
/// is `note_value * 3 + modifier`, base and modifier packed into one byte.
/// Variants are ordered longest note value first, straight/triplet/dotted
/// within each, which is also the order the GUI dropdown lists.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
#[repr(u8)]
pub enum Rate {
    Whole = 0,
    WholeTriplet = 1,
    WholeDotted = 2,
    Half = 3,
    HalfTriplet = 4,
    HalfDotted = 5,
    Quarter = 6,
    QuarterTriplet = 7,
    QuarterDotted = 8,
    Eighth = 9,
    EighthTriplet = 10,
    EighthDotted = 11,
    #[default]
    Sixteenth = 12,
    SixteenthTriplet = 13,
    SixteenthDotted = 14,
    ThirtySecond = 15,
    ThirtySecondTriplet = 16,
    ThirtySecondDotted = 17,
    SixtyFourth = 18,
    SixtyFourthTriplet = 19,
    SixtyFourthDotted = 20,
}

impl Rate {
    /// What a lane gets when nothing says otherwise, and what an unknown
    /// discriminant from a snapshot decodes to.
    pub const DEFAULT: Rate = Rate::Sixteenth;

    /// Every variant, in dropdown order. `ALL[d as usize] == d` for every
    /// discriminant, which is what makes [`Rate::from_u8`] a table lookup.
    pub const ALL: [Rate; 21] = [
        Rate::Whole,
        Rate::WholeTriplet,
        Rate::WholeDotted,
        Rate::Half,
        Rate::HalfTriplet,
        Rate::HalfDotted,
        Rate::Quarter,
        Rate::QuarterTriplet,
        Rate::QuarterDotted,
        Rate::Eighth,
        Rate::EighthTriplet,
        Rate::EighthDotted,
        Rate::Sixteenth,
        Rate::SixteenthTriplet,
        Rate::SixteenthDotted,
        Rate::ThirtySecond,
        Rate::ThirtySecondTriplet,
        Rate::ThirtySecondDotted,
        Rate::SixtyFourth,
        Rate::SixtyFourthTriplet,
        Rate::SixtyFourthDotted,
    ];

    /// The base note value this rate modifies.
    pub const fn note_value(self) -> NoteValue {
        NoteValue::ALL[(self as usize) / 3]
    }

    /// The modifier applied to [`Rate::note_value`].
    pub const fn modifier(self) -> Modifier {
        Modifier::ALL[(self as usize) % 3]
    }

    /// The inverse of [`Rate::note_value`] + [`Rate::modifier`].
    pub const fn from_parts(note_value: NoteValue, modifier: Modifier) -> Rate {
        Rate::ALL[(note_value as usize) * 3 + modifier as usize]
    }

    /// The step length in beats: 1/1 = 4.0, 1/16 = 0.25, a triplet is the
    /// base × 2/3 and a dotted note the base × 3/2.
    ///
    /// Callable both ways — `rate.rate_beats()` and, in free-function
    /// spelling, `Rate::rate_beats(rate)`.
    pub const fn rate_beats(self) -> f64 {
        let base = self.note_value().beats();
        match self.modifier() {
            Modifier::Straight => base,
            Modifier::Triplet => base * 2.0 / 3.0,
            // ×1.5 is exact in binary, so a dotted note is as precise as its
            // base.
            Modifier::Dotted => base * 3.0 / 2.0,
        }
    }

    /// The discriminant stored in `LaneData.rate` and in the snapshot.
    pub const fn to_u8(self) -> u8 {
        self as u8
    }

    /// Decodes a stored discriminant. Anything out of range — a hostile
    /// snapshot, or a rate written by a future build that knows more variants
    /// — becomes [`Rate::DEFAULT`] rather than panicking.
    pub const fn from_u8(raw: u8) -> Rate {
        if (raw as usize) < Rate::ALL.len() {
            Rate::ALL[raw as usize]
        } else {
            Rate::DEFAULT
        }
    }
}

impl fmt::Display for Rate {
    /// "1/16", "1/8T", "1/4D" — the label in the GUI's rate dropdown.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}", self.note_value(), self.modifier())
    }
}

/// The plugin's random source: a free-running xorshift64\* generator.
///
/// One of these lives in the engine and is **never reseeded** — not on a
/// transport jump, not on a pattern switch — because step probability is
/// meant to be non-repeating from one bar to the next. Tests get determinism
/// by constructing their own with a fixed seed.
///
/// Deliberately not the `rand` crate: this is called from `process()`, so it
/// has to be allocation-free and a handful of instructions.
#[derive(Clone, Debug)]
pub struct Prng(u64);

impl Prng {
    /// The seed used when nothing in particular is asked for. Any non-zero
    /// constant does; this is the odd 64-bit golden-ratio constant.
    pub const DEFAULT_SEED: u64 = 0x9E37_79B9_7F4A_7C15;

    /// A generator seeded with `seed`.
    ///
    /// The xorshift state must be non-zero — zero is a fixed point that would
    /// return 0 for ever — so a zero seed silently becomes
    /// [`Prng::DEFAULT_SEED`] instead of being rejected: callers may be
    /// passing a value derived from the host or the clock.
    pub const fn new(seed: u64) -> Self {
        Prng(if seed == 0 { Self::DEFAULT_SEED } else { seed })
    }

    /// Advances the state and returns 64 random bits.
    ///
    /// Plain xorshift64\*: three shift-xors update the state, then one
    /// multiply scrambles the output. The state itself is never returned, so
    /// xorshift's weak low bits don't leak directly.
    #[inline]
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Advances the state and returns the next 32 random bits — the form the
    /// probability roll uses (`rng.next_u32() % 100 < probability`).
    ///
    /// Takes the **high** half of the 64-bit output, which is where the
    /// multiply mixes best; the low half of a multiply is markedly weaker,
    /// and `% 100` would read exactly those bits.
    #[inline]
    pub fn next_u32(&mut self) -> u32 {
        (self.next_u64() >> 32) as u32
    }
}

impl Default for Prng {
    fn default() -> Self {
        Prng::new(Prng::DEFAULT_SEED)
    }
}

/// How far apart the host's block start and our previous block's end may be
/// and still count as the same continuous stretch of the timeline.
///
/// 1e-6 beats is ~31 ns at 120 BPM — far below one sample at any sane rate,
/// so it only ever absorbs the host's own rounding (it usually derives
/// `pos_beats` from a sample counter), never a real relocation. Even the
/// shortest rate, a 1/64 triplet, is 1/24 of a beat, so no genuine step
/// boundary hides inside the tolerance.
pub const STITCH_TOLERANCE_BEATS: f64 = 1e-6;

/// The span of the timeline one block covers, as a **half-open** beat range
/// `[start, end)`.
///
/// Half-open is what makes block stitching exact: an event fires iff its beat
/// time `t` satisfies `start <= t < end`, so when consecutive ranges share a
/// bit-identical edge (see [`Stitch`]) a step landing exactly on that edge
/// belongs to the later block and to that block only — it can neither
/// double-fire nor vanish.
///
/// `start > end` is never produced by the constructors here; a degenerate
/// range (including one built from a NaN host position) reads as
/// [`BeatRange::is_empty`] and the scan skips it.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct BeatRange {
    pub start: f64,
    pub end: f64,
}

impl BeatRange {
    /// A range covering nothing at all — the "stopped" / zero-sample case.
    pub const EMPTY: BeatRange = BeatRange { start: 0.0, end: 0.0 };

    /// The range `[start, end)`, taken as given.
    ///
    /// Use [`BeatRange::from_span`] for block ranges: it is the one that
    /// guarantees `end >= start` from a host-provided length.
    pub const fn new(start: f64, end: f64) -> Self {
        BeatRange { start, end }
    }

    /// The range starting at `start` and `span` beats long.
    ///
    /// A span that isn't a positive finite number — a zero-sample block, or a
    /// nonsense tempo/sample rate from the host — collapses to an empty range
    /// at `start` rather than producing a backwards or infinite one. `start`
    /// itself is passed through untouched: it is the timeline anchor, and the
    /// engine stitches against it bit-for-bit.
    pub fn from_span(start: f64, span: f64) -> Self {
        let span = if span.is_finite() && span > 0.0 { span } else { 0.0 };
        BeatRange { start, end: start + span }
    }

    /// The length in beats, never negative (0.0 for an empty range).
    pub fn beats(self) -> f64 {
        if self.end > self.start { self.end - self.start } else { 0.0 }
    }

    /// Whether the range contains no beat positions at all.
    ///
    /// The NaN edges are tested first so a NaN range counts as empty instead
    /// of silently comparing `false` everywhere downstream.
    pub fn is_empty(self) -> bool {
        self.start.is_nan() || self.end.is_nan() || self.end <= self.start
    }

    /// The half-open membership test the whole sequencer hangs off:
    /// `start <= beat < end`. False for a NaN `beat`.
    pub fn contains(self, beat: f64) -> bool {
        self.start <= beat && beat < self.end
    }

    /// Whether `next` continues exactly where this range stopped.
    ///
    /// Compared with `==`, deliberately: on the stitching path the edges are
    /// built to be bit-identical, so an epsilon here would hide the very bug
    /// this is meant to detect.
    pub fn meets(self, next: BeatRange) -> bool {
        self.end == next.start
    }
}

/// What happened at the seam between the previous block and this one.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum BlockStart {
    /// The host carried on where we left off (within
    /// [`STITCH_TOLERANCE_BEATS`]), so the block starts at the previous
    /// block's end **exactly** — not at the host's slightly-rounded value.
    Stitched(f64),
    /// A discontinuity: the first block after transport start, a locate, a
    /// loop jump, or a host that reordered time on us. The block starts at
    /// the host's position, and the engine must flush sounding notes before
    /// sequencing it.
    Jump(f64),
}

impl BlockStart {
    /// The beat the block actually starts at, however we got there.
    pub const fn beat(self) -> f64 {
        match self {
            BlockStart::Stitched(beat) | BlockStart::Jump(beat) => beat,
        }
    }

    /// True when held notes have to be flushed before this block.
    pub const fn is_jump(self) -> bool {
        matches!(self, BlockStart::Jump(_))
    }

    /// True when this block continues the previous one seamlessly.
    pub const fn is_stitched(self) -> bool {
        matches!(self, BlockStart::Stitched(_))
    }
}

/// One block's worth of stitching decision: where it runs and whether getting
/// there was a jump.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct StitchedBlock {
    pub range: BeatRange,
    /// Set when the previous block's end did not line up with the host's
    /// position — the engine flushes all voices at offset 0 in that case.
    pub jumped: bool,
}

/// The one piece of state behind block stitching: where the previous block
/// ended.
///
/// Lives in the engine, is `None` whenever continuity is unknown (fresh
/// instance, transport stopped, `reset()`), and never allocates — so it is
/// free to use from `process()`.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct Stitch {
    prev_end: Option<f64>,
}

impl Stitch {
    /// See [`STITCH_TOLERANCE_BEATS`]; exposed here so callers can talk about
    /// `Stitch::TOLERANCE`.
    pub const TOLERANCE: f64 = STITCH_TOLERANCE_BEATS;

    /// A stitcher with no history: the next block is a jump.
    pub const fn new() -> Self {
        Stitch { prev_end: None }
    }

    /// Where the next block would continue from, if continuity is known.
    pub const fn prev_end(&self) -> Option<f64> {
        self.prev_end
    }

    /// Forgets continuity, making the next block a jump. Used when the
    /// transport stops and on `reset()`.
    pub fn reset(&mut self) {
        self.prev_end = None;
    }

    /// Declares where the next block should continue from, overriding the end
    /// of the range [`Stitch::begin`] just produced.
    ///
    /// The engine needs this for a loop wrap: the block ran up to `loop_end`
    /// and on into `loop_start`, so what the *host* will report next block is
    /// the wrapped position, not the un-wrapped end.
    pub fn resume_from(&mut self, beat: f64) {
        self.prev_end = Some(beat);
    }

    /// Decides where a block starting at the host's `host_start` really
    /// begins, without recording anything.
    ///
    /// The comparison is `abs(host_start - prev_end) < TOLERANCE`; on a match
    /// the answer is `prev_end` itself, so consecutive ranges share a
    /// bit-identical edge. A NaN host position fails the comparison and so
    /// reports a jump, which is the safe reading.
    pub fn start_of(&self, host_start: f64) -> BlockStart {
        match self.prev_end {
            Some(prev_end) if (host_start - prev_end).abs() < Self::TOLERANCE => {
                BlockStart::Stitched(prev_end)
            }
            _ => BlockStart::Jump(host_start),
        }
    }

    /// Resolves the block start via [`Stitch::start_of`], builds the
    /// `span_beats`-long range from it, and records its end as the point the
    /// next block should stitch against.
    ///
    /// The recorded end is the range's own end, i.e. the pre-loop-split value
    /// on the host timeline; a caller that splits the range at a loop
    /// boundary follows up with [`Stitch::resume_from`].
    pub fn begin(&mut self, host_start: f64, span_beats: f64) -> StitchedBlock {
        let start = self.start_of(host_start);
        let range = BeatRange::from_span(start.beat(), span_beats);
        self.prev_end = Some(range.end);
        StitchedBlock { range, jumped: start.is_jump() }
    }
}

/// The host's active loop (cycle) region, as a half-open beat range
/// `[start, end)`.
///
/// Constructed only through [`LoopRegion::new`] / [`LoopRegion::from_host`],
/// which reject anything the splitter could not reason about — a backwards or
/// zero-length region, or a non-finite edge from a host that reports loop
/// points it is not actually using. A rejected region reads as "no loop", and
/// the block then passes through unsplit, which is always the safe reading.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct LoopRegion {
    start: f64,
    end: f64,
}

impl LoopRegion {
    /// The loop `[start, end)`, or `None` if it is not a usable region.
    pub fn new(start: f64, end: f64) -> Option<Self> {
        if start.is_finite() && end.is_finite() && end > start {
            Some(LoopRegion { start, end })
        } else {
            None
        }
    }

    /// The same check applied to the host's optional `(start, end)` pair, so
    /// the engine can hand `BlockCtx::loop_range` straight over.
    pub fn from_host(range: Option<(f64, f64)>) -> Option<Self> {
        range.and_then(|(start, end)| LoopRegion::new(start, end))
    }

    /// The beat playback jumps back to when the loop wraps.
    pub const fn start(self) -> f64 {
        self.start
    }

    /// The beat playback wraps at — never played itself, since the region is
    /// half-open just like a [`BeatRange`].
    pub const fn end(self) -> f64 {
        self.end
    }

    /// The loop's length in beats, always positive.
    pub fn beats(self) -> f64 {
        self.end - self.start
    }

    /// Whether `beat` is inside the loop: `start <= beat < end`.
    pub fn contains(self, beat: f64) -> bool {
        self.start <= beat && beat < self.end
    }
}

/// The audio-buffer side of one block: its length in samples and how long one
/// beat lasts in samples at the block's (constant) tempo.
///
/// This is the only place the beat ↔ sample conversion lives:
/// `samples_per_beat = 60.0 / tempo * sample_rate`.
///
/// Nonsense transport data — a zero or negative tempo or sample rate, a NaN —
/// collapses `samples_per_beat` to 0.0, which makes [`BlockTiming::span_beats`]
/// 0.0 and every offset 0. The block then covers an empty range and emits
/// nothing, rather than panicking or scheduling events at wild offsets.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct BlockTiming {
    samples_per_beat: f64,
    num_samples: u32,
}

impl BlockTiming {
    /// Timing for a `num_samples`-long block at `tempo` BPM and `sample_rate`
    /// Hz. See the type docs for how unusable values are handled.
    pub fn new(tempo: f64, sample_rate: f64, num_samples: usize) -> Self {
        let samples_per_beat = 60.0 / tempo * sample_rate;
        let samples_per_beat =
            if samples_per_beat.is_finite() && samples_per_beat > 0.0 { samples_per_beat } else { 0.0 };
        BlockTiming { samples_per_beat, num_samples: num_samples.min(u32::MAX as usize) as u32 }
    }

    /// How many samples one beat lasts; 0.0 when the transport data was
    /// unusable.
    pub const fn samples_per_beat(self) -> f64 {
        self.samples_per_beat
    }

    /// The block's length in samples.
    pub const fn num_samples(self) -> u32 {
        self.num_samples
    }

    /// The largest sample offset an event may carry, i.e. the clamp target.
    /// 0 for an empty block (nothing can be emitted into one anyway).
    pub const fn last_offset(self) -> u32 {
        self.num_samples.saturating_sub(1)
    }

    /// The block's length in beats — what the engine adds to the stitched
    /// start to get `[start, end)`.
    pub fn span_beats(self) -> f64 {
        if self.samples_per_beat > 0.0 { self.num_samples as f64 / self.samples_per_beat } else { 0.0 }
    }

    /// `round(beats * samples_per_beat)`, clamped into the buffer.
    ///
    /// Negative and non-finite inputs clamp to 0 so a stray beat position can
    /// never produce a wrapped or out-of-buffer offset.
    pub fn beats_to_samples(self, beats: f64) -> u32 {
        let last = self.last_offset();
        let samples = beats * self.samples_per_beat;
        if samples.is_nan() || samples <= 0.0 {
            // NaN first, so the comparison below only sees ordered values.
            0
        } else if samples.round() >= last as f64 {
            last
        } else {
            samples.round() as u32
        }
    }
}

/// One contiguous piece of a block's timeline together with the buffer offset
/// its `range.start` maps to.
///
/// After a loop wrap a block covers two disjoint stretches of the timeline but
/// still one run of samples, so the second segment's `base_samples` is the
/// number of samples the first one consumed: offsets accumulate across the
/// split.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct BlockSegment {
    /// The beats this segment covers, half-open as always.
    pub range: BeatRange,
    /// The buffer offset of `range.start`.
    pub base_samples: u32,
    timing: BlockTiming,
}

impl BlockSegment {
    /// A segment covering `range`, starting `base_samples` into the buffer.
    pub fn new(range: BeatRange, base_samples: u32, timing: BlockTiming) -> Self {
        BlockSegment { range, base_samples: base_samples.min(timing.last_offset()), timing }
    }

    /// The buffer offset for a beat position inside this segment:
    /// `base_samples + round((beat - range.start) * samples_per_beat)`,
    /// clamped to the buffer.
    ///
    /// The clamp is what keeps the last step of a block schedulable: the beat
    /// just shy of `range.end` can round up to `num_samples`, which is one
    /// past the buffer.
    pub fn offset_of(&self, beat: f64) -> u32 {
        let within = self.timing.beats_to_samples(beat - self.range.start);
        self.base_samples.saturating_add(within).min(self.timing.last_offset())
    }
}

/// The most timeline segments one block can cover: a loop can wrap at most
/// once per block in v1 (a block longer than the whole loop is clamped rather
/// than wrapped twice).
pub const MAX_BLOCK_SEGMENTS: usize = 2;

/// A block's beat range after loop splitting, plus the sample offsets each
/// piece starts at.
///
/// The split rule:
/// ```text
/// if loop_active && end > loop_end && start < loop_end:
///     [start, loop_end)  then  [loop_start, loop_start + (end - loop_end))
/// else: [start, end)
/// ```
/// A block whose end lands *exactly* on `loop_end` is not split — the second
/// range would be empty — but it still counts as a wrap, because the host's
/// next block starts at `loop_start`: see [`BlockRanges::resume_beat`].
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct BlockRanges {
    segments: [BlockSegment; MAX_BLOCK_SEGMENTS],
    len: usize,
    resume: f64,
    wrapped: bool,
}

impl BlockRanges {
    /// The unsplit case: one segment covering the whole block from offset 0.
    pub fn single(range: BeatRange, timing: BlockTiming) -> Self {
        let segment = BlockSegment::new(range, 0, timing);
        let len = usize::from(!range.is_empty());
        BlockRanges { segments: [segment; MAX_BLOCK_SEGMENTS], len, resume: range.end, wrapped: false }
    }

    /// Splits `range` at an active loop's end, accumulating sample offsets
    /// across the two pieces.
    ///
    /// `loop_region` is `None` (or unusable, see [`LoopRegion::new`]) whenever
    /// the host is not looping, and the range then passes straight through.
    ///
    /// A loop shorter than the block cannot be expressed in
    /// [`MAX_BLOCK_SEGMENTS`] pieces, so the wrapped segment is clipped at
    /// `loop_end` instead of wrapping again. Playback loses the overshoot and
    /// the next block reports a jump — acceptable for a case no host produces
    /// at musical tempos, and it keeps this path allocation- and loop-free.
    pub fn split(range: BeatRange, loop_region: Option<LoopRegion>, timing: BlockTiming) -> Self {
        let Some(region) = loop_region else { return BlockRanges::single(range, timing) };

        // `>=` rather than the split rule's `>` so that ending exactly on the
        // loop end is still recognised as a wrap for `resume_beat`; it produces
        // one segment either way. NaN edges fail both tests and pass through.
        let wraps = !range.is_empty() && range.start < region.end() && range.end >= region.end();
        if !wraps {
            return BlockRanges::single(range, timing);
        }

        let head = BeatRange::new(range.start, region.end());
        let overshoot = range.end - region.end();
        let tail = BeatRange::new(region.start(), (region.start() + overshoot).min(region.end()));

        let head_segment = BlockSegment::new(head, 0, timing);
        let tail_segment = BlockSegment::new(tail, timing.beats_to_samples(head.beats()), timing);

        let len = 1 + usize::from(!tail.is_empty());
        BlockRanges {
            segments: [head_segment, tail_segment],
            len,
            resume: tail.end,
            wrapped: true,
        }
    }

    /// The live segments, in playback order — 0, 1 or 2 of them.
    pub fn segments(&self) -> &[BlockSegment] {
        &self.segments[..self.len]
    }

    /// How many live segments there are.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the block covers no beats at all (a stopped or zero-sample
    /// block), in which case there is nothing to scan.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Whether the block reached the loop end and wrapped.
    pub const fn wrapped(&self) -> bool {
        self.wrapped
    }

    /// The beat the next block continues from, i.e. what the engine feeds to
    /// [`Stitch::resume_from`]: the wrapped position after a loop wrap, or the
    /// block's own end otherwise.
    pub const fn resume_beat(&self) -> f64 {
        self.resume
    }
}

/// One generated note event, in this module's own vocabulary.
///
/// `velocity` and `pan` arrive here already mapped to the payloads a host
/// wants — velocity `0.0..=1.0`, pan `-1.0..=1.0` — so the shell never needs
/// to know how a [`Step`] packs them; it only renames fields on the way to
/// nice-plug's `NoteEvent`, plus a `PolyPan` right after each `NoteOn`.
///
/// `beat` rides along with the sample offset ([`TimedEvent`]) because the
/// offset is a *rounded* sample index: two events a fraction of a sample apart
/// collapse onto one offset, and the beat is what still tells them apart —
/// for ordering here and for exact assertions in the tests.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum SeqEvent {
    NoteOn {
        beat: f64,
        /// Position of the lane that generated it — what the playhead and the
        /// GUI index by. Note-offs deliberately lack it: by then the voice
        /// table owns the note and the lane may be gone.
        lane_pos: usize,
        key: u8,
        channel: u8,
        /// Mapped by [`velocity_to_f32`].
        velocity: f32,
        /// Mapped by [`pan_to_f32`].
        pan: f32,
        /// Semitones, `-24..=24`; 0 means no pitch change. Sent as a per-note
        /// tuning expression, so the key (the Drum Machine pad) stays put.
        pitch: i8,
        /// The lane's volume in dB; 0 means as played. Sent as a per-note
        /// volume expression.
        volume_db: i8,
        voice_id: i32,
    },
    NoteOff {
        beat: f64,
        key: u8,
        channel: u8,
        voice_id: i32,
    },
}

impl SeqEvent {
    /// A note-on from a step's **packed** velocity and pan, i.e. the fields
    /// exactly as [`Step`] stores them; the mapping to `f32` happens here so
    /// that it happens in only one place.
    pub const fn note_on(
        beat: f64,
        lane_pos: usize,
        key: u8,
        channel: u8,
        velocity: u8,
        pan: u8,
        voice_id: i32,
    ) -> SeqEvent {
        SeqEvent::NoteOn {
            beat,
            lane_pos,
            key,
            channel,
            velocity: velocity_to_f32(velocity),
            pan: pan_to_f32(pan),
            pitch: 0,
            volume_db: 0,
            voice_id,
        }
    }

    /// The same note-on with a pitch offset in semitones (clamped to the
    /// step's range). A note-off is returned unchanged: it has no pitch.
    pub const fn with_pitch(self, semitones: i8) -> SeqEvent {
        match self {
            SeqEvent::NoteOn {
                beat, lane_pos, key, channel, velocity, pan, volume_db, voice_id, ..
            } => SeqEvent::NoteOn {
                beat,
                lane_pos,
                key,
                channel,
                velocity,
                pan,
                pitch: crate::model::clamp_i8(semitones, Step::PITCH_MIN, Step::PITCH_MAX),
                volume_db,
                voice_id,
            },
            off @ SeqEvent::NoteOff { .. } => off,
        }
    }

    /// The same note-on with the lane's volume in dB (clamped to the lane's
    /// range). A note-off is returned unchanged.
    pub const fn with_volume(self, db: i8) -> SeqEvent {
        match self {
            SeqEvent::NoteOn {
                beat, lane_pos, key, channel, velocity, pan, pitch, voice_id, ..
            } => SeqEvent::NoteOn {
                beat,
                lane_pos,
                key,
                channel,
                velocity,
                pan,
                pitch,
                volume_db: crate::model::clamp_i8(db, Lane::VOLUME_MIN, Lane::VOLUME_MAX),
                voice_id,
            },
            off @ SeqEvent::NoteOff { .. } => off,
        }
    }

    /// The note-on a `step` of a lane at `lane_pos` fires at `beat` — the form
    /// the scan uses, since it is holding the [`Step`] anyway.
    pub const fn from_step(
        beat: f64,
        lane_pos: usize,
        key: u8,
        channel: u8,
        step: Step,
        voice_id: i32,
    ) -> SeqEvent {
        SeqEvent::note_on(beat, lane_pos, key, channel, step.velocity(), step.pan(), voice_id)
            .with_pitch(step.pitch())
    }

    /// The matching note-off. Velocity and pan are meaningless on a note-off,
    /// which is why the variant does not carry them.
    pub const fn note_off(beat: f64, key: u8, channel: u8, voice_id: i32) -> SeqEvent {
        SeqEvent::NoteOff { beat, key, channel, voice_id }
    }

    /// The beat time the event was generated for.
    pub const fn beat(self) -> f64 {
        match self {
            SeqEvent::NoteOn { beat, .. } | SeqEvent::NoteOff { beat, .. } => beat,
        }
    }

    pub const fn key(self) -> u8 {
        match self {
            SeqEvent::NoteOn { key, .. } | SeqEvent::NoteOff { key, .. } => key,
        }
    }

    pub const fn channel(self) -> u8 {
        match self {
            SeqEvent::NoteOn { channel, .. } | SeqEvent::NoteOff { channel, .. } => channel,
        }
    }

    /// The voice id, which pairs a note-off with its note-on. Generated
    /// voices live in their own id range from `1 << 20` up.
    pub const fn voice_id(self) -> i32 {
        match self {
            SeqEvent::NoteOn { voice_id, .. } | SeqEvent::NoteOff { voice_id, .. } => voice_id,
        }
    }

    /// The lane that generated the event, for a note-on; `None` for a
    /// note-off, which belongs to the voice table rather than to a lane.
    pub const fn lane_pos(self) -> Option<usize> {
        match self {
            SeqEvent::NoteOn { lane_pos, .. } => Some(lane_pos),
            SeqEvent::NoteOff { .. } => None,
        }
    }

    pub const fn is_note_on(self) -> bool {
        matches!(self, SeqEvent::NoteOn { .. })
    }

    pub const fn is_note_off(self) -> bool {
        matches!(self, SeqEvent::NoteOff { .. })
    }

    /// Sort key for the note-offs-before-note-ons rule at equal offsets
    /// ([`EventBuf::sort`]).
    const fn kind_rank(self) -> u8 {
        match self {
            SeqEvent::NoteOff { .. } => 0,
            SeqEvent::NoteOn { .. } => 1,
        }
    }
}

/// Maps a step's packed velocity (`1..=127`) to the `0.0..=1.0` payload CLAP
/// uses: `v as f32 / 127.0`.
///
/// Out-of-range input is clamped exactly as [`Step::velocity`] clamps it, so
/// no snapshot or GUI slip can put a velocity outside the unit range — note
/// that 0 clamps *up* to 1, because velocity 0 means note-off on the wire.
pub const fn velocity_to_f32(velocity: u8) -> f32 {
    let velocity = clamp_u8(velocity, Step::VELOCITY_MIN, Step::VELOCITY_MAX);
    velocity as f32 / Step::VELOCITY_MAX as f32
}

/// Maps a step's packed pan (`0..=126`, centre 63) to the `[-1.0, 1.0]` range
/// CLAP's `PolyPan` note expression uses.
///
/// The packed range is even-sized on purpose: 63 is exactly halfway, so
/// centre is a clean `0.0` and the two sides have the same resolution —
/// `0 → -1.0`, `63 → 0.0`, `126 → 1.0`.
pub const fn pan_to_f32(pan: u8) -> f32 {
    let pan = clamp_u8(pan, Step::PAN_MIN, Step::PAN_MAX);
    (pan as f32 - Step::PAN_CENTRE as f32) / Step::PAN_CENTRE as f32
}

/// A generated event together with the buffer offset it is scheduled at —
/// the `(sample_offset, SeqEvent)` pair the engine hands to the plugin shell.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct TimedEvent {
    /// Sample offset inside the block's buffer, as produced by
    /// [`BlockSegment::offset_of`] (already clamped to the buffer, and already
    /// accumulated across a loop split).
    pub offset: u32,
    /// Which piece of the block's timeline the event belongs to: 0 for the
    /// piece before a loop wrap (and for flushes), 1 for the piece after it.
    /// Beats jump backwards at the wrap, so this is what keeps [`EventBuf::sort`]
    /// in timeline order when the two pieces meet on one sample offset.
    pub segment: u8,
    pub event: SeqEvent,
}

impl TimedEvent {
    pub const fn new(offset: u32, event: SeqEvent) -> TimedEvent {
        TimedEvent { offset, segment: 0, event }
    }

    /// [`TimedEvent::new`] for an event in a later piece of a loop-split
    /// block.
    pub const fn in_segment(offset: u32, segment: u8, event: SeqEvent) -> TimedEvent {
        TimedEvent { offset, segment, event }
    }
}

/// A block's output events in a buffer that never grows.
///
/// Everything that touches this runs on the audio thread, so the storage is
/// allocated once — [`EventBuf::new`] reserves [`EventBuf::CAPACITY`] entries
/// — and [`EventBuf::push`] *drops* an event once the buffer is full, counting
/// it in [`EventBuf::overflows`] rather than reallocating.
///
/// 4096 events is ~2048 notes in a single block; reaching it needs something
/// like 250 lanes of ×8-ratcheted 1/64 triplets at a huge block size, so
/// overflow means an absurd patch, not normal use — and dropping a note there
/// is far better than an allocation (or a panic) in `process()`.
#[derive(Clone, Debug)]
pub struct EventBuf {
    events: Vec<TimedEvent>,
    overflows: u32,
}

impl EventBuf {
    /// The hard event ceiling for one block.
    pub const CAPACITY: usize = 4096;

    /// An empty buffer with all [`EventBuf::CAPACITY`] entries' worth of
    /// storage already allocated — the one allocation this type ever makes,
    /// off the audio thread.
    pub fn new() -> EventBuf {
        EventBuf { events: Vec::with_capacity(Self::CAPACITY), overflows: 0 }
    }

    /// Schedules `event` at buffer offset `offset`, returning whether it fit.
    ///
    /// A full buffer drops the event and counts it; it never reallocates, so
    /// this is safe to call from `process()`.
    pub fn push(&mut self, offset: u32, event: SeqEvent) -> bool {
        self.push_timed(TimedEvent::new(offset, event))
    }

    /// [`EventBuf::push`] for an already-paired [`TimedEvent`].
    pub fn push_timed(&mut self, timed: TimedEvent) -> bool {
        if self.events.len() >= Self::CAPACITY {
            self.overflows = self.overflows.saturating_add(1);
            return false;
        }
        debug_assert!(self.events.capacity() >= Self::CAPACITY, "EventBuf storage was reallocated");
        self.events.push(timed);
        true
    }

    /// The events held, in push order until [`EventBuf::sort`] runs.
    pub fn events(&self) -> &[TimedEvent] {
        &self.events
    }

    pub fn iter(&self) -> std::slice::Iter<'_, TimedEvent> {
        self.events.iter()
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Whether the next [`EventBuf::push`] will be dropped.
    pub fn is_full(&self) -> bool {
        self.events.len() >= Self::CAPACITY
    }

    /// Counts `dropped` events that never reached [`EventBuf::push`] because
    /// an earlier fixed-capacity stage was already full, so the block's
    /// overflow count stays the one number that says notes were lost.
    pub fn count_overflow(&mut self, dropped: u32) {
        self.overflows = self.overflows.saturating_add(dropped);
    }

    /// How many events have been dropped since the last [`EventBuf::clear`] —
    /// a debug counter, so dropped events are visible rather than silent.
    pub const fn overflows(&self) -> u32 {
        self.overflows
    }

    /// Empties the buffer for the next block, keeping its allocation and
    /// resetting the overflow count (which is per block, like the buffer).
    pub fn clear(&mut self) {
        self.events.clear();
        self.overflows = 0;
    }

    /// Puts the events in the order the host must receive them: ascending
    /// sample offset, then **timeline order** (segment, then beat), with
    /// **note-offs before note-ons** only when the beat is the same.
    ///
    /// Timeline order before event kind is what keeps every voice's note-on
    /// ahead of its own note-off. Two events can round onto one sample, and
    /// sorting all note-offs first would then put a short note's off before
    /// its on — the host ignores that off and the note hangs. A voice's off is
    /// never at an earlier beat than its on (the voice table strikes in time
    /// order and merges strikes that land together, `voices.rs`), so ordering
    /// by beat first can't invert a pair.
    ///
    /// The off-first tie-break at an equal beat is the retrigger rule: re-
    /// striking a key that is still sounding closes the old note at the new
    /// note's beat, and the host must see that off before the new on.
    ///
    /// `sort_unstable_by` is used because it allocates nothing — which is also
    /// why the comparator carries on into key, channel and voice id until it is
    /// a total order: an unstable sort is free to reshuffle equal elements, and
    /// the output must not depend on push order.
    pub fn sort(&mut self) {
        self.events.sort_unstable_by(|a, b| {
            a.offset
                .cmp(&b.offset)
                .then_with(|| a.segment.cmp(&b.segment))
                .then_with(|| a.event.beat().total_cmp(&b.event.beat()))
                .then_with(|| a.event.kind_rank().cmp(&b.event.kind_rank()))
                .then_with(|| a.event.key().cmp(&b.event.key()))
                .then_with(|| a.event.channel().cmp(&b.event.channel()))
                .then_with(|| a.event.voice_id().cmp(&b.event.voice_id()))
        });
    }
}

impl Default for EventBuf {
    fn default() -> Self {
        EventBuf::new()
    }
}

/// The fraction of its subdivision a generated note sounds for: v1's gate is
/// fixed at 50%, like Stepwise's Pulse mode.
///
/// A per-step gate is one of the reserved [`Step`] bits, so this constant is
/// the single place that changes when it arrives.
pub const GATE_FRACTION: f64 = 0.5;

/// Hard ceiling on the step occurrences one lane may be asked to walk in one
/// block.
///
/// A sane block is a handful of steps; reaching this needs a host reporting a
/// nonsense tempo (the block span is `num_samples / sample_rate * tempo / 60`
/// beats, and nothing bounds `tempo` from the plugin's side). Since the block
/// could not carry more events than this anyway — [`EventBuf`] drops the rest
/// — truncating the walk is free in every real case and is what keeps a
/// garbage transport from spinning `process()` for minutes.
pub const MAX_STEPS_PER_LANE: usize = EventBuf::CAPACITY;

/// Beyond 2^53 an `f64` can no longer tell neighbouring integers apart, so a
/// step index derived from a beat position that large is meaningless. Ranges
/// past it are refused rather than wrapped into nonsense.
const MAX_STEP_INDEX: f64 = (1i64 << 53) as f64;

/// One step occurrence that fired: the lane scan's output, before the voice
/// table turns it into a [`SeqEvent`] pair.
///
/// The scan deliberately stops here rather than emitting events directly. A
/// note-off belongs to whoever owns note lifetimes — the voice table —
/// because `end_beat` routinely falls in a *later* block than `beat`, and
/// because re-striking a key that is still sounding has to emit the old
/// note's off first. The scan knows neither of those things; it only knows
/// when a step fires and for how long.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct StepHit {
    /// Exact beat the note starts at: `k * step_len + j * subdivision`,
    /// computed from song position 0 so the phase never depends on where
    /// playback started.
    pub beat: f64,
    /// Exact beat the note ends at: `beat + GATE_FRACTION * subdivision`.
    pub end_beat: f64,
    /// Position of the lane that fired, for the playhead and for `SeqEvent`.
    pub lane_pos: usize,
    /// Index of the step within the lane's `length`, i.e. the `k.rem_euclid`
    /// already applied — this is what the GUI highlights.
    pub step_index: usize,
    pub key: u8,
    pub channel: u8,
    /// The lane's volume in dB, read with its key and channel.
    pub volume_db: i8,
    /// The packed step as read, so velocity/pan/ratchet/probability travel
    /// together with the hit instead of being unpacked twice.
    pub step: Step,
}

impl StepHit {
    /// The note-on this hit stands for, once a voice id has been assigned.
    pub const fn note_on(self, voice_id: i32) -> SeqEvent {
        SeqEvent::from_step(self.beat, self.lane_pos, self.key, self.channel, self.step, voice_id)
            .with_volume(self.volume_db)
    }

    /// The matching note-off, at the end of the gate.
    pub const fn note_off(self, voice_id: i32) -> SeqEvent {
        SeqEvent::note_off(self.end_beat, self.key, self.channel, voice_id)
    }

    /// How long the note sounds, in beats.
    pub fn gate_beats(self) -> f64 {
        self.end_beat - self.beat
    }
}

/// Where the scan sends the steps it finds.
///
/// A trait rather than a returned collection because the scan runs on the
/// audio thread and must not allocate: the real sink is the voice table,
/// which writes straight into the block's [`EventBuf`]. The blanket impl for
/// closures is what lets tests collect hits into a `Vec`.
pub trait StepSink {
    fn strike(&mut self, hit: StepHit);
}

impl<F: FnMut(StepHit)> StepSink for F {
    fn strike(&mut self, hit: StepHit) {
        self(hit)
    }
}

/// Whether a step occurrence fires, given its probability percentage and the
/// generator the scan is drawing from.
///
/// 100 always fires and 0 never does, both **without touching `rng`**: only a
/// genuinely uncertain step (1..=99) draws. That keeps a pattern using no
/// probability at all free of randomness, so it plays identically no matter
/// what else shares the generator — and it is why `probability == 0` can be
/// skipped before the ratchet loop without changing any other step's roll.
///
/// The draw is `rng.next_u32() % 100 < probability`. The modulo bias is one
/// extra chance in ~43 million of the low buckets, which is well below
/// audible and far cheaper than rejection sampling on the audio thread.
pub fn probability_fires(probability: u8, rng: &mut Prng) -> bool {
    match probability {
        0 => false,
        p if p >= Step::PROBABILITY_MAX => true,
        p => rng.next_u32() % 100 < p as u32,
    }
}

/// Whether a lane is heard at all this block: muted lanes are silent, and
/// once *any* lane is soloed the un-soloed ones are too.
///
/// Mute wins over solo on the same lane — a lane that is both reads as
/// "silenced deliberately".
pub fn lane_is_audible(lane: &Lane, any_solo: bool) -> bool {
    !lane.is_muted() && (!any_solo || lane.is_soloed())
}

/// `floor(beat / step_len)` as the step index `k`, or `None` when the result
/// is not a usable integer (a non-finite beat, or one so large that `f64`
/// can't resolve single steps any more).
fn step_index_at(beat: f64, step_len: f64) -> Option<i64> {
    let k = (beat / step_len).floor();
    if k.is_finite() && k.abs() < MAX_STEP_INDEX { Some(k as i64) } else { None }
}

/// The step index `k` of the last step occurrence that starts *before*
/// `beat` — `floor` turned exclusive, i.e. `ceil(beat / step_len) - 1`.
///
/// It is the half-open end of a [`BeatRange`] asked "which step were you
/// still playing?": a range ending exactly on a step boundary has not reached
/// that step, so `[0, 0.25)` at 1/16 answers 0, not 1. Computing it with
/// `ceil` rather than nudging the beat keeps that exact for every step length
/// whose boundaries are exactly representable.
fn step_index_before(beat: f64, step_len: f64) -> Option<i64> {
    let k = (beat / step_len).ceil() - 1.0;
    if k.is_finite() && k.abs() < MAX_STEP_INDEX { Some(k as i64) } else { None }
}

/// The step index a lane is on at the end of `range`: the last step
/// occurrence the range reaches, wrapped into the lane's `length`.
///
/// This is what the GUI highlights, so it is the lane's position rather than
/// its sound: an off step, a muted lane and a zero-probability step all still
/// report where the lane is. `None` means "nothing to show" — an empty range,
/// or a lane with no usable rate or length.
///
/// Note the asymmetry with [`scan_lane`]: that one asks which steps *start*
/// inside the range, this one asks where the range *leaves* the lane, so it
/// answers for the step occurrence spanning `range.end` even when that
/// occurrence began in an earlier block.
pub fn playhead_step(data: &LaneData, range: BeatRange) -> Option<usize> {
    if range.is_empty() {
        return None;
    }

    let step_len = data.rate().rate_beats();
    if !step_len.is_finite() || step_len <= 0.0 {
        return None;
    }

    let len = (data.length() as usize).min(data.capacity());
    if len == 0 {
        return None;
    }

    let k = step_index_before(range.end, step_len)?;
    Some(k.rem_euclid(len as i64) as usize)
}

/// The first beat inside `range` at which this lane starts a new step, or
/// `None` when the range stays within one step occurrence.
///
/// This is where a pending pattern change comes due for the lane: everything
/// before it still belongs to the step already in progress, and the step
/// beginning *at* it is the first one read from the new pattern.
///
/// The value is one of the `t`s [`scan_lane`] tests against — `k * step_len`
/// for an integer `k`, computed the same way — so splitting a range here and
/// scanning the two halves strikes every step exactly once: the boundary beat
/// falls outside the half-open head and inside the tail.
pub fn next_step_boundary(data: &LaneData, range: BeatRange) -> Option<f64> {
    if range.is_empty() || !range.start.is_finite() || !range.end.is_finite() {
        return None;
    }

    let step_len = data.rate().rate_beats();
    if !step_len.is_finite() || step_len <= 0.0 {
        return None;
    }

    // Padded and range-tested rather than trusted, exactly as in `scan_lane`:
    // for a step length that is not a binary fraction the quotient at an
    // exact boundary can land a hair either side of the integer, so the
    // candidate indices are generated loosely and filtered exactly.
    let first = step_index_at(range.start, step_len)?;
    (first..=first.saturating_add(2))
        .map(|k| k as f64 * step_len)
        .find(|&boundary| range.contains(boundary))
}

/// Walks one lane over `range` and reports every step occurrence that fires
/// Returns how many hits it sent.
///
/// Mute and solo are *not* checked here — [`scan_lanes`] applies them, so a
/// caller that wants one lane's output regardless (an audition, a test) can
/// have it.
///
/// Positions come from the beat range alone, never from a step counter: `k`
/// is an `i64` so the negative beats of a count-in index with `rem_euclid`
/// exactly as the positive ones do, and a block starting mid-song lands on
/// the same phase as one that played from zero.
///
/// An on-step fires `step.ratchet()` times, on the exact subdivisions
/// `k * step_len + j * step_len / ratchet`, and each hit's gate is measured
/// against that subdivision rather than against the whole step. Probability
/// is rolled **once per step occurrence** and shared by every one of its
/// ratchets, so a ratcheted step either plays as a whole burst or not at all.
///
/// `rng` is drawn from in lane order and then in ascending step order, and
/// only for steps whose probability is genuinely uncertain (see
/// [`probability_fires`]) *and* which reach into `range`. One caveat follows
/// from the "once per occurrence" rule: a ratchet burst straddling a block
/// edge is visited — and so rolled — in both blocks, so for probabilities in
/// `1..=99` the result depends on where the host split the timeline. Fully
/// deterministic steps (0 or 100) are block-size invariant.
pub fn scan_lane(
    lane_pos: usize,
    lane: &Lane,
    data: &LaneData,
    range: BeatRange,
    rng: &mut Prng,
    sink: &mut impl StepSink,
) -> usize {
    if range.is_empty() || !range.start.is_finite() || !range.end.is_finite() {
        return 0;
    }

    let step_len = data.rate().rate_beats();
    if !step_len.is_finite() || step_len <= 0.0 {
        return 0;
    }

    // `length()` already clamps into `1..=capacity`, which gives
    // `len = min(length, capacity)`; the `min` is kept so a capacity past
    // `u32` (impossible today, `MAX_STEPS` is 4096) could not widen it.
    let len = (data.length() as usize).min(data.capacity());
    if len == 0 {
        return 0;
    }

    // The ±1 padding costs one filtered iteration at each end and buys
    // independence from how `beat / step_len` rounds: for a step length that
    // is not a binary fraction (every triplet), the quotient at an exact step
    // boundary can land a hair either side of the integer. Which `k` values
    // actually fire is decided by the half-open range test below, and *that*
    // is exact, so the padding cannot double-fire anything across a stitched
    // edge.
    let (Some(first), Some(last)) =
        (step_index_at(range.start, step_len), step_index_at(range.end, step_len))
    else {
        return 0;
    };
    let first = first.saturating_sub(1);
    let last = last.saturating_add(1).min(first.saturating_add(MAX_STEPS_PER_LANE as i64 - 1));

    let key = lane.key();
    let channel = lane.channel();
    let volume_db = lane.volume_db();
    let mut hits = 0;

    for k in first..=last {
        // Not range-tested yet: a step occurrence that began *before* the
        // range can still drop ratchet sub-hits inside it, so only the
        // individual `t` values below may be filtered.
        let base = k as f64 * step_len;
        let step_index = k.rem_euclid(len as i64) as usize;
        let step = data.step(step_index);
        if !step.is_on() {
            continue;
        }
        let probability = step.probability();
        if probability == 0 {
            continue;
        }

        let ratchet = step.ratchet().max(1);
        let sub = step_len / ratchet as f64;
        let gate = sub * GATE_FRACTION;
        // `None` until the occurrence is known to touch this block; see the
        // roll comment below.
        let mut fires: Option<bool> = None;

        for j in 0..ratchet as u32 {
            // Recomputed from `k` and `j` every time rather than accumulated,
            // so the same hit has a bit-identical beat in every block that
            // sees it and the half-open test can place it in exactly one.
            let t = base + j as f64 * sub;
            if !range.contains(t) {
                continue;
            }
            // Rolled here, after the range test, so an occurrence that
            // contributes nothing to this block consumes no randomness: a
            // block boundary falling between two bursts must not shift the
            // rolls of everything after it.
            if !*fires.get_or_insert_with(|| probability_fires(probability, rng)) {
                break;
            }
            sink.strike(StepHit {
                beat: t,
                end_beat: t + gate,
                lane_pos,
                step_index,
                key,
                channel,
                volume_db,
                step,
            });
            hits += 1;
        }
    }

    hits
}

/// Runs [`scan_lane`] over every audible lane, in lane order, reading each
/// one from the pattern `active_pattern` names for it.
///
/// The pattern is a per-lane function because a pattern change takes effect
/// at each lane's *own* next step boundary, so mid-switch the lanes are
/// genuinely reading different patterns. Pass `|_| p` for a single pattern.
/// An out-of-range pattern index reads as "this lane has nothing to play"
/// rather than panicking.
///
/// All lanes share one `rng`, drawn from in lane order — a single free-running
/// generator per plugin instance.
pub fn scan_lanes(
    data: &SeqData,
    range: BeatRange,
    active_pattern: impl Fn(usize) -> usize,
    rng: &mut Prng,
    sink: &mut impl StepSink,
) -> usize {
    if range.is_empty() {
        return 0;
    }

    let any_solo = data.any_solo();
    let mut hits = 0;
    for (lane_pos, lane) in data.lanes.iter().enumerate() {
        if !lane_is_audible(lane, any_solo) {
            continue;
        }
        let Some(lane_data) = data.lane_data(active_pattern(lane_pos), lane_pos) else {
            continue;
        };
        hits += scan_lane(lane_pos, lane, lane_data, range, rng, sink);
    }
    hits
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{NUM_PATTERNS, chunked_capacity};

    /// The beat value of every one of the 21 variants, written out rather
    /// than recomputed from the formula under test. Straight and dotted
    /// values are exact in binary floating point, so they are compared
    /// exactly; triplets are not, so they get a tolerance.
    #[test]
    fn rate_beats_matches_the_spec_for_all_21_variants() {
        // (rate, expected beats). 1/1 = 4.0 beats and halving from there;
        // T = ×2/3, D = ×3/2.
        let expected: [(Rate, f64); 21] = [
            (Rate::Whole, 4.0),
            (Rate::WholeTriplet, 8.0 / 3.0),
            (Rate::WholeDotted, 6.0),
            (Rate::Half, 2.0),
            (Rate::HalfTriplet, 4.0 / 3.0),
            (Rate::HalfDotted, 3.0),
            (Rate::Quarter, 1.0),
            (Rate::QuarterTriplet, 2.0 / 3.0),
            (Rate::QuarterDotted, 1.5),
            (Rate::Eighth, 0.5),
            (Rate::EighthTriplet, 1.0 / 3.0),
            (Rate::EighthDotted, 0.75),
            (Rate::Sixteenth, 0.25),
            (Rate::SixteenthTriplet, 1.0 / 6.0),
            (Rate::SixteenthDotted, 0.375),
            (Rate::ThirtySecond, 0.125),
            (Rate::ThirtySecondTriplet, 1.0 / 12.0),
            (Rate::ThirtySecondDotted, 0.1875),
            (Rate::SixtyFourth, 0.0625),
            (Rate::SixtyFourthTriplet, 1.0 / 24.0),
            (Rate::SixtyFourthDotted, 0.09375),
        ];
        assert_eq!(expected.len(), Rate::ALL.len());

        for (rate, want) in expected {
            let got = rate.rate_beats();
            match rate.modifier() {
                Modifier::Triplet => assert!(
                    (got - want).abs() <= 1e-15 * want,
                    "{rate}: got {got}, want {want}"
                ),
                // Powers of two and ×1.5 are representable exactly; a sloppy
                // implementation (e.g. base * 1.5000000001, or dividing by a
                // decimal) would show up here.
                _ => assert_eq!(got, want, "{rate}: not exact"),
            }
            assert!(got > 0.0, "{rate}: step length must be positive");
        }

        // The two anchors: a whole note and a sixteenth.
        assert_eq!(Rate::Whole.rate_beats(), 4.0);
        assert_eq!(Rate::Sixteenth.rate_beats(), 0.25);
    }

    /// The modifier relations, stated as ratios so they hold for every base
    /// note value: three triplets fill two straight notes, a dotted note
    /// fills one and a half.
    #[test]
    fn modifiers_scale_their_base_note_value() {
        for note_value in NoteValue::ALL {
            let straight = Rate::from_parts(note_value, Modifier::Straight).rate_beats();
            let triplet = Rate::from_parts(note_value, Modifier::Triplet).rate_beats();
            let dotted = Rate::from_parts(note_value, Modifier::Dotted).rate_beats();

            assert_eq!(straight, note_value.beats(), "{note_value}");
            assert!(
                (triplet * 3.0 - straight * 2.0).abs() <= 1e-15 * straight,
                "{note_value}T: 3 triplets != 2 straight notes"
            );
            assert_eq!(dotted, straight * 1.5, "{note_value}D");
            assert!(triplet < straight && straight < dotted, "{note_value}");
        }
    }

    /// Halving the note value halves the step length, with no drift down the
    /// chain, for every modifier.
    #[test]
    fn each_note_value_is_half_the_previous_one() {
        for modifier in Modifier::ALL {
            for pair in NoteValue::ALL.windows(2) {
                let longer = Rate::from_parts(pair[0], modifier).rate_beats();
                let shorter = Rate::from_parts(pair[1], modifier).rate_beats();
                assert_eq!(shorter, longer / 2.0, "{} vs {}", pair[0], pair[1]);
            }
        }
    }

    /// `rate` is persisted as a bare `u8`, so encode/decode has to be a
    /// lossless round-trip for every variant.
    #[test]
    fn u8_round_trips_every_variant() {
        for (index, rate) in Rate::ALL.into_iter().enumerate() {
            assert_eq!(rate.to_u8() as usize, index, "ALL is not discriminant-ordered");
            assert_eq!(Rate::from_u8(rate.to_u8()), rate);
        }
        // Every valid discriminant is covered, with no gaps.
        for raw in 0..Rate::ALL.len() as u8 {
            assert_eq!(Rate::from_u8(raw).to_u8(), raw);
        }
    }

    /// A discriminant from a hostile snapshot or a newer build must decode to
    /// the default instead of panicking.
    #[test]
    fn invalid_u8_decodes_to_the_default_rate() {
        assert_eq!(Rate::DEFAULT, Rate::Sixteenth);
        assert_eq!(Rate::default(), Rate::Sixteenth);
        assert_eq!(Rate::DEFAULT.rate_beats(), 0.25);

        for raw in [21u8, 22, 100, 128, 200, u8::MAX] {
            assert_eq!(Rate::from_u8(raw), Rate::DEFAULT, "raw {raw}");
        }
    }

    /// The discriminant layout is the saved-state wire format:
    /// base × 3 + modifier. Reordering the variants would silently rewrite
    /// every saved project's rates.
    #[test]
    fn discriminant_is_note_value_times_three_plus_modifier() {
        for note_value in NoteValue::ALL {
            for modifier in Modifier::ALL {
                let rate = Rate::from_parts(note_value, modifier);
                assert_eq!(rate.to_u8(), (note_value as u8) * 3 + modifier as u8, "{rate}");
                assert_eq!(rate.note_value(), note_value, "{rate}");
                assert_eq!(rate.modifier(), modifier, "{rate}");
            }
        }
        // Pin a few absolute values so the ordering itself is nailed down.
        assert_eq!(Rate::Whole.to_u8(), 0);
        assert_eq!(Rate::Sixteenth.to_u8(), 12);
        assert_eq!(Rate::SixtyFourthDotted.to_u8(), 20);
    }

    /// The GUI dropdown renders rates through `Display`.
    #[test]
    fn display_matches_the_gui_labels() {
        assert_eq!(Rate::Sixteenth.to_string(), "1/16");
        assert_eq!(Rate::EighthTriplet.to_string(), "1/8T");
        assert_eq!(Rate::QuarterDotted.to_string(), "1/4D");
        assert_eq!(Rate::Whole.to_string(), "1/1");
        assert_eq!(Rate::WholeTriplet.to_string(), "1/1T");
        assert_eq!(Rate::SixtyFourthDotted.to_string(), "1/64D");

        let all: Vec<String> = Rate::ALL.iter().map(Rate::to_string).collect();
        assert_eq!(
            all,
            [
                "1/1", "1/1T", "1/1D", "1/2", "1/2T", "1/2D", "1/4", "1/4T", "1/4D", "1/8", "1/8T",
                "1/8D", "1/16", "1/16T", "1/16D", "1/32", "1/32T", "1/32D", "1/64", "1/64T",
                "1/64D",
            ]
        );
        // Labels are what the user picks by, so they have to be unique.
        let mut unique = all.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), all.len(), "duplicate rate labels");
    }

    /// `ALL` is the dropdown's source of truth: 21 entries, no duplicates.
    #[test]
    fn all_lists_every_variant_once() {
        let mut seen = [false; 21];
        for rate in Rate::ALL {
            let slot = &mut seen[rate.to_u8() as usize];
            assert!(!*slot, "{rate} listed twice");
            *slot = true;
        }
        assert!(seen.iter().all(|&s| s), "ALL is missing a variant");
        assert_eq!(NoteValue::ALL.len() * Modifier::ALL.len(), Rate::ALL.len());
    }

    /// `rate_beats` is called from the per-lane scan, so it must be usable in
    /// a `const` context and free of surprises at the edges.
    #[test]
    fn rate_beats_is_const_evaluable() {
        const LONGEST: f64 = Rate::WholeDotted.rate_beats();
        const SHORTEST: f64 = Rate::SixtyFourthTriplet.rate_beats();
        assert_eq!(LONGEST, 6.0);
        assert_eq!(SHORTEST, 4.0 / 96.0);
        // No variant is shorter than a 1/64 triplet or longer than a dotted
        // whole note.
        for rate in Rate::ALL {
            let beats = rate.rate_beats();
            assert!((SHORTEST..=LONGEST).contains(&beats), "{rate}: {beats}");
        }
    }

    /// A fixed seed has to replay exactly — the randomised sequencer tests
    /// rely on it for reproducibility in CI.
    #[test]
    fn a_fixed_seed_replays_the_same_sequence() {
        for seed in [1u64, 42, Prng::DEFAULT_SEED, u64::MAX] {
            let mut a = Prng::new(seed);
            let mut b = Prng::new(seed);
            let first: Vec<u32> = (0..256).map(|_| a.next_u32()).collect();
            let second: Vec<u32> = (0..256).map(|_| b.next_u32()).collect();
            assert_eq!(first, second, "seed {seed} is not deterministic");

            // A generator that got stuck (or returned its seed unchanged)
            // would also "replay" identically, so check it actually moves.
            let distinct: std::collections::HashSet<u32> = first.iter().copied().collect();
            assert_eq!(distinct.len(), first.len(), "seed {seed}: repeated output");
            assert!(
                first.iter().all(|&v| v != 0),
                "seed {seed}: produced the zero fixed point"
            );
        }
    }

    /// Different seeds must give different streams, and a zero seed must not
    /// land on xorshift's zero fixed point.
    #[test]
    fn seeds_are_independent_and_zero_is_handled() {
        let a: Vec<u32> = (0..64).scan(Prng::new(1), |p, _| Some(p.next_u32())).collect();
        let b: Vec<u32> = (0..64).scan(Prng::new(2), |p, _| Some(p.next_u32())).collect();
        assert_ne!(a, b, "seeds 1 and 2 give the same stream");

        // Zero falls back to the default seed rather than degenerating.
        let mut zero = Prng::new(0);
        let mut default = Prng::default();
        assert_eq!(zero.next_u32(), default.next_u32());
        let zeroed: Vec<u32> = (0..64).map(|_| zero.next_u32()).collect();
        assert!(zeroed.iter().any(|&v| v != 0), "zero seed degenerated");
    }

    /// Sanity check on the exact expression the probability roll uses
    /// (`rng.next_u32() % 100 < probability`): the 100 residues must be
    /// roughly equally likely, or probability would be skewed. This is a
    /// smoke test for gross bias, not a statistical test — it is
    /// deterministic, so the bounds are generous and never flaky.
    #[test]
    fn modulo_100_is_roughly_uniform() {
        const DRAWS: usize = 200_000;
        const EXPECTED: f64 = DRAWS as f64 / 100.0; // 2000 per bucket

        let mut rng = Prng::new(0x1234_5678_9abc_def0);
        let mut buckets = [0usize; 100];
        let mut sum = 0f64;
        for _ in 0..DRAWS {
            let value = rng.next_u32() % 100;
            buckets[value as usize] += 1;
            sum += value as f64;
        }

        for (value, &count) in buckets.iter().enumerate() {
            let ratio = count as f64 / EXPECTED;
            assert!(
                (0.85..=1.15).contains(&ratio),
                "residue {value} appeared {count} times, expected ~{EXPECTED}"
            );
        }

        // The mean of a uniform 0..=99 is 49.5.
        let mean = sum / DRAWS as f64;
        assert!((mean - 49.5).abs() < 0.5, "mean of % 100 was {mean}");

        // Chi-square over 99 degrees of freedom: ~123 is the 0.95 critical
        // value, so a well-behaved generator sits far below the bound here.
        let chi2: f64 = buckets
            .iter()
            .map(|&count| {
                let diff = count as f64 - EXPECTED;
                diff * diff / EXPECTED
            })
            .sum();
        assert!(chi2 < 200.0, "chi-square of % 100 was {chi2}");
    }

    /// The whole 32-bit output range has to be exercised, not just the low
    /// bits: ratchet and randomise callers slice it in other ways.
    #[test]
    fn output_spans_the_full_u32_range() {
        let mut rng = Prng::new(7);
        let mut low_bits = [0usize; 32];
        let (mut min, mut max) = (u32::MAX, 0u32);
        const DRAWS: usize = 10_000;

        for _ in 0..DRAWS {
            let value = rng.next_u32();
            min = min.min(value);
            max = max.max(value);
            for (bit, count) in low_bits.iter_mut().enumerate() {
                *count += ((value >> bit) & 1) as usize;
            }
        }

        assert!(min < u32::MAX / 16, "no small values: min {min}");
        assert!(max > u32::MAX / 16 * 15, "no large values: max {max}");
        // Every bit position, including bit 0, should be set about half the
        // time — this is what catches returning the weak low half of the
        // multiply.
        for (bit, &count) in low_bits.iter().enumerate() {
            let ratio = count as f64 / DRAWS as f64;
            assert!((0.45..=0.55).contains(&ratio), "bit {bit} set {ratio} of the time");
        }
    }

    /// `next_u32` is one `next_u64` draw, not a cached half of a previous
    /// one: every call must advance the state.
    #[test]
    fn next_u32_is_the_high_half_of_one_next_u64_draw() {
        let mut a = Prng::new(99);
        let mut b = Prng::new(99);
        // next_u32 is exactly the high half of the same next_u64 draw.
        assert_eq!(a.next_u32(), (b.next_u64() >> 32) as u32);
        assert_eq!(a.next_u32(), (b.next_u64() >> 32) as u32);

        // A clone is an explicit snapshot, used by tests that need to rewind.
        let mut c = a.clone();
        assert_eq!(a.next_u32(), c.next_u32());
    }

    // ---- BeatRange + block stitching -------------------------------------

    /// Host positions the way a DAW actually reports them: derived from a
    /// running sample counter, so they drift a few ULPs away from the sum of
    /// the per-block beat spans. The drift is what the stitch tolerance
    /// exists for.
    fn host_block(tempo: f64, sample_rate: f64, block_samples: usize, index: usize) -> (f64, f64) {
        let beats_per_sample = tempo / 60.0 / sample_rate;
        let host_start = (index * block_samples) as f64 * beats_per_sample;
        let span = block_samples as f64 * beats_per_sample;
        (host_start, span)
    }

    /// Half-open `[start, end)` is the rule the whole sequencer rests on: a
    /// step exactly on an edge belongs to the later block only.
    #[test]
    fn a_beat_range_includes_its_start_and_excludes_its_end() {
        let range = BeatRange::new(1.0, 2.0);

        assert!(range.contains(1.0), "start must be inside");
        assert!(!range.contains(2.0), "end must be outside");
        assert!(range.contains(1.999_999_999));
        assert!(!range.contains(0.999_999_999));
        assert!(!range.contains(f64::NAN), "a NaN beat is in no range");
        assert_eq!(range.beats(), 1.0);
        assert!(!range.is_empty());

        // The pair of ranges either side of an edge: the edge fires exactly
        // once, in the second one.
        let next = BeatRange::new(2.0, 3.0);
        assert!(range.meets(next));
        assert!(!range.contains(2.0) && next.contains(2.0));
    }

    /// Degenerate ranges have to be inert rather than panicking or looping:
    /// an empty range contains nothing and reports zero length.
    #[test]
    fn an_empty_range_contains_nothing() {
        for range in [
            BeatRange::EMPTY,
            BeatRange::new(4.0, 4.0),
            BeatRange::new(-1.5, -1.5),
            // Never produced by `from_span`, but must still be inert if an
            // engine bug builds one.
            BeatRange::new(4.0, 3.0),
            BeatRange::new(f64::NAN, f64::NAN),
        ] {
            assert!(range.is_empty(), "{range:?} should be empty");
            assert_eq!(range.beats(), 0.0, "{range:?}");
            for beat in [-1.5, 0.0, 3.5, 4.0, 4.5] {
                assert!(!range.contains(beat), "{range:?} contains {beat}");
            }
        }
    }

    /// `from_span` is how every block range is built, straight from
    /// host-provided numbers — it must never hand back a backwards or
    /// non-finite range.
    #[test]
    fn from_span_never_builds_a_backwards_or_infinite_range() {
        let range = BeatRange::from_span(2.5, 0.25);
        assert_eq!((range.start, range.end), (2.5, 2.75));

        // A zero-sample block, or a nonsense tempo/sample rate, collapses to
        // an empty range anchored at `start`.
        for span in [0.0, -0.0, -1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let range = BeatRange::from_span(2.5, span);
            assert_eq!(range.start, 2.5, "span {span}");
            assert_eq!(range.end, 2.5, "span {span}");
            assert!(range.is_empty(), "span {span}");
        }

        // Negative positions (pre-roll / count-in) are perfectly legal.
        let pre_roll = BeatRange::from_span(-1.0, 0.5);
        assert_eq!((pre_roll.start, pre_roll.end), (-1.0, -0.5));
        assert!(pre_roll.contains(-1.0) && !pre_roll.contains(-0.5));
    }

    /// With no history there is nothing to stitch to, so the first block of a
    /// playback run is a jump and starts exactly where the host says.
    #[test]
    fn the_first_block_is_a_jump_at_the_host_position() {
        let mut stitch = Stitch::new();
        assert_eq!(stitch.prev_end(), None);
        assert_eq!(stitch.start_of(7.5), BlockStart::Jump(7.5));

        let block = stitch.begin(7.5, 0.25);
        assert!(block.jumped, "the first block must flush and jump");
        assert_eq!(block.range, BeatRange::new(7.5, 7.75));
        assert_eq!(stitch.prev_end(), Some(7.75));
    }

    /// The core of the rule: when the host's position is within the
    /// tolerance, the block starts at `prev_end` **bit-for-bit**, so
    /// consecutive ranges share an identical edge and the host's own rounding
    /// never leaks into our beat grid.
    #[test]
    fn stitched_blocks_share_bit_identical_edges() {
        let mut drifting_hosts = 0usize;

        for (tempo, sample_rate, block_samples) in [
            (120.0, 48_000.0, 512),
            (174.0, 44_100.0, 128),
            (60.0, 48_000.0, 2048),
            (180.0, 44_100.0, 64),
            (97.3, 96_000.0, 1),
        ] {
            let mut stitch = Stitch::new();
            let mut previous: Option<BeatRange> = None;

            for index in 0..512 {
                let (host_start, span) = host_block(tempo, sample_rate, block_samples, index);
                let block = stitch.begin(host_start, span);

                match previous {
                    None => assert!(block.jumped, "first block"),
                    Some(previous) => {
                        assert!(
                            !block.jumped,
                            "{tempo} BPM / {block_samples} samples: block {index} reported a jump"
                        );
                        // Not `==` on the value but on the bits: the
                        // stitching path has to be bit-identical, not merely
                        // close.
                        assert_eq!(
                            block.range.start.to_bits(),
                            previous.end.to_bits(),
                            "block {index} does not continue the previous range exactly"
                        );
                        assert!(previous.meets(block.range));
                        if host_start != block.range.start {
                            drifting_hosts += 1;
                        }
                        // The host's own number stays within the tolerance —
                        // the drift absorbed here is rounding, not relocation.
                        assert!(
                            (host_start - block.range.start).abs() < Stitch::TOLERANCE,
                            "block {index}: drift {} is too large to stitch",
                            host_start - block.range.start
                        );
                    }
                }

                // `beats()` is `end - start`, so it reproduces the span to
                // within an ULP of the *position* (a 0.02-beat block at beat
                // 400 cannot do better) — the edges are what has to be exact.
                let slack = 2.0 * f64::EPSILON * block.range.end.abs().max(1.0);
                assert!(
                    (block.range.beats() - span).abs() <= slack,
                    "block {index}: span {span} became {}",
                    block.range.beats()
                );
                previous = Some(block.range);
            }
        }

        // If the simulated host never drifted, the assertions above would be
        // vacuous — a `start = host_start` implementation would pass too.
        assert!(drifting_hosts > 0, "the host positions never drifted; test proves nothing");
    }

    /// Stitched ranges tile the timeline: every step boundary in the covered
    /// span falls in exactly one block, so no step can double-fire or vanish
    /// at a block edge.
    #[test]
    fn every_step_boundary_lands_in_exactly_one_stitched_block() {
        // 1/16 at 120 BPM is 125 ms = 6000 samples, so 512-sample blocks cut
        // across step boundaries at an irrational-looking phase.
        const STEP_LEN: f64 = 0.25;
        let mut stitch = Stitch::new();
        let mut ranges = Vec::new();

        for index in 0..2000 {
            let (host_start, span) = host_block(120.0, 48_000.0, 512, index);
            ranges.push(stitch.begin(host_start, span).range);
        }

        let covered = BeatRange::new(ranges[0].start, ranges.last().unwrap().end);
        let mut boundaries = 0usize;
        let mut k = 0i64;
        while (k as f64) * STEP_LEN < covered.end {
            let beat = k as f64 * STEP_LEN;
            let hits = ranges.iter().filter(|range| range.contains(beat)).count();
            assert_eq!(hits, 1, "beat {beat} fired in {hits} blocks");
            boundaries += 1;
            k += 1;
        }
        assert!(boundaries > 100, "only {boundaries} boundaries checked");

        // And the tiling has no holes: the ranges chain end-to-start.
        for pair in ranges.windows(2) {
            assert!(pair[0].meets(pair[1]));
        }
    }

    /// The tolerance boundary, stated exactly: strictly less than 1e-6 beats
    /// stitches, 1e-6 or more is a jump.
    #[test]
    fn the_stitch_tolerance_boundary_is_exclusive() {
        const PREV_END: f64 = 12.0;
        assert_eq!(Stitch::TOLERANCE, 1e-6);
        assert_eq!(STITCH_TOLERANCE_BEATS, 1e-6);

        for delta in [0.0, 1e-12, 1e-9, 5e-7, 9.999e-7] {
            for signed in [delta, -delta] {
                let mut stitch = Stitch::new();
                stitch.resume_from(PREV_END);
                let host_start = PREV_END + signed;
                assert_eq!(
                    stitch.start_of(host_start),
                    BlockStart::Stitched(PREV_END),
                    "delta {signed} should stitch"
                );
                assert!(!stitch.begin(host_start, 0.25).jumped, "delta {signed}");
            }
        }

        // Exactly 1e-6 is outside the window. Measured from beat 0, where
        // the gap is representable exactly — `12.0 + 1e-6` actually rounds to
        // a gap a hair *under* the tolerance, so it stitches, and should.
        let mut at_boundary = Stitch::new();
        at_boundary.resume_from(0.0);
        for host_start in [Stitch::TOLERANCE, -Stitch::TOLERANCE] {
            assert_eq!(host_start.abs(), Stitch::TOLERANCE, "gap is not exact");
            assert_eq!(at_boundary.start_of(host_start), BlockStart::Jump(host_start));
        }
        // One ULP inside it still stitches.
        let just_inside = f64::from_bits(Stitch::TOLERANCE.to_bits() - 1);
        assert!(just_inside < Stitch::TOLERANCE);
        assert_eq!(at_boundary.start_of(just_inside), BlockStart::Stitched(0.0));

        // Bigger gaps, and any backwards move (a locate, or a loop the engine
        // did not predict), are jumps too.
        for delta in [2e-6, 1e-3, 0.25, 4.0, 128.0] {
            for signed in [delta, -delta] {
                let mut stitch = Stitch::new();
                stitch.resume_from(PREV_END);
                let host_start = PREV_END + signed;
                assert_eq!(
                    stitch.start_of(host_start),
                    BlockStart::Jump(host_start),
                    "delta {signed} should be a jump"
                );

                let block = stitch.begin(host_start, 0.25);
                assert!(block.jumped, "delta {signed}");
                // A jump abandons `prev_end` and uses the host's position
                // verbatim — no snapping.
                assert_eq!(block.range.start, host_start, "delta {signed}");
                assert_eq!(stitch.prev_end(), Some(host_start + 0.25), "delta {signed}");
            }
        }
    }

    /// `BlockStart` is what the engine branches on; its accessors must agree
    /// with the variant.
    #[test]
    fn block_start_reports_its_beat_and_kind() {
        let stitched = BlockStart::Stitched(2.5);
        assert_eq!(stitched.beat(), 2.5);
        assert!(stitched.is_stitched() && !stitched.is_jump());

        let jump = BlockStart::Jump(-4.0);
        assert_eq!(jump.beat(), -4.0);
        assert!(jump.is_jump() && !jump.is_stitched());
    }

    /// Stop and `reset()` clear continuity, so the next block flushes and
    /// re-anchors to the host instead of stitching onto a stale position.
    #[test]
    fn reset_makes_the_next_block_a_jump() {
        let mut stitch = Stitch::new();
        stitch.begin(0.0, 0.25);
        assert!(!stitch.begin(0.25, 0.25).jumped);
        assert_eq!(stitch.prev_end(), Some(0.5));

        stitch.reset();
        assert_eq!(stitch.prev_end(), None);
        // Even resuming at the exact same beat is a jump: after a stop, held
        // notes have to be flushed.
        let block = stitch.begin(0.5, 0.25);
        assert!(block.jumped);
        assert_eq!(block.range, BeatRange::new(0.5, 0.75));
    }

    /// A loop wrap: the block runs up to `loop_end`, the engine redirects the
    /// continuation to the wrapped position, and the host's next block — which
    /// reports that wrapped position — stitches onto it seamlessly instead of
    /// being read as a jump.
    #[test]
    fn resume_from_stitches_against_a_wrapped_loop_position() {
        const LOOP: (f64, f64) = (0.0, 4.0);
        let mut stitch = Stitch::new();
        stitch.resume_from(3.9);

        // The block would run [3.9, 4.15) — past the loop end by 0.15 beats.
        let block = stitch.begin(3.9, 0.25);
        assert!(!block.jumped);
        assert_eq!(block.range, BeatRange::new(3.9, 4.15));
        assert_eq!(stitch.prev_end(), Some(4.15), "begin records the un-wrapped end");

        // The engine splits at `loop_end` and tells the stitcher where the
        // host will actually continue.
        let wrapped = LOOP.0 + (block.range.end - LOOP.1);
        stitch.resume_from(wrapped);
        assert_eq!(stitch.prev_end(), Some(wrapped));

        let next = stitch.begin(wrapped, 0.25);
        assert!(!next.jumped, "the wrap must not look like a jump");
        assert_eq!(next.range.start.to_bits(), wrapped.to_bits());

        // Without the redirect the same host position would have been a jump.
        let mut naive = Stitch::new();
        naive.resume_from(4.15);
        assert!(naive.start_of(wrapped).is_jump());
    }

    /// A zero-sample block (hosts do send them) must not move the timeline or
    /// break the next block's stitch.
    #[test]
    fn a_zero_length_block_does_not_move_the_timeline() {
        let mut stitch = Stitch::new();
        stitch.resume_from(1.0);

        let block = stitch.begin(1.0, 0.0);
        assert!(!block.jumped);
        assert!(block.range.is_empty());
        assert_eq!(stitch.prev_end(), Some(1.0));

        assert_eq!(stitch.start_of(1.0), BlockStart::Stitched(1.0));
    }

    /// Hostile transport data: a non-finite `pos_beats` must not panic, must
    /// not emit anything (the range is empty), and must not poison the
    /// stitcher — the next sane block simply reports a jump.
    #[test]
    fn a_non_finite_host_position_is_an_inert_jump() {
        for host_start in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let mut stitch = Stitch::new();
            stitch.resume_from(8.0);

            let block = stitch.begin(host_start, 0.25);
            assert!(block.jumped, "{host_start} should be a jump");
            // Infinities give start == end; NaN compares false either way.
            assert!(block.range.is_empty(), "{host_start} produced a live range");
            assert!(!block.range.contains(0.0) && !block.range.contains(8.0));

            // Recovery: whatever got stored, a real position afterwards is a
            // jump, not a silent mis-stitch.
            let recovered = stitch.begin(8.25, 0.25);
            assert!(recovered.jumped);
            assert_eq!(recovered.range, BeatRange::new(8.25, 8.5));
        }
    }

    /// Pre-roll: Bitwig counts in from negative beats, so stitching has to
    /// work across beat 0 without a spurious jump.
    #[test]
    fn stitching_crosses_beat_zero_from_pre_roll() {
        // A span that does not divide -1.0, so one block straddles beat 0
        // rather than landing on it.
        const SPAN: f64 = 0.3;
        let mut stitch = Stitch::new();
        let mut start = -1.0;
        let mut crossed = false;

        for index in 0..16 {
            let block = stitch.begin(start, SPAN);
            assert_eq!(block.range.start.to_bits(), start.to_bits(), "block {index}");
            assert_eq!(block.jumped, index == 0, "block {index}");
            if block.range.contains(0.0) {
                assert!(block.range.start < 0.0, "block {index} should straddle beat 0");
                crossed = true;
            }
            start = block.range.end;
        }

        assert!(crossed, "the run never crossed beat 0");
        assert!((start - 3.8).abs() < 1e-12, "ended at {start}");
    }

    /// A 1/16-at-120-BPM block: 24000 samples per beat, 6000-sample blocks.
    /// Written out so the offset arithmetic in these tests is checkable by
    /// hand rather than recomputed from the code under test.
    const SPB_120: f64 = 24_000.0;

    fn timing_120(num_samples: usize) -> BlockTiming {
        BlockTiming::new(120.0, 48_000.0, num_samples)
    }

    /// `samples_per_beat = 60.0 / tempo * sample_rate`, and `span_beats` is
    /// its exact inverse applied to the block length.
    #[test]
    fn block_timing_converts_between_beats_and_samples() {
        let timing = timing_120(512);
        assert_eq!(timing.samples_per_beat(), SPB_120);
        assert_eq!(timing.num_samples(), 512);
        assert_eq!(timing.last_offset(), 511);
        assert_eq!(timing.span_beats(), 512.0 / SPB_120);

        // The tempos and rates the invariance tests use.
        for (tempo, sample_rate, want) in [
            (60.0, 48_000.0, 48_000.0),
            (120.0, 44_100.0, 22_050.0),
            (180.0, 48_000.0, 16_000.0),
            (120.0, 96_000.0, 48_000.0),
        ] {
            let timing = BlockTiming::new(tempo, sample_rate, 64);
            assert_eq!(timing.samples_per_beat(), want, "{tempo} BPM @ {sample_rate}");
            // 64 samples back to beats and forward again lands on 64.
            assert_eq!(timing.beats_to_samples(timing.span_beats()), 63, "clamped to the buffer");
        }

        // round(), not truncation: a beat half a sample past a boundary.
        let timing = timing_120(1_024);
        assert_eq!(timing.beats_to_samples(100.4 / SPB_120), 100);
        assert_eq!(timing.beats_to_samples(100.5 / SPB_120), 101);
        assert_eq!(timing.beats_to_samples(0.0), 0);
    }

    /// Hostile transport values must leave the block inert instead of
    /// producing wild offsets or panicking on the cast to `u32`.
    #[test]
    fn block_timing_survives_nonsense_transport_data() {
        for (tempo, sample_rate) in [
            (0.0, 48_000.0),
            (-120.0, 48_000.0),
            (120.0, 0.0),
            (120.0, -48_000.0),
            (f64::NAN, 48_000.0),
            (120.0, f64::NAN),
            (f64::INFINITY, 48_000.0),
            (120.0, f64::INFINITY),
        ] {
            let timing = BlockTiming::new(tempo, sample_rate, 512);
            assert_eq!(timing.samples_per_beat(), 0.0, "{tempo} BPM @ {sample_rate}");
            assert_eq!(timing.span_beats(), 0.0, "{tempo} BPM @ {sample_rate}");
            assert_eq!(timing.beats_to_samples(1.0), 0, "{tempo} BPM @ {sample_rate}");
        }

        // Out-of-range beat positions clamp rather than wrap around.
        let timing = timing_120(256);
        for beats in [-1.0, -1e9, f64::NEG_INFINITY, f64::NAN] {
            assert_eq!(timing.beats_to_samples(beats), 0, "{beats}");
        }
        for beats in [1.0, 1e9, f64::INFINITY] {
            assert_eq!(timing.beats_to_samples(beats), 255, "{beats}");
        }

        // A zero-sample block has exactly one legal offset, 0.
        let empty = timing_120(0);
        assert_eq!(empty.last_offset(), 0);
        assert_eq!(empty.beats_to_samples(0.5), 0);
    }

    /// Only a usable region counts as a loop; everything else reads as "not
    /// looping" so the block passes through unsplit.
    #[test]
    fn a_loop_region_rejects_unusable_bounds() {
        let region = LoopRegion::new(1.0, 5.0).expect("a sane loop");
        assert_eq!((region.start(), region.end()), (1.0, 5.0));
        assert_eq!(region.beats(), 4.0);
        assert!(region.contains(1.0) && region.contains(4.999));
        assert!(!region.contains(5.0), "the region is half-open");
        assert!(!region.contains(0.999) && !region.contains(f64::NAN));

        for (start, end) in [
            (4.0, 4.0),
            (4.0, 3.0),
            (f64::NAN, 4.0),
            (0.0, f64::NAN),
            (f64::NEG_INFINITY, 0.0),
            (0.0, f64::INFINITY),
        ] {
            assert!(LoopRegion::new(start, end).is_none(), "({start}, {end}) is not a loop");
            assert!(LoopRegion::from_host(Some((start, end))).is_none(), "({start}, {end})");
        }
        assert!(LoopRegion::from_host(None).is_none());
        assert!(LoopRegion::from_host(Some((0.0, 4.0))).is_some());

        // Negative loops are legal — Bitwig can loop over the count-in.
        assert_eq!(LoopRegion::new(-4.0, -1.0).map(LoopRegion::beats), Some(3.0));
    }

    /// No loop (or a loop the block never reaches): one segment, offsets
    /// straight from the block start, and the next block stitches onto the
    /// range's own end.
    #[test]
    fn a_block_without_an_active_loop_passes_through_unsplit() {
        let range = BeatRange::new(2.0, 2.25);
        let timing = timing_120(6_000);

        let region_cases = [
            None,                                 // not looping at all
            LoopRegion::new(8.0, 12.0),           // loop entirely ahead of us
            LoopRegion::new(0.0, 1.0),            // loop entirely behind us
            LoopRegion::new(0.0, 4.0),            // we are inside it, not at its end
        ];
        for region in region_cases {
            let ranges = BlockRanges::split(range, region, timing);
            assert_eq!(ranges.len(), 1, "{region:?}");
            assert!(!ranges.is_empty() && !ranges.wrapped(), "{region:?}");
            assert_eq!(ranges.segments()[0].range, range, "{region:?}");
            assert_eq!(ranges.segments()[0].base_samples, 0, "{region:?}");
            assert_eq!(ranges.resume_beat(), range.end, "{region:?}");
        }

        // The offsets of the unsplit case are just the block's own mapping.
        let segment = BlockRanges::single(range, timing).segments()[0];
        assert_eq!(segment.offset_of(2.0), 0);
        assert_eq!(segment.offset_of(2.125), 3_000);
        assert_eq!(segment.offset_of(2.25 - 1e-9), 5_999, "clamped to the last sample");

        // An empty block (stopped, or zero samples) has nothing to scan.
        let nothing = BlockRanges::split(BeatRange::EMPTY, LoopRegion::new(0.0, 4.0), timing);
        assert!(nothing.is_empty() && nothing.segments().is_empty());
        assert!(!nothing.wrapped());
        assert_eq!(nothing.resume_beat(), 0.0);
    }

    /// The worked case: `[3.9, 4.15)` against loop `[0, 4)` becomes
    /// `[3.9, 4.0)` then `[0.0, 0.15)`, with the second segment starting where
    /// the first one stopped in the buffer.
    #[test]
    fn a_loop_wrap_mid_block_splits_into_two_ranges() {
        let range = BeatRange::new(3.9, 4.15);
        let timing = timing_120(6_000); // 0.25 beats
        let region = LoopRegion::new(0.0, 4.0);

        let ranges = BlockRanges::split(range, region, timing);
        assert_eq!(ranges.len(), 2);
        assert!(ranges.wrapped());

        let [head, tail] = [ranges.segments()[0], ranges.segments()[1]];
        assert_eq!(head.range, BeatRange::new(3.9, 4.0));
        assert_eq!(tail.range.start, 0.0);
        // The overshoot is `end - loop_end` as the host's floats give it —
        // 0.15 plus a few ULPs here — so it is compared with a tolerance.
        // Only the *stitched* edges have to be bit-identical.
        assert!((tail.range.end - 0.15).abs() < 1e-12, "tail ends at {}", tail.range.end);
        assert!((head.range.beats() + tail.range.beats() - range.beats()).abs() < 1e-12);

        // Offsets accumulate: the tail starts 0.1 beats = 2400 samples in.
        assert_eq!(head.base_samples, 0);
        assert_eq!(tail.base_samples, 2_400);
        assert_eq!(head.offset_of(3.9), 0);
        assert_eq!(head.offset_of(3.95), 1_200);
        assert_eq!(tail.offset_of(0.0), 2_400);
        assert_eq!(tail.offset_of(0.1), 4_800);
        assert_eq!(tail.offset_of(tail.range.end - 1e-12), 6_000 - 1, "the end clamps into the buffer");

        // The host continues at the wrapped position, not at 4.15.
        assert_eq!(ranges.resume_beat().to_bits(), tail.range.end.to_bits());
        assert!((ranges.resume_beat() - 0.15).abs() < 1e-12);

        // The split tiles the covered timeline: a beat inside the block falls
        // in exactly one segment, and one outside it in none.
        for beat in [3.75, 4.0, 4.25, 0.2] {
            assert_eq!(
                ranges.segments().iter().filter(|s| s.range.contains(beat)).count(),
                0,
                "beat {beat} is outside the block"
            );
        }
        for beat in [3.9, 3.95, 0.0, 0.1] {
            assert_eq!(
                ranges.segments().iter().filter(|s| s.range.contains(beat)).count(),
                1,
                "beat {beat} must be covered once"
            );
        }
        // 4.0 is the loop end: excluded from the head, and not re-entered.
        assert!(!head.range.contains(4.0) && !tail.range.contains(4.0));
    }

    /// The boundary case: the block ends exactly on `loop_end`. There is no
    /// second range to emit (it would be empty), but the host still continues
    /// from `loop_start`, so the engine must be told to resume there — feeding
    /// it the un-wrapped `4.0` would cost a spurious flush.
    #[test]
    fn a_loop_boundary_exactly_at_the_block_edge_wraps_without_splitting() {
        let range = BeatRange::new(3.75, 4.0);
        let timing = timing_120(6_000);
        let region = LoopRegion::new(0.0, 4.0);

        let ranges = BlockRanges::split(range, region, timing);
        assert_eq!(ranges.len(), 1, "the second range would be empty");
        assert!(ranges.wrapped(), "the host wraps after this block");
        assert_eq!(ranges.resume_beat(), 0.0, "resume at loop_start, not 4.0");

        let head = ranges.segments()[0];
        assert_eq!(head.range.start.to_bits(), range.start.to_bits());
        assert_eq!(head.range.end.to_bits(), range.end.to_bits(), "the head is the whole block");
        assert!(!head.range.contains(4.0), "the loop end belongs to the next pass");
        assert_eq!(head.offset_of(3.75), 0);

        // And the block that starts exactly at `loop_start` is an ordinary
        // unsplit block again.
        let next = BlockRanges::split(BeatRange::new(0.0, 0.25), region, timing);
        assert_eq!(next.len(), 1);
        assert!(!next.wrapped());
        assert_eq!(next.resume_beat(), 0.25);
    }

    /// Offsets must never go backwards as the block is scanned in order —
    /// events are sorted by offset later, and a dip would reorder note-offs
    /// across the wrap.
    #[test]
    fn sample_offsets_rise_monotonically_across_the_split() {
        const NUM_SAMPLES: usize = 2_048;
        let timing = timing_120(NUM_SAMPLES);
        let span = timing.span_beats();
        let region = LoopRegion::new(0.0, 1.0);
        let loop_end = region.expect("loop").end();

        // Sweep the block start across the loop end so the split lands at
        // every phase, including exactly on the edge and just either side.
        let mut phases: Vec<f64> = (0..=32).map(|i| loop_end - span * i as f64 / 32.0).collect();
        phases.push(loop_end - span);
        phases.push(loop_end - 1e-9);
        phases.push(loop_end + 1e-9);

        for start in phases {
            let range = BeatRange::from_span(start, span);
            let ranges = BlockRanges::split(range, region, timing);

            let mut previous = 0u32;
            let mut covered = 0.0;
            for (index, segment) in ranges.segments().iter().enumerate() {
                assert!(
                    segment.base_samples >= previous,
                    "start {start}: segment {index} base went backwards"
                );
                // 64 probes per segment, from its start up to just shy of its
                // end — the half-open end itself is never emitted.
                for step in 0..64 {
                    let beat = segment.range.start + segment.range.beats() * step as f64 / 64.0;
                    let offset = segment.offset_of(beat);
                    assert!(
                        offset >= previous,
                        "start {start}: offset {offset} < {previous} at beat {beat}"
                    );
                    assert!(offset < NUM_SAMPLES as u32, "start {start}: offset {offset} out of buffer");
                    previous = offset;
                }
                covered += segment.range.beats();
            }

            assert!(!ranges.is_empty(), "start {start}: the block vanished");
            assert!((covered - range.beats()).abs() < 1e-12, "start {start}: covered {covered} beats");
            // The whole buffer is used: the last segment reaches its far end.
            let last = ranges.segments().last().expect("a live segment");
            assert_eq!(last.offset_of(last.range.end - 1e-12), NUM_SAMPLES as u32 - 1, "start {start}");
        }
    }

    /// A loop wrap must not look like a jump to the next block: with the
    /// wrapped resume position fed back into the stitcher, a looping transport
    /// runs for pass after pass without a single flush, and every step of the
    /// loop fires exactly once per pass.
    #[test]
    fn looping_blocks_stitch_across_the_wrap_and_fire_every_step_once() {
        const STEP_LEN: f64 = 0.25; // 1/16
        const BLOCK: usize = 512;
        const PASSES: usize = 6;
        let region = LoopRegion::new(0.0, 2.0).expect("loop");
        let timing = timing_120(BLOCK);
        let span = timing.span_beats(); // 512 / 24000 beats — divides nothing

        let mut stitch = Stitch::new();
        let mut host_start = region.start();
        let mut hits = vec![0usize; (region.beats() / STEP_LEN) as usize];
        let mut jumps = 0;
        let mut passes = 0;

        for block_index in 0..4_000 {
            let block = stitch.begin(host_start, span);
            if block.jumped {
                jumps += 1;
            }
            let ranges = BlockRanges::split(block.range, Some(region), timing);

            // The run has to stop on a loop boundary for the counts to come
            // out even, so on the last block the wrapped tail — already part
            // of the next pass — is left uncounted.
            let last = ranges.wrapped() && passes + 1 == PASSES;
            let counted = if last { &ranges.segments()[..1] } else { ranges.segments() };
            for segment in counted {
                for (index, hit) in hits.iter_mut().enumerate() {
                    let beat = index as f64 * STEP_LEN;
                    if segment.range.contains(beat) {
                        *hit += 1;
                        assert!(
                            segment.offset_of(beat) < BLOCK as u32,
                            "block {block_index}: step {index} scheduled past the buffer"
                        );
                    }
                }
            }

            if ranges.wrapped() {
                passes += 1;
                stitch.resume_from(ranges.resume_beat());
            }
            if last {
                break;
            }
            // The host reports the position we told the stitcher to expect.
            host_start = stitch.prev_end().expect("continuity");
        }

        assert_eq!(passes, PASSES, "the run did not finish {PASSES} loop passes");
        assert_eq!(jumps, 1, "only the very first block may be a jump");
        for (index, hit) in hits.iter().enumerate() {
            assert_eq!(*hit, PASSES, "step {index} fired {hit} times over {PASSES} passes");
        }
    }

    /// Degenerate case: a loop shorter than one block. v1 clips the wrapped
    /// segment at the loop end rather than wrapping twice, and the next block
    /// re-anchors with a jump.
    #[test]
    fn a_loop_shorter_than_the_block_is_clipped_not_wrapped_twice() {
        let region = LoopRegion::new(1.0, 1.1);
        let timing = timing_120(24_000); // one full beat per block
        let ranges = BlockRanges::split(BeatRange::new(1.05, 2.05), region, timing);

        assert_eq!(ranges.len(), 2);
        assert!(ranges.wrapped());
        assert_eq!(ranges.segments()[0].range, BeatRange::new(1.05, 1.1));
        let tail = ranges.segments()[1].range;
        assert_eq!(tail.start, 1.0);
        assert_eq!(tail.end, 1.1, "clipped at the loop end instead of wrapping again");
        assert_eq!(ranges.resume_beat(), 1.1);
        // Nothing lands outside the loop, which is what matters musically.
        for segment in ranges.segments() {
            assert!(region.expect("loop").contains(segment.range.start));
            assert!(segment.range.end <= 1.1);
        }
    }

    /// The velocity and pan mappings, at their anchors and across their
    /// whole packed range.
    #[test]
    fn velocity_and_pan_map_to_their_host_ranges() {
        assert_eq!(velocity_to_f32(127), 1.0);
        assert_eq!(velocity_to_f32(1), 1.0 / 127.0);
        assert_eq!(velocity_to_f32(100), 100.0 / 127.0);
        // Velocity 0 is a note-off on the wire, so it clamps up, not down.
        assert_eq!(velocity_to_f32(0), velocity_to_f32(Step::VELOCITY_MIN));
        assert_eq!(velocity_to_f32(255), 1.0);
        for velocity in Step::VELOCITY_MIN..=Step::VELOCITY_MAX {
            let mapped = velocity_to_f32(velocity);
            assert!(mapped > 0.0 && mapped <= 1.0, "velocity {velocity} mapped to {mapped}");
        }

        assert_eq!(pan_to_f32(Step::PAN_CENTRE), 0.0);
        assert_eq!(pan_to_f32(Step::PAN_MIN), -1.0);
        assert_eq!(pan_to_f32(Step::PAN_MAX), 1.0);
        assert_eq!(pan_to_f32(255), 1.0, "out-of-range pan clamps to hard right");
        // Symmetric about the centre, which is the point of the even range.
        for offset in 0..=Step::PAN_CENTRE {
            let left = pan_to_f32(Step::PAN_CENTRE - offset);
            let right = pan_to_f32(Step::PAN_CENTRE + offset);
            assert_eq!(left, -right, "pan centre ±{offset} is not symmetric");
            assert!((-1.0..=1.0).contains(&right));
        }
    }

    /// A note-on built from a step carries that step's velocity and pan,
    /// mapped; a note-off carries neither, and both remember their beat, key,
    /// channel and voice id.
    #[test]
    fn a_hit_carries_its_lane_volume_into_the_note_on() {
        let hit = StepHit {
            beat: 0.0,
            end_beat: 0.5,
            lane_pos: 0,
            step_index: 0,
            key: 42,
            channel: 0,
            volume_db: -6,
            step: Step::default_on().with_pitch(2),
        };
        let SeqEvent::NoteOn { volume_db, pitch, .. } = hit.note_on(1 << 20) else {
            panic!("not a note-on");
        };
        assert_eq!((volume_db, pitch), (-6, 2));

        let plain = SeqEvent::note_on(0.0, 0, 42, 0, 100, Step::PAN_CENTRE, 1 << 20);
        let SeqEvent::NoteOn { volume_db, .. } = plain.with_volume(i8::MAX) else {
            panic!("not a note-on");
        };
        assert_eq!(volume_db, Lane::VOLUME_MAX, "clamped");
        let off = SeqEvent::note_off(0.5, 42, 0, 1 << 20);
        assert_eq!(off.with_volume(5), off, "a note-off has no volume");
    }

    #[test]
    fn a_pitched_step_s_note_on_carries_its_pitch() {
        let step = Step::default_on().with_pitch(-5);
        let on = SeqEvent::from_step(0.0, 0, 36, 0, step, 1 << 20);
        let SeqEvent::NoteOn { pitch, key, .. } = on else { panic!("not a note-on") };
        assert_eq!((pitch, key), (-5, 36), "the key stays: pitch is an expression");
        let off = SeqEvent::note_off(0.5, 36, 0, 1 << 20);
        assert_eq!(off.with_pitch(3), off, "a note-off has no pitch");
    }

    #[test]
    fn events_carry_mapped_step_values() {
        let step = Step::pack(true, 100, Step::PAN_CENTRE, 4, 100);
        let on = SeqEvent::from_step(1.25, 3, 64, 9, step, (1 << 20) + 7);

        assert_eq!(
            on,
            SeqEvent::NoteOn {
                beat: 1.25,
                lane_pos: 3,
                key: 64,
                channel: 9,
                velocity: 100.0 / 127.0,
                pan: 0.0,
                pitch: 0,
                volume_db: 0,
                voice_id: (1 << 20) + 7,
            }
        );
        assert_eq!(on.beat(), 1.25);
        assert_eq!(on.key(), 64);
        assert_eq!(on.channel(), 9);
        assert_eq!(on.voice_id(), (1 << 20) + 7);
        assert_eq!(on.lane_pos(), Some(3));
        assert!(on.is_note_on() && !on.is_note_off());

        // Hard left, lowest velocity: the extremes survive the packing too.
        let step = Step::pack(true, 1, Step::PAN_MIN, 1, 100);
        let on = SeqEvent::from_step(0.0, 0, 0, 0, step, 1 << 20);
        match on {
            SeqEvent::NoteOn { velocity, pan, .. } => {
                assert_eq!(velocity, 1.0 / 127.0);
                assert_eq!(pan, -1.0);
            }
            SeqEvent::NoteOff { .. } => unreachable!(),
        }

        let off = SeqEvent::note_off(1.5, 64, 9, (1 << 20) + 7);
        assert_eq!(off, SeqEvent::NoteOff { beat: 1.5, key: 64, channel: 9, voice_id: (1 << 20) + 7 });
        assert_eq!(off.lane_pos(), None, "a note-off belongs to the voice table, not a lane");
        assert!(off.is_note_off() && !off.is_note_on());
    }

    /// Overflow is counted, not grown into: past [`EventBuf::CAPACITY`] the
    /// buffer keeps its length, keeps its allocation (same pointer — nothing
    /// was reallocated under the audio thread) and keeps a tally of what it
    /// dropped. `clear` then puts it back to a fresh state.
    #[test]
    fn event_buf_counts_overflow_instead_of_growing() {
        let mut buf = EventBuf::new();
        assert!(buf.is_empty() && !buf.is_full() && buf.overflows() == 0);

        for index in 0..EventBuf::CAPACITY {
            let event = SeqEvent::note_off(index as f64, 60, 0, index as i32);
            assert!(buf.push(index as u32, event), "event {index} should still fit");
        }
        assert_eq!(buf.len(), EventBuf::CAPACITY);
        assert!(buf.is_full());
        assert_eq!(buf.overflows(), 0);

        let storage = buf.events().as_ptr();
        for index in 0..10 {
            let event = SeqEvent::note_off(index as f64, 61, 0, index);
            assert!(!buf.push(0, event), "a full buffer must reject the push");
        }
        assert_eq!(buf.len(), EventBuf::CAPACITY, "the buffer grew");
        assert_eq!(buf.overflows(), 10);
        assert_eq!(buf.events().as_ptr(), storage, "the storage was reallocated");
        // The dropped events are the late ones: the first CAPACITY survive.
        assert_eq!(buf.events()[EventBuf::CAPACITY - 1].event.key(), 60);

        buf.clear();
        assert!(buf.is_empty() && !buf.is_full());
        assert_eq!(buf.overflows(), 0, "the overflow count is per block");
        assert_eq!(buf.events().as_ptr(), storage, "clear must keep the allocation");
    }

    /// The ordering contract: ascending sample offset, note-offs first at
    /// equal offsets — and a total order beyond that, so the sorted stream
    /// does not depend on push order.
    #[test]
    fn sort_orders_by_offset_with_note_offs_first() {
        let on_a = SeqEvent::from_step(1.0, 0, 60, 0, Step::default_on(), 1 << 20);
        let on_b = SeqEvent::from_step(1.0, 1, 48, 0, Step::default_on(), (1 << 20) + 1);
        let off_a = SeqEvent::note_off(0.999, 60, 0, (1 << 20) - 1);
        let off_b = SeqEvent::note_off(1.0, 72, 0, (1 << 20) - 2);
        let later = SeqEvent::from_step(2.0, 0, 36, 0, Step::default_on(), (1 << 20) + 2);

        let mut buf = EventBuf::new();
        // Pushed deliberately out of order, note-ons before their note-offs.
        buf.push(10, later);
        buf.push(5, on_a);
        buf.push(5, off_b);
        buf.push(5, on_b);
        buf.push(5, off_a);
        buf.push(0, on_b);
        buf.sort();

        let offsets: Vec<u32> = buf.iter().map(|timed| timed.offset).collect();
        assert_eq!(offsets, [0, 5, 5, 5, 5, 10], "not sorted by offset");

        // Within offset 5: both note-offs, then both note-ons.
        let kinds: Vec<bool> = buf.iter().map(|timed| timed.event.is_note_off()).collect();
        assert_eq!(kinds, [false, true, true, false, false, false]);
        let at_five: Vec<SeqEvent> =
            buf.iter().filter(|timed| timed.offset == 5).map(|timed| timed.event).collect();
        // Ties past the kind rank break on beat, then key: off_a (beat 0.999)
        // before off_b (beat 1.0), then on_b (key 48) before on_a (key 60).
        assert_eq!(at_five, [off_a, off_b, on_b, on_a]);

        // A different push order sorts to exactly the same stream, which is
        // what the invariance tests rely on.
        let mut other = EventBuf::new();
        for (offset, event) in [(5, off_a), (5, on_b), (0, on_b), (5, on_a), (10, later), (5, off_b)] {
            other.push(offset, event);
        }
        other.sort();
        assert_eq!(other.events(), buf.events());
    }

    /// A short note whose on and off round onto one sample keeps its on
    /// first: the off is at a later beat, and beat order comes before the
    /// off-first rule. Sorting the off first would leave the note hanging.
    #[test]
    fn sort_keeps_a_voice_s_note_on_ahead_of_its_own_note_off() {
        let on = SeqEvent::from_step(1.0, 0, 60, 0, Step::default_on(), 1 << 20);
        let off = SeqEvent::note_off(1.00001, 60, 0, 1 << 20);
        let mut buf = EventBuf::new();
        buf.push(7, off);
        buf.push(7, on);
        buf.sort();
        assert_eq!(buf.iter().map(|timed| timed.event).collect::<Vec<_>>(), [on, off]);
    }

    /// After a loop wrap the beats jump backwards. Where the two pieces meet
    /// on one sample, the piece before the wrap still sorts first.
    #[test]
    fn sort_keeps_a_loop_wrap_in_timeline_order_on_a_shared_offset() {
        let before_wrap = SeqEvent::from_step(3.99999, 0, 60, 0, Step::default_on(), 1 << 20);
        let after_wrap = SeqEvent::note_off(0.0, 60, 0, 1 << 20);
        let mut buf = EventBuf::new();
        buf.push_timed(TimedEvent::in_segment(100, 1, after_wrap));
        buf.push_timed(TimedEvent::in_segment(100, 0, before_wrap));
        buf.sort();
        let sorted: Vec<SeqEvent> = buf.iter().map(|timed| timed.event).collect();
        assert_eq!(sorted, [before_wrap, after_wrap]);
    }

    /// An empty buffer and a single event are the degenerate cases the engine
    /// hits on most blocks; sorting them must not care.
    #[test]
    fn sort_handles_empty_and_single_event_buffers() {
        let mut buf = EventBuf::default();
        buf.sort();
        assert!(buf.events().is_empty());

        let event = SeqEvent::note_off(0.0, 60, 0, 1 << 20);
        buf.push(7, event);
        buf.sort();
        assert_eq!(buf.events(), [TimedEvent::new(7, event)]);
    }

    // ---- Per-lane scan ----------------------------------------------------

    /// One lane's content, as the scan tests describe it: MIDI key, loop
    /// length, rate and which step indices are on.
    struct LaneSpec {
        key: u8,
        length: u32,
        rate: Rate,
        on: &'static [usize],
    }

    /// A lane whose steps are all on — the shape the polymeter and pre-roll
    /// tests want, since then every `k` reports its own index.
    const ALL_STEPS: &[usize] = &[
        0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24,
        25, 26, 27, 28, 29, 30, 31,
    ];

    const fn spec(key: u8, length: u32, rate: Rate, on: &'static [usize]) -> LaneSpec {
        LaneSpec { key, length, rate, on }
    }

    /// Builds a `SeqData` holding the given lanes, with identical content in
    /// all 8 patterns (the tests that care about patterns overwrite one
    /// afterwards).
    fn seq_data(specs: &[LaneSpec]) -> SeqData {
        let lanes: Vec<Lane> = specs
            .iter()
            .enumerate()
            .map(|(i, s)| Lane::new(i as u32 + 1, format!("lane {i}"), 0x00_00_00, s.key, 0))
            .collect();
        let patterns = std::array::from_fn(|_| {
            specs
                .iter()
                .map(|s| {
                    let highest = s.on.iter().copied().max().map_or(0, |i| i + 1);
                    let data =
                        LaneData::with_capacity(chunked_capacity(highest.max(s.length as usize)));
                    data.set_length(s.length);
                    data.set_rate(s.rate);
                    for &i in s.on {
                        data.set_step(i, Step::default_on());
                    }
                    data
                })
                .collect()
        });
        SeqData::new(lanes, patterns)
    }

    /// Every hit `data` produces over `range`, reading pattern 0.
    fn hits_over(data: &SeqData, range: BeatRange) -> Vec<StepHit> {
        let mut hits = Vec::new();
        let count = scan_lanes(data, range, |_| 0, &mut Prng::default(), &mut |hit: StepHit| hits.push(hit));
        assert_eq!(count, hits.len(), "scan_lanes miscounted its hits");
        hits
    }

    /// Every hit `data` produces over `total_beats`, chopped into stitched
    /// blocks of `block_samples` the way the engine will drive it. Returns
    /// the concatenated hits, which is what the per-block tests assert on.
    fn hits_in_blocks(
        data: &SeqData,
        tempo: f64,
        sample_rate: f64,
        block_samples: usize,
        blocks: usize,
    ) -> Vec<StepHit> {
        let mut stitch = Stitch::new();
        let mut hits = Vec::new();
        for index in 0..blocks {
            let (host_start, span) = host_block(tempo, sample_rate, block_samples, index);
            let block = stitch.begin(host_start, span);
            scan_lanes(data, block.range, |_| 0, &mut Prng::default(), &mut |hit: StepHit| hits.push(hit));
        }
        hits
    }

    fn beats_of(hits: &[StepHit]) -> Vec<f64> {
        hits.iter().map(|hit| hit.beat).collect()
    }

    fn step_indices_of(hits: &[StepHit]) -> Vec<usize> {
        hits.iter().map(|hit| hit.step_index).collect()
    }

    /// The canonical case: a 16-step 1/16 lane with steps 0/4/8/12 on fires
    /// on every downbeat, once each, however the host chops the timeline up.
    #[test]
    fn four_on_the_floor_fires_once_per_beat_across_blocks() {
        let data = seq_data(&[spec(36, 16, Rate::Sixteenth, &[0, 4, 8, 12])]);

        // 3.84 beats of 1/16 at 120 BPM / 48 kHz: 92160 samples, which all
        // three block sizes divide exactly. Deliberately not a whole number
        // of beats — stitching sums the per-block spans, so after ~10^5
        // one-sample blocks the running end has drifted a few ULPs from the
        // host's own position, and a range ending *at* beat 4.0 could land
        // either side of the step there. That drift is the stitch
        // tolerance's job, not the scan's.
        for &block_samples in &[1usize, 64, 2048] {
            let blocks = 92_160 / block_samples;
            let hits = hits_in_blocks(&data, 120.0, 48_000.0, block_samples, blocks);
            assert_eq!(
                beats_of(&hits),
                vec![0.0, 1.0, 2.0, 3.0],
                "block size {block_samples}"
            );
            assert_eq!(step_indices_of(&hits), vec![0, 4, 8, 12]);
            for hit in &hits {
                assert_eq!(hit.key, 36);
                assert_eq!(hit.channel, 0);
                assert_eq!(hit.lane_pos, 0);
                // Gate is 50% of the 1/16 subdivision.
                assert_eq!(hit.gate_beats(), 0.125);
                assert_eq!(hit.end_beat, hit.beat + 0.125);
            }
        }
    }

    /// The same four beats in one block must give exactly the same hits as
    /// the stitched small blocks above — the invariance this whole design is
    /// for, asserted on bit-identical beats.
    #[test]
    fn one_big_block_equals_many_stitched_small_ones() {
        let data = seq_data(&[
            spec(36, 16, Rate::Sixteenth, &[0, 4, 8, 12]),
            spec(42, 15, Rate::EighthTriplet, ALL_STEPS),
            spec(38, 13, Rate::QuarterDotted, &[0, 3, 7]),
        ]);

        let one = hits_in_blocks(&data, 137.0, 44_100.0, 96_000, 1);
        let many = hits_in_blocks(&data, 137.0, 44_100.0, 64, 1_500);

        assert!(!one.is_empty());
        // Per-block scans run lane-by-lane within each block, so compare the
        // streams in a canonical order rather than the emission order.
        let sort_key = |hits: &mut Vec<StepHit>| {
            hits.sort_by(|a, b| {
                a.beat.total_cmp(&b.beat).then_with(|| a.lane_pos.cmp(&b.lane_pos))
            })
        };
        let (mut one, mut many) = (one, many);
        sort_key(&mut one);
        sort_key(&mut many);
        assert_eq!(one, many);
    }

    /// Polymeter: lanes of 16, 15 and 13 steps at the same rate drift apart
    /// and each wraps on its own length, with no shared counter anywhere.
    #[test]
    fn polymeter_lanes_wrap_independently() {
        let lengths = [16u32, 15, 13];
        let data = seq_data(&[
            spec(36, lengths[0], Rate::Sixteenth, ALL_STEPS),
            spec(38, lengths[1], Rate::Sixteenth, ALL_STEPS),
            spec(42, lengths[2], Rate::Sixteenth, ALL_STEPS),
        ]);

        // 9.984 beats = 40 sixteenths, long enough for all three to wrap and
        // to drift apart (they only realign after lcm(16,15,13) = 3120
        // steps).
        let hits = hits_in_blocks(&data, 120.0, 48_000.0, 512, 468);

        for (pos, len) in lengths.iter().enumerate() {
            let lane_hits: Vec<&StepHit> = hits.iter().filter(|h| h.lane_pos == pos).collect();
            assert_eq!(lane_hits.len(), 40, "lane {pos} should fire every sixteenth");
            for (k, hit) in lane_hits.iter().enumerate() {
                assert_eq!(hit.beat, k as f64 * 0.25);
                assert_eq!(hit.step_index, k % *len as usize, "lane {pos} step {k}");
            }
        }

        // They really are out of phase with one another: at k = 16 the lanes
        // are reading steps 0, 1 and 3.
        let at_sixteen: Vec<usize> =
            hits.iter().filter(|h| h.beat == 4.0).map(|h| h.step_index).collect();
        assert_eq!(at_sixteen, vec![0, 1, 3]);
    }

    /// Phase is anchored to song position 0, so starting playback in the
    /// middle of the song gives exactly the steps that lane would have
    /// reached had it played from the top.
    #[test]
    fn phase_after_a_mid_song_start_matches_playing_from_zero() {
        let data = seq_data(&[
            spec(36, 13, Rate::Sixteenth, ALL_STEPS),
            spec(38, 7, Rate::EighthTriplet, ALL_STEPS),
        ]);

        // One beat starting at bar 26 (beat 101), played cold.
        let cold = hits_over(&data, BeatRange::new(101.0, 102.0));

        // The same beat reached by playing from zero in 1/8-beat blocks.
        let mut stitch = Stitch::new();
        let mut warm = Vec::new();
        for index in 0..816 {
            let block = stitch.begin(index as f64 * 0.125, 0.125);
            scan_lanes(&data, block.range, |_| 0, &mut Prng::default(), &mut |hit: StepHit| {
                if hit.beat >= 101.0 && hit.beat < 102.0 {
                    warm.push(hit);
                }
            });
        }

        let key = |hits: &mut Vec<StepHit>| {
            hits.sort_by(|a, b| a.beat.total_cmp(&b.beat).then_with(|| a.lane_pos.cmp(&b.lane_pos)))
        };
        let (mut cold, mut warm) = (cold, warm);
        key(&mut cold);
        key(&mut warm);
        assert!(!cold.is_empty());
        assert_eq!(cold, warm);

        // And the indices are the arithmetic ones, computed from zero.
        for hit in &cold {
            let (step_len, len) = if hit.lane_pos == 0 { (0.25, 13) } else { (1.0 / 3.0, 7) };
            let k = (hit.beat / step_len).round() as i64;
            assert_eq!(hit.step_index, k.rem_euclid(len) as usize);
        }
    }

    /// A count-in puts the transport at negative beats; `rem_euclid` has to
    /// wrap those the same way, with no "-1 % 16 = -1" surprise.
    #[test]
    fn negative_pre_roll_indexes_with_rem_euclid() {
        let data = seq_data(&[spec(36, 16, Rate::Sixteenth, ALL_STEPS)]);

        // Two beats of pre-roll, then across zero into the first beat.
        let hits = hits_over(&data, BeatRange::new(-2.0, 1.0));
        assert_eq!(hits.len(), 12);
        for (i, hit) in hits.iter().enumerate() {
            let k = i as i64 - 8;
            assert_eq!(hit.beat, k as f64 * 0.25);
            assert_eq!(hit.step_index, k.rem_euclid(16) as usize);
        }
        // Step 15 sounds immediately before the downbeat, step 0 on it.
        assert_eq!(step_indices_of(&hits[7..9]), vec![15, 0]);

        // A lane whose length does not divide the bar wraps just as cleanly
        // in the negatives: k = -1 on a 13-step lane is step 12.
        let odd = seq_data(&[spec(36, 13, Rate::Sixteenth, ALL_STEPS)]);
        let hits = hits_over(&odd, BeatRange::new(-0.25, 0.0));
        assert_eq!(step_indices_of(&hits), vec![12]);
    }

    /// Muting silences a lane; soloing any lane silences every lane that is
    /// not soloed; mute wins on a lane that is both.
    #[test]
    fn mute_and_solo_decide_which_lanes_are_heard() {
        let data = seq_data(&[
            spec(36, 16, Rate::Quarter, ALL_STEPS),
            spec(38, 16, Rate::Quarter, ALL_STEPS),
            spec(42, 16, Rate::Quarter, ALL_STEPS),
        ]);
        let range = BeatRange::new(0.0, 1.0);
        let heard = |data: &SeqData| -> Vec<usize> {
            hits_over(data, range).iter().map(|h| h.lane_pos).collect()
        };

        assert_eq!(heard(&data), vec![0, 1, 2]);

        data.lanes[1].set_mute(true);
        assert_eq!(heard(&data), vec![0, 2]);

        // One solo silences everything else, muted or not.
        data.lanes[2].set_solo(true);
        assert_eq!(heard(&data), vec![2]);

        // A second solo joins it; a lane that is muted *and* soloed stays
        // silent.
        data.lanes[0].set_solo(true);
        data.lanes[1].set_solo(true);
        assert_eq!(heard(&data), vec![0, 2]);

        data.lanes[1].set_mute(false);
        assert_eq!(heard(&data), vec![0, 1, 2]);
    }

    /// Off steps are skipped, shortening `length` hides the steps past it
    /// without erasing them, and the hidden ones come back when it grows.
    #[test]
    fn off_steps_and_hidden_steps_are_silent() {
        let data = seq_data(&[spec(36, 16, Rate::Sixteenth, &[0, 7, 20])]);
        let lane = data.lane_data(0, 0).unwrap();
        assert!(lane.capacity() >= 21);

        // Step 20 is past `length`, so the lane loops over 16 steps only.
        let hits = hits_over(&data, BeatRange::new(0.0, 4.0));
        assert_eq!(step_indices_of(&hits), vec![0, 7]);

        // Growing the length within capacity reveals it (and re-wraps the
        // others), with no shape edit involved.
        lane.set_length(21);
        let hits = hits_over(&data, BeatRange::new(0.0, 5.25));
        assert_eq!(step_indices_of(&hits), vec![0, 7, 20]);

        // Shrinking to 1 leaves a one-step loop.
        lane.set_length(1);
        let hits = hits_over(&data, BeatRange::new(0.0, 1.0));
        assert_eq!(step_indices_of(&hits), vec![0, 0, 0, 0]);
    }

    /// A step landing exactly on a block edge belongs to the later block and
    /// to that block only — including for triplet rates, whose step length is
    /// not representable in binary and whose `beat / step_len` therefore
    /// rounds either way.
    #[test]
    fn a_step_on_a_block_edge_fires_in_exactly_one_block() {
        for rate in [Rate::SixteenthTriplet, Rate::EighthTriplet, Rate::QuarterTriplet] {
            let step_len = rate.rate_beats();
            let data = seq_data(&[spec(36, 4, rate, ALL_STEPS)]);

            // Blocks that start and end exactly on step boundaries: every
            // boundary is both an `end` and a `start`.
            let mut stitch = Stitch::new();
            let mut hits = Vec::new();
            for k in 0..24i64 {
                let block = stitch.begin(k as f64 * step_len, step_len);
                scan_lanes(&data, block.range, |_| 0, &mut Prng::default(), &mut |hit: StepHit| hits.push(hit));
            }

            assert_eq!(hits.len(), 24, "{rate}: every step fires exactly once");
            for (k, hit) in hits.iter().enumerate() {
                assert_eq!(hit.beat, k as f64 * step_len, "{rate}");
                assert_eq!(hit.step_index, k % 4);
            }
        }
    }

    /// Each lane reads the pattern it is told to, so a half-finished pattern
    /// switch is representable.
    #[test]
    fn each_lane_reads_its_own_pattern() {
        let data = seq_data(&[
            spec(36, 16, Rate::Quarter, &[0]),
            spec(38, 16, Rate::Quarter, &[0]),
        ]);
        // In pattern 3, lane 1 plays step 1 instead of step 0.
        data.lane_data(3, 1).unwrap().set_step(0, Step::OFF);
        data.lane_data(3, 1).unwrap().set_step(1, Step::default_on());

        let range = BeatRange::new(0.0, 2.0);
        let mut hits = Vec::new();
        scan_lanes(
            &data,
            range,
            |pos| if pos == 1 { 3 } else { 0 },
            &mut Prng::default(),
            &mut |hit: StepHit| hits.push(hit),
        );

        assert_eq!(
            hits.iter().map(|h| (h.lane_pos, h.step_index, h.beat)).collect::<Vec<_>>(),
            vec![(0, 0, 0.0), (1, 1, 1.0)]
        );

        // A pattern index that does not exist plays nothing rather than
        // panicking — the GUI can race a pattern count change.
        let hits = {
            let mut hits = Vec::new();
            scan_lanes(&data, range, |_| NUM_PATTERNS, &mut Prng::default(), &mut |hit| {
                hits.push(hit)
            });
            hits
        };
        assert!(hits.is_empty());
    }

    /// A hit converts to the `NoteOn`/`NoteOff` pair the shell sends, sharing
    /// one voice id and carrying the step's mapped velocity and pan.
    #[test]
    fn a_hit_becomes_a_note_on_note_off_pair() {
        let data = seq_data(&[spec(60, 16, Rate::Eighth, &[0])]);
        data.lane_data(0, 0)
            .unwrap()
            .set_step(0, Step::default_on().with_velocity(64).with_pan(126));
        data.lanes[0].set_channel(9);

        let hits = hits_over(&data, BeatRange::new(0.0, 0.5));
        assert_eq!(hits.len(), 1);
        let hit = hits[0];

        let voice_id = 1 << 20;
        assert_eq!(
            hit.note_on(voice_id),
            SeqEvent::NoteOn {
                beat: 0.0,
                lane_pos: 0,
                key: 60,
                channel: 9,
                velocity: 64.0 / 127.0,
                pan: 1.0,
                pitch: 0,
                volume_db: 0,
                voice_id,
            }
        );
        assert_eq!(
            hit.note_off(voice_id),
            SeqEvent::NoteOff { beat: 0.25, key: 60, channel: 9, voice_id }
        );
    }

    /// Ranges the scan cannot make sense of produce nothing at all, instead
    /// of events at wild positions: an empty or backwards range, and the NaN
    /// a host can hand us through `pos_beats`.
    #[test]
    fn unusable_ranges_emit_nothing() {
        let data = seq_data(&[spec(36, 16, Rate::Sixteenth, ALL_STEPS)]);
        let empty = [
            BeatRange::EMPTY,
            BeatRange::new(4.0, 4.0),
            BeatRange::new(4.0, 2.0),
            BeatRange::new(f64::NAN, 1.0),
            BeatRange::new(0.0, f64::NAN),
            BeatRange::new(0.0, f64::INFINITY),
            BeatRange::new(f64::NEG_INFINITY, 0.0),
            // Past 2^53 an f64 cannot resolve single 1/16 steps any more.
            BeatRange::new(1e18, 1e18 + 1.0),
        ];
        for range in empty {
            assert!(hits_over(&data, range).is_empty(), "{range:?} should emit nothing");
        }

        // A lane with no steps in any pattern is skipped, and so is a
        // `SeqData` with no lanes at all.
        let none = SeqData::empty();
        assert!(hits_over(&none, BeatRange::new(0.0, 4.0)).is_empty());
    }

    /// A nonsense tempo can hand the scan a block spanning millions of beats.
    /// It must come back — truncated — rather than spin `process()`.
    #[test]
    fn an_absurd_range_is_truncated_not_walked_forever() {
        let data = seq_data(&[spec(36, 16, Rate::SixtyFourthTriplet, ALL_STEPS)]);
        let lane = data.lane_data(0, 0).unwrap();

        let mut hits = Vec::new();
        let emitted =
            scan_lane(
                0,
                &data.lanes[0],
                lane,
                BeatRange::new(0.0, 1e9),
                &mut Prng::default(),
                &mut |hit: StepHit| hits.push(hit),
            );

        // The cap bounds the step positions *walked*, so the hits are at most
        // that many — here one fewer, the padding position at k = -1 having
        // fallen outside the range.
        assert_eq!(emitted, hits.len());
        assert!(hits.len() <= MAX_STEPS_PER_LANE, "walked past the cap: {}", hits.len());
        assert!(hits.len() >= MAX_STEPS_PER_LANE - 2, "truncated too early: {}", hits.len());

        // What it did produce is the honest beginning of the range, in order
        // — and, the cap being one block's event ceiling, never more than the
        // block could have carried anyway.
        let step_len = Rate::SixtyFourthTriplet.rate_beats();
        for (k, hit) in hits.iter().enumerate() {
            assert_eq!(hit.beat, k as f64 * step_len);
        }
    }

    /// `scan_lane` ignores mute and solo; only `scan_lanes` applies them.
    #[test]
    fn scan_lane_leaves_mute_and_solo_to_its_caller() {
        let data = seq_data(&[spec(36, 16, Rate::Quarter, ALL_STEPS)]);
        data.lanes[0].set_mute(true);

        assert!(hits_over(&data, BeatRange::new(0.0, 1.0)).is_empty());

        let mut hits = Vec::new();
        scan_lane(
            0,
            &data.lanes[0],
            data.lane_data(0, 0).unwrap(),
            BeatRange::new(0.0, 1.0),
            &mut Prng::default(),
            &mut |hit: StepHit| hits.push(hit),
        );
        assert_eq!(hits.len(), 1);

        assert!(lane_is_audible(&Lane::new(1, "x", 0, 36, 0), false));
        assert!(!lane_is_audible(&Lane::new(1, "x", 0, 36, 0), true));
    }

    // ---- Ratchets + probability -------------------------------------------

    /// Every hit `data` produces over `range`, drawing from `rng` — the
    /// probability-aware form of [`hits_over`].
    fn hits_with(data: &SeqData, range: BeatRange, rng: &mut Prng) -> Vec<StepHit> {
        let mut hits = Vec::new();
        let count = scan_lanes(data, range, |_| 0, rng, &mut |hit: StepHit| hits.push(hit));
        assert_eq!(count, hits.len(), "scan_lanes miscounted its hits");
        hits
    }

    /// Rewrites the given steps of lane 0, pattern 0.
    fn patch_steps(data: &SeqData, indices: &[usize], step: Step) {
        let lane = data.lane_data(0, 0).unwrap();
        for &i in indices {
            lane.set_step(i, step);
        }
    }

    /// The headline contract: ratchet `n` places its hits on
    /// `k * step_len + j * step_len / n`, for every count the three stored
    /// bits can hold.
    #[test]
    fn ratchets_land_on_exact_subdivisions_for_every_count() {
        let step_len = Rate::Eighth.rate_beats();

        for n in Step::RATCHET_MIN..=Step::RATCHET_MAX {
            // A 4-step 1/8 lane with steps 0 and 2 on: two bursts per half
            // bar, at k = 0 and k = 2, so the `k * step_len` term is exercised
            // and not just `j * sub`.
            let data = seq_data(&[spec(36, 4, Rate::Eighth, &[0, 2])]);
            patch_steps(&data, &[0, 2], Step::default_on().with_ratchet(n));

            let hits = hits_over(&data, BeatRange::new(0.0, 2.0));
            assert_eq!(hits.len(), 2 * n as usize, "ratchet {n} fired the wrong number of hits");

            // Expected positions derived differently from the implementation
            // — `(k + j/n) * step_len` rather than `k * step_len + j * sub` —
            // so agreeing to 1e-12 is a real check and not a tautology.
            let mut expected = hits.iter();
            for k in [0i64, 2] {
                for j in 0..n as i64 {
                    let hit = expected.next().unwrap();
                    let want = (k as f64 + j as f64 / n as f64) * step_len;
                    assert!(
                        (hit.beat - want).abs() < 1e-12,
                        "ratchet {n} hit {j} of k={k}: {} vs {want}",
                        hit.beat
                    );
                    assert_eq!(hit.step_index, k as usize, "ratchets share their step");
                    // The first sub-hit *is* the step, bit for bit: a ratchet
                    // must not nudge the beat it would have played on anyway.
                    if j == 0 {
                        assert_eq!(hit.beat, k as f64 * step_len);
                    }
                    // Each note sounds for half of its own subdivision, not
                    // half of the whole step (`GATE_FRACTION`).
                    assert!((hit.gate_beats() - step_len * GATE_FRACTION / n as f64).abs() < 1e-12);
                }
            }
        }
    }

    /// Powers of two divide a binary float exactly, so those ratchets must
    /// land on bit-identical beats — nothing else on the stitching path is
    /// allowed an epsilon either.
    #[test]
    fn power_of_two_ratchets_are_exact() {
        for n in [1u8, 2, 4, 8] {
            let data = seq_data(&[spec(36, 1, Rate::Quarter, &[0])]);
            patch_steps(&data, &[0], Step::default_on().with_ratchet(n));

            let hits = hits_over(&data, BeatRange::new(3.0, 4.0));
            let expected: Vec<f64> = (0..n).map(|j| 3.0 + j as f64 / n as f64).collect();
            assert_eq!(beats_of(&hits), expected, "ratchet {n}");
        }
    }

    /// A burst that began before this block still drops its remaining
    /// sub-hits into it — the reason the scan walks `k` from
    /// `floor(start / step_len)` and range-tests each `t` instead of the
    /// step's own beat.
    #[test]
    fn a_ratchet_burst_continues_into_the_next_block() {
        let data = seq_data(&[spec(36, 1, Rate::Quarter, &[0])]);
        patch_steps(&data, &[0], Step::default_on().with_ratchet(4));

        // The burst at k = 0 is 0.0 / 0.25 / 0.5 / 0.75; a range opening
        // mid-step gets the tail of it.
        assert_eq!(beats_of(&hits_over(&data, BeatRange::new(0.3, 1.0))), vec![0.5, 0.75]);

        // Chopped into uneven blocks, every sub-hit fires in exactly one of
        // them, in order and exactly once.
        let mut stitch = Stitch::new();
        let mut hits = Vec::new();
        for (start, span) in [(0.0, 0.3), (0.3, 0.3), (0.6, 0.2), (0.8, 0.2)] {
            let block = stitch.begin(start, span);
            scan_lanes(&data, block.range, |_| 0, &mut Prng::default(), &mut |hit: StepHit| {
                hits.push(hit)
            });
        }
        assert_eq!(beats_of(&hits), vec![0.0, 0.25, 0.5, 0.75]);
    }

    /// Block-size invariance with ratchets on, including a ratchet of 3 whose
    /// subdivision (1/12 beat) is not representable in binary: the beats must
    /// still be bit-identical however the host splits the timeline.
    #[test]
    fn ratcheted_steps_fire_once_each_however_the_host_splits_the_block() {
        let data = seq_data(&[spec(36, 16, Rate::Sixteenth, &[0, 4, 8, 12])]);
        patch_steps(&data, &[0, 8], Step::default_on().with_ratchet(3));
        patch_steps(&data, &[4, 12], Step::default_on().with_ratchet(4));

        // 92160 samples = 3.84 beats at 120 BPM / 48 kHz, divisible by every
        // block size below (the same span the four-on-the-floor test uses).
        const SAMPLES: usize = 92_160;
        let one = hits_in_blocks(&data, 120.0, 48_000.0, SAMPLES, 1);
        // 3.84 beats is one pass short of the 4-beat loop: steps 0 and 8
        // burst 3 hits each, steps 4 and 12 burst 4.
        assert_eq!(one.len(), 3 + 4 + 3 + 4);

        for block in [1usize, 64, 2048] {
            let many = hits_in_blocks(&data, 120.0, 48_000.0, block, SAMPLES / block);
            assert_eq!(beats_of(&many), beats_of(&one), "block size {block}");
        }
    }

    /// The two certain probabilities are exactly that, and — because neither
    /// draws — a pattern built only from them plays the same whatever else
    /// shares the generator.
    #[test]
    fn probability_0_never_fires_and_100_always_does() {
        const SEED: u64 = 0x00C0_FFEE;
        let data = seq_data(&[spec(36, 1, Rate::Sixteenth, &[0])]);
        let range = BeatRange::new(0.0, 64.0);

        patch_steps(&data, &[0], Step::default_on().with_probability(0));
        let mut rng = Prng::new(SEED);
        assert!(hits_with(&data, range, &mut rng).is_empty(), "probability 0 fired");

        // A never-firing step must not suppress its ratchets either.
        patch_steps(&data, &[0], Step::default_on().with_probability(0).with_ratchet(8));
        assert!(hits_with(&data, range, &mut rng).is_empty(), "probability 0 ratcheted");

        patch_steps(&data, &[0], Step::default_on().with_probability(100));
        assert_eq!(hits_with(&data, range, &mut rng).len(), 64 * 4);

        // Neither extreme consumed a draw, so the generator is untouched.
        assert_eq!(rng.next_u32(), Prng::new(SEED).next_u32());

        assert!(!probability_fires(0, &mut rng));
        assert!(probability_fires(Step::PROBABILITY_MAX, &mut rng));
    }

    /// One roll per step *occurrence*, shared by the whole burst: a ratcheted
    /// step is all-or-nothing, never a partial group.
    #[test]
    fn one_roll_covers_every_ratchet_of_a_step() {
        // Ratchet 8 at 1/4 keeps a whole burst inside one beat, so a block of
        // exactly one beat always holds whole groups.
        let data = seq_data(&[spec(36, 1, Rate::Quarter, &[0])]);
        patch_steps(&data, &[0], Step::default_on().with_ratchet(8).with_probability(50));

        let mut rng = Prng::new(7);
        let mut fired = 0;
        for beat in 0..400i64 {
            let range = BeatRange::new(beat as f64, beat as f64 + 1.0);
            let hits = hits_with(&data, range, &mut rng);
            assert!(hits.len() == 8 || hits.is_empty(), "partial burst of {}", hits.len());
            fired += usize::from(!hits.is_empty());
        }

        // A 50% step over 400 occurrences: both outcomes happen, roughly
        // evenly. Wide bounds — this asserts "the roll is used", not the
        // generator's quality (that has its own tests).
        assert!((150..=250).contains(&fired), "{fired} of 400 bursts fired");
    }

    /// Fixed seed in, identical output out; a different seed diverges.
    #[test]
    fn a_seeded_run_replays_exactly_and_different_seeds_diverge() {
        let data = seq_data(&[spec(36, 16, Rate::Sixteenth, ALL_STEPS)]);
        for i in 0..16usize {
            // Every step genuinely uncertain, so every occurrence draws, and
            // a different probability per step so the draw order matters.
            let step = Step::default_on().with_ratchet(2).with_probability(40 + i as u8);
            patch_steps(&data, &[i], step);
        }

        let run = |seed: u64| {
            let mut rng = Prng::new(seed);
            beats_of(&hits_with(&data, BeatRange::new(0.0, 32.0), &mut rng))
        };

        let baseline = run(0x1234_5678);
        assert_eq!(baseline, run(0x1234_5678), "a seeded run must replay exactly");
        assert_ne!(baseline, run(0x8765_4321), "two seeds produced the same run");

        // 128 occurrences at ~47% average, two hits each: enough fired to
        // make the equality above meaningful, and not all of them.
        assert!((100..=250).contains(&baseline.len()), "{} hits", baseline.len());

        // Same seed, same data, but driven as stitched one-beat blocks: the
        // draws line up because each occurrence sits wholly inside a block.
        let mut rng = Prng::new(0x1234_5678);
        let mut stitch = Stitch::new();
        let mut blocked = Vec::new();
        for beat in 0..32 {
            let block = stitch.begin(beat as f64, 1.0);
            scan_lanes(&data, block.range, |_| 0, &mut rng, &mut |hit: StepHit| {
                blocked.push(hit)
            });
        }
        assert_eq!(beats_of(&blocked), baseline);
    }

    /// An occurrence with nothing in this block must not consume a draw —
    /// otherwise the padding step positions the scan walks at each end would
    /// shift every later roll.
    #[test]
    fn a_step_outside_the_range_consumes_no_randomness() {
        let data = seq_data(&[spec(36, 16, Rate::Sixteenth, ALL_STEPS)]);
        patch_steps(&data, ALL_STEPS, Step::default_on().with_probability(50));

        // [1.0, 2.0) holds steps 4..=7; the padding positions k = 3 and k = 8
        // are walked but must stay out of the generator.
        let mut rng = Prng::new(0xABCD);
        let hits = hits_with(&data, BeatRange::new(1.0, 2.0), &mut rng);

        let mut expected = Prng::new(0xABCD);
        let fired: Vec<f64> = (4..8)
            .filter(|_| probability_fires(50, &mut expected))
            .map(|k| k as f64 * Rate::Sixteenth.rate_beats())
            .collect();
        // `filter` is lazy and left-to-right, so the rolls above are drawn in
        // the same order the scan draws them.
        assert_eq!(beats_of(&hits), fired);
        assert_eq!(rng.next_u32(), expected.next_u32(), "the scan drew the wrong number of times");
    }

    // ---- Playhead position -----------------------------------------------

    /// A lane for the playhead tests: `length` steps at `rate`, all of them
    /// off, since where a lane *is* has nothing to do with what it plays.
    fn playhead_lane(length: u32, rate: Rate) -> LaneData {
        let data = LaneData::with_capacity(chunked_capacity(length as usize));
        data.set_length(length);
        data.set_rate(rate);
        data
    }

    /// The step a block ends on is the one it was still playing: a range that
    /// stops exactly on a step boundary has not reached that step yet.
    #[test]
    fn playhead_step_is_exclusive_at_the_range_end() {
        let lane = playhead_lane(16, Rate::Sixteenth);

        assert_eq!(playhead_step(&lane, BeatRange::new(0.0, 0.25)), Some(0));
        assert_eq!(playhead_step(&lane, BeatRange::new(0.0, 0.26)), Some(1));
        assert_eq!(playhead_step(&lane, BeatRange::new(0.25, 0.5)), Some(1));
        // A whole bar of blocks collapses to its last step, not its first.
        assert_eq!(playhead_step(&lane, BeatRange::new(0.0, 4.0)), Some(15));
        assert_eq!(playhead_step(&lane, BeatRange::new(0.0, 4.25)), Some(0));
    }

    /// The index wraps on `length` exactly as the scan's does, including for
    /// the negative beats of a count-in.
    #[test]
    fn playhead_step_wraps_on_length_in_both_directions() {
        let lane = playhead_lane(16, Rate::Sixteenth);
        for k in -40i64..40 {
            let range = BeatRange::new(k as f64 * 0.25, (k + 1) as f64 * 0.25);
            assert_eq!(
                playhead_step(&lane, range),
                Some(k.rem_euclid(16) as usize),
                "step {k} of a 16-step lane"
            );
        }

        // Shorter than its capacity, and the degenerate one-step case.
        let short = playhead_lane(3, Rate::Quarter);
        let steps: Vec<Option<usize>> = (0..7)
            .map(|k| playhead_step(&short, BeatRange::new(k as f64, k as f64 + 1.0)))
            .collect();
        assert_eq!(steps, [Some(0), Some(1), Some(2), Some(0), Some(1), Some(2), Some(0)]);

        let single = playhead_lane(1, Rate::Sixteenth);
        for k in 0..9 {
            let range = BeatRange::new(k as f64, k as f64 + 1.0);
            assert_eq!(playhead_step(&single, range), Some(0), "a one-step lane at beat {k}");
        }
    }

    /// The playhead walks the same grid the scan strikes on, for every rate:
    /// the step a hit reports and the step the range it fell in reports are
    /// the same one.
    #[test]
    fn playhead_step_agrees_with_the_scan_for_every_rate() {
        for rate in Rate::ALL {
            let data = seq_data(&[spec(36, 16, rate, ALL_STEPS)]);
            let lane = data.lane_data(0, 0).expect("lane exists");
            let step_len = rate.rate_beats();

            // One range per step occurrence, each covering just that step.
            for k in 0..24i64 {
                let start = k as f64 * step_len;
                let range = BeatRange::new(start, (k + 1) as f64 * step_len);
                let hits = hits_over(&data, range);
                assert_eq!(hits.len(), 1, "{rate}: step {k} must strike exactly once");
                assert_eq!(
                    playhead_step(lane, range),
                    Some(hits[0].step_index),
                    "{rate}: playhead disagrees with the scan at step {k}"
                );
            }
        }
    }

    /// Nothing to show rather than a wrong cell: an empty range, a lane with
    /// no steps, and a nonsense beat position all report `None`.
    #[test]
    fn playhead_step_reports_nothing_when_there_is_nothing_to_show() {
        let lane = playhead_lane(16, Rate::Sixteenth);

        assert_eq!(playhead_step(&lane, BeatRange::EMPTY), None);
        assert_eq!(playhead_step(&lane, BeatRange::new(1.0, 1.0)), None);
        assert_eq!(playhead_step(&lane, BeatRange::new(0.0, f64::NAN)), None);
        assert_eq!(playhead_step(&lane, BeatRange::new(0.0, f64::INFINITY)), None);
        // A beat position past 2^53 can no longer resolve single steps.
        assert_eq!(playhead_step(&lane, BeatRange::new(0.0, 1e18)), None);

        // A fresh lane always has a position, though: `length` is clamped
        // to at least one step, so the "no steps" guard is unreachable in
        // practice and only `rem_euclid`'s divisor depends on it.
        assert_eq!(playhead_step(&LaneData::empty(), BeatRange::new(0.0, 0.25)), Some(0));
    }

    // ---- Step boundaries -------------------------------------------------

    /// Where a pending pattern change lands: the first step start inside the
    /// range, half-open at both ends like everything else on this path.
    #[test]
    fn next_step_boundary_is_the_first_step_start_in_the_range() {
        let lane = playhead_lane(16, Rate::Sixteenth);

        // A range opening exactly on a boundary is due at once; one that
        // opens a hair later waits for the next step.
        assert_eq!(next_step_boundary(&lane, BeatRange::new(0.0, 0.25)), Some(0.0));
        assert_eq!(next_step_boundary(&lane, BeatRange::new(0.001, 0.25)), None);
        assert_eq!(next_step_boundary(&lane, BeatRange::new(0.001, 0.26)), Some(0.25));
        assert_eq!(next_step_boundary(&lane, BeatRange::new(0.25, 0.5)), Some(0.25));
        // The *first* of several, not the last.
        assert_eq!(next_step_boundary(&lane, BeatRange::new(0.1, 4.0)), Some(0.25));
        // Negative beats (a count-in) read the same way.
        assert_eq!(next_step_boundary(&lane, BeatRange::new(-0.3, -0.1)), Some(-0.25));

        // Nothing to switch on: no range, or a nonsense one.
        assert_eq!(next_step_boundary(&lane, BeatRange::EMPTY), None);
        assert_eq!(next_step_boundary(&lane, BeatRange::new(1.0, 1.0)), None);
        assert_eq!(next_step_boundary(&lane, BeatRange::new(f64::NAN, 1.0)), None);
        assert_eq!(next_step_boundary(&lane, BeatRange::new(0.0, f64::INFINITY)), None);
        assert_eq!(next_step_boundary(&lane, BeatRange::new(1e18, 1e18 + 1.0)), None);
    }

    /// The boundary is a beat the scan itself strikes on — exactly, for every
    /// rate including the triplets whose step length is not a binary
    /// fraction. That is what lets the engine cut a block there and still
    /// fire each step once.
    #[test]
    fn next_step_boundary_lands_on_the_grid_the_scan_strikes_on() {
        for rate in Rate::ALL {
            let data = seq_data(&[spec(36, 16, rate, ALL_STEPS)]);
            let lane = data.lane_data(0, 0).expect("lane exists");
            let step_len = rate.rate_beats();

            for k in 0..24i64 {
                let start = k as f64 * step_len;
                let next = (k + 1) as f64 * step_len;
                let hits = hits_over(&data, BeatRange::new(start, next));
                assert_eq!(hits.len(), 1, "{rate}: step {k} must strike exactly once");

                // A range opening on the step start is due there, and the
                // beat is the very one the scan struck.
                assert_eq!(
                    next_step_boundary(lane, BeatRange::new(start, next)),
                    Some(hits[0].beat),
                    "{rate}: step {k} boundary is off the scan's grid"
                );
                // And one opening inside the step is not due until the next
                // step, which this range stops just short of.
                let inside = start + step_len * 0.5;
                assert_eq!(
                    next_step_boundary(lane, BeatRange::new(inside, next)),
                    None,
                    "{rate}: step {k} switched mid-step"
                );
            }
        }
    }

    // ---- Block-size invariance --------------------------------------------

    /// Property-style tests over seeded random patches: however the host
    /// chops the timeline up, the concatenated event stream has to come out
    /// identical, down to the last bit of every beat position.
    ///
    /// Hand-rolled rather than `proptest` — the generator below is forty
    /// lines, the seeds are fixed, and the suite stays dependency-free and
    /// reproducible.
    mod invariance {
        use super::*;

        /// Seeds for the patch generator and for the scan's own generator.
        /// Fixed, so any failure here is reproducible for ever.
        const CASE_SEED: u64 = 0x51EE_D000_B107_C0DE;
        const SCAN_SEED: u64 = 0x0B10_C51E_ED00_7777;

        /// How far a generated note has to sit from the end of the compared
        /// span for a case to be usable.
        ///
        /// Stitching sums per-block spans, so after tens of thousands of
        /// blocks the running end sits a few ULPs — on the order of 1e-11
        /// beats — away from one big block's own `start + span`. A note
        /// landing inside that sliver is genuinely ambiguous: it belongs to
        /// whichever block reaches past it, and the two runs disagree about
        /// where exactly that is. Rather than let the comparison flap, the
        /// generator drops a case whose span ends that close to any lane's
        /// grid. The tolerance is ~10^5 times the drift it has to cover, and
        /// still far below one sample at any sane tempo.
        const END_CLEARANCE_BEATS: f64 = 1e-6;

        /// One randomly generated patch, plus the finest sub-hit grid each of
        /// its lanes can produce — what [`tail_is_clear`] tests the span end
        /// against.
        struct Patch {
            data: SeqData,
            grids: Vec<f64>,
        }

        /// A uniform draw from `lo..=hi`.
        fn rand_in(rng: &mut Prng, lo: u32, hi: u32) -> u32 {
            lo + rng.next_u32() % (hi - lo + 1)
        }

        /// Quarter notes and shorter: the rates that still put several
        /// occurrences inside the short spans the one-sample and loop runs
        /// can afford to walk.
        fn short_rates() -> Vec<Rate> {
            Rate::ALL[Rate::Quarter as usize..].to_vec()
        }

        /// A patch of up to `max_lanes` lanes with random keys, channels,
        /// rates, lengths, step masks, ratchets, velocities and pans, written
        /// into pattern 0 (the only one these tests read).
        ///
        /// Probabilities are only ever 0 or 100. A step in `1..=99` is rolled
        /// once per occurrence *per block*, so a ratchet burst that a block
        /// edge cuts in half is rolled in both blocks and its output really
        /// does depend on where the host split the timeline (a known open
        /// question, noted on `scan_lane`). That is the one thing this module
        /// cannot assert; mixing it in would make the test lie about the
        /// rest.
        fn random_patch(
            rng: &mut Prng,
            max_lanes: u32,
            rates: &[Rate],
            max_length: u32,
        ) -> Patch {
            let lane_count = rand_in(rng, 1, max_lanes) as usize;
            let mut lanes = Vec::with_capacity(lane_count);
            let mut datas = Vec::with_capacity(lane_count);
            let mut grids = Vec::with_capacity(lane_count);

            for pos in 0..lane_count {
                let key = rand_in(rng, 24, 96) as u8;
                let channel = rand_in(rng, 0, 15) as u8;
                let lane =
                    Lane::new(pos as u32 + 1, format!("lane {pos}"), 0x33_33_33, key, channel);
                // One lane in eight is muted: silence has to stay silent at
                // every block size too.
                if rand_in(rng, 0, 7) == 0 {
                    lane.set_mute(true);
                }

                let rate = rates[rand_in(rng, 0, rates.len() as u32 - 1) as usize];
                let step_len = rate.rate_beats();
                let length = rand_in(rng, 1, max_length);
                let data = LaneData::with_capacity(chunked_capacity(length as usize));
                data.set_length(length);
                data.set_rate(rate);

                let mut finest = step_len;
                for index in 0..length as usize {
                    // A third of the steps stay off, so the lanes have gaps
                    // for block edges to fall into.
                    if rand_in(rng, 0, 2) == 0 {
                        continue;
                    }
                    let ratchet =
                        rand_in(rng, Step::RATCHET_MIN as u32, Step::RATCHET_MAX as u32) as u8;
                    let probability =
                        if rand_in(rng, 0, 7) == 0 { 0 } else { Step::PROBABILITY_MAX };
                    let velocity = rand_in(rng, Step::VELOCITY_MIN as u32, 127) as u8;
                    let pan = rand_in(rng, 0, Step::PAN_MAX as u32) as u8;
                    data.set_step(index, Step::pack(true, velocity, pan, ratchet, probability));
                    finest = finest.min(step_len / ratchet as f64);
                }

                lanes.push(lane);
                datas.push(data);
                grids.push(finest);
            }

            let mut patterns: [Vec<LaneData>; NUM_PATTERNS] = std::array::from_fn(|_| Vec::new());
            patterns[0] = datas;
            Patch { data: SeqData::new(lanes, patterns), grids }
        }

        /// Whether `end` sits clear of every lane's sub-hit grid — see
        /// [`END_CLEARANCE_BEATS`] for why a case that fails this is dropped
        /// instead of compared.
        fn tail_is_clear(grids: &[f64], end: f64) -> bool {
            grids.iter().all(|&grid| {
                let nearest = (end / grid).round() * grid;
                (end - nearest).abs() > END_CLEARANCE_BEATS
            })
        }

        /// The hits in an emission-order-independent order: a per-block scan
        /// runs lane by lane *inside* each block, so two runs that agree on
        /// every hit still report them in different orders.
        ///
        /// Beats are compared with `total_cmp` and then asserted with `==`:
        /// the whole stitching design is that they are bit-identical.
        fn canonical(mut hits: Vec<StepHit>) -> Vec<StepHit> {
            hits.sort_by(|a, b| {
                a.beat
                    .total_cmp(&b.beat)
                    .then_with(|| a.lane_pos.cmp(&b.lane_pos))
                    .then_with(|| a.step_index.cmp(&b.step_index))
            });
            hits
        }

        /// Drives `data` through `blocks` stitched blocks of `block_samples`
        /// from song position 0 and returns the concatenated hits.
        ///
        /// Asserts the run really is one continuous stretch of timeline: only
        /// the very first block may report a jump. A jump later on would
        /// flush voices in the real engine, so a comparison across one would
        /// be meaningless — and it would mean the accumulated stitch end had
        /// drifted past [`Stitch::TOLERANCE`] from the host's position.
        fn stitched_hits(
            data: &SeqData,
            tempo: f64,
            sample_rate: f64,
            block_samples: usize,
            blocks: usize,
        ) -> Vec<StepHit> {
            let mut stitch = Stitch::new();
            let mut rng = Prng::new(SCAN_SEED);
            let mut hits = Vec::new();
            for index in 0..blocks {
                let (host_start, span) = host_block(tempo, sample_rate, block_samples, index);
                let block = stitch.begin(host_start, span);
                assert_eq!(
                    block.jumped,
                    index == 0,
                    "block {index} of {block_samples} samples broke continuity"
                );
                scan_lanes(data, block.range, |_| 0, &mut rng, &mut |hit: StepHit| {
                    hits.push(hit)
                });
            }
            hits
        }

        /// The beat span one `block_samples`-long block covers, i.e. the end
        /// of the timeline [`stitched_hits`] walks when given one block.
        fn span_of(tempo: f64, sample_rate: f64, block_samples: usize) -> f64 {
            host_block(tempo, sample_rate, block_samples, 0).1
        }

        /// Drives `data` round the host's loop `region` for `passes` passes
        /// in `block_samples`-long blocks, the way the engine does it: split
        /// the block at `loop_end`, scan each segment, then stitch the next
        /// block against the wrapped position.
        fn loop_hits(
            data: &SeqData,
            region: LoopRegion,
            tempo: f64,
            sample_rate: f64,
            block_samples: usize,
            passes: usize,
        ) -> Vec<StepHit> {
            let timing = BlockTiming::new(tempo, sample_rate, block_samples);
            let span = timing.span_beats();
            assert!(span > 0.0 && span < region.beats(), "the block must be shorter than the loop");

            let mut stitch = Stitch::new();
            let mut rng = Prng::new(SCAN_SEED);
            let mut hits = Vec::new();
            let mut host_start = region.start();
            let mut done = 0;
            let cap = (region.beats() * passes as f64 / span).ceil() as usize + passes + 4;

            for index in 0..cap {
                let block = stitch.begin(host_start, span);
                assert_eq!(block.jumped, index == 0, "block {index} broke continuity");
                let ranges = BlockRanges::split(block.range, Some(region), timing);

                // On the pass-completing wrap the tail belongs to the *next*
                // pass already, so it is left unscanned — otherwise the run
                // would be `passes` passes plus a fragment.
                let last = ranges.wrapped() && done + 1 == passes;
                let segments = if last { &ranges.segments()[..1] } else { ranges.segments() };
                for segment in segments {
                    scan_lanes(data, segment.range, |_| 0, &mut rng, &mut |hit: StepHit| {
                        hits.push(hit)
                    });
                }

                if ranges.wrapped() {
                    done += 1;
                    stitch.resume_from(ranges.resume_beat());
                }
                if last {
                    break;
                }
                // The host reports the position we told the stitcher to
                // expect, wrapped position included.
                host_start = stitch.prev_end().expect("continuity");
            }

            assert_eq!(done, passes, "the run did not finish {passes} loop passes");
            hits
        }

        /// The headline property: random tempos, sample rates and lane shapes
        /// played once as a single long block and again as stitched 64- and
        /// 2048-sample blocks. All three streams must be bit-identical.
        #[test]
        fn stitched_blocks_equal_one_big_block_for_random_patches() {
            // 2048 · 120 samples divides by both block sizes exactly and runs
            // 2.5–5.6 s of audio depending on the sample rate, i.e. 5 to 20
            // beats — long enough for every lane length here to wrap.
            const TOTAL_SAMPLES: usize = 2048 * 120;
            const CASES: usize = 48;
            let mut rng = Prng::new(CASE_SEED);
            let mut compared = 0;
            let mut total_hits = 0;

            for case in 0..CASES {
                // Two cases in three pin the tempo to 60 or 180 BPM; the
                // rest roam, off the integers so no step lands on a round
                // sample count.
                let tempo = match case % 3 {
                    0 => 60.0,
                    1 => 180.0,
                    _ => rand_in(&mut rng, 40, 220) as f64 + 0.5,
                };
                let sample_rate = [44_100.0, 48_000.0, 96_000.0][rand_in(&mut rng, 0, 2) as usize];
                let patch = random_patch(&mut rng, 5, &Rate::ALL, 32);

                if !tail_is_clear(&patch.grids, span_of(tempo, sample_rate, TOTAL_SAMPLES)) {
                    continue; // see END_CLEARANCE_BEATS
                }

                let one =
                    canonical(stitched_hits(&patch.data, tempo, sample_rate, TOTAL_SAMPLES, 1));
                for &block_samples in &[64usize, 2048] {
                    let many = canonical(stitched_hits(
                        &patch.data,
                        tempo,
                        sample_rate,
                        block_samples,
                        TOTAL_SAMPLES / block_samples,
                    ));
                    assert_eq!(
                        one, many,
                        "case {case}: {tempo} BPM at {sample_rate} Hz, \
                         {block_samples}-sample blocks"
                    );
                }
                compared += 1;
                total_hits += one.len();
            }

            // The clearance filter exists for the rare case, and an all-muted
            // patch compares trivially: neither may quietly hollow the test
            // out.
            assert!(compared >= CASES - 4, "only {compared} of {CASES} cases were compared");
            assert!(total_hits > 10_000, "only {total_hits} hits over {compared} cases");
        }

        /// The same comparison with the host handing over a single sample at a
        /// time, at both pinned tempos (60 and 180 BPM).
        ///
        /// One-sample blocks are the worst case for stitching: the running end
        /// is re-derived tens of thousands of times, and every step boundary
        /// either falls strictly inside some block or exactly on an edge
        /// between two. The span is shorter than the test above because the
        /// run walks one block per sample.
        #[test]
        fn single_sample_blocks_equal_one_big_block_at_60_and_180_bpm() {
            const TOTAL_SAMPLES: usize = 2048 * 32;
            const SAMPLE_RATE: f64 = 48_000.0;
            const CASES: usize = 8;
            let rates = short_rates();
            let mut rng = Prng::new(CASE_SEED ^ 1);
            let mut compared = 0;
            let mut total_hits = 0;

            for case in 0..CASES {
                let patch = random_patch(&mut rng, 3, &rates, 16);
                for &tempo in &[60.0, 180.0] {
                    if !tail_is_clear(&patch.grids, span_of(tempo, SAMPLE_RATE, TOTAL_SAMPLES)) {
                        continue; // see END_CLEARANCE_BEATS
                    }
                    let one = canonical(stitched_hits(
                        &patch.data,
                        tempo,
                        SAMPLE_RATE,
                        TOTAL_SAMPLES,
                        1,
                    ));
                    for &block_samples in &[1usize, 64, 2048] {
                        let many = canonical(stitched_hits(
                            &patch.data,
                            tempo,
                            SAMPLE_RATE,
                            block_samples,
                            TOTAL_SAMPLES / block_samples,
                        ));
                        assert_eq!(
                            one, many,
                            "case {case}: {tempo} BPM, {block_samples}-sample blocks"
                        );
                    }
                    compared += 1;
                    total_hits += one.len();
                }
            }

            assert_eq!(compared, CASES * 2, "a case was dropped by the clearance filter");
            assert!(total_hits > 500, "only {total_hits} hits over {compared} runs");
        }

        /// Loop wraps: a patch played over several passes of the host's cycle
        /// region must emit exactly what scanning `[loop_start, loop_end)`
        /// once per pass does — at every block size, including one sample.
        ///
        /// This needs no clearance filter. The split puts the head segment's
        /// end exactly on `loop_end` and the tail's start exactly on
        /// `loop_start`, and every edge in between is stitched bit-identically,
        /// so each pass is covered by `[loop_start, loop_end)` exactly however
        /// the blocks fall across it.
        #[test]
        fn loop_wraps_repeat_one_pass_at_every_block_size() {
            // (loop start, loop beats, tempo, block samples, passes). The
            // one-sample runs get the short loops: they walk a block per
            // sample, and 48 kHz is a lot of samples per beat at 60 BPM.
            const CONFIGS: &[(f64, f64, f64, usize, usize)] = &[
                (0.0, 4.0, 60.0, 2048, 3),
                (0.0, 4.0, 60.0, 64, 3),
                (1.0, 3.0, 180.0, 2048, 4),
                (1.0, 3.0, 180.0, 64, 4),
                (0.75, 0.5, 180.0, 1, 3),
                (2.5, 1.0, 60.0, 1, 2),
            ];
            let rates = short_rates();
            let mut rng = Prng::new(CASE_SEED ^ 2);
            let mut total_hits = 0;

            for (case, &(start, beats, tempo, block_samples, passes)) in
                CONFIGS.iter().enumerate()
            {
                let patch = random_patch(&mut rng, 3, &rates, 16);
                let region = LoopRegion::new(start, start + beats).expect("a usable loop");

                let driven = canonical(loop_hits(
                    &patch.data,
                    region,
                    tempo,
                    48_000.0,
                    block_samples,
                    passes,
                ));

                let pass = hits_over(&patch.data, BeatRange::new(region.start(), region.end()));
                let mut expected = Vec::with_capacity(pass.len() * passes);
                for _ in 0..passes {
                    expected.extend(pass.iter().copied());
                }

                assert_eq!(
                    driven,
                    canonical(expected),
                    "config {case}: loop [{start}, {}) at {tempo} BPM, \
                     {block_samples}-sample blocks",
                    start + beats
                );
                total_hits += driven.len();
            }

            assert!(total_hits > 200, "only {total_hits} hits over {} configs", CONFIGS.len());
        }
    }

    // ---- Edge cases --------------------------------------------------------

    /// The extremes of lane shape: the shortest and longest loops the model
    /// allows, no lanes at all, [`MAX_LANES`] of them, and rates whose grids
    /// share nothing.
    mod edge_cases {
        use super::*;
        use crate::model::{CAPACITY_CHUNK, MAX_LANES, MAX_STEPS};

        /// A `LaneData` of exactly `capacity` steps, loop length `length`,
        /// with every index in `on` switched on.
        ///
        /// Written out rather than reusing [`seq_data`]'s `LaneSpec`, whose
        /// `on` list tops out at the 32 indices [`ALL_STEPS`] holds while the
        /// lanes here run to [`MAX_STEPS`].
        fn lane_of(capacity: usize, length: u32, rate: Rate, on: &[usize]) -> LaneData {
            let data = LaneData::with_capacity(capacity);
            data.set_length(length);
            data.set_rate(rate);
            for &i in on {
                data.set_step(i, Step::default_on());
            }
            data
        }

        /// A one-lane patch holding `data` in pattern 0.
        fn one_lane(key: u8, data: LaneData) -> SeqData {
            let mut patterns: [Vec<LaneData>; NUM_PATTERNS] = std::array::from_fn(|_| Vec::new());
            patterns[0] = vec![data];
            SeqData::new(vec![Lane::new(1, "edge", 0x33_33_33, key, 0)], patterns)
        }

        /// `MAX_LANES`-sized patches: `count` lanes, each one step on at 1/4,
        /// with keys and channels that say which position they came from.
        fn many_lanes(count: usize) -> SeqData {
            let lanes: Vec<Lane> = (0..count)
                .map(|pos| {
                    let (key, channel) = ((24 + pos % 64) as u8, (pos % 16) as u8);
                    Lane::new(pos as u32 + 1, format!("lane {pos}"), 0x33_33_33, key, channel)
                })
                .collect();
            let mut patterns: [Vec<LaneData>; NUM_PATTERNS] = std::array::from_fn(|_| Vec::new());
            patterns[0] =
                (0..count).map(|_| lane_of(CAPACITY_CHUNK, 1, Rate::Quarter, &[0])).collect();
            SeqData::new(lanes, patterns)
        }

        /// The shortest loop there is: one step, re-struck every `step_len`
        /// beats for ever. Its index never leaves 0, and the steps sitting
        /// hidden past `length` stay silent.
        #[test]
        fn a_length_one_lane_repeats_its_only_step() {
            // A full chunk of steps, all on, but length 1: only step 0 may
            // ever sound.
            let on: Vec<usize> = (0..CAPACITY_CHUNK).collect();
            let data = one_lane(36, lane_of(CAPACITY_CHUNK, 1, Rate::Sixteenth, &on));

            let hits = hits_over(&data, BeatRange::new(0.0, 2.0));
            assert_eq!(hits.len(), 8, "one step, eight sixteenths");
            for (k, hit) in hits.iter().enumerate() {
                assert_eq!(hit.beat, k as f64 * 0.25);
                assert_eq!(hit.step_index, 0, "a length-1 lane only ever plays step 0");
            }

            // Pre-roll wraps the same way: `k.rem_euclid(1)` is 0 for every
            // negative `k` too, with no sign surprise and no division by a
            // zero length.
            let hits = hits_over(&data, BeatRange::new(-1.0, 0.0));
            assert_eq!(beats_of(&hits), vec![-1.0, -0.75, -0.5, -0.25]);
            assert_eq!(step_indices_of(&hits), vec![0; 4]);

            // And chopping the timeline up changes nothing: 0.25 beats is
            // 6000 samples at 120 BPM / 48 kHz, so each block holds exactly
            // one occurrence.
            let blocked = hits_in_blocks(&data, 120.0, 48_000.0, 6_000, 8);
            assert_eq!(beats_of(&blocked), beats_of(&hits_over(&data, BeatRange::new(0.0, 2.0))));
        }

        /// Length 1 at every one of the 21 rates: four occurrences on the
        /// exact multiples of that rate's step length.
        #[test]
        fn a_length_one_lane_wraps_at_every_rate() {
            for rate in Rate::ALL {
                let step_len = rate.rate_beats();
                let data = one_lane(36, lane_of(1, 1, rate, &[0]));

                let hits = hits_over(&data, BeatRange::new(0.0, step_len * 4.0));
                assert_eq!(hits.len(), 4, "{rate} should fire four times");
                for (k, hit) in hits.iter().enumerate() {
                    assert_eq!(hit.beat, k as f64 * step_len, "{rate} occurrence {k}");
                    assert_eq!(hit.step_index, 0);
                }
            }
        }

        /// The densest thing the scan can be asked for: one step, the maximum
        /// ratchet, so the sub-hits tile the timeline end to end. The seam
        /// where the single step wraps must be just another subdivision — no
        /// gap, no doubled hit.
        #[test]
        fn a_length_one_lane_ratchets_without_a_seam_at_the_wrap() {
            let data = one_lane(36, lane_of(1, 1, Rate::Sixteenth, &[]));
            let step = Step::pack(true, 100, 63, Step::RATCHET_MAX, Step::PROBABILITY_MAX);
            data.lane_data(0, 0).unwrap().set_step(0, step);

            // 1/16 split eight ways; four steps' worth over one beat.
            let sub = 0.25 / Step::RATCHET_MAX as f64;
            let hits = hits_over(&data, BeatRange::new(0.0, 1.0));
            assert_eq!(hits.len(), 32);
            for (i, hit) in hits.iter().enumerate() {
                assert_eq!(hit.beat, i as f64 * sub, "sub-hit {i}");
                assert_eq!(hit.gate_beats(), sub * GATE_FRACTION);
                assert_eq!(hit.step_index, 0);
            }
        }

        /// The longest loop there is: [`MAX_STEPS`] steps. It wraps at 4096 —
        /// not at a capacity chunk, not at some 16-bit boundary — and the
        /// index is `k.rem_euclid(4096)` throughout.
        #[test]
        fn a_max_steps_lane_wraps_after_its_last_step() {
            // 1/64 steps, so the whole loop is a (long) 256 beats.
            const STEP_LEN: f64 = 0.0625;
            let on: Vec<usize> = (0..MAX_STEPS).collect();
            let data = one_lane(36, lane_of(MAX_STEPS, MAX_STEPS as u32, Rate::SixtyFourth, &on));
            let lane_data = data.lane_data(0, 0).unwrap();
            assert_eq!(lane_data.length(), MAX_STEPS as u32);

            // Asking for a longer loop than the capacity cannot widen it, so
            // the wrap below is the real ceiling rather than one this test
            // happened to set.
            lane_data.set_length(MAX_STEPS as u32 + 1_000);
            assert_eq!(lane_data.length(), MAX_STEPS as u32);

            // The wrap itself: the last two steps of a pass, then the first
            // two of the next.
            let wrap = hits_over(&data, BeatRange::new(4_094.0 * STEP_LEN, 4_098.0 * STEP_LEN));
            assert_eq!(beats_of(&wrap), vec![255.875, 255.937_5, 256.0, 256.062_5]);
            assert_eq!(step_indices_of(&wrap), vec![4_094, 4_095, 0, 1]);

            // And every step of a pass fires exactly once, in order, when the
            // loop is played through in 2048-sample blocks. 2999 of them at
            // 120 BPM / 48 kHz end at 255.9147 beats — inside the gap between
            // steps 4094 and 4095, so the tail is unambiguous.
            let hits = hits_in_blocks(&data, 120.0, 48_000.0, 2_048, 2_999);
            assert_eq!(hits.len(), 4_095);
            for (k, hit) in hits.iter().enumerate() {
                assert_eq!(hit.beat, k as f64 * STEP_LEN, "step {k}");
                assert_eq!(hit.step_index, k, "step {k}");
            }
        }

        /// No lanes at all — a fresh instance before the user adds one, or one
        /// they emptied. Every range is silent and nothing divides by the lane
        /// count.
        #[test]
        fn no_lanes_emit_nothing() {
            let empty = SeqData::empty();
            assert_eq!(empty.lane_count(), 0);

            for range in
                [BeatRange::new(0.0, 4.0), BeatRange::new(-8.0, 8.0), BeatRange::new(1e6, 1e6 + 4.0)]
            {
                assert!(hits_over(&empty, range).is_empty(), "{range:?} should be silent");
            }
            assert!(hits_in_blocks(&empty, 120.0, 48_000.0, 512, 100).is_empty());

            // A pattern index that does not exist reads as "this lane has
            // nothing to play" rather than panicking — the engine may ask
            // while a state load is replacing the data.
            let data = seq_data(&[spec(36, 16, Rate::Sixteenth, ALL_STEPS)]);
            let mut hits = 0;
            let count = scan_lanes(
                &data,
                BeatRange::new(0.0, 4.0),
                |_| NUM_PATTERNS,
                &mut Prng::default(),
                &mut |_: StepHit| hits += 1,
            );
            assert_eq!((count, hits), (0, 0));
        }

        /// [`MAX_LANES`] lanes all scan, in position order, and the cap really
        /// caps: lanes past it never reach the scan at all.
        #[test]
        fn max_lanes_all_scan_in_lane_order() {
            let data = many_lanes(MAX_LANES);
            assert_eq!(data.lane_count(), MAX_LANES);

            let hits = hits_over(&data, BeatRange::new(0.0, 1.0));
            assert_eq!(hits.len(), MAX_LANES);
            for (pos, hit) in hits.iter().enumerate() {
                assert_eq!(hit.lane_pos, pos, "lanes scan in position order");
                assert_eq!(hit.beat, 0.0);
                assert_eq!((hit.key, hit.channel), ((24 + pos % 64) as u8, (pos % 16) as u8));
            }

            // Muting every other lane halves the output without disturbing
            // the order of what is left.
            for (pos, lane) in data.lanes.iter().enumerate() {
                lane.set_mute(pos % 2 == 1);
            }
            let audible = hits_over(&data, BeatRange::new(0.0, 1.0));
            assert_eq!(audible.len(), MAX_LANES / 2);
            assert!(audible.iter().enumerate().all(|(i, hit)| hit.lane_pos == i * 2));

            // Past the cap `SeqData` drops the surplus, so the scan can never
            // be handed more lanes than this.
            let over = many_lanes(MAX_LANES + 8);
            assert_eq!(over.lane_count(), MAX_LANES);
            assert_eq!(hits_over(&over, BeatRange::new(0.0, 1.0)).len(), MAX_LANES);
        }

        /// Straight, triplet and dotted rates at co-prime lengths, scanned far
        /// from song start: each lane lands exactly where the phase formula
        /// computed from position 0 puts it, both cold and after playing all
        /// the way there.
        ///
        /// Polyrhythm rather than polymeter: the lanes disagree about the
        /// length of a *step*, not just about how many steps a loop holds, so
        /// their grids share no point inside the window at all.
        #[test]
        fn triplet_and_dotted_polyrhythm_matches_phase_from_zero() {
            // (loop length, rate, hits expected in the window below).
            const LANES: &[(u32, Rate, usize)] = &[
                (16, Rate::Sixteenth, 16),
                (15, Rate::EighthTriplet, 12),
                (13, Rate::QuarterDotted, 3),
                (11, Rate::SixteenthTriplet, 24),
            ];
            // Four beats whose edges sit off every lane's grid — no hit can
            // land on a window boundary, so the counts above are exact and
            // not a question of which side of an edge a triplet rounds to.
            let window = BeatRange::new(97.1, 101.1);

            let data = seq_data(&[
                spec(36, LANES[0].0, LANES[0].1, ALL_STEPS),
                spec(38, LANES[1].0, LANES[1].1, ALL_STEPS),
                spec(40, LANES[2].0, LANES[2].1, ALL_STEPS),
                spec(41, LANES[3].0, LANES[3].1, ALL_STEPS),
            ]);

            // Where each lane "would be": derived from song position 0 with no
            // scan involved, as `(beat, lane, step index)`.
            let mut expected: Vec<(f64, usize, usize)> = Vec::new();
            for (pos, &(length, rate, want)) in LANES.iter().enumerate() {
                let step_len = rate.rate_beats();
                let first = (window.start / step_len).floor() as i64 - 2;
                let last = (window.end / step_len).floor() as i64 + 2;
                let mut count = 0;
                for k in first..=last {
                    let beat = k as f64 * step_len;
                    if window.contains(beat) {
                        expected.push((beat, pos, k.rem_euclid(length as i64) as usize));
                        count += 1;
                    }
                }
                assert_eq!(count, want, "lane {pos} ({rate}) hit count");
            }

            // Beats are compared exactly: both sides compute them as
            // `k * step_len`, which the scan is required to do too.
            let key = |rows: &mut Vec<(f64, usize, usize)>| {
                rows.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1)))
            };
            key(&mut expected);
            assert_eq!(expected.len(), LANES.iter().map(|l| l.2).sum::<usize>());

            let mut cold: Vec<(f64, usize, usize)> = hits_over(&data, window)
                .iter()
                .map(|hit| (hit.beat, hit.lane_pos, hit.step_index))
                .collect();
            key(&mut cold);
            assert_eq!(cold, expected, "a cold scan at bar 25 is off its from-zero phase");

            // Reaching the window by playing from the top gives the same hits
            // bit for bit: the triplet and dotted grids do not drift across
            // ~800 stitched blocks.
            let mut stitch = Stitch::new();
            let mut warm = Vec::new();
            for index in 0..816 {
                let block = stitch.begin(index as f64 * 0.125, 0.125);
                assert_eq!(block.jumped, index == 0, "block {index} broke continuity");
                scan_lanes(&data, block.range, |_| 0, &mut Prng::default(), &mut |hit: StepHit| {
                    if window.contains(hit.beat) {
                        warm.push((hit.beat, hit.lane_pos, hit.step_index));
                    }
                });
            }
            key(&mut warm);
            assert_eq!(warm, expected, "playing from zero drifted off the phase formula");
        }
    }
}
