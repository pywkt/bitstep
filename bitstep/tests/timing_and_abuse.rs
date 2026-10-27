//! Sample-accurate timing and transport abuse, proven without a host or
//! display, so that anything heard against Bitwig's metronome — or left
//! hanging after scrubbing, tempo changes, stopping mid-note, deleting a
//! playing lane or deleting the device while it plays — can be attributed to
//! the host rather than to bitstep:
//!
//! * [`every_strike_and_gate_lands_on_the_sample_the_grid_asks_for`] drives
//!   the whole composition at **60 and 180 BPM** with **64- and
//!   2048-sample** buffers — a slow and a fast tempo, a small and a large
//!   buffer — and checks every generated note-on and note-off against the
//!   absolute sample its beat is worth on the host's sample clock. If a
//!   transient is late in Bitwig, it is not late here.
//! * [`an_abuse_session_never_leaves_a_note_hanging`] runs that abuse list
//!   through the same composition, keeping a ledger of every open voice id,
//!   and asserts the ledger closes after every single one of them. "Zero
//!   stuck notes" is exactly that ledger being empty.
//!
//! Like `integration_wiring.rs`, this drives the real composition — one
//! `SeqModel` written through its public writer API (the path a GUI edit
//! takes), one `Engine`, one reused `BlockOutput` — under
//! `assert_no_alloc`, because abuse arriving on the audio thread must not
//! allocate either. The two parts of the shell left out are the host
//! transport → `BlockCtx` read and the event hand-back, both covered by
//! `lib.rs`'s unit tests.

use std::collections::BTreeMap;
use std::sync::Arc;

use nice_assert_no_alloc::assert_no_alloc;

use bitstep::engine::{BlockCtx, BlockOutput, Engine};
use bitstep::model::{Playhead, SeqModel, Step};
use bitstep::sequencer::{BeatRange, BlockRanges, Rate, TimedEvent};
use bitstep::state;

/// The only sample rate these tests use; both tempos divide it exactly, which
/// is what makes every expected sample a whole number.
const SAMPLE_RATE: f32 = 48_000.0;

/// One generated event as this file reads it: *where* it came out on the
/// host's own sample clock, and what it was.
///
/// The absolute sample is the point of the type. A block's `TimedEvent`
/// offset is relative to that block's buffer, so a one-block-late note-on
/// looks perfectly timed inside its own block; adding the blocks already
/// played is what turns the stream back into the timeline a listener hears.
#[derive(Clone, Copy, PartialEq, Debug)]
struct Fired {
    sample: usize,
    on: bool,
    key: u8,
    voice_id: i32,
    /// The beat the event was generated for, for the failure message.
    beat: f64,
}

/// Which voices are open, by voice id — the "zero stuck notes" ledger.
///
/// Every note-on must open exactly one entry and every note-off must close
/// the one its voice id names, with the key it was opened with. A voice left
/// in here when the transport is not playing is a stuck note, and a note-off
/// for a voice that is not in here is a double release.
#[derive(Default)]
struct Ledger {
    open: BTreeMap<i32, u8>,
}

impl Ledger {
    fn apply(&mut self, fired: Fired) {
        if fired.on {
            let previous = self.open.insert(fired.voice_id, fired.key);
            assert_eq!(previous, None, "voice {} was opened twice: {fired:?}", fired.voice_id);
        } else {
            let key = self.open.remove(&fired.voice_id);
            assert_eq!(key, Some(fired.key), "note-off for an unopened voice: {fired:?}");
        }
    }

    /// The keys still sounding, in order — what a "stuck note" assertion
    /// prints when it fails.
    fn keys(&self) -> Vec<u8> {
        self.open.values().copied().collect()
    }

    fn holds(&self, key: u8) -> bool {
        self.open.values().any(|&open| open == key)
    }
}

/// The plugin's composition plus the crude host driving it: one model, one
/// engine, one output buffer, a sample clock and a transport.
struct Rig {
    model: Arc<SeqModel>,
    playhead: Arc<Playhead>,
    engine: Engine,
    out: BlockOutput,
    /// The buffer the host hands us, constant for the life of a rig (a
    /// Bitwig block-size change is a new audio engine, not a changed block).
    block: usize,
    /// Tempo, constant across a block;
    /// [`Rig::set_tempo`] is the host's tempo change.
    tempo: f64,
    /// Where the host will report the next block from.
    pos: f64,
    loop_range: Option<(f64, f64)>,
    /// Blocks handed to `process()` so far: the host's own sample clock, and
    /// what makes a [`Fired`] sample absolute.
    played: usize,
    ledger: Ledger,
}

impl Rig {
    /// A fresh instance — the default sequence, nothing sounding —
    /// with the host at beat 0.
    fn new(tempo: f64, block: usize) -> Rig {
        Rig {
            model: Arc::new(SeqModel::new(state::default_data())),
            playhead: Arc::new(Playhead::new()),
            engine: Engine::new(),
            out: BlockOutput::new(),
            block,
            tempo,
            pos: 0.0,
            loop_range: None,
            played: 0,
            ledger: Ledger::default(),
        }
    }

    fn samples_per_beat(&self) -> f64 {
        60.0 / self.tempo * SAMPLE_RATE as f64
    }

    fn ctx(&self) -> BlockCtx {
        let ctx = BlockCtx::new(self.pos, self.tempo, SAMPLE_RATE, self.block);
        match self.loop_range {
            Some((start, end)) => ctx.with_loop(start, end),
            None => ctx,
        }
    }

    /// One block, run the way `process()` runs one: the transport into a
    /// [`BlockCtx`], **one** `shared()` for the whole block, `run_block`, and
    /// the generated events read back out — allocation-free throughout.
    fn block(&mut self, ctx: &BlockCtx) -> Vec<Fired> {
        let Rig { model, playhead, engine, out, .. } = self;
        let playhead: &Playhead = playhead;
        assert_no_alloc(|| {
            let data = model.shared();
            engine.run_block(ctx, &data, playhead, out);
        });
        let base = self.played * self.block;
        let fired: Vec<Fired> = self
            .out
            .events()
            .iter()
            .map(|timed: &TimedEvent| {
                assert!(
                    (timed.offset as usize) < ctx.num_samples,
                    "offset {} is outside a {}-sample block",
                    timed.offset,
                    ctx.num_samples
                );
                Fired {
                    sample: base + timed.offset as usize,
                    on: timed.event.is_note_on(),
                    key: timed.event.key(),
                    voice_id: timed.event.voice_id(),
                    beat: timed.event.beat(),
                }
            })
            .collect();
        for &event in &fired {
            self.ledger.apply(event);
        }
        self.played += 1;
        fired
    }

    /// Plays `blocks` consecutive blocks, advancing the host the way the host
    /// itself does — which after a loop wrap is the wrapped position, not the
    /// block's own end (hence [`BlockRanges`] rather than a second copy of
    /// the arithmetic).
    fn play(&mut self, blocks: usize) -> Vec<Fired> {
        (0..blocks)
            .flat_map(|_| {
                let ctx = self.ctx();
                let fired = self.block(&ctx);
                let timing = ctx.timing();
                let range = BeatRange::from_span(ctx.pos_beats, timing.span_beats());
                self.pos = BlockRanges::split(range, ctx.loop_region(), timing).resume_beat();
                fired
            })
            .collect()
    }

    /// Plays about `beats` of timeline, in whole blocks — the unit the abuse
    /// session's phases are written in ("three passes of the cycle, then half
    /// a beat into the pad's gate"). Rounded up, so a phase always reaches
    /// the position it names.
    fn play_beats(&mut self, beats: f64) -> Vec<Fired> {
        let per_block = self.block as f64 / self.samples_per_beat();
        self.play((beats / per_block).ceil() as usize)
    }

    /// The host relocating: the next block starts at `beats` with no
    /// continuity, which the engine must read as a jump.
    fn locate(&mut self, beats: f64) {
        self.pos = beats;
    }

    /// The host's tempo change, which arrives as a different `tempo` on the
    /// next block and nothing else.
    fn set_tempo(&mut self, tempo: f64) {
        self.tempo = tempo;
    }

    /// One block with the transport stopped, the position standing still.
    fn stop(&mut self) -> Vec<Fired> {
        let ctx = BlockCtx { playing: false, ..self.ctx() };
        self.block(&ctx)
    }

    /// What the host does when the device is deleted or the engine is put to
    /// sleep: `Plugin::deactivate` can only *ask* for the flush (`lib.rs`),
    /// and the note-offs come out of the next block — if there is one.
    fn deactivate(&mut self) {
        self.engine.request_flush();
    }

    fn lane_ids(&self) -> Vec<u32> {
        let data = self.model.shared();
        (0..data.lane_count()).filter_map(|pos| data.lane(pos).map(|lane| lane.id)).collect()
    }

    /// Asserts the abuse about to happen has something to abuse: a phase that
    /// scrubs or stops with nothing sounding proves nothing at all.
    fn assert_sounding(&self, key: u8, what: &str) {
        assert!(
            self.ledger.holds(key),
            "{what}: key {key} should be sounding, open voices are {:?}",
            self.ledger.keys()
        );
    }

    /// The counterpart: that key is *not* sounding any more, whatever else is.
    fn assert_silent_of(&self, key: u8, what: &str) {
        assert!(
            !self.ledger.holds(key),
            "{what}: key {key} is still sounding, open voices are {:?}",
            self.ledger.keys()
        );
    }

    /// The voice id currently holding `key` — the handle a note-off has to
    /// carry, which is how a release is matched to the note it ends.
    fn open_voice_of(&self, key: u8) -> i32 {
        let found = self.ledger.open.iter().find(|&(_, &open)| open == key);
        *found.unwrap_or_else(|| panic!("key {key} is not sounding")).0
    }

    fn assert_silent(&self, what: &str) {
        assert_eq!(self.ledger.keys(), Vec::<u8>::new(), "{what}: stuck notes");
        assert!(self.engine.voices().is_empty(), "{what}: the voice table still holds a note");
    }
}

/// The timing matrix: a slow and a fast tempo, each with a small and a large
/// buffer.
const MATRIX: [(f64, usize); 4] = [(60.0, 64), (60.0, 2048), (180.0, 64), (180.0, 2048)];

/// Every generated event comes out at the sample its beat is worth, at both
/// of the matrix's tempos and both of its buffer sizes.
///
/// The patch is one 1/16 lane with every step on, so there is a transient on
/// every sixteenth: at 60 BPM / 48 kHz a sixteenth is 12 000 samples and at
/// 180 BPM it is 4 000, both exact, and the gate (`GATE_FRACTION` = half the
/// subdivision) halves them again. So the expected stream is a note-on every
/// `step` samples with its note-off `step / 2` later, counted on the host's
/// sample clock across *all* the blocks — which is the only way a flam would
/// show up, since a late event looks on time inside its own block.
///
/// Compared exactly, with one documented exception: a grid point that is a
/// whole number of buffers from the start of the run can come out on the
/// *previous* block's last sample, one sample early. The engine's block start
/// is an accumulated sum of f64 spans, so when it drifts a few ulps past the
/// exact beat, the strike falls inside the closing block and its offset
/// clamps to that block's last sample (`BlockTiming::beats_to_samples`). It
/// fires exactly once either way, and one sample at 48 kHz is 21 µs — but it
/// is bitstep's own rounding rather than the host's, so it is written down
/// here, as a known open question, instead of being hidden in a tolerance.
/// Everywhere else the sample is the exact one, which is what makes a
/// recording against Bitwig's metronome worth making: a flam there is the
/// host's.
#[test]
fn every_strike_and_gate_lands_on_the_sample_the_grid_asks_for() {
    for (tempo, block) in MATRIX {
        let mut rig = Rig::new(tempo, block);
        let lane = rig.lane_ids()[0];
        let key = rig.model.shared().lane(0).expect("a default lane").key();
        for index in 0..16 {
            assert!(rig.model.set_step(0, lane, index, Step::default_on()));
        }

        // Two beats of it, in whole blocks.
        let step = rig.samples_per_beat() / 4.0;
        let blocks = (2.0 * rig.samples_per_beat() / block as f64).floor() as usize;
        let covered = blocks * block;
        let fired = rig.play(blocks);

        // The grid the run should carry: a note-on every `step` samples, each
        // followed by the gate that closes it half a step later. A point
        // exactly at `covered` is the first one of the *next* window, and is
        // allowed either to be absent or to have drifted a sample into this
        // one (see the note above).
        let mut grid: Vec<(usize, bool)> = Vec::new();
        for index in 0.. {
            let on = (index as f64 * step) as usize;
            if on > covered {
                break;
            }
            grid.push((on, true));
            let off = on + (step / 2.0) as usize;
            if off <= covered {
                grid.push((off, false));
            }
        }

        let case = format!("{tempo} BPM, {block}-sample blocks");
        let mut events = fired.iter();
        for (sample, on) in grid {
            let Some(event) = events.next() else {
                assert_eq!(sample, covered, "{case}: nothing fired for sample {sample}");
                break;
            };
            assert_eq!(event.on, on, "{case}: wrong direction at {sample}: {event:?}");
            let on_a_block_edge = sample > 0 && sample % block == 0;
            assert!(
                event.sample == sample
                    || (on_a_block_edge && event.sample + 1 == sample),
                "{case}: expected sample {sample}, got {event:?}"
            );
        }
        assert!(events.next().is_none(), "{case}: extra events: {fired:#?}");
        assert!(fired.iter().all(|f| f.key == key), "{case}: every event is the lane's key");
        // The run ends mid-gate (its last strike is the one on the window's
        // edge), so stopping is what closes the ledger: every voice id the
        // run opened was released, once, by the end of the stopped block.
        rig.stop();
        rig.assert_silent(&case);
    }
}

/// The abuse list, in one session, with the ledger closed after every
/// item: a 2-bar loop, a scrub, a tempo change, a lane deleted while it
/// plays, a stop mid-note, and the device deleted while it plays.
///
/// The patch mixes rates deliberately. The pad is a 1/1 lane one step long,
/// so it strikes every four beats and its gate is **two beats** long — long
/// enough that each abuse below lands in the middle of a sounding note
/// rather than in a gap, which every phase asserts before it does anything;
/// the kick covers the phases the pad has been deleted for, and the hat
/// fills in the sixteenths so the voice table is never down to one note.
/// The buffer is 512 samples, i.e. nothing like a whole number of steps, so
/// gates straddle block edges the way they do in a host.
///
/// What is *not* asserted is which notes the host hears after a given abuse:
/// that is `engine.rs`'s business, phase by phase (a jump re-fires the step
/// it lands on, a stop drops the pending pattern, and so on). This test is
/// about the one property all of them share and the only one a listener
/// notices — that nothing is left hanging.
#[test]
fn an_abuse_session_never_leaves_a_note_hanging() {
    // 120 BPM at 48 kHz is 24 000 samples per beat; a 512-sample block is
    // 0.0213 of one.
    let mut rig = Rig::new(120.0, 512);
    let ids = rig.lane_ids();
    let (kick, pad, hat) = (ids[0], ids[1], ids[2]);
    let (kick_key, pad_key) = (36, 38);
    // Three different keys, set here rather than taken from the defaults:
    // this session tracks each note by its key.
    for (lane, key) in [(kick, kick_key), (pad, pad_key), (hat, 42)] {
        assert!(rig.model.set_key(lane, key));
    }

    assert!(rig.model.set_rate(0, kick, Rate::Quarter));
    assert!(rig.model.set_length(0, kick, 4));
    for index in [0, 2] {
        assert!(rig.model.set_step(0, kick, index, Step::default_on()));
    }
    // One step at 1/1: a strike every four beats, held for two.
    assert!(rig.model.set_rate(0, pad, Rate::Whole));
    assert!(rig.model.set_length(0, pad, 1));
    assert!(rig.model.set_step(0, pad, 0, Step::default_on()));
    for index in 0..16 {
        assert!(rig.model.set_step(0, hat, index, Step::default_on()));
    }

    // ---- a 2-bar loop, three passes ----
    rig.loop_range = Some((0.0, 8.0));
    let looped = rig.play_beats(24.0);
    assert!(
        looped.iter().filter(|f| f.on && f.key == pad_key).count() >= 6,
        "the pad struck twice on each of the three passes"
    );
    // Half a beat into the gate the strike at beat 4 opened.
    rig.play_beats(4.5);
    rig.assert_sounding(pad_key, "three passes of a 2-bar loop");

    // ---- a scrub, backwards, mid-gate ----
    // Two notes are sounding here (the pad's and a hat's), so the scrub has
    // two voices to release and the flush assertion below is not vacuous.
    let open_before = rig.ledger.keys().len();
    assert!(open_before >= 2, "open voices before the scrub: {:?}", rig.ledger.keys());
    rig.locate(1.75);
    let base = rig.played * rig.block;
    let scrubbed = rig.play(1);
    assert!(
        scrubbed.iter().take(open_before).all(|f| !f.on && f.sample == base),
        "the jump's flush note-offs come first, at offset 0: {scrubbed:#?}"
    );
    rig.assert_silent_of(pad_key, "a scrub releases the note it interrupted");

    // ---- a tempo change, mid-gate ----
    rig.locate(4.0);
    rig.play_beats(0.5);
    rig.assert_sounding(pad_key, "before the tempo change");
    rig.set_tempo(180.0);
    // The buffer is unchanged, so the same 512 samples now cover half again
    // as much timeline: the stitch has to hold across a block whose span
    // changed under it, and the gate still has to close where it was going
    // to rather than being flushed by a jump.
    let faster = rig.play_beats(2.0);
    let closed: Vec<&Fired> = faster.iter().filter(|f| !f.on && f.key == pad_key).collect();
    assert_eq!(closed.len(), 1, "the pad was released once: {faster:#?}");
    assert_eq!(closed[0].beat, 6.0, "at its own gate end, not at the tempo change");
    rig.set_tempo(120.0);

    // ---- a lane deleted while it is playing ----
    rig.locate(4.0);
    rig.play_beats(0.5);
    rig.assert_sounding(pad_key, "before deleting the pad lane");
    let pad_voice = rig.open_voice_of(pad_key);
    assert!(rig.model.remove_lane(pad));
    // Past the gate end at beat 6: the voice table holds the note, not the
    // lane, so a deleted lane's gate still closes where it was going to.
    let after = rig.play_beats(2.0);
    let released: Vec<&Fired> =
        after.iter().filter(|f| !f.on && f.voice_id == pad_voice).collect();
    assert_eq!(released.len(), 1, "the deleted lane's note was released exactly once");
    assert_eq!(released[0].beat, 6.0, "and at its own gate end: {:?}", released[0]);
    assert!(
        !after.iter().any(|f| f.on && f.key == pad_key),
        "and the lane struck no more notes after being deleted"
    );
    // One lane fewer, so the slot just past the new last lane is the one
    // left over.
    let vacated = rig.lane_ids().len();
    assert_eq!(rig.playhead.step(vacated), None, "the vacated playhead slot went blank");

    // ---- stop, mid-note ----
    rig.locate(0.0);
    rig.play_beats(0.25);
    rig.assert_sounding(kick_key, "before stopping");
    let base = rig.played * rig.block;
    let stopped = rig.stop();
    assert!(!stopped.is_empty(), "the stop released what was sounding");
    assert!(
        stopped.iter().all(|f| !f.on && f.sample == base),
        "every stop note-off goes out at offset 0: {stopped:#?}"
    );
    rig.assert_silent("after a stop mid-note");
    // A second stopped block has nothing left to release.
    assert!(rig.stop().is_empty(), "the flush happens once");

    // ---- the device deleted while playing ----
    rig.locate(0.0);
    rig.play_beats(0.25);
    rig.assert_sounding(kick_key, "before deleting the device");
    rig.deactivate();
    assert!(rig.engine.flush_pending(), "deactivate() leaves the flush for the next block");
    let flushed = rig.play(1);
    assert!(
        flushed.iter().any(|f| !f.on),
        "the block after deactivate() releases everything: {flushed:#?}"
    );
    rig.assert_silent("after the device was deleted while playing");
}

/// Several lanes on **one key** — a duplicated lane, plus lanes at other rates
/// and ratchets landing on and between its steps — through a loop, at block
/// sizes that put collisions inside one block and across block edges. Every
/// note-off must close a voice the host has already seen open, and stopping
/// must leave nothing sounding. (The session above gives every lane its own
/// key, so it never exercises a retrigger between lanes.)
#[test]
fn lanes_sharing_a_key_never_leave_a_note_hanging() {
    for block in [64, 512, 1999, 8192] {
        let mut rig = Rig::new(120.0, block);
        let ids = rig.lane_ids();
        let (kick, snare, hat) = (ids[0], ids[1], ids[2]);

        for index in [0, 4, 8, 12] {
            assert!(rig.model.set_step(0, kick, index, Step::default_on()));
        }
        // A copy of the kick: the same key, striking on the same beats.
        assert!(rig.model.duplicate_lane(kick).is_some());
        // Triplet ratchets on the kick's key, landing between its steps.
        assert!(rig.model.set_key(snare, 36));
        assert!(rig.model.set_rate(0, snare, Rate::SixteenthTriplet));
        for index in [1, 5, 7] {
            assert!(rig.model.set_step(0, snare, index, Step::default_on().with_ratchet(3)));
        }
        // Long 1/4 notes on the same key, cut short by everything else.
        assert!(rig.model.set_key(hat, 36));
        assert!(rig.model.set_rate(0, hat, Rate::Quarter));
        assert!(rig.model.set_length(0, hat, 3));
        assert!(rig.model.set_step(0, hat, 0, Step::default_on().with_ratchet(2)));

        rig.loop_range = Some((0.0, 6.0));
        let played = rig.play_beats(20.0);
        assert!(played.iter().filter(|fired| fired.on).count() > 40, "block {block}: too quiet");
        rig.stop();
        rig.assert_silent(&format!("block {block}"));
    }
}
