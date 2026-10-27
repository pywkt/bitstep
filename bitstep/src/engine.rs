//! The per-block driver: one function that turns "where the host is" into
//! "the note events this block carries".
//!
//! Everything `process()` does with the timeline happens here, so it can be
//! driven by unit tests with a hand-built [`BlockCtx`] instead of a host.
//! `process()` itself stays thin: read the transport into a [`BlockCtx`],
//! take one `SeqData` snapshot, call [`Engine::run_block`], then convert the
//! [`BlockOutput`] into nice-plug events.
//!
//! Like the modules it drives, this one imports no nice-plug types — a
//! [`BlockCtx`] is plain numbers, which is exactly what makes the driver
//! testable. Nothing here allocates, locks or blocks: the voice table and the
//! output buffer are fixed-capacity and allocated before the audio thread
//! ever sees them.

use crate::model::{MAX_LANES, NUM_PATTERNS, Playhead, SeqData};
use crate::sequencer::{
    BeatRange, BlockRanges, BlockSegment, BlockTiming, EventBuf, LoopRegion, Prng, SeqEvent,
    StepHit, StepSink, Stitch, TimedEvent, next_step_boundary, playhead_step, scan_lanes,
};
use crate::voices::{EventSink, VoiceTable};

/// Everything the engine needs to know about one block, extracted from the
/// host transport.
///
/// The shell is responsible for the "missing data reads as stopped" rule: a
/// host that reports no position while the transport sits still produces
/// `playing: false`, and then the only thing the block does is release
/// whatever is still sounding.
///
/// Hostile or nonsense values are safe to pass: a non-finite position, a zero
/// or NaN tempo or sample rate, a zero-sample block and an out-of-range
/// pattern selection all resolve to "nothing to sequence" rather than to a
/// panic.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct BlockCtx {
    pub playing: bool,
    /// The host's absolute song position at the start of the block, in beats.
    pub pos_beats: f64,
    /// Block-constant tempo in BPM; v1 has no mid-block tempo ramps.
    pub tempo: f64,
    pub sample_rate: f32,
    pub num_samples: usize,
    /// The host's active loop (cycle) region in beats, if it is looping.
    pub loop_range: Option<(f64, f64)>,
    /// The `patsel` parameter, `1..=8`.
    pub pattern_select: u8,
}

impl BlockCtx {
    /// The lowest and highest values the `patsel` parameter takes; the
    /// pattern *index* is one less.
    pub const PATTERN_SELECT_MIN: u8 = 1;
    pub const PATTERN_SELECT_MAX: u8 = NUM_PATTERNS as u8;

    /// A stopped block of no particular length — what the shell builds when
    /// the host reports no usable transport at all.
    pub const STOPPED: BlockCtx = BlockCtx {
        playing: false,
        pos_beats: 0.0,
        tempo: 0.0,
        sample_rate: 0.0,
        num_samples: 0,
        loop_range: None,
        pattern_select: Self::PATTERN_SELECT_MIN,
    };

    /// A playing block at `pos_beats`, with no loop and pattern 1 selected;
    /// [`BlockCtx::with_loop`] and [`BlockCtx::with_pattern_select`] add the
    /// rest.
    pub const fn new(pos_beats: f64, tempo: f64, sample_rate: f32, num_samples: usize) -> BlockCtx {
        BlockCtx {
            playing: true,
            pos_beats,
            tempo,
            sample_rate,
            num_samples,
            loop_range: None,
            pattern_select: Self::PATTERN_SELECT_MIN,
        }
    }

    /// The same block with the host looping over `[start, end)`.
    pub const fn with_loop(mut self, start: f64, end: f64) -> BlockCtx {
        self.loop_range = Some((start, end));
        self
    }

    /// The same block with `patsel` set to `select` (`1..=8`).
    pub const fn with_pattern_select(mut self, select: u8) -> BlockCtx {
        self.pattern_select = select;
        self
    }

    /// The block's sample ↔ beat conversion. Unusable transport data
    /// collapses to a zero-length block (see [`BlockTiming`]).
    pub fn timing(&self) -> BlockTiming {
        BlockTiming::new(self.tempo, self.sample_rate as f64, self.num_samples)
    }

    /// The host loop as the splitter wants it, or `None` when the host is not
    /// looping over a usable region (see [`LoopRegion::new`]).
    pub fn loop_region(&self) -> Option<LoopRegion> {
        LoopRegion::from_host(self.loop_range)
    }

    /// The index into `SeqData::patterns` that `pattern_select` names, i.e.
    /// `pattern_select - 1`, clamped so a value from outside the parameter's
    /// range can still only select a real pattern.
    pub const fn pattern_index(&self) -> usize {
        let select = if self.pattern_select < Self::PATTERN_SELECT_MIN {
            Self::PATTERN_SELECT_MIN
        } else if self.pattern_select > Self::PATTERN_SELECT_MAX {
            Self::PATTERN_SELECT_MAX
        } else {
            self.pattern_select
        };
        (select - Self::PATTERN_SELECT_MIN) as usize
    }

    /// The beat stamp the offset-0 flush note-offs carry on a stop or a jump.
    ///
    /// Only a stamp — the events go out at sample offset 0 whatever it says.
    /// It is the host's own position, because that is where playback actually
    /// broke off, which is what makes a flush legible in the output; and it is
    /// sanitised, because a non-finite value would poison the block's sort
    /// order.
    pub fn flush_beat(&self) -> f64 {
        if self.pos_beats.is_finite() { self.pos_beats } else { 0.0 }
    }
}

impl Default for BlockCtx {
    fn default() -> BlockCtx {
        BlockCtx::STOPPED
    }
}

/// One block's generated events, each paired with the sample offset it fires
/// at: a fixed buffer of `(sample_offset, SeqEvent)`.
///
/// This *is* [`EventBuf`] rather than a wrapper around it: the buffer already
/// is the block's fixed-capacity output, with the overflow counter and the
/// note-offs-before-note-ons sort the shell needs. The alias exists so the
/// type reads as what it is at the [`Engine::run_block`] boundary.
pub type BlockOutput = EventBuf;

/// Where the voice table's events go: into the block's output, with each
/// event's beat turned into a sample offset within one segment.
struct SegmentSink<'a> {
    segment: &'a BlockSegment,
    /// The segment's position in the block (0, or 1 after a loop wrap), so
    /// the sort can keep the two pieces in timeline order.
    index: u8,
    out: &'a mut BlockOutput,
}

impl EventSink for SegmentSink<'_> {
    fn emit(&mut self, event: SeqEvent) {
        let offset = self.segment.offset_of(event.beat());
        self.out.push_timed(TimedEvent::in_segment(offset, self.index, event));
    }
}

/// How many step hits one scan piece can hold before the rest are dropped.
/// Each hit becomes at least one event, so anything past the event buffer's
/// own capacity would be dropped there anyway.
const HIT_CAPACITY: usize = EventBuf::CAPACITY;

/// Collects a scan piece's hits so they can be struck in time order.
///
/// The scan walks lane by lane, but the voice table has to see strikes in
/// the order they happen: two lanes on one key interleave, and a strike
/// processed out of order would retrigger a voice that musically starts
/// after it — releasing that voice before its own note-on. Fixed capacity,
/// allocated once in [`Engine::new`], so this is safe on the audio thread.
struct HitBuf<'a> {
    hits: &'a mut Vec<StepHit>,
    dropped: u32,
}

impl StepSink for HitBuf<'_> {
    fn strike(&mut self, hit: StepHit) {
        if self.hits.len() < HIT_CAPACITY {
            self.hits.push(hit);
        } else {
            self.dropped = self.dropped.saturating_add(1);
        }
    }
}

/// Where the flush note-offs go: sample offset 0, whatever beat they are
/// stamped with.
struct FlushSink<'a> {
    out: &'a mut BlockOutput,
}

impl EventSink for FlushSink<'_> {
    fn emit(&mut self, event: SeqEvent) {
        self.out.push(0, event);
    }
}

/// Which pattern each lane is currently reading, and which one `patsel` has
/// asked for.
///
/// A parameter change must not land mid-step: a lane half-way through a 1/4
/// note keeps playing the old pattern's step until that step ends. So the
/// selection is held as `pending` and copied into a lane's slot of `active`
/// only once the scan reaches that lane's next step boundary. Lanes at
/// different rates therefore change at different moments, which is the point
/// — a 1/16 hat follows within a sixteenth, a 1/1 pad finishes its bar — and
/// a lane whose step grid the block never crosses simply waits.
///
/// A fixed array of [`MAX_LANES`] entries, written and read by lane position
/// only, so there is nothing to allocate or resize when the patch's shape
/// changes under it: a position no lane occupies holds a harmless value.
#[derive(Clone, Debug)]
struct PatternSwitch {
    pending: usize,
    active: [usize; MAX_LANES],
}

impl PatternSwitch {
    /// Everything reading pattern 1, nothing pending.
    fn new() -> PatternSwitch {
        PatternSwitch { pending: 0, active: [0; MAX_LANES] }
    }

    /// Every lane onto `pattern` at once — what a stop or a jump does, since
    /// no step survives either of them to be protected.
    fn reset_to(&mut self, pattern: usize) {
        self.pending = pattern;
        self.active = [pattern; MAX_LANES];
    }

    /// Takes this block's parameter value as the pattern to switch *towards*.
    fn request(&mut self, pattern: usize) {
        self.pending = pattern;
    }

    /// The pattern lane `pos` is reading right now.
    fn active(&self, pos: usize) -> usize {
        self.active.get(pos).copied().unwrap_or(self.pending)
    }

    /// The pattern to show the GUI: the pending one once every lane has
    /// caught up, and otherwise one still being played by a lane that has
    /// not, so a pattern button lights up when its switch has actually landed
    /// rather than when the parameter moved.
    fn published(&self) -> usize {
        self.active.iter().copied().find(|&pattern| pattern != self.pending).unwrap_or(self.pending)
    }

    /// The earliest beat in `range` at which some lane's pending change comes
    /// due, or `None` when no lane has one coming due inside it.
    ///
    /// A lane the pattern it is reading does not hold has no step in progress
    /// to finish, so its change is due immediately; a real lane waits for its
    /// next step boundary.
    fn next_due(&self, data: &SeqData, range: BeatRange) -> Option<f64> {
        let mut soonest: Option<f64> = None;
        for pos in 0..MAX_LANES {
            if let Some(due) = self.due_at(data, pos, range)
                && soonest.is_none_or(|best| due < best)
            {
                soonest = Some(due);
            }
        }
        soonest
    }

    /// When lane `pos`'s pending change comes due within `range`, if it has
    /// one pending at all.
    fn due_at(&self, data: &SeqData, pos: usize, range: BeatRange) -> Option<f64> {
        if self.active(pos) == self.pending {
            return None;
        }
        match data.lane_data(self.active(pos), pos) {
            None => Some(range.start),
            Some(lane_data) => next_step_boundary(lane_data, range),
        }
    }

    /// Switches every lane whose pending change is due exactly at `due`
    /// within `range` — the counterpart to [`PatternSwitch::next_due`], which
    /// found that beat.
    fn apply_due(&mut self, data: &SeqData, due: f64, range: BeatRange) {
        for pos in 0..MAX_LANES {
            if self.due_at(data, pos, range) == Some(due) {
                self.active[pos] = self.pending;
            }
        }
    }
}

/// The per-block driver, and the state that outlives a block: where the last
/// block ended, which notes are sounding, which pattern each lane reads, and
/// the random source the probability rolls draw from.
///
/// The output buffer is deliberately *not* in here — it is passed to
/// [`Engine::run_block`] — so the shell owns one buffer for the lifetime of
/// the plugin instance while the engine stays a function of
/// `(state, ctx, data)`.
#[derive(Clone, Debug)]
pub struct Engine {
    stitch: Stitch,
    voices: VoiceTable,
    patterns: PatternSwitch,
    prng: Prng,
    /// Scratch space for one scan piece's hits ([`HitBuf`]); empty between
    /// pieces, capacity [`HIT_CAPACITY`] for the engine's whole life.
    hits: Vec<StepHit>,
    /// Set by [`Engine::request_flush`], honoured by the next
    /// [`Engine::run_block`]. `reset()` can only ask for the flush — note
    /// events exist solely inside `process()`.
    pending_flush: bool,
}

impl Engine {
    /// An engine with no history, nothing sounding, and the default seed.
    pub fn new() -> Engine {
        Engine::with_seed(Prng::DEFAULT_SEED)
    }

    /// [`Engine::new`] with a chosen random seed, which is how tests get
    /// reproducible probability rolls.
    pub fn with_seed(seed: u64) -> Engine {
        Engine {
            stitch: Stitch::new(),
            voices: VoiceTable::new(),
            patterns: PatternSwitch::new(),
            prng: Prng::new(seed),
            hits: Vec::with_capacity(HIT_CAPACITY),
            pending_flush: false,
        }
    }

    /// Asks the next block to release everything that is sounding, whatever
    /// the transport says.
    ///
    /// This is what `Plugin::reset()` and `deactivate()` call: they run where
    /// no events can be emitted (and `reset()` runs on the audio thread), so
    /// they cannot flush themselves — they leave a flag, and the next
    /// `process()` turns it into note-offs at sample offset 0. Setting a
    /// `bool` allocates nothing and cannot block, which is what makes it safe
    /// there.
    pub fn request_flush(&mut self) {
        self.pending_flush = true;
    }

    /// Whether a [`Engine::request_flush`] is still waiting for a block to
    /// carry it out.
    pub fn flush_pending(&self) -> bool {
        self.pending_flush
    }

    /// The notes currently sounding.
    pub fn voices(&self) -> &VoiceTable {
        &self.voices
    }

    /// The pattern index lane `pos` is reading, which lags the `patsel`
    /// parameter until that lane's next step boundary.
    pub fn active_pattern(&self, pos: usize) -> usize {
        self.patterns.active(pos)
    }

    /// Where the next block will stitch against, or `None` when continuity is
    /// unknown — a fresh instance, or a stopped transport.
    pub fn prev_end(&self) -> Option<f64> {
        self.stitch.prev_end()
    }

    /// Generates one block's events into `out` and publishes where playback
    /// now is into `playhead`.
    ///
    /// In order:
    /// 0. **Pending flush** ([`Engine::request_flush`]): release everything
    ///    sounding at offset 0 and forget the stitch, so the rest of the
    ///    block starts from silence.
    /// 1. **Stopped** (`!ctx.playing`): flush every sounding note as a
    ///    note-off at offset 0, forget the stitch so the next playing block
    ///    reads as a jump, blank the playhead, and return. Forwarding the
    ///    host's own input is `process()`'s job, not this one's.
    /// 2. **Stitch**: continue from the previous block's end when the host's
    ///    position is within [`Stitch::TOLERANCE`] of it — bit-identically,
    ///    so a step on the seam fires in exactly one block — and otherwise
    ///    treat the block as a jump, flushing first.
    /// 3. **Sequence** each segment of the (loop-split) block: release the
    ///    gates closing inside it, run the lane scan through the voice table,
    ///    then release the gates the scan itself just opened inside it. A
    ///    host loop wrapping inside the block gives two segments, and the
    ///    second one's offsets carry on where the first one's left off.
    /// 4. **Publish** each lane's current step and the pattern being played.
    /// 5. **Sort** the block into the order a host must receive it.
    ///
    /// `out` is cleared first, so one buffer serves every block (its overflow
    /// counter is per block too).
    pub fn run_block(
        &mut self,
        ctx: &BlockCtx,
        data: &SeqData,
        playhead: &Playhead,
        out: &mut BlockOutput,
    ) {
        out.clear();
        let flush_beat = ctx.flush_beat();
        let pattern = ctx.pattern_index();
        if std::mem::take(&mut self.pending_flush) {
            // Playback was interrupted somewhere the engine never saw (a
            // `reset()`, a deactivate): release everything and forget the
            // stitch, so whatever this block does starts from silence rather
            // than from a continuity the host has already broken.
            self.stitch.reset();
            self.voices.flush_all(flush_beat, &mut FlushSink { out });
        }

        if !ctx.playing {
            // Forget continuity before anything else can read it: the
            // transport may restart anywhere, and resuming a stale stitch
            // would sequence a stretch of timeline the host never played.
            self.stitch.reset();
            self.patterns.reset_to(pattern);
            self.voices.flush_all(flush_beat, &mut FlushSink { out });
            playhead.clear();
            out.sort();
            return;
        }

        let timing = ctx.timing();
        let block = self.stitch.begin(ctx.pos_beats, timing.span_beats());
        if block.jumped {
            self.voices.flush_all(flush_beat, &mut FlushSink { out });
            // Nothing is mid-step across a jump, so there is no boundary to
            // wait for: the selected pattern takes effect here.
            self.patterns.reset_to(pattern);
        } else {
            self.patterns.request(pattern);
        }

        let ranges = BlockRanges::split(block.range, ctx.loop_region(), timing);
        // After a loop wrap what the host reports next block is the wrapped
        // position, not the block's un-wrapped end; with no wrap this is that
        // end, i.e. exactly what `begin` already recorded.
        self.stitch.resume_from(ranges.resume_beat());

        for (index, segment) in ranges.segments().iter().enumerate() {
            let index = index as u8;
            // Gates closing in this segment that were opened in an earlier
            // one, or in an earlier block.
            self.voices
                .drain_ending(segment.range, &mut SegmentSink { segment, index, out: &mut *out });
            self.scan_segment(segment, index, data, out);
            // A gate is a fraction of a subdivision, so most notes open and
            // close inside one segment; without this second pass they would
            // all be held until the next block.
            self.voices
                .drain_ending(segment.range, &mut SegmentSink { segment, index, out: &mut *out });
        }

        // The GUI follows the *last* stretch the block played: after a wrap
        // that is the piece after the loop point, not the one before it.
        if let Some(last) = ranges.segments().last() {
            publish_playhead(data, &self.patterns, last.range, playhead);
        }

        out.sort();
    }

    /// Runs the lane scan across one segment, cut at every beat where a
    /// pending pattern change comes due.
    ///
    /// With nothing pending — the overwhelmingly common case — that is one
    /// scan over the whole segment, bit-identical to scanning it undivided.
    /// Otherwise the segment is walked in pieces: each piece is scanned with
    /// the patterns in force for it, then the lanes due at its end are
    /// switched before the next piece. Because the cut lands exactly on a
    /// step boundary, the step starting there is read from the new pattern
    /// and the tail of the step before it from the old one.
    ///
    /// A cut is a block edge as far as the scan is concerned, so it carries
    /// the same caveat: a ratchet burst straddling it is rolled on both
    /// sides, and a step whose probability is neither 0 nor 100 may land
    /// differently than it would have in an uncut block. Only blocks that
    /// actually carry a switch are ever cut.
    fn scan_segment(
        &mut self,
        segment: &BlockSegment,
        index: u8,
        data: &SeqData,
        out: &mut BlockOutput,
    ) {
        let mut rest = segment.range;
        // Every pass but the last switches at least one lane onto `pending`,
        // and a lane only switches once, so `MAX_LANES` cuts is the most a
        // segment can need. The bound is written out rather than left to that
        // argument: this runs on the audio thread, where a spin on a stray
        // float would be fatal.
        for _ in 0..=MAX_LANES {
            let due = self.patterns.next_due(data, rest);
            let piece = match due {
                Some(beat) => BeatRange::new(rest.start, beat),
                None => rest,
            };
            if !piece.is_empty() {
                let Engine { voices, patterns, prng, hits, .. } = self;
                // Scan first (lane order, so the probability rolls draw from
                // the generator exactly as before), then strike in time order.
                hits.clear();
                let mut collected = HitBuf { hits, dropped: 0 };
                scan_lanes(data, piece, |pos| patterns.active(pos), prng, &mut collected);
                let dropped = collected.dropped;
                // (beat, lane) is unique per hit, so this is a total order.
                hits.sort_unstable_by(|a, b| {
                    a.beat.total_cmp(&b.beat).then_with(|| a.lane_pos.cmp(&b.lane_pos))
                });
                let mut events = SegmentSink { segment, index, out: &mut *out };
                let mut striker = voices.striking(&mut events);
                for hit in hits.iter() {
                    striker.strike(*hit);
                }
                out.count_overflow(dropped);
            }
            let Some(beat) = due else { return };
            self.patterns.apply_due(data, beat, rest);
            rest = BeatRange::new(beat, rest.end);
        }
    }
}

/// Stores every lane's current step, plus the pattern being read, into the
/// playhead atomics.
///
/// Indexed by lane position, and written for all [`MAX_LANES`] slots rather
/// than just the live ones: a lane removed while playing has to stop showing
/// a playhead, and blanking the tail unconditionally is both cheaper and
/// harder to get wrong than tracking how many lanes the last block had. Every
/// slot is one relaxed store, so the whole publish is a fixed, allocation-free
/// sweep of a few cache lines.
///
/// Mute and solo are deliberately ignored: this is where a lane *is*, not
/// whether it is heard, and a silenced lane still has to show its position.
fn publish_playhead(
    data: &SeqData,
    patterns: &PatternSwitch,
    range: BeatRange,
    playhead: &Playhead,
) {
    for pos in 0..MAX_LANES {
        // Each lane's own pattern: mid-switch they differ, and a lane's step
        // grid is its pattern's, so reading them all from one would draw the
        // playhead on the wrong cell.
        let step = data
            .lane_data(patterns.active(pos), pos)
            .and_then(|lane_data| playhead_step(lane_data, range))
            .map_or(Playhead::NONE, |step| step as u32);
        playhead.set_step(pos, step);
    }
    playhead.set_pattern(patterns.published() as u32);
}

impl Default for Engine {
    fn default() -> Engine {
        Engine::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Lane, LaneData, Step, chunked_capacity};
    use crate::sequencer::{Rate, TimedEvent};
    use crate::voices::FIRST_VOICE_ID;

    const TEMPO: f64 = 120.0;
    const SAMPLE_RATE: f32 = 48_000.0;
    /// `60 / 120 * 48000` — the conversion every offset in these tests is
    /// read against.
    const SAMPLES_PER_BEAT: usize = 24_000;
    /// A quarter-beat block, the size most of these tests drive: an exact
    /// binary fraction of a beat, so stitched starts are bit-identical and
    /// the expected beats can be written down.
    const QUARTER_BEAT: usize = SAMPLES_PER_BEAT / 4;

    /// One lane's content, in the terms the engine tests care about.
    struct LaneSpec {
        key: u8,
        length: u32,
        rate: Rate,
        on: &'static [usize],
    }

    const fn spec(key: u8, length: u32, rate: Rate, on: &'static [usize]) -> LaneSpec {
        LaneSpec { key, length, rate, on }
    }

    /// Builds a `SeqData` holding the given lanes, identical in all 8
    /// patterns (the pattern test overwrites one afterwards).
    fn seq_data(specs: &[LaneSpec]) -> SeqData {
        let lanes: Vec<Lane> = specs
            .iter()
            .enumerate()
            .map(|(i, s)| Lane::new(i as u32 + 1, format!("lane {i}"), 0, s.key, 0))
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

    /// The canonical patch: a 16-step 1/16 lane striking every downbeat.
    fn four_on_the_floor() -> SeqData {
        seq_data(&[spec(36, 16, Rate::Sixteenth, &[0, 4, 8, 12])])
    }

    /// The transport values a host would report for block `index` of a run of
    /// `block_samples`-long blocks that started at song position `from_beats`.
    fn block_at(from_beats: f64, block_samples: usize, index: usize) -> BlockCtx {
        let beats_per_sample = TEMPO / 60.0 / SAMPLE_RATE as f64;
        let pos = from_beats + (index * block_samples) as f64 * beats_per_sample;
        BlockCtx::new(pos, TEMPO, SAMPLE_RATE, block_samples)
    }

    /// Runs `blocks` stitched blocks from `from_beats`, returning each
    /// block's sorted output.
    fn run_blocks(
        engine: &mut Engine,
        data: &SeqData,
        from_beats: f64,
        block_samples: usize,
        blocks: usize,
    ) -> Vec<Vec<TimedEvent>> {
        let mut out = BlockOutput::new();
        let playhead = Playhead::new();
        (0..blocks)
            .map(|index| {
                let ctx = block_at(from_beats, block_samples, index);
                engine.run_block(&ctx, data, &playhead, &mut out);
                out.events().to_vec()
            })
            .collect()
    }

    /// One block's output, for the tests that only need a single block.
    fn run_one(engine: &mut Engine, data: &SeqData, ctx: &BlockCtx) -> Vec<TimedEvent> {
        let mut out = BlockOutput::new();
        engine.run_block(ctx, data, &Playhead::new(), &mut out);
        out.events().to_vec()
    }

    /// Every lane position the playhead is showing a step for, in order.
    fn playing_steps(playhead: &Playhead) -> Vec<(usize, u32)> {
        (0..MAX_LANES).filter_map(|pos| playhead.step(pos).map(|step| (pos, step))).collect()
    }

    fn beats_of(events: &[TimedEvent], want_on: bool) -> Vec<f64> {
        events
            .iter()
            .filter(|timed| timed.event.is_note_on() == want_on)
            .map(|timed| timed.event.beat())
            .collect()
    }

    /// What the invariance comparison looks at: the event stream with the
    /// sample offsets dropped, since those are relative to whichever block
    /// the event landed in.
    #[derive(Clone, Copy, PartialEq, Debug)]
    struct Note {
        beat: f64,
        on: bool,
        key: u8,
        channel: u8,
    }

    fn notes(blocks: &[Vec<TimedEvent>]) -> Vec<Note> {
        let mut notes: Vec<Note> = blocks
            .iter()
            .flatten()
            .map(|timed| Note {
                beat: timed.event.beat(),
                on: timed.event.is_note_on(),
                key: timed.event.key(),
                channel: timed.event.channel(),
            })
            .collect();
        // Generation runs lane by lane within each block, so compare the
        // streams in a canonical order rather than in emission order. Beats
        // are compared exactly: the stitching path is built to be
        // bit-identical.
        notes.sort_by(|a, b| {
            a.beat
                .total_cmp(&b.beat)
                .then_with(|| a.on.cmp(&b.on))
                .then_with(|| a.key.cmp(&b.key))
                .then_with(|| a.channel.cmp(&b.channel))
        });
        notes
    }

    /// A lane's volume rides on every note-on it plays, read live: a change
    /// between blocks reaches the next note.
    #[test]
    fn a_lane_volume_reaches_its_notes() {
        let data = four_on_the_floor();
        let mut engine = Engine::new();
        let volume = |block: &[TimedEvent]| -> Vec<i8> {
            block
                .iter()
                .filter_map(|timed| match timed.event {
                    SeqEvent::NoteOn { volume_db, .. } => Some(volume_db),
                    SeqEvent::NoteOff { .. } => None,
                })
                .collect()
        };
        let first = run_blocks(&mut engine, &data, 0.0, QUARTER_BEAT, 4);
        assert_eq!(first.iter().flat_map(|b| volume(b)).collect::<Vec<_>>(), [0]);
        data.lanes[0].set_volume_db(-9);
        let next = run_blocks(&mut engine, &data, 1.0, QUARTER_BEAT, 4);
        assert_eq!(next.iter().flat_map(|b| volume(b)).collect::<Vec<_>>(), [-9]);
    }

    /// The baseline: four downbeats and their gates, each firing exactly once
    /// however the blocks fall, with every offset inside the buffer it was
    /// generated for.
    #[test]
    fn stitched_blocks_fire_each_step_once_with_its_gate() {
        let data = four_on_the_floor();
        let mut engine = Engine::new();
        // 17 quarter-beat blocks cover [0, 4.25): five downbeats and all five
        // of their gate ends.
        let blocks = run_blocks(&mut engine, &data, 0.0, QUARTER_BEAT, 17);

        let mut ons = Vec::new();
        let mut offs = Vec::new();
        for (index, block) in blocks.iter().enumerate() {
            for timed in block {
                assert!(timed.offset < QUARTER_BEAT as u32, "block {index}: offset out of buffer");
                match timed.event.is_note_on() {
                    true => ons.push(timed.event),
                    false => offs.push(timed.event),
                }
            }
        }

        assert_eq!(ons.iter().map(|e| e.beat()).collect::<Vec<_>>(), [0.0, 1.0, 2.0, 3.0, 4.0]);
        // The gate is 50 % of the 1/16 subdivision.
        assert_eq!(
            offs.iter().map(|e| e.beat()).collect::<Vec<_>>(),
            [0.125, 1.125, 2.125, 3.125, 4.125]
        );
        for (on, off) in ons.iter().zip(&offs) {
            assert_eq!((on.key(), off.key()), (36, 36));
            assert_eq!((on.channel(), off.channel()), (0, 0));
            assert_eq!(on.voice_id(), off.voice_id(), "the note-off must match its note-on");
            assert_eq!(on.lane_pos(), Some(0));
        }
        assert!(engine.voices().is_empty(), "every gate closed inside the run");
        assert_eq!(engine.prev_end(), Some(4.25));
    }

    /// Each event lands at the sample offset its beat maps to inside its own
    /// block, not at the offset it would have in the song.
    #[test]
    fn offsets_are_relative_to_the_block_the_event_lands_in() {
        let data = four_on_the_floor();
        let mut engine = Engine::new();
        let blocks = run_blocks(&mut engine, &data, 0.0, QUARTER_BEAT, 8);

        // Block 0 covers [0, 0.25): the downbeat is its first sample and the
        // gate end at 0.125 is halfway through it.
        assert_eq!(blocks[0].iter().map(|t| t.offset).collect::<Vec<_>>(), [0, 3_000]);
        assert!(blocks[0][0].event.is_note_on() && blocks[0][1].event.is_note_off());
        // Nothing happens in [0.25, 1.0).
        assert!(blocks[1..4].iter().all(Vec::is_empty));
        // Block 4 covers [1.0, 1.25), so the beat-1 downbeat repeats the
        // pattern from offset 0.
        assert_eq!(blocks[4].iter().map(|t| t.offset).collect::<Vec<_>>(), [0, 3_000]);
        assert_eq!(beats_of(&blocks[4], true), [1.0]);
    }

    /// A note whose gate closes after the block it started in is released in
    /// the block that holds its end — the voice table carries it across, so
    /// nothing hangs and nothing fires twice.
    #[test]
    fn a_gate_closing_in_a_later_block_is_released_there() {
        let data = four_on_the_floor();
        let mut engine = Engine::new();
        // A twelfth of a beat per block (2000 samples), against a gate that
        // closes 0.125 beats in: the note-off is 1.5 blocks later.
        let blocks = run_blocks(&mut engine, &data, 0.0, 2_000, 4);

        assert_eq!(blocks[0].len(), 1, "only the note-on fits in [0, 1/12)");
        assert!(blocks[0][0].event.is_note_on());
        let voice_id = blocks[0][0].event.voice_id();

        let offs: Vec<(usize, TimedEvent)> = blocks
            .iter()
            .enumerate()
            .flat_map(|(index, block)| block.iter().map(move |timed| (index, *timed)))
            .filter(|(_, timed)| timed.event.is_note_off())
            .collect();
        assert_eq!(offs.len(), 1, "exactly one note-off, in exactly one block");
        let (index, timed) = offs[0];
        assert_eq!(index, 1, "0.125 beats is 1.5 blocks of 1/12 beat");
        assert_eq!(timed.event, SeqEvent::note_off(0.125, 36, 0, voice_id));
        // 0.125 beats is 3000 samples; block 1 starts at sample 2000.
        assert_eq!(timed.offset, 1_000);
        assert!(engine.voices().is_empty());
    }

    /// Stopping the transport releases everything sounding, at offset 0, and
    /// forgets where playback was.
    #[test]
    fn a_stopped_block_flushes_every_voice_at_offset_zero() {
        // Two lanes with 1/1 steps: their gates are two beats long, so both
        // notes are still sounding when the transport stops.
        let data = seq_data(&[spec(36, 4, Rate::Whole, &[0]), spec(42, 4, Rate::Whole, &[0])]);
        let mut engine = Engine::new();
        let mut out = BlockOutput::new();

        engine.run_block(&block_at(0.0, QUARTER_BEAT, 0), &data, &Playhead::new(), &mut out);
        assert_eq!(out.len(), 2, "both lanes struck");
        assert_eq!(engine.voices().len(), 2);
        assert_eq!(engine.prev_end(), Some(0.25));

        let stopped = BlockCtx { playing: false, ..block_at(0.0, QUARTER_BEAT, 1) };
        engine.run_block(&stopped, &data, &Playhead::new(), &mut out);

        assert_eq!(out.len(), 2);
        for timed in out.events() {
            assert_eq!(timed.offset, 0, "flush note-offs go out at offset 0");
            assert!(timed.event.is_note_off());
            assert_eq!(timed.event.beat(), stopped.pos_beats, "stamped where playback stopped");
        }
        let mut keys: Vec<u8> = out.events().iter().map(|t| t.event.key()).collect();
        keys.sort();
        assert_eq!(keys, [36, 42]);

        assert!(engine.voices().is_empty(), "nothing may be left sounding");
        assert_eq!(engine.prev_end(), None, "the stitch is forgotten on stop");

        // Staying stopped is silent: there is nothing left to release.
        engine.run_block(&stopped, &data, &Playhead::new(), &mut out);
        assert!(out.is_empty());
    }

    /// Restarting after a stop is a jump, so the phase comes from the host's
    /// new position — never from where playback stopped.
    #[test]
    fn playback_resumes_at_the_host_position_after_a_stop() {
        let data = four_on_the_floor();
        let mut engine = Engine::new();
        let mut out = BlockOutput::new();

        engine.run_block(&block_at(0.0, QUARTER_BEAT, 0), &data, &Playhead::new(), &mut out);
        engine.run_block(
            &BlockCtx { playing: false, ..block_at(0.0, QUARTER_BEAT, 1) },
            &data,
            &Playhead::new(),
            &mut out,
        );

        // Restart at beat 2: the downbeat there fires, in phase with a run
        // that had played from zero: the grid is anchored to song position.
        let events = run_one(&mut engine, &data, &block_at(2.0, QUARTER_BEAT, 0));
        assert_eq!(beats_of(&events, true), [2.0]);
        assert_eq!(engine.prev_end(), Some(2.25));
    }

    /// A locate mid-playback flushes what is sounding at offset 0 and then
    /// sequences the new position — and the block after it stitches again.
    #[test]
    fn a_jump_flushes_held_voices_then_sequences_the_new_position() {
        // A 1/1 lane with step 0 on: a note every four bars, with a two-beat
        // gate, so a jump always lands mid-note.
        let data = seq_data(&[spec(60, 4, Rate::Whole, &[0])]);
        let mut engine = Engine::new();
        let mut out = BlockOutput::new();

        engine.run_block(&block_at(0.0, QUARTER_BEAT, 0), &data, &Playhead::new(), &mut out);
        assert_eq!(out.len(), 1);
        let first_id = out.events()[0].event.voice_id();
        assert_eq!(engine.voices().len(), 1);

        // Locate to beat 16, the next 1/1 step boundary: the held note is cut
        // at offset 0 and the step there fires in the same block.
        engine.run_block(&block_at(16.0, QUARTER_BEAT, 0), &data, &Playhead::new(), &mut out);

        assert_eq!(out.len(), 2);
        assert_eq!(out.events()[0], TimedEvent::new(0, SeqEvent::note_off(16.0, 60, 0, first_id)));
        let second = out.events()[1];
        assert_eq!(second.offset, 0);
        assert!(second.event.is_note_on());
        assert_eq!(second.event.beat(), 16.0);
        assert_ne!(second.event.voice_id(), first_id, "a new note gets a new voice id");
        assert_eq!(engine.prev_end(), Some(16.25));

        // The next block continues seamlessly: no flush, and the note that
        // just started is still held.
        engine.run_block(&block_at(16.0, QUARTER_BEAT, 1), &data, &Playhead::new(), &mut out);
        assert!(out.is_empty(), "a stitched block must not flush");
        assert_eq!(engine.voices().len(), 1);
    }

    /// A jump backwards is a jump too — the stitch tolerance must not swallow
    /// a relocation, and the phase at the new position is computed from song
    /// zero rather than carried over.
    #[test]
    fn a_backwards_jump_flushes_and_re_fires_the_step_there() {
        let data = four_on_the_floor();
        let mut engine = Engine::new();

        // Play [2.0, 2.25): the downbeat at 2.0 fires, its gate is still open
        // when the host relocates back to 1.0.
        let played = run_one(&mut engine, &data, &block_at(2.0, QUARTER_BEAT / 2, 0));
        assert_eq!(beats_of(&played, true), [2.0]);
        assert_eq!(engine.voices().len(), 1);

        let back = run_one(&mut engine, &data, &block_at(1.0, QUARTER_BEAT, 0));
        assert_eq!(beats_of(&back, false), [1.0, 1.125], "the flush, then the new gate");
        assert_eq!(back[0].offset, 0, "the flush note-off is at offset 0");
        assert_eq!(beats_of(&back, true), [1.0]);
    }

    /// However the host chops the timeline up, the events come out the same:
    /// bit-identical beats, in the same order, with the same ids.
    #[test]
    fn stitched_blocks_equal_one_big_block() {
        let data = four_on_the_floor();

        // 92160 samples is 3.84 beats — deliberately not a step boundary, so
        // the few ULPs of drift a run of one-sample blocks accumulates cannot
        // decide whether a step on the end fires. Every block size below
        // divides it exactly.
        let total = 92_160;
        let one = run_blocks(&mut Engine::new(), &data, 0.0, total, 1);
        for &block_samples in &[1usize, 64, 512, 2048] {
            let mut engine = Engine::new();
            let many = run_blocks(&mut engine, &data, 0.0, block_samples, total / block_samples);
            assert_eq!(notes(&many), notes(&one), "block size {block_samples}");

            // A single lane strikes in time order whichever way the blocks
            // fall, so even the generated voice ids have to match.
            let ids = |blocks: &[Vec<TimedEvent>]| -> Vec<i32> {
                let mut ids: Vec<i32> =
                    blocks.iter().flatten().map(|t| t.event.voice_id()).collect();
                ids.sort();
                ids
            };
            assert_eq!(ids(&many), ids(&one), "block size {block_samples}");
            assert!(engine.voices().is_empty(), "block size {block_samples}");
        }
        assert_eq!(notes(&one).len(), 8, "four downbeats, each with its gate");
    }

    /// The same invariance over a polymetric patch of mixed rates, where the
    /// lanes drift apart and share nothing but the timeline and the voice
    /// table.
    ///
    /// Voice ids are not compared here: one big block strikes lane by lane,
    /// while stitched blocks interleave the lanes in time order, so the ids
    /// are handed out in a different order. The notes themselves — key,
    /// channel, exact beat, on or off — are what the host hears, and those
    /// are identical.
    #[test]
    fn stitched_blocks_equal_one_big_block_for_a_polymetric_patch() {
        const ALL_STEPS: &[usize] = &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
        let data = seq_data(&[
            spec(36, 16, Rate::Sixteenth, &[0, 4, 8, 12]),
            spec(42, 15, Rate::EighthTriplet, ALL_STEPS),
            spec(38, 13, Rate::QuarterDotted, &[0, 3, 7]),
            spec(46, 7, Rate::ThirtySecond, &[0, 2, 5]),
        ]);

        let total = 92_160;
        let one = run_blocks(&mut Engine::new(), &data, 0.0, total, 1);
        assert!(one[0].len() > 50, "the patch must actually produce events");
        for &block_samples in &[1usize, 64, 512, 2048] {
            let many = run_blocks(&mut Engine::new(), &data, 0.0, block_samples, total / block_samples);
            assert_eq!(notes(&many), notes(&one), "block size {block_samples}");
        }
    }

    /// Every note-off in `events` closes a voice that is already sounding —
    /// opened earlier in the stream, or in `held` (voices left from earlier
    /// blocks) — and every voice opened is either closed in the stream or
    /// still held by `engine`. A note-off sorted ahead of its own note-on is
    /// a note the host never releases.
    fn assert_well_formed(engine: &Engine, events: &[TimedEvent]) {
        let mut open = std::collections::BTreeSet::new();
        for timed in events {
            let id = timed.event.voice_id();
            if timed.event.is_note_on() {
                assert!(open.insert(id), "voice {id} opened twice: {events:?}");
            } else {
                assert!(open.remove(&id), "note-off for voice {id} before its note-on: {events:?}");
            }
        }
        let held: std::collections::BTreeSet<i32> =
            engine.voices().iter().map(|voice| voice.voice_id()).collect();
        assert_eq!(open, held, "voices left open must be exactly the ones still held");
    }

    /// Two lanes on one key hitting the same beat play **one** note, not a
    /// note that is opened and closed again at the same instant. This is what
    /// "duplicate lane" produces on every step.
    #[test]
    fn two_lanes_striking_one_key_on_one_beat_merge_into_one_note() {
        let data = seq_data(&[
            spec(60, 4, Rate::Quarter, &[0, 1, 2, 3]),
            spec(60, 4, Rate::Quarter, &[0, 1, 2, 3]),
        ]);
        let mut engine = Engine::new();
        let events = run_one(
            &mut engine,
            &data,
            &BlockCtx::new(0.0, TEMPO, SAMPLE_RATE, 2 * SAMPLES_PER_BEAT),
        );

        assert_eq!(beats_of(&events, true), [0.0, 1.0]);
        assert_eq!(beats_of(&events, false), [0.5, 1.5]);
        assert_well_formed(&engine, &events);
    }

    /// A strike while the key is still sounding closes the old note at the new
    /// note's timing, note-off first — and that off belongs to a voice the
    /// host has already seen open.
    #[test]
    fn a_retrigger_closes_the_sounding_note_before_the_new_one() {
        // Lane 0: a 1/4 on beat 0 (gate to 0.5). Lane 1: a 1/16 on step 1,
        // i.e. beat 0.25, while lane 0's note is still sounding.
        let data =
            seq_data(&[spec(60, 4, Rate::Quarter, &[0]), spec(60, 16, Rate::Sixteenth, &[1])]);
        let mut engine = Engine::new();
        let events = run_one(
            &mut engine,
            &data,
            &BlockCtx::new(0.0, TEMPO, SAMPLE_RATE, SAMPLES_PER_BEAT),
        );

        assert_eq!(beats_of(&events, true), [0.0, 0.25]);
        assert_eq!(beats_of(&events, false), [0.25, 0.375]);
        let at_retrigger: Vec<&TimedEvent> =
            events.iter().filter(|timed| timed.event.beat() == 0.25).collect();
        assert!(at_retrigger[0].event.is_note_off(), "the retrigger off must come first");
        assert!(at_retrigger[1].event.is_note_on());
        assert_well_formed(&engine, &events);
    }

    /// Lanes are scanned one after another, but two lanes on one key whose
    /// hits interleave inside a block must reach the voice table in time
    /// order — otherwise a later hit of the first lane is struck before an
    /// earlier hit of the second, which then retriggers a voice that has not
    /// started yet and releases it before its note-on.
    #[test]
    fn interleaved_hits_on_one_key_are_struck_in_time_order() {
        // Lane 0: 1/8 steps 0 and 1 (beats 0.0 and 0.5). Lane 1: 1/16 step 1
        // (beat 0.25). All on key 60, all inside one block.
        let data =
            seq_data(&[spec(60, 8, Rate::Eighth, &[0, 1]), spec(60, 16, Rate::Sixteenth, &[1])]);
        let mut engine = Engine::new();
        let events = run_one(
            &mut engine,
            &data,
            &BlockCtx::new(0.0, TEMPO, SAMPLE_RATE, SAMPLES_PER_BEAT),
        );

        assert_eq!(beats_of(&events, true), [0.0, 0.25, 0.5]);
        assert_well_formed(&engine, &events);
    }

    /// A muted lane is silent and a soloed lane silences the rest, read
    /// through the engine rather than through the scan.
    #[test]
    fn mute_and_solo_are_honoured() {
        let data = seq_data(&[spec(36, 4, Rate::Quarter, &[0]), spec(42, 4, Rate::Quarter, &[0])]);
        let block = block_at(0.0, QUARTER_BEAT, 0);

        let both = run_one(&mut Engine::new(), &data, &block);
        assert_eq!(both.len(), 2);

        data.lanes[0].set_mute(true);
        let muted = run_one(&mut Engine::new(), &data, &block);
        assert_eq!(muted.iter().map(|t| t.event.key()).collect::<Vec<_>>(), [42]);

        data.lanes[0].set_mute(false);
        data.lanes[0].set_solo(true);
        let soloed = run_one(&mut Engine::new(), &data, &block);
        assert_eq!(soloed.iter().map(|t| t.event.key()).collect::<Vec<_>>(), [36]);
    }

    /// `pattern_select` picks the pattern the lanes are read from, one-based,
    /// and a value from outside the parameter's range can still only select a
    /// real pattern.
    #[test]
    fn pattern_select_reads_the_matching_pattern() {
        let data = four_on_the_floor();
        // Pattern 3 (index 2) moves the downbeat onto the offbeat.
        let lane_data = data.lane_data(2, 0).expect("pattern 3 exists");
        lane_data.set_step(0, Step::OFF);
        lane_data.set_step(2, Step::default_on());

        // One beat, [0, 1.0).
        let block = BlockCtx::new(0.0, TEMPO, SAMPLE_RATE, SAMPLES_PER_BEAT);
        let first = run_one(&mut Engine::new(), &data, &block.with_pattern_select(1));
        assert_eq!(beats_of(&first, true), [0.0]);

        let third = run_one(&mut Engine::new(), &data, &block.with_pattern_select(3));
        assert_eq!(beats_of(&third, true), [0.5], "pattern 3 strikes the offbeat");

        // Out-of-range selections clamp into 1..=8 instead of silencing the
        // plugin or indexing past the pattern array.
        assert_eq!(BlockCtx::STOPPED.with_pattern_select(0).pattern_index(), 0);
        assert_eq!(BlockCtx::STOPPED.with_pattern_select(1).pattern_index(), 0);
        assert_eq!(BlockCtx::STOPPED.with_pattern_select(8).pattern_index(), NUM_PATTERNS - 1);
        assert_eq!(BlockCtx::STOPPED.with_pattern_select(9).pattern_index(), NUM_PATTERNS - 1);
        assert_eq!(BlockCtx::STOPPED.with_pattern_select(u8::MAX).pattern_index(), NUM_PATTERNS - 1);
    }

    // ---- Pattern switching ------------------------------------------------

    /// An eighth-beat block: short enough that several fall inside one 1/4
    /// step, which is what "mid-step" means for these tests.
    const EIGHTH_BEAT: usize = SAMPLES_PER_BEAT / 8;

    /// Rewrites one pattern's copy of lane `pos` — the listed steps as given,
    /// everything else off — so that what a block emits says which pattern it
    /// was read from.
    fn write_pattern(data: &SeqData, pattern: usize, pos: usize, on: &[(usize, Step)]) {
        let lane_data = data.lane_data(pattern, pos).expect("the lane exists");
        for index in 0..lane_data.capacity() {
            lane_data.set_step(index, Step::OFF);
        }
        for &(index, step) in on {
            lane_data.set_step(index, step);
        }
    }

    /// Every step on, each striking `ratchet` times.
    fn every_step(length: usize, ratchet: u8) -> Vec<(usize, Step)> {
        (0..length).map(|index| (index, Step::default_on().with_ratchet(ratchet))).collect()
    }

    /// Runs stitched `block_samples`-long blocks from `from_beats`, taking
    /// each block's `patsel` from `select`, and returns every block's output.
    fn run_selecting(
        engine: &mut Engine,
        data: &SeqData,
        playhead: &Playhead,
        from_beats: f64,
        blocks: usize,
        select: impl Fn(usize) -> u8,
    ) -> Vec<Vec<TimedEvent>> {
        let mut out = BlockOutput::new();
        (0..blocks)
            .map(|index| {
                let ctx =
                    block_at(from_beats, EIGHTH_BEAT, index).with_pattern_select(select(index));
                engine.run_block(&ctx, data, playhead, &mut out);
                out.events().to_vec()
            })
            .collect()
    }

    /// The note-on beats for one key across a run of blocks.
    fn key_beats(blocks: &[Vec<TimedEvent>], key: u8) -> Vec<f64> {
        blocks
            .iter()
            .flatten()
            .filter(|timed| timed.event.is_note_on() && timed.event.key() == key)
            .map(|timed| timed.event.beat())
            .collect()
    }

    /// A patch whose two patterns are told apart by what they strike: a 1/4
    /// lane ratcheting four times per step in pattern 1, and striking once
    /// per step in pattern 2.
    fn ratchet_then_plain() -> SeqData {
        let data = seq_data(&[spec(36, 4, Rate::Quarter, &[])]);
        write_pattern(&data, 0, 0, &every_step(4, 4));
        write_pattern(&data, 1, 0, &every_step(4, 1));
        data
    }

    /// A change to `patsel` is not heard until the lane finishes the step it
    /// is on: the ratchet burst already under way plays out in full, and only
    /// the step starting on the boundary comes from the new pattern.
    #[test]
    fn a_pattern_change_waits_for_the_lane_s_next_step_boundary() {
        let data = ratchet_then_plain();
        let mut engine = Engine::new();
        let playhead = Playhead::new();

        // Eighth-beat blocks over [0, 2.25); `patsel` moves to pattern 2
        // during the first step, a quarter of the way into it.
        let blocks = run_selecting(&mut engine, &data, &playhead, 0.0, 18, |index| {
            if index < 2 { 1 } else { 2 }
        });

        assert_eq!(
            key_beats(&blocks, 36),
            [0.0, 0.25, 0.5, 0.75, 1.0, 2.0],
            "the burst in progress must finish, then pattern 2 strikes once a beat"
        );
        // Every step is on in both patterns; what changed is the ratchet, so
        // from the boundary on there is one strike a beat instead of four.
        assert_eq!(engine.active_pattern(0), 1);
    }

    /// Mid-step is where the change is *held*, which the internal state has
    /// to show block by block: pending through the whole step, applied on the
    /// boundary, and from then on with nothing left pending.
    #[test]
    fn the_lane_reads_the_old_pattern_for_every_block_of_the_step_it_is_on() {
        let data = ratchet_then_plain();
        let mut engine = Engine::new();
        let playhead = Playhead::new();
        let mut out = BlockOutput::new();

        // [0, 0.125): the first block selects pattern 1 and, being a jump,
        // takes it immediately.
        engine.run_block(&block_at(0.0, EIGHTH_BEAT, 0), &data, &playhead, &mut out);
        assert_eq!(engine.active_pattern(0), 0);

        // The seven remaining eighths of the first 1/4 step all keep reading
        // pattern 1 however many blocks the change has been pending for.
        for index in 1..8 {
            let ctx = block_at(0.0, EIGHTH_BEAT, index).with_pattern_select(2);
            engine.run_block(&ctx, &data, &playhead, &mut out);
            assert_eq!(engine.active_pattern(0), 0, "block {index} switched mid-step");
            assert_eq!(playhead.pattern(), Some(0), "block {index} published the wrong pattern");
        }

        // [1.0, 1.125) opens on the boundary, so the switch lands here.
        let ctx = block_at(0.0, EIGHTH_BEAT, 8).with_pattern_select(2);
        engine.run_block(&ctx, &data, &playhead, &mut out);
        assert_eq!(engine.active_pattern(0), 1);
        assert_eq!(playhead.pattern(), Some(1));
    }

    /// Lanes switch on their own grids, so the same parameter move lands at
    /// different beats for different rates: a 1/16 lane follows within a
    /// sixteenth while a 1/4 lane is still playing out its step.
    #[test]
    fn lanes_at_different_rates_switch_at_their_own_boundaries() {
        let data = seq_data(&[spec(36, 4, Rate::Quarter, &[]), spec(42, 16, Rate::Sixteenth, &[])]);
        // Pattern 1: the 1/4 lane ratchets eight times per step, the 1/16
        // lane strikes every step. Pattern 2 silences both.
        write_pattern(&data, 0, 0, &every_step(4, 8));
        write_pattern(&data, 0, 1, &every_step(16, 1));
        write_pattern(&data, 1, 0, &[]);
        write_pattern(&data, 1, 1, &[]);

        let mut engine = Engine::new();
        let playhead = Playhead::new();
        // Eighth-beat blocks over [0, 1.25), switching to pattern 2 inside
        // the first step of both lanes.
        let blocks = run_selecting(&mut engine, &data, &playhead, 0.0, 10, |index| {
            if index == 0 { 1 } else { 2 }
        });

        // The 1/16 lane's next boundary is 0.25, so it falls silent there.
        assert_eq!(key_beats(&blocks, 42), [0.0], "the 1/16 lane switched late");
        // The 1/4 lane's is 1.0: its eight-hit burst plays out first, and the
        // step at 1.0 is the first one read from the silent pattern.
        assert_eq!(
            key_beats(&blocks, 36),
            [0.0, 0.125, 0.25, 0.375, 0.5, 0.625, 0.75, 0.875],
            "the 1/4 lane switched early"
        );
    }

    /// The pattern the GUI shows is the one being played: while any lane is
    /// still finishing a step on the old pattern, that is the old one, and it
    /// only moves on once every lane has caught up.
    #[test]
    fn the_playhead_holds_the_old_pattern_until_every_lane_has_switched() {
        let data = seq_data(&[spec(36, 4, Rate::Quarter, &[]), spec(42, 16, Rate::Sixteenth, &[])]);
        let mut engine = Engine::new();
        let playhead = Playhead::new();
        let mut out = BlockOutput::new();

        engine.run_block(&block_at(0.0, EIGHTH_BEAT, 0), &data, &playhead, &mut out);
        assert_eq!(playhead.pattern(), Some(0));

        // The 1/16 lane switches at 0.25 and the 1/4 lane at 1.0; the
        // published pattern waits for the second of them.
        for index in 1..8 {
            let ctx = block_at(0.0, EIGHTH_BEAT, index).with_pattern_select(3);
            engine.run_block(&ctx, &data, &playhead, &mut out);
            assert_eq!(engine.active_pattern(1), if index < 2 { 0 } else { 2 });
            assert_eq!(playhead.pattern(), Some(0), "block {index} published too early");
        }

        let ctx = block_at(0.0, EIGHTH_BEAT, 8).with_pattern_select(3);
        engine.run_block(&ctx, &data, &playhead, &mut out);
        assert_eq!(engine.active_pattern(0), 2);
        assert_eq!(playhead.pattern(), Some(2));
    }

    /// Each lane's playhead cell comes from the pattern *that lane* is
    /// playing — patterns hold their own rate and length, so reading them all
    /// from one would put the highlight on the wrong step.
    #[test]
    fn the_playhead_reads_each_lane_from_the_pattern_it_is_playing() {
        // All steps off: a playhead is a position, not a sound.
        let data = seq_data(&[spec(36, 16, Rate::Sixteenth, &[])]);
        let slower = data.lane_data(1, 0).expect("pattern 2 exists");
        slower.set_rate(Rate::Quarter);
        slower.set_length(4);

        let mut engine = Engine::new();
        let playhead = Playhead::new();
        let mut out = BlockOutput::new();

        // [2.0, 2.25) on pattern 1: the 1/16 grid puts the lane on step 8.
        let ctx = block_at(2.0, QUARTER_BEAT, 0);
        engine.run_block(&ctx, &data, &playhead, &mut out);
        assert_eq!(playing_steps(&playhead), [(0, 8)]);

        // [2.25, 2.5) opens on a 1/16 boundary, so pattern 2 takes over —
        // and on its 1/4 grid of 4 steps the same beats are step 2.
        let ctx = block_at(2.0, QUARTER_BEAT, 1).with_pattern_select(2);
        engine.run_block(&ctx, &data, &playhead, &mut out);
        assert_eq!(playing_steps(&playhead), [(0, 2)]);
        assert_eq!(playhead.pattern(), Some(1));
    }

    /// Nothing survives a stop to be protected, so the pending pattern is
    /// simply the one playback comes back on.
    #[test]
    fn a_stop_drops_the_pending_pattern_onto_every_lane() {
        let data = ratchet_then_plain();
        let mut engine = Engine::new();
        let playhead = Playhead::new();
        let mut out = BlockOutput::new();

        // Pattern 2 requested a quarter of the way into the first step, then
        // the transport stops before the boundary.
        run_selecting(&mut engine, &data, &playhead, 0.0, 2, |index| {
            if index == 0 { 1 } else { 2 }
        });
        assert_eq!(engine.active_pattern(0), 0, "the switch cannot have landed yet");

        let stopped = BlockCtx { playing: false, ..block_at(0.0, EIGHTH_BEAT, 2) }
            .with_pattern_select(2);
        engine.run_block(&stopped, &data, &playhead, &mut out);
        assert_eq!(engine.active_pattern(0), 1);

        // Resuming at 1.0 for a whole beat therefore plays pattern 2's single
        // strike, not pattern 1's burst.
        let resumed =
            BlockCtx::new(1.0, TEMPO, SAMPLE_RATE, SAMPLES_PER_BEAT).with_pattern_select(2);
        assert_eq!(beats_of(&run_one(&mut engine, &data, &resumed), true), [1.0]);
    }

    /// A jump is the same case: the step the lane was on is gone, so the
    /// selected pattern applies at the new position straight away.
    #[test]
    fn a_jump_drops_the_pending_pattern_onto_every_lane() {
        let data = ratchet_then_plain();
        let mut engine = Engine::new();
        let playhead = Playhead::new();

        run_selecting(&mut engine, &data, &playhead, 0.0, 2, |index| {
            if index == 0 { 1 } else { 2 }
        });
        assert_eq!(engine.active_pattern(0), 0);

        let jumped =
            BlockCtx::new(4.0, TEMPO, SAMPLE_RATE, SAMPLES_PER_BEAT).with_pattern_select(2);
        assert_eq!(beats_of(&run_one(&mut engine, &data, &jumped), true), [4.0]);
        assert_eq!(engine.active_pattern(0), 1);
    }

    /// A switch due inside a block cuts the scan there rather than colouring
    /// the whole block: one long block must produce exactly what the stitched
    /// small blocks do, pattern change and all.
    #[test]
    fn a_switch_inside_a_block_matches_the_same_switch_across_blocks() {
        let data = ratchet_then_plain();

        // Eighth-beat blocks over [0, 2.0), on pattern 2 from the second one.
        let mut stitched_engine = Engine::new();
        let stitched = run_selecting(
            &mut stitched_engine,
            &data,
            &Playhead::new(),
            0.0,
            16,
            |index| if index == 0 { 1 } else { 2 },
        );

        // The same two beats as one block on pattern 1 followed by one block
        // on pattern 2 — the switch falls inside the second block, at 1.0.
        let mut one_engine = Engine::new();
        let first = BlockCtx::new(0.0, TEMPO, SAMPLE_RATE, SAMPLES_PER_BEAT);
        let second =
            BlockCtx::new(1.0, TEMPO, SAMPLE_RATE, SAMPLES_PER_BEAT).with_pattern_select(2);
        let whole = vec![
            run_one(&mut one_engine, &data, &first),
            run_one(&mut one_engine, &data, &second),
        ];

        assert_eq!(notes(&whole), notes(&stitched));
        assert_eq!(one_engine.active_pattern(0), stitched_engine.active_pattern(0));
    }

    /// A host loop wrapping inside a block leaves the stitch at the *wrapped*
    /// position, so the next block — which the host reports from `loop_start`
    /// — stitches instead of flushing, and the events after the wrap carry
    /// offsets accumulated across the split.
    #[test]
    fn a_loop_wrap_stitches_against_the_wrapped_position() {
        let data = four_on_the_floor();
        let mut engine = Engine::new();

        // A four-beat loop and a quarter-beat block starting at 3.9: the
        // block covers [3.9, 4.0) and then [0.0, 0.15).
        let wrapping = BlockCtx::new(3.9, TEMPO, SAMPLE_RATE, QUARTER_BEAT).with_loop(0.0, 4.0);
        let wrapped = run_one(&mut engine, &data, &wrapping);
        let resume = 3.9 + 0.25 - 4.0;

        // The step at beat 0 is in the wrapped half, so its offset is the
        // 0.1 beats the first half consumed: 2400 samples.
        assert_eq!(beats_of(&wrapped, true), [0.0]);
        assert_eq!(wrapped[0].offset, 2_400);
        assert_eq!(beats_of(&wrapped, false), [0.125]);
        assert_eq!(wrapped[1].offset, 2_400 + 3_000);
        assert!((engine.prev_end().expect("stitched") - resume).abs() < 1e-12);

        // The host reports the wrapped position next block, and that stitches:
        // [0.15, 0.4) holds no step, so the block is silent rather than
        // flushing.
        let next = run_one(&mut engine, &data, &BlockCtx { pos_beats: resume, ..wrapping });
        assert!(next.is_empty(), "a stitched block after a wrap must not flush");
        assert!((engine.prev_end().expect("stitched") - (resume + 0.25)).abs() < 1e-12);
    }

    /// A block the host loop wraps inside carries *both* stretches of
    /// timeline: the beats up to the loop end, then the beats from the loop
    /// start, with the second piece's sample offsets carrying on where the
    /// first piece's left off.
    #[test]
    fn a_loop_wrap_emits_both_sub_ranges_with_accumulated_offsets() {
        // A 16-step 1/16 lane striking beats 0.0, 0.5, 1.75 and 2.0, against
        // a two-beat loop: the step at 2.0 sits exactly on the loop end, so
        // the loop never reaches it.
        let data = seq_data(&[spec(36, 16, Rate::Sixteenth, &[0, 2, 7, 8])]);
        let mut engine = Engine::new();

        // [1.7, 2.7) → [1.7, 2.0) then [0.0, 0.7).
        let ctx = BlockCtx::new(1.7, TEMPO, SAMPLE_RATE, SAMPLES_PER_BEAT).with_loop(0.0, 2.0);
        let events = run_one(&mut engine, &data, &ctx);

        // One beat is 24000 samples, so the head consumes 0.3 * 24000 = 7200
        // of them and everything after the wrap is pushed out by that much.
        let got: Vec<(u32, bool, f64)> =
            events.iter().map(|t| (t.offset, t.event.is_note_on(), t.event.beat())).collect();
        assert_eq!(
            got,
            [
                (1_200, true, 1.75),
                (4_200, false, 1.875),
                (7_200, true, 0.0),
                (10_200, false, 0.125),
                (19_200, true, 0.5),
                (22_200, false, 0.625),
            ]
        );
        assert!(
            got.windows(2).all(|pair| pair[0].0 <= pair[1].0),
            "offsets must not go backwards across the split"
        );
        assert!(got.iter().all(|&(offset, ..)| offset < SAMPLES_PER_BEAT as u32));
        assert!(engine.voices().is_empty(), "every gate closed inside the block");
    }

    /// The wrap is half-open at both ends: the step sitting exactly on the
    /// loop end never plays, and the one on the loop start plays once per
    /// pass — a looping transport must neither double-strike nor swallow the
    /// seam.
    #[test]
    fn looping_plays_the_seam_steps_exactly_once_per_pass() {
        // Steps on beats 0.0 (loop start), 1.75 (last step of the loop) and
        // 2.0 (the loop end itself).
        let data = seq_data(&[spec(36, 16, Rate::Sixteenth, &[0, 7, 8])]);
        let mut engine = Engine::new();
        let mut out = BlockOutput::new();
        let playhead = Playhead::new();

        // Sixteen quarter-beat blocks: two full passes of the two-beat loop,
        // driven the way a host does it — each block reported from where the
        // previous one resumed.
        let mut beats = Vec::new();
        let mut pos = 0.0;
        for index in 0..16 {
            let ctx = BlockCtx::new(pos, TEMPO, SAMPLE_RATE, QUARTER_BEAT).with_loop(0.0, 2.0);
            engine.run_block(&ctx, &data, &playhead, &mut out);
            assert!(
                out.events().iter().all(|t| t.offset < QUARTER_BEAT as u32),
                "block {index}: offset out of buffer"
            );
            let ons = out.events().iter().filter(|t| t.event.is_note_on());
            beats.extend(ons.map(|t| t.event.beat()));
            pos = engine.prev_end().expect("the loop keeps the stitch alive");
        }

        // Two passes, each playing the loop start and the loop's last step —
        // and never the step on the loop end.
        assert_eq!(beats, [0.0, 1.75, 0.0, 1.75]);
        // The eighth block ends exactly on the loop end, so each pass leaves
        // the stitch back at the loop start — exactly, not nearly, which is
        // what keeps the seam from drifting over a long take.
        assert_eq!(pos, 0.0);
        assert!(engine.voices().is_empty());
    }

    /// Where the GUI draws the playhead: every lane's current step and the
    /// pattern being read, published once per block and blanked when the
    /// transport stops.
    #[test]
    fn the_playhead_publishes_every_lane_then_clears_on_stop() {
        // Three lanes with different rates and lengths, so each one reports a
        // different cell. All steps are off: a playhead is a position, not a
        // sound.
        let data = seq_data(&[
            spec(36, 16, Rate::Sixteenth, &[]),
            spec(42, 4, Rate::Quarter, &[]),
            spec(38, 3, Rate::Eighth, &[]),
        ]);
        let mut engine = Engine::new();
        let mut out = BlockOutput::new();
        let playhead = Playhead::new();

        // A fresh playhead is all sentinel: nothing is playing yet.
        assert_eq!(playing_steps(&playhead), []);
        assert_eq!(playhead.pattern(), None);

        // [1.0, 1.25): the 1/16 lane is on its step 4, the 1/4 lane on step
        // 1, and the 3-step 1/8 lane on `2.rem_euclid(3)`.
        engine.run_block(&block_at(1.0, QUARTER_BEAT, 0), &data, &playhead, &mut out);
        assert_eq!(playing_steps(&playhead), [(0, 4), (1, 1), (2, 2)]);
        assert_eq!(playhead.pattern(), Some(0));
        assert!(out.is_empty(), "no step is on; only the playhead moved");

        // [2.0, 2.25), reading pattern 5 (index 4).
        let later = block_at(2.0, QUARTER_BEAT, 0).with_pattern_select(5);
        engine.run_block(&later, &data, &playhead, &mut out);
        assert_eq!(playing_steps(&playhead), [(0, 8), (1, 2), (2, 1)]);
        assert_eq!(playhead.pattern(), Some(4), "the pattern actually being played");

        // An unusable block (no samples) covers no beats, so it has no
        // position to publish and leaves the last one standing.
        let empty = BlockCtx { num_samples: 0, ..later };
        engine.run_block(&empty, &data, &playhead, &mut out);
        assert_eq!(playing_steps(&playhead), [(0, 8), (1, 2), (2, 1)]);

        // Stopping blanks all of it, lanes and pattern alike.
        engine.run_block(&BlockCtx { playing: false, ..later }, &data, &playhead, &mut out);
        assert_eq!(playing_steps(&playhead), []);
        assert_eq!(playhead.pattern(), None);
    }

    /// The playhead follows the *wrapped* half of a block the loop wrapped
    /// inside — the GUI has to show where playback now is, not where it was
    /// before the seam.
    #[test]
    fn the_playhead_follows_the_wrapped_half_of_the_block() {
        let data = seq_data(&[spec(36, 16, Rate::Sixteenth, &[])]);
        let mut engine = Engine::new();
        let mut out = BlockOutput::new();
        let playhead = Playhead::new();

        // [1.7, 2.0) then [0.0, 0.7): the block ends inside step 2.
        let ctx = BlockCtx::new(1.7, TEMPO, SAMPLE_RATE, SAMPLES_PER_BEAT).with_loop(0.0, 2.0);
        engine.run_block(&ctx, &data, &playhead, &mut out);
        assert_eq!(playing_steps(&playhead), [(0, 2)]);

        // Without the loop the same block ends inside step 10 instead.
        engine.run_block(&BlockCtx { loop_range: None, ..ctx }, &data, &playhead, &mut out);
        assert_eq!(playing_steps(&playhead), [(0, 10)]);
    }

    /// A playhead slot is only ever as live as its lane: a patch with fewer
    /// lanes blanks the rest, and a silenced lane still shows its position.
    #[test]
    fn the_playhead_blanks_lanes_the_patch_no_longer_has() {
        let four = seq_data(&[
            spec(36, 16, Rate::Sixteenth, &[]),
            spec(38, 16, Rate::Sixteenth, &[]),
            spec(40, 16, Rate::Sixteenth, &[]),
            spec(42, 16, Rate::Sixteenth, &[]),
        ]);
        let mut engine = Engine::new();
        let mut out = BlockOutput::new();
        let playhead = Playhead::new();

        let block = block_at(0.0, QUARTER_BEAT, 0);
        engine.run_block(&block, &four, &playhead, &mut out);
        assert_eq!(playing_steps(&playhead), [(0, 0), (1, 0), (2, 0), (3, 0)]);

        // Muting and soloing change what is heard, never where a lane is.
        four.lanes[0].set_mute(true);
        four.lanes[1].set_solo(true);
        engine.run_block(&block, &four, &playhead, &mut out);
        assert_eq!(playing_steps(&playhead), [(0, 0), (1, 0), (2, 0), (3, 0)]);

        // The same instance now holds one lane (a removal republishes
        // `SeqData`): the three stale slots have to go back to the sentinel.
        let one = seq_data(&[spec(36, 16, Rate::Sixteenth, &[])]);
        engine.run_block(&block, &one, &playhead, &mut out);
        assert_eq!(playing_steps(&playhead), [(0, 0)]);

        // And an empty patch shows nothing at all, while still reporting the
        // pattern the transport is playing.
        engine.run_block(&block, &SeqData::empty(), &playhead, &mut out);
        assert_eq!(playing_steps(&playhead), []);
        assert_eq!(playhead.pattern(), Some(0));
    }

    /// Unusable transport data behaves as an empty block rather than
    /// panicking or scheduling events at wild offsets.
    #[test]
    fn unusable_transport_data_produces_an_empty_block() {
        let data = four_on_the_floor();
        let mut out = BlockOutput::new();

        for ctx in [
            BlockCtx::new(0.0, 0.0, SAMPLE_RATE, QUARTER_BEAT),
            BlockCtx::new(0.0, TEMPO, 0.0, QUARTER_BEAT),
            BlockCtx::new(0.0, TEMPO, SAMPLE_RATE, 0),
            BlockCtx::new(f64::NAN, TEMPO, SAMPLE_RATE, QUARTER_BEAT),
            BlockCtx::new(f64::INFINITY, TEMPO, SAMPLE_RATE, QUARTER_BEAT),
            BlockCtx::new(0.0, f64::NAN, SAMPLE_RATE, QUARTER_BEAT),
            BlockCtx::new(0.0, -120.0, SAMPLE_RATE, QUARTER_BEAT),
        ] {
            let mut engine = Engine::new();
            engine.run_block(&ctx, &data, &Playhead::new(), &mut out);
            assert!(out.is_empty(), "{ctx:?} produced events");
            assert!(engine.voices().is_empty(), "{ctx:?} opened a voice");
            assert_eq!(out.overflows(), 0);
        }
    }

    /// A non-finite host position must not poison the flush path either: the
    /// note-offs still go out at offset 0, with a usable beat stamp.
    #[test]
    fn a_stop_at_a_non_finite_position_still_flushes() {
        let data = seq_data(&[spec(36, 4, Rate::Whole, &[0])]);
        let mut engine = Engine::new();
        let mut out = BlockOutput::new();

        engine.run_block(&block_at(0.0, QUARTER_BEAT, 0), &data, &Playhead::new(), &mut out);
        assert_eq!(engine.voices().len(), 1);

        let stopped = BlockCtx { pos_beats: f64::NAN, ..BlockCtx::STOPPED };
        engine.run_block(&stopped, &data, &Playhead::new(), &mut out);
        assert_eq!(
            out.events(),
            [TimedEvent::new(0, SeqEvent::note_off(0.0, 36, 0, FIRST_VOICE_ID))]
        );
        assert!(engine.voices().is_empty());
    }

    /// `reset()` can only ask for a flush, so the ask has to survive until a
    /// block can emit it — and the block that does must start from silence:
    /// the long note held before the reset is released at offset 0, stamped
    /// where the host actually is, and the stitch onto the interrupted block
    /// is dropped rather than carried on.
    #[test]
    fn a_requested_flush_releases_everything_in_the_next_block() {
        let data = seq_data(&[spec(36, 4, Rate::Whole, &[0])]);
        let mut engine = Engine::new();
        let mut out = BlockOutput::new();

        // A 1/1 lane: the gate stays open well past this block's end.
        engine.run_block(&block_at(0.0, QUARTER_BEAT, 0), &data, &Playhead::new(), &mut out);
        assert_eq!(engine.voices().len(), 1);
        assert_eq!(engine.prev_end(), Some(0.25));

        engine.request_flush();
        assert!(engine.flush_pending(), "the ask must outlive the call that made it");

        // The host carries on playing exactly where it left off; the flush
        // still happens, and the stitch is dropped with it.
        let resumed = block_at(0.0, QUARTER_BEAT, 1);
        engine.run_block(&resumed, &data, &Playhead::new(), &mut out);
        assert!(!engine.flush_pending(), "the flush must be consumed, not repeated");
        assert_eq!(
            out.events(),
            [TimedEvent::new(0, SeqEvent::note_off(0.25, 36, 0, FIRST_VOICE_ID))]
        );
        assert!(engine.voices().is_empty());
        assert_eq!(engine.prev_end(), Some(0.5), "the block itself still ran");
    }

    /// The same ask arriving while the transport is already stopped: one
    /// flush, no duplicate note-offs, and nothing left pending.
    #[test]
    fn a_requested_flush_on_a_stopped_transport_fires_once() {
        let data = seq_data(&[spec(36, 4, Rate::Whole, &[0])]);
        let mut engine = Engine::new();
        let mut out = BlockOutput::new();

        engine.run_block(&block_at(0.0, QUARTER_BEAT, 0), &data, &Playhead::new(), &mut out);
        assert_eq!(engine.voices().len(), 1);

        engine.request_flush();
        engine.run_block(&BlockCtx::STOPPED, &data, &Playhead::new(), &mut out);
        assert_eq!(
            out.events(),
            [TimedEvent::new(0, SeqEvent::note_off(0.0, 36, 0, FIRST_VOICE_ID))]
        );
        assert!(!engine.flush_pending());
        assert!(engine.voices().is_empty());
    }

    /// An empty patch is the fresh-instance case, and must do nothing at all.
    #[test]
    fn an_empty_patch_emits_nothing() {
        let data = SeqData::empty();
        let mut engine = Engine::new();
        let mut out = BlockOutput::new();
        for index in 0..4 {
            let ctx = block_at(0.0, QUARTER_BEAT, index);
            engine.run_block(&ctx, &data, &Playhead::new(), &mut out);
            assert!(out.is_empty());
        }
        assert!(engine.voices().is_empty());
        assert_eq!(engine.prev_end(), Some(1.0));
    }

    /// The engine's own state after a block is what the next block reads, so
    /// two engines driven identically must stay identical — including the
    /// probability rolls, which is what makes a seeded engine reproducible.
    #[test]
    fn a_seeded_engine_replays_exactly() {
        let data = four_on_the_floor();
        data.lane_data(0, 0).expect("lane exists").update_step(4, |s| s.with_probability(50));

        let runs: Vec<Vec<Note>> = (0..2)
            .map(|_| {
                let mut engine = Engine::with_seed(0x1234_5678_9ABC_DEF0);
                notes(&run_blocks(&mut engine, &data, 0.0, QUARTER_BEAT, 64))
            })
            .collect();
        assert_eq!(runs[0], runs[1]);
        // A different seed is allowed to differ, but must still be internally
        // consistent: every note-on is matched by a note-off.
        let mut other = Engine::with_seed(0xDEAD_BEEF_CAFE_F00D);
        let events = notes(&run_blocks(&mut other, &data, 0.0, QUARTER_BEAT, 64));
        assert_eq!(
            events.iter().filter(|n| n.on).count(),
            events.iter().filter(|n| !n.on).count()
        );
    }

    // ---- Host transport shapes --------------------------------------------
    // Each of these drives `run_block` with a transport shape a host may
    // produce, so what the engine does with a pre-roll, a moving stopped
    // position, a tempo change or a host-side loop split is settled here,
    // whichever of these shapes Bitwig turns out to use.

    /// Pre-roll: Bitwig counts in from negative beats, and the grid
    /// simply continues backwards (`k.rem_euclid(len)`), so the count-in
    /// plays the same downbeats the song does and runs into beat 0 without a
    /// flush.
    #[test]
    fn pre_roll_from_negative_beats_stitches_into_the_song() {
        let data = four_on_the_floor();
        let mut engine = Engine::new();
        // Two beats of count-in, then into the song: ten quarter-beat blocks
        // cover [-2.0, 0.5).
        let blocks = run_blocks(&mut engine, &data, -2.0, QUARTER_BEAT, 10);

        // Steps 8 and 12 of the 16-step lane are the ones at beats -2.0 and
        // -1.0, so the count-in is in phase with song zero.
        let ons: Vec<f64> = blocks.iter().flat_map(|block| beats_of(block, true)).collect();
        assert_eq!(ons, [-2.0, -1.0, 0.0]);
        let offs: Vec<f64> = blocks.iter().flat_map(|block| beats_of(block, false)).collect();
        assert_eq!(offs, [-1.875, -0.875, 0.125]);

        // Nothing after the first block reads as a jump: a flush is the only
        // thing that puts a note-off at offset 0.
        for (index, block) in blocks.iter().enumerate().skip(1) {
            assert!(
                block.iter().all(|timed| timed.offset > 0 || timed.event.is_note_on()),
                "block {index} flushed",
            );
        }
        assert_eq!(engine.prev_end(), Some(0.5));
        assert!(engine.voices().is_empty());
    }

    /// A host that keeps reporting a position while the transport sits
    /// still, and keeps reporting its cycle with it: the first stopped block
    /// releases what was sounding and every one after it is silent, wherever
    /// the parked playhead says it is.
    #[test]
    fn a_moving_position_while_stopped_changes_nothing() {
        let data = seq_data(&[spec(36, 4, Rate::Whole, &[0])]);
        let mut engine = Engine::new();
        let mut out = BlockOutput::new();
        let playhead = Playhead::new();

        engine.run_block(&block_at(0.0, QUARTER_BEAT, 0), &data, &playhead, &mut out);
        assert_eq!(engine.voices().len(), 1);

        // A stopped scrub: the position moves, and one of the stops lands
        // exactly on a step that would strike if it were playing.
        for (index, pos) in [0.25, 1.5, 4.0, 4.0].into_iter().enumerate() {
            let stopped = BlockCtx {
                playing: false,
                pos_beats: pos,
                ..block_at(0.0, QUARTER_BEAT, 0).with_loop(0.0, 2.0)
            };
            engine.run_block(&stopped, &data, &playhead, &mut out);
            if index == 0 {
                assert_eq!(beats_of(out.events(), false), [0.25], "the held note is released");
            } else {
                assert!(out.is_empty(), "stopped block {index} emitted {:?}", out.events());
            }
            assert_eq!(engine.prev_end(), None, "a stopped block keeps no continuity");
            assert_eq!(playing_steps(&playhead), [], "and shows no playhead");
        }
        assert!(engine.voices().is_empty());
    }

    /// A tempo change arrives as a new block-constant tempo: the block covers
    /// fewer beats than the last one did, but it still starts where that one ended, so the
    /// stitch holds and nothing is flushed.
    #[test]
    fn a_tempo_change_between_blocks_keeps_the_stitch() {
        // A 1/1 lane: the gate opened in the first block is two beats long,
        // so a jump at the tempo change would cut it at offset 0.
        let data = seq_data(&[spec(60, 4, Rate::Whole, &[0])]);
        let mut engine = Engine::new();
        let mut out = BlockOutput::new();

        engine.run_block(&block_at(0.0, QUARTER_BEAT, 0), &data, &Playhead::new(), &mut out);
        assert_eq!(out.len(), 1, "the note-on");
        assert_eq!(engine.prev_end(), Some(0.25));

        // Three quarters of the tempo through the same buffer: 6000 samples
        // at 90 BPM is 0.1875 beats instead of 0.25.
        for (index, expected) in [0.4375, 0.625].into_iter().enumerate() {
            let pos = engine.prev_end().expect("playing");
            let slower = BlockCtx::new(pos, 90.0, SAMPLE_RATE, QUARTER_BEAT);
            engine.run_block(&slower, &data, &Playhead::new(), &mut out);
            assert!(out.is_empty(), "block {index} after the tempo change emitted events");
            assert_eq!(engine.prev_end(), Some(expected), "the span follows the new tempo");
            assert_eq!(engine.voices().len(), 1, "the gate must keep sounding across it");
        }
    }

    /// A host that *splits* the block at the loop point rather than wrapping
    /// inside one: it hands us a short block ending exactly on `loop_end` and
    /// then reports `loop_start`. A block ending on the loop end still counts
    /// as a wrap, so the engine resumes from the loop start and the next
    /// block stitches.
    #[test]
    fn a_host_that_shortens_the_block_at_the_loop_end_stitches() {
        // The 1/16 lane strikes the loop start; the dotted-quarter lane
        // strikes beat 1.5 and its gate runs to 2.25, past the loop end, so a
        // jump at the seam would show up as a note-off at offset 0.
        let data = seq_data(&[
            spec(36, 16, Rate::Sixteenth, &[0, 4, 8, 12]),
            spec(60, 4, Rate::QuarterDotted, &[1]),
        ]);
        let mut engine = Engine::new();
        let mut out = BlockOutput::new();
        let playhead = Playhead::new();

        // 0.375-beat blocks (9000 samples) against a two-beat cycle: five of
        // them reach 1.875 and the sixth would overshoot, so a splitting host
        // cuts it to 0.125 beats to land on the loop end.
        let full = SAMPLES_PER_BEAT * 3 / 8;
        let mut pos = 0.0;
        for index in 0..6 {
            let samples = if index == 5 { SAMPLES_PER_BEAT / 8 } else { full };
            let ctx = BlockCtx::new(pos, TEMPO, SAMPLE_RATE, samples).with_loop(0.0, 2.0);
            engine.run_block(&ctx, &data, &playhead, &mut out);
            pos = engine.prev_end().expect("playing");
        }
        assert_eq!(engine.voices().len(), 1, "the dotted lane's gate is still open");
        assert_eq!(pos, 0.0, "a block ending on the loop end resumes from the loop start");

        // The host starts the pass again from `loop_start`, which is exactly
        // where the engine is expecting it.
        let ctx = BlockCtx::new(0.0, TEMPO, SAMPLE_RATE, full).with_loop(0.0, 2.0);
        engine.run_block(&ctx, &data, &playhead, &mut out);
        assert_eq!(beats_of(out.events(), true), [0.0], "the step on the loop start");
        assert_eq!(beats_of(out.events(), false), [0.125], "and its gate — no flush");
        assert_eq!(engine.voices().len(), 1, "a stitched seam leaves the open gate alone");
    }

    /// The same split, and a mismatch that needs no host to find: when the
    /// host splits at the loop point, the stitch holds only if the engine's
    /// *accumulated* block end lands on the loop end as exactly as the host's
    /// sample arithmetic does. [`Stitch::TOLERANCE`] is 1e-6 beats — a
    /// fortieth of a sample at 120 BPM / 48 kHz — so sub-sample disagreement
    /// is a jump, and every pass of the loop then flushes.
    ///
    /// This asserts what the engine does **today**; the fix (a tolerance
    /// scaled to the sample grid, or a wrap-aware stitch) is still open.
    #[test]
    fn a_host_split_at_the_loop_end_stitches_only_when_the_sums_agree() {
        const LOOP: (f64, f64) = (0.0, 2.0);
        // One lane whose gate straddles the loop end, so the flush a jump
        // performs is audible in the output.
        let data = seq_data(&[spec(60, 4, Rate::QuarterDotted, &[1])]);
        let mut stitched = Vec::new();

        // 120 BPM: a pass is 48000 samples, and the accumulated end lands a
        //          few ulps above 2.0 — the engine wraps, resumes from the
        //          loop start, and the seam is seamless.
        // 100 BPM: a pass is 57600 samples, just as exact, but the sum lands a
        //          few ulps *below* 2.0, so the engine sees no wrap at all and
        //          expects the next block near 2.0.
        // 137 BPM: a pass is 42043.8 samples, so the host's own split cannot
        //          land on 2.0: it overshoots by two thirds of a sample
        //          (9.7e-6 beats), ten times the tolerance.
        for tempo in [120.0, 100.0, 137.0] {
            let mut engine = Engine::new();
            let mut out = BlockOutput::new();
            let samples_per_beat = 60.0 / tempo * SAMPLE_RATE as f64;
            // What the host knows: where the loop ends on its own sample
            // clock, and that it must not play past it.
            let pass = (LOOP.1 * samples_per_beat).round() as usize;
            let mut played = 0;
            while played < pass {
                let samples = (pass - played).min(512);
                let pos = played as f64 / samples_per_beat;
                let ctx = BlockCtx::new(pos, tempo, SAMPLE_RATE, samples)
                    .with_loop(LOOP.0, LOOP.1);
                engine.run_block(&ctx, &data, &Playhead::new(), &mut out);
                played += samples;
            }
            assert_eq!(engine.voices().len(), 1, "{tempo}: the gate across the seam");
            let resume = engine.prev_end().expect("playing");

            // The host starts the next pass at the loop start.
            let next = BlockCtx::new(LOOP.0, tempo, SAMPLE_RATE, 512).with_loop(LOOP.0, LOOP.1);
            engine.run_block(&next, &data, &Playhead::new(), &mut out);
            let flushed = out.events().iter().any(|t| t.offset == 0 && t.event.is_note_off());
            assert_eq!(
                flushed,
                (resume - LOOP.0).abs() >= Stitch::TOLERANCE,
                "{tempo}: resumed from {resume}",
            );
            stitched.push(!flushed);
        }

        assert_eq!(stitched, [true, false, false], "the seam survives by luck, not by design");
    }
}
