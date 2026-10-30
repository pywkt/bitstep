//! A walkthrough of each editor feature, driven through the real composition
//! without a host or display and checked against the notes it generates:
//! building a beat by hand while the transport runs, dragging lane lengths
//! into polymeter, long lanes, what a step carries, driving `Pattern Select`
//! from a modulator, and saving/closing/reopening the project. Proving each
//! of these here means anything a user sees in Bitwig can be attributed:
//!
//! * [`a_beat_is_built_while_the_transport_runs`] — steps toggled, a lane
//!   added and a length dragged, all between blocks, with the output after
//!   each edit re-derived from the step-timing formula.
//! * [`polymeter_lanes_keep_their_own_phase`] — 16/15/13/11 at one rate,
//!   each lane on its own cycle, and the same phase after a scrub.
//! * [`long_lanes_grow_hide_and_give_their_steps_back`] — 64/37/128 steps,
//!   the chunked capacity they get, and a tail hidden by a shorter length
//!   and restored by a longer one.
//! * [`ratchets_probability_and_pan_reach_every_note`] — ratchet
//!   subdivisions and their gates, a probability rolled once per occurrence,
//!   and the velocity/pan payload each note carries.
//! * [`a_modulator_sweeping_pattern_select_switches_lane_by_lane`] —
//!   `patsel` moved every single block, as a Steps modulator or an LFO moves
//!   it, with each lane switching at its own step boundary.
//! * [`the_walkthrough_patch_survives_save_close_and_reopen`] — the whole
//!   patch through JSON into a fresh instance, hidden steps and all eight
//!   patterns included, and the same notes out of it afterwards.
//!
//! Like `integration_wiring.rs` and `timing_and_abuse.rs` this drives the
//! real composition — one `SeqModel` written through its public writer API
//! (the path every GUI edit takes), the params struct over it, one `Engine`,
//! one reused `BlockOutput` — under `assert_no_alloc`, because an edit
//! arriving between two blocks must not make the next one allocate. The two
//! parts of the shell left out are the host transport → `BlockCtx` read and
//! the event hand-back, both covered by `lib.rs`'s unit tests.

use std::collections::BTreeMap;
use std::sync::Arc;

use nice_assert_no_alloc::assert_no_alloc;
use nice_plug::params::InternalParamMut;

use bitstep::BitstepParams;
use bitstep::engine::{BlockCtx, BlockOutput, Engine};
use bitstep::model::{
    CAPACITY_CHUNK, Lane, LaneData, NUM_PATTERNS, Playhead, SeqData, SeqModel, Step,
    chunked_capacity,
};
use bitstep::sequencer::{
    BeatRange, BlockRanges, GATE_FRACTION, Rate, SeqEvent, TimedEvent, pan_to_f32,
    velocity_to_f32,
};
use bitstep::state::{self, SeqSnapshot};

/// 120 BPM at 48 kHz, i.e. 24 000 samples per beat — the tempo every
/// expected beat in this file is written against.
const TEMPO: f64 = 120.0;
const SAMPLE_RATE: f32 = 48_000.0;
const SAMPLES_PER_BEAT: f64 = 24_000.0;

/// The PRNG seed every rig here runs with, so the one test that uses a
/// probability in `1..=99` is deterministic.
const SEED: u64 = 0x5713_0815_F00D_B33F;

// ---- the walkthrough's own vocabulary ----

/// One generated note as this file compares them: the exact beat it was
/// generated for, which way it went, and the key it carries.
///
/// Beats are compared **exactly**: every one of them is recomputed by the
/// engine from the step index and the rate, so a stitched block edge cannot
/// move one.
#[derive(Clone, Copy, PartialEq, Debug)]
struct Note {
    beat: f64,
    on: bool,
    key: u8,
}

/// The notes of a run in a canonical order, so a comparison does not depend
/// on which lane the scan visited first.
///
/// Two lanes striking the same key at the same beat would collapse here, and
/// that is deliberate: no patch in this file does it (the retrigger rule
/// would make it a different test).
fn sorted(mut notes: Vec<Note>) -> Vec<Note> {
    notes.sort_by(|a, b| {
        a.beat.total_cmp(&b.beat).then(a.key.cmp(&b.key)).then(a.on.cmp(&b.on))
    });
    notes
}

/// One step occurrence's note lifetime, as the per-lane scan defines it:
/// `on = k * step_len + j * sub`, `off = on + sub / 2`.
#[derive(Clone, Copy, PartialEq, Debug)]
struct Hit {
    on: f64,
    off: f64,
}

/// One lane as this file describes it — a key, a rate, a loop length
/// and the steps that are on — and as this file re-derives its output.
///
/// The point of the type is that the expectations below come from the
/// step-timing formula rather than from a second run of the engine: a patch is written
/// into the model with [`Patch::write`] and read back out of the output with
/// [`Patch::notes`], and the two meet only through the plugin.
struct Patch<'a> {
    key: u8,
    rate: Rate,
    length: usize,
    /// `(step index, packed step)`. An index at or past `length` is one of
    /// the hidden steps — written into the lane, never played until the
    /// length reaches it.
    steps: &'a [(usize, Step)],
}

impl Patch<'_> {
    fn step(&self, index: usize) -> Step {
        self.steps.iter().find(|&&(i, _)| i == index).map_or(Step::OFF, |&(_, step)| step)
    }

    /// Writes this patch into one pattern of one lane, through exactly the
    /// writer calls the editor makes: `ensure_capacity` for the end-handle
    /// drag past the current capacity, then the value edits.
    fn write(&self, model: &SeqModel, pattern: usize, lane_id: u32) {
        let highest = self.steps.iter().map(|&(index, _)| index + 1).max().unwrap_or(0);
        // The key is the lane's, not the pattern's, so writing the same patch
        // into a second bank simply re-states it.
        assert!(model.set_key(lane_id, self.key));
        assert!(model.ensure_capacity(pattern, lane_id, highest.max(self.length)));
        assert!(model.set_rate(pattern, lane_id, self.rate));
        assert!(model.set_length(pattern, lane_id, self.length as u32));
        for &(index, step) in self.steps {
            assert!(model.set_step(pattern, lane_id, index, step), "step {index} is in range");
        }
    }

    /// Every note lifetime this lane owns that *touches* `range`, including
    /// the occurrence that opened just before it.
    ///
    /// Only deterministic probabilities are allowed through here: a step in
    /// `1..=99` depends on where the host split the timeline, which
    /// [`ratchets_probability_and_pan_reach_every_note`] asserts directly
    /// instead of predicting.
    fn hits(&self, range: BeatRange) -> Vec<Hit> {
        let step_len = self.rate.rate_beats();
        // The same ±1 padding the scan uses, for the same reason: a step
        // whose base beat is before the range can still drop ratchet
        // sub-hits inside it, and a gate can close one step late.
        let first = (range.start / step_len).floor() as i64 - 1;
        let last = (range.end / step_len).floor() as i64 + 1;
        let mut hits = Vec::new();
        for k in first..=last {
            let step = self.step(k.rem_euclid(self.length as i64) as usize);
            if !step.is_on() {
                continue;
            }
            assert!(
                matches!(step.probability(), 0 | 100),
                "a predicted patch may only hold 0 or 100 probabilities, not {}",
                step.probability()
            );
            if step.probability() == 0 {
                continue;
            }
            let ratchet = step.ratchet().max(1);
            // Written in the engine's own order so the beats are
            // bit-identical to the ones it emits, not merely close.
            let sub = step_len / ratchet as f64;
            for j in 0..ratchet as u32 {
                let on = k as f64 * step_len + j as f64 * sub;
                hits.push(Hit { on, off: on + sub * GATE_FRACTION });
            }
        }
        hits
    }

    /// The notes of every hit that *opens* inside `range` — the whole stream
    /// of a lane that did not exist, or played something else, before the
    /// window began.
    fn notes_opening_in(&self, range: BeatRange) -> Vec<Note> {
        let mut notes = Vec::new();
        for hit in self.hits(range) {
            if !range.contains(hit.on) {
                continue;
            }
            notes.push(Note { beat: hit.on, on: true, key: self.key });
            if range.contains(hit.off) {
                notes.push(Note { beat: hit.off, on: false, key: self.key });
            }
        }
        notes
    }

    /// The gates that close inside `range` from hits that opened before it —
    /// what the previous window left sounding across the seam.
    fn gates_carried_into(&self, range: BeatRange) -> Vec<Note> {
        self.hits(range)
            .into_iter()
            .filter(|hit| hit.on < range.start && range.contains(hit.off))
            .map(|hit| Note { beat: hit.off, on: false, key: self.key })
            .collect()
    }

    /// The notes a host receives from a lane that has been playing this patch
    /// since before `range`: the hits opening inside it, plus the gate of
    /// whatever was already sounding when it began (a gate routinely closes
    /// in a later block than the one that opened it).
    fn notes(&self, range: BeatRange) -> Vec<Note> {
        let mut notes = self.notes_opening_in(range);
        notes.extend(self.gates_carried_into(range));
        notes
    }
}

/// The notes over a window whose lane content *changed* at `range.start`:
/// what the new patch owes, plus the gate of anything the old one left
/// sounding across the seam.
///
/// That second part is the "edit while playing" rule made explicit: a note's
/// lifetime belongs to the voice table, not to the lane or the pattern that
/// started it, so an edit never cuts a sounding note short — it only changes
/// what is struck next.
fn notes_across_an_edit(old: &Patch, new: &Patch, range: BeatRange) -> Vec<Note> {
    let mut notes = new.notes_opening_in(range);
    notes.extend(old.gates_carried_into(range));
    notes
}

/// Compares a phase's output against its expectation, refusing a vacuous
/// pass: a walkthrough phase that generated nothing proves nothing, and an
/// empty expectation is the one way every assertion in this file could go
/// green while the plugin did nothing at all.
fn assert_plays(got: Vec<Note>, want: Vec<Note>, what: &str) {
    assert!(!want.is_empty(), "{what}: nothing was expected, so the phase proves nothing");
    assert_eq!(sorted(got), sorted(want), "{what}");
}

/// Which voices are open, by voice id — the "nothing is left sounding"
/// ledger of `timing_and_abuse.rs`, reused here because every phase of the
/// walkthrough ends by stopping the transport.
#[derive(Default)]
struct Ledger {
    open: BTreeMap<i32, u8>,
}

impl Ledger {
    fn apply(&mut self, timed: &TimedEvent) {
        let (key, voice_id) = (timed.event.key(), timed.event.voice_id());
        if timed.event.is_note_on() {
            let previous = self.open.insert(voice_id, key);
            assert_eq!(previous, None, "voice {voice_id} was opened twice");
        } else {
            let was = self.open.remove(&voice_id);
            assert_eq!(was, Some(key), "note-off for an unopened voice: {timed:?}");
        }
    }

    fn keys(&self) -> Vec<u8> {
        self.open.values().copied().collect()
    }
}

// ---- the instance under test ----

/// The plugin's composition, assembled the way `Bitstep::default()`
/// assembles it, plus the crude host that drives it.
struct Rig {
    params: BitstepParams,
    /// The one sequence every writer writes and the audio thread reads — the
    /// same `Arc` the params struct persists.
    model: Arc<SeqModel>,
    playhead: Arc<Playhead>,
    engine: Engine,
    out: BlockOutput,
    /// The buffer the host hands us, constant for the life of a rig.
    block: usize,
    /// Where the host will report the next block from.
    pos: f64,
    ledger: Ledger,
}

/// The four-lane drum kit the walkthrough is written against: name and
/// key. Fixed here rather than taken from the
/// plugin's defaults, so the walkthrough keeps testing the same patch when
/// those change.
const KIT: [(&str, u8); 4] = [("Kick", 36), ("Snare", 38), ("Hat", 42), ("Perc", 45)];

/// A fresh sequence holding [`KIT`], laid out as `state::default_data` lays
/// out its own lanes: ids from 1, palette colours, channel 0, empty patterns.
/// Every lane is one bar of 1/16 in one capacity chunk, spelled out rather
/// than left to `LaneData::empty`, for the same reason as [`KIT`].
fn kit_data() -> SeqData {
    let lanes: Vec<Lane> = KIT
        .iter()
        .enumerate()
        .map(|(index, &(name, key))| {
            Lane::new(1 + index as u32, name, state::LANE_PALETTE[index], key, 0)
        })
        .collect();
    let one_bar = || LaneData::with_capacity(CAPACITY_CHUNK);
    let patterns = std::array::from_fn(|_| lanes.iter().map(|_| one_bar()).collect());
    SeqData::new(lanes, patterns)
}

impl Rig {
    /// A fresh instance holding the walkthrough's kit ([`kit_data`]), nothing
    /// sounding, with the host at beat 0.
    fn new(block: usize) -> Rig {
        Rig::from_model(Arc::new(SeqModel::new(kit_data())), block)
    }

    /// The same, around a model that is already loaded — a project reopened.
    fn from_model(model: Arc<SeqModel>, block: usize) -> Rig {
        Rig {
            params: BitstepParams::new(Arc::clone(&model)),
            model,
            playhead: Arc::new(Playhead::new()),
            engine: Engine::with_seed(SEED),
            out: BlockOutput::new(),
            block,
            pos: 0.0,
            ledger: Ledger::default(),
        }
    }

    /// The transport a host would report for the next block, with `patsel`
    /// read out of the parameter exactly as `process()` reads it.
    fn ctx(&self) -> BlockCtx {
        BlockCtx::new(self.pos, TEMPO, SAMPLE_RATE, self.block)
            .with_pattern_select(self.params.pattern_select())
    }

    /// One block, run the way `process()` runs one: the parameter into a
    /// [`BlockCtx`], **one** `shared()` for the whole block, `run_block`, and
    /// the generated events read back out — allocation-free throughout.
    fn block(&mut self, ctx: &BlockCtx) -> Vec<Note> {
        let Rig { model, playhead, engine, out, .. } = self;
        let playhead: &Playhead = playhead;
        assert_no_alloc(|| {
            let data = model.shared();
            engine.run_block(ctx, &data, playhead, out);
        });
        assert_eq!(self.out.overflows(), 0, "a walkthrough block dropped events");
        let mut notes = Vec::with_capacity(self.out.len());
        for timed in self.out.events() {
            assert!(
                (timed.offset as usize) < ctx.num_samples,
                "offset {} is outside a {}-sample block",
                timed.offset,
                ctx.num_samples
            );
            self.ledger.apply(timed);
            notes.push(Note {
                beat: timed.event.beat(),
                on: timed.event.is_note_on(),
                key: timed.event.key(),
            });
        }
        notes
    }

    /// Plays `blocks` consecutive blocks, advancing the host the way the host
    /// itself does (through [`BlockRanges`], so the resume position is
    /// bit-identical to the engine's own and the stitching path stays under
    /// test).
    fn play(&mut self, blocks: usize) -> Vec<Note> {
        (0..blocks)
            .flat_map(|_| {
                let ctx = self.ctx();
                let notes = self.block(&ctx);
                let timing = ctx.timing();
                let range = BeatRange::from_span(ctx.pos_beats, timing.span_beats());
                self.pos = BlockRanges::split(range, ctx.loop_region(), timing).resume_beat();
                notes
            })
            .collect()
    }

    /// Plays at least `beats` of timeline, in whole blocks — the unit the
    /// walkthrough's phases are written in. Returns the notes and the window
    /// they were generated in, which is what an expectation is computed over.
    fn play_beats(&mut self, beats: f64) -> (Vec<Note>, BeatRange) {
        let per_block = self.block as f64 / SAMPLES_PER_BEAT;
        let from = self.pos;
        let notes = self.play((beats / per_block).ceil() as usize);
        (notes, BeatRange::new(from, self.pos))
    }

    /// The host relocating: the next block starts at `beats` with no
    /// continuity, which the engine must read as a jump.
    fn locate(&mut self, beats: f64) {
        self.pos = beats;
    }

    /// One block with the transport stopped — which releases everything
    /// sounding at offset 0 and forgets the stitch.
    fn stop(&mut self) -> Vec<Note> {
        let ctx = BlockCtx { playing: false, ..self.ctx() };
        self.block(&ctx)
    }

    /// The host moving `patsel`: an automation event, a modulator, or a
    /// click on one of the editor's pattern buttons.
    ///
    /// `IntParam`'s setters are `unsafe` because only a wrapper may call
    /// them, and here the wrapper is us — the same thing the editor's own
    /// fake host does in `editor/mod.rs`. Going through the parameter rather
    /// than handing the engine a pattern index is the point: `patsel` is
    /// what a Steps modulator actually moves.
    fn host_moves_patsel(&self, pattern: i32) {
        // SAFETY: nothing else holds this parameter — no wrapper, no
        // smoother, no editor — so there is no concurrent reader for a value
        // to tear under.
        unsafe { self.params.pattern_select._internal_set_plain_value(pattern) };
        assert_eq!(self.params.pattern_select(), pattern as u8);
    }

    fn lane_ids(&self) -> Vec<u32> {
        let data = self.model.shared();
        (0..data.lane_count()).filter_map(|pos| data.lane(pos).map(|lane| lane.id)).collect()
    }

    /// Stops the transport and asserts the walkthrough left nothing
    /// sounding — every phase below ends with this.
    fn stop_and_assert_silent(&mut self, what: &str) {
        self.stop();
        assert_eq!(self.ledger.keys(), Vec::<u8>::new(), "{what}: stuck notes");
        assert!(self.engine.voices().is_empty(), "{what}: the voice table still holds a note");
    }
}

/// The walkthrough's four-on-the-floor starting point: the first default
/// lane, 1/16, one bar, a strike on every beat.
const FOUR_ON_THE_FLOOR: Patch = Patch {
    key: 36,
    rate: Rate::Sixteenth,
    length: 16,
    steps: &[
        (0, Step::default_on()),
        (4, Step::default_on()),
        (8, Step::default_on()),
        (12, Step::default_on()),
    ],
};

// ---- the walkthrough ----

/// Phase 1: a beat built while the transport runs.
///
/// Every edit lands between two blocks, which is where a GUI edit lands in
/// reality (the editor writes from its own thread while `process()` reads the
/// published `SeqData`), and after each one the output is re-derived from the
/// step-timing formula rather than from the engine. Three kinds of edit, in the
/// order a user makes them:
///
/// 1. **Steps toggled** while a silent sequence plays — audible from the
///    next block, with no restart and no reset of the lane's phase.
/// 2. **A lane added** mid-bar — it starts playing on its own grid, and the
///    lanes already running are untouched.
/// 3. **A length dragged** from 16 to 15 — the lane "jumps to where it
///    would be", because positions come from the host's beat and never from
///    a step counter. That is the one edit whose result surprises people, so
///    it is asserted beat by beat.
#[test]
fn a_beat_is_built_while_the_transport_runs() {
    // 4096 samples is 0.1706666 beats — nothing like a whole number of
    // steps, so gates and edits routinely straddle block edges.
    let mut rig = Rig::new(4096);
    let ids = rig.lane_ids();
    let (kick, snare) = (ids[0], ids[1]);

    // A fresh instance plays nothing at all: four lanes, every step off.
    let (silent, _) = rig.play_beats(0.5);
    assert_eq!(silent, Vec::new(), "a fresh sequence is silent");

    // ---- steps toggled while it runs ----
    for &(index, _) in FOUR_ON_THE_FLOOR.steps {
        assert!(rig.model.toggle_step(0, kick, index));
    }
    let (built, window) = rig.play_beats(4.0);
    assert_plays(
        built,
        FOUR_ON_THE_FLOOR.notes(window),
        "the toggled steps play from the next block, on the host's own grid",
    );

    // ---- a lane added while it runs ----
    let clap = rig.model.add_lane("Clap", 0x65_43_21, 39, 0).expect("room for a fifth lane");
    let clap_patch = Patch {
        key: 39,
        rate: Rate::EighthDotted,
        length: 4,
        steps: &[(0, Step::default_on()), (2, Step::default_on())],
    };
    clap_patch.write(&rig.model, 0, clap);
    let (added, window) = rig.play_beats(4.0);
    let mut want = FOUR_ON_THE_FLOOR.notes(window);
    // Added mid-bar, so only the hits it opens inside this window exist —
    // there was no lane to have left a gate open at the seam.
    want.extend(clap_patch.notes_opening_in(window));
    assert_plays(
        added,
        want,
        "the new lane plays on its own 1/8D grid and the kick keeps its phase",
    );

    // ---- a length dragged, mid-bar ----
    let shortened = Patch { length: 15, ..FOUR_ON_THE_FLOOR };
    assert!(rig.model.set_length(0, kick, 15));
    let (dragged, window) = rig.play_beats(8.0);
    let mut want = notes_across_an_edit(&FOUR_ON_THE_FLOOR, &shortened, window);
    want.extend(clap_patch.notes(window));
    assert_plays(
        dragged.clone(),
        want,
        "a 15-step lane is read at k.rem_euclid(15), so it jumps to where it would be",
    );
    // The two lanes now disagree about where the bar is, which is the whole
    // point of the drag: the kick's cycle is 3.75 beats and the clap's 3.0,
    // so neither is on the host's bar line.
    assert!(
        dragged.iter().any(|note| note.beat.fract() != 0.0),
        "a shortened lane strikes off the beat: {dragged:#?}"
    );

    // An untouched lane is still untouched after all of it.
    let snare_data = rig.model.shared();
    let snare_data = snare_data.lane_data(0, 1).expect("the snare lane");
    assert_eq!(snare_data.length(), 16, "the snare kept its length");
    assert!(!snare_data.step(0).is_on(), "and its steps");
    assert_eq!(rig.model.shared().lane(1).expect("the snare lane").id, snare);

    rig.stop_and_assert_silent("a beat built while playing");
}

/// Phase 2: polymeter — 16/15/13/11 steps at one rate.
///
/// Each lane is read at `k.rem_euclid(len)` from song position 0 (never from
/// a step counter), so the four cycles are 4, 3.75, 3.25 and 2.75 beats long
/// and they drift apart on purpose. Two properties
/// matter to a user building one:
///
/// * over a window, every lane's strikes are exactly where its own cycle puts
///   them — asserted against the formula, lane by lane;
/// * the phase does not depend on where playback started, so scrubbing into
///   the middle of the arrangement lands on the same pattern of hits a
///   continuous play would have produced there. That is what makes polymeter
///   usable at all in a host, and it is asserted by locating to a beat no
///   cycle divides.
#[test]
fn polymeter_lanes_keep_their_own_phase() {
    let mut rig = Rig::new(4096);
    let ids = rig.lane_ids();
    // One strike per cycle each, on four different keys: the sparsest patch
    // that still shows four different cycle lengths.
    let lengths = [16, 15, 13, 11];
    const FIRST_STEP_ON: [(usize, Step); 1] = [(0, Step::default_on())];
    let patches: Vec<Patch> = ids
        .iter()
        .zip(lengths)
        .enumerate()
        .map(|(index, (&id, length))| {
            let patch = Patch {
                key: KIT[index].1,
                rate: Rate::Sixteenth,
                length,
                steps: &FIRST_STEP_ON,
            };
            patch.write(&rig.model, 0, id);
            patch
        })
        .collect();

    let (played, window) = rig.play_beats(16.0);
    let want: Vec<Note> = patches.iter().flat_map(|patch| patch.notes(window)).collect();
    assert_plays(played.clone(), want, "each lane on its own cycle");

    // The cycles are 4, 3.75, 3.25 and 2.75 beats, whose least common
    // multiple is 8580 beats: inside this window the only beat where all
    // four strike together is the one they all started on.
    let mut together: Vec<f64> = Vec::new();
    for note in played.iter().filter(|note| note.on) {
        let keys = played.iter().filter(|other| other.on && other.beat == note.beat).count();
        if keys == patches.len() && !together.contains(&note.beat) {
            together.push(note.beat);
        }
    }
    assert_eq!(together, vec![0.0], "four polymeter lanes realign only at their lcm");

    // ---- the same phase after a scrub ----
    // Stopped first so the jump has nothing to flush, and then to a beat no
    // cycle divides: 97.37 is mid-step for all four lanes.
    rig.stop_and_assert_silent("polymeter, before the scrub");
    rig.locate(97.37);
    let (scrubbed, window) = rig.play_beats(4.0);
    let want: Vec<Note> = patches.iter().flat_map(|patch| patch.notes(window)).collect();
    assert_plays(
        scrubbed,
        want,
        "phase is anchored to song position 0, not to where playback started",
    );

    rig.stop_and_assert_silent("polymeter after a scrub");
}

/// Phase 3: long lanes — 64, 37 and 128 steps.
///
/// Three things a user needs to see, none of which a 16-step lane can
/// show: that growing past a fresh lane's capacity works at all (it is a
/// shape edit, `SeqModel::ensure_capacity`, and the capacity it lands on is
/// chunked); that a 37-step lane wraps mid-bar like any other polymeter; and
/// that shortening a lane **hides** its tail instead of erasing it —
/// including the step the user has to drag the length back out to hear again.
#[test]
fn long_lanes_grow_hide_and_give_their_steps_back() {
    let mut rig = Rig::new(4096);
    let ids = rig.lane_ids();
    let (long, odd, longest) = (ids[0], ids[1], ids[2]);

    // 64 steps of 1/16 is four bars; 37 is two bars and a quarter of one; 128
    // is eight. The steps are picked so that each lane's wrap is visible
    // inside the window played below.
    let long_patch = Patch {
        key: 36,
        rate: Rate::Sixteenth,
        length: 64,
        steps: &[(0, Step::default_on()), (37, Step::default_on()), (63, Step::default_on())],
    };
    let odd_patch = Patch {
        key: 38,
        rate: Rate::Sixteenth,
        length: 37,
        steps: &[(0, Step::default_on()), (36, Step::default_on())],
    };
    let longest_patch = Patch {
        key: 42,
        rate: Rate::Sixteenth,
        length: 128,
        steps: &[(0, Step::default_on()), (101, Step::default_on()), (127, Step::default_on())],
    };
    for (patch, id) in [(&long_patch, long), (&odd_patch, odd), (&longest_patch, longest)] {
        patch.write(&rig.model, 0, id);
    }

    // Capacity is allocated in chunks of 16, so a 37-step lane holds 48 and
    // the other two land exactly on their length.
    let data = rig.model.shared();
    for (pos, length) in [(0, 64), (1, 37), (2, 128)] {
        let lane_data = data.lane_data(0, pos).expect("a long lane");
        assert_eq!(lane_data.length(), length);
        assert_eq!(
            lane_data.capacity(),
            chunked_capacity(length as usize),
            "lane {pos} got a chunked capacity"
        );
    }
    drop(data);

    // Eight bars: the 128-step lane's whole cycle, three wraps of the
    // 37-step one, and two of the 64.
    let (played, window) = rig.play_beats(33.0);
    let want: Vec<Note> = [&long_patch, &odd_patch, &longest_patch]
        .iter()
        .flat_map(|patch| patch.notes(window))
        .collect();
    assert_plays(played, want, "long lanes wrap at their own length");

    // ---- the tail hidden, then given back ----
    let hidden = Patch { length: 100, ..longest_patch };
    assert!(rig.model.set_length(0, longest, 100));
    {
        let data = rig.model.shared();
        let lane_data = data.lane_data(0, 2).expect("the longest lane");
        assert_eq!(lane_data.capacity(), 128, "shortening keeps the capacity");
        assert!(lane_data.step(101).is_on(), "and the hidden steps themselves");
        assert!(lane_data.step(127).is_on());
    }
    // Beats 24..26 hold step 100 (a wrap of the shortened lane, which is its
    // step 0) and step 101 (hidden). Played from a stop so the jump has
    // nothing to flush.
    rig.stop_and_assert_silent("long lanes, before shortening");
    rig.locate(24.0);
    let (shortened, window) = rig.play_beats(2.0);
    let mut want = hidden.notes(window);
    want.extend(long_patch.notes(window));
    want.extend(odd_patch.notes(window));
    assert_plays(
        shortened,
        want,
        "steps past the new length are hidden, and the lane wraps at 100",
    );

    assert!(rig.model.set_length(0, longest, 128));
    rig.stop_and_assert_silent("long lanes, before restoring the length");
    rig.locate(24.0);
    let (restored, window) = rig.play_beats(2.0);
    let mut want = longest_patch.notes(window);
    want.extend(long_patch.notes(window));
    want.extend(odd_patch.notes(window));
    assert_plays(
        restored,
        want,
        "dragging the length back out gives the hidden steps back, unchanged",
    );

    rig.stop_and_assert_silent("long lanes");
}

/// Phase 4: what a step carries — ratchets, probability, velocity and pan.
///
/// The block here is 6000 samples, i.e. exactly a quarter beat, so a 1/16
/// step occurrence never straddles a block edge: that keeps the probability
/// assertion honest, because a ratchet burst cut by a block boundary is
/// rolled on both sides (a known open question).
#[test]
fn ratchets_probability_and_pan_reach_every_note() {
    let mut rig = Rig::new(6000);
    let ids = rig.lane_ids();
    let (burst, hard_left, never, maybe) = (ids[0], ids[1], ids[2], ids[3]);

    // A four-ratchet step at the top of every beat, panned hard right and
    // struck at velocity 100.
    let burst_patch = Patch {
        key: 36,
        rate: Rate::Sixteenth,
        length: 4,
        steps: &[(0, Step::pack(true, 100, Step::PAN_MAX, 4, 100))],
    };
    // A plain step, panned hard left at the lowest velocity there is.
    let left_patch = Patch {
        key: 38,
        rate: Rate::Quarter,
        length: 2,
        steps: &[(1, Step::pack(true, Step::VELOCITY_MIN, Step::PAN_MIN, 1, 100))],
    };
    // On, but at probability 0: never heard, and no randomness consumed.
    let never_patch = Patch {
        key: 42,
        rate: Rate::Sixteenth,
        length: 4,
        steps: &[(0, Step::pack(true, 90, Step::PAN_CENTRE, 1, 0))],
    };
    for (patch, id) in [(&burst_patch, burst), (&left_patch, hard_left), (&never_patch, never)] {
        patch.write(&rig.model, 0, id);
    }
    // The uncertain one: a four-ratchet step at a coin-flip probability, on
    // its own lane so its hits are identifiable by key.
    assert!(rig.model.set_length(0, maybe, 4));
    assert!(rig.model.set_step(0, maybe, 0, Step::pack(true, 64, Step::PAN_CENTRE, 4, 50)));

    // Sixteen beats, i.e. sixteen occurrences of each one-beat lane.
    let occurrences = 16;
    let (played, window) = rig.play_beats(occurrences as f64);

    // The deterministic three, exactly: the ratchet subdivisions and their
    // half-subdivision gates, the plain lane, and nothing at all from the
    // probability-0 one.
    let deterministic: Vec<Note> = played
        .iter()
        .copied()
        .filter(|note| note.key != KIT[3].1)
        .collect();
    let mut want = burst_patch.notes(window);
    want.extend(left_patch.notes(window));
    want.extend(never_patch.notes(window));
    assert_plays(deterministic, want, "ratchets and gates land on the subdivision");
    assert!(
        !played.iter().any(|note| note.key == never_patch.key),
        "a probability-0 step never fires"
    );
    // Four hits per beat, 0.0625 beats apart, each gate half a subdivision.
    let first_beat: Vec<Note> =
        sorted(played.iter().copied().filter(|note| note.beat < 0.25).collect());
    assert_eq!(
        first_beat,
        vec![
            Note { beat: 0.0, on: true, key: 36 },
            Note { beat: 0.03125, on: false, key: 36 },
            Note { beat: 0.0625, on: true, key: 36 },
            Note { beat: 0.09375, on: false, key: 36 },
            Note { beat: 0.125, on: true, key: 36 },
            Note { beat: 0.15625, on: false, key: 36 },
            Note { beat: 0.1875, on: true, key: 36 },
            Note { beat: 0.21875, on: false, key: 36 },
        ],
        "a ratchet of 4 is four hits on the subdivision, each gated at half of it"
    );

    // The uncertain one: rolled **once per occurrence**, so every beat
    // carries either the whole burst of four or nothing — and over sixteen
    // beats at probability 50 it does some of each.
    let mut fired = 0;
    for beat in 0..occurrences {
        let occurrence = BeatRange::new(beat as f64, beat as f64 + 1.0);
        let hits = played
            .iter()
            .filter(|note| note.on && note.key == 45 && occurrence.contains(note.beat))
            .count();
        assert!(matches!(hits, 0 | 4), "beat {beat} fired {hits} of a four-ratchet burst");
        fired += usize::from(hits == 4);
    }
    assert!(
        (1..occurrences).contains(&fired),
        "a probability-50 step fired {fired} of {occurrences} occurrences"
    );

    // ---- the payload each note-on carries ----
    // Read off the raw events this time: velocity and pan are what the shell
    // turns into a note-on plus a `PolyPan`, and nothing above looks at them.
    rig.locate(0.0);
    let ctx = rig.ctx();
    rig.block(&ctx);
    let payloads: Vec<(u8, f32, f32)> = rig
        .out
        .events()
        .iter()
        .filter_map(|timed| match timed.event {
            SeqEvent::NoteOn { key, velocity, pan, .. } => Some((key, velocity, pan)),
            SeqEvent::NoteOff { .. } => None,
        })
        .collect();
    assert!(
        payloads.iter().any(|&(key, velocity, pan)| key == 36
            && velocity == velocity_to_f32(100)
            && pan == 1.0),
        "the ratcheted lane's notes carry its velocity and hard-right pan: {payloads:?}"
    );
    assert_eq!(pan_to_f32(Step::PAN_MIN), -1.0, "hard left is -1.0");
    assert_eq!(pan_to_f32(Step::PAN_CENTRE), 0.0, "centre is exactly 0.0");

    rig.stop_and_assert_silent("ratchets, probability and pan");
}

/// Phase 5: `Pattern Select` driven from a modulator.
///
/// A Bitwig Steps modulator or LFO writes the parameter on the host's own
/// grid, which means a new value can arrive on *every* block — so this test
/// moves `patsel` through the parameter (the thing a modulator actually
/// moves) rather than handing the engine a pattern index.
///
/// Two halves:
///
/// 1. One switch, watched lane by lane: a 1/4 lane follows within a quarter
///    note while a 1/1 lane finishes its bar first (each lane switches at
///    its own next step boundary), and the pattern the editor lights up is
///    the one that has actually landed, not the one the parameter asks for.
/// 2. The sweep: a new pattern every block for sixteen beats, which is a
///    pattern change roughly every 0.085 beats — far faster than any user
///    could click. Nothing may hang, nothing may be dropped, and every note
///    must still land on a step boundary of the rate that produced it.
#[test]
fn a_modulator_sweeping_pattern_select_switches_lane_by_lane() {
    let mut rig = Rig::new(2048);
    let ids = rig.lane_ids();
    let (kick, pad) = (ids[0], ids[1]);

    // Eight banks a sweep can tell apart: in pattern p the kick strikes the
    // beats of one parity, and the pad — one step of 1/1, i.e. a four-beat
    // step with a two-beat gate — strikes only in the even-numbered banks.
    for pattern in 0..NUM_PATTERNS {
        let offset = pattern % 2;
        let steps: Vec<(usize, Step)> =
            (0..4).map(|i| (i * 2 + offset, Step::default_on())).collect();
        Patch { key: 36, rate: Rate::Quarter, length: 8, steps: &steps }
            .write(&rig.model, pattern, kick);
        let pad_steps: &[(usize, Step)] =
            if offset == 0 { &[(0, Step::default_on())] } else { &[] };
        Patch { key: 50, rate: Rate::Whole, length: 1, steps: pad_steps }
            .write(&rig.model, pattern, pad);
    }

    // ---- one switch, lane by lane ----
    // A beat and a half in: the kick struck at 0 and 2 is between steps, and
    // the pad struck at 0 is half-way through its four-beat step.
    let (opening, window) = rig.play_beats(1.5);
    assert!(opening.iter().any(|note| note.on && note.key == 50), "the pad struck at beat 0");
    assert_eq!(rig.engine.active_pattern(0), 0, "both lanes are on pattern 1");
    assert_eq!(rig.engine.active_pattern(1), 0);
    assert_eq!(rig.playhead.pattern(), Some(0), "and that is what the editor lights up");
    assert!(window.contains(1.0), "the window reached the kick's second step");

    rig.host_moves_patsel(2);
    // Past the kick's next 1/4 boundary (beat 2.0) but well short of the
    // pad's next 1/1 boundary (beat 4.0).
    rig.play_beats(1.0);
    assert_eq!(rig.engine.active_pattern(0), 1, "the 1/4 kick switched at its own next step");
    assert_eq!(rig.engine.active_pattern(1), 0, "the 1/1 pad is still finishing its step");
    assert_eq!(
        rig.playhead.pattern(),
        Some(0),
        "a pattern button lights when its switch lands, not when the parameter moves"
    );

    // Past beat 4, where the pad's step ends and it catches up.
    let (caught_up, window) = rig.play_beats(2.5);
    assert!(window.contains(4.0), "the window crossed the pad's step boundary");
    assert_eq!(rig.engine.active_pattern(1), 1, "the pad switched at its own boundary");
    assert_eq!(rig.playhead.pattern(), Some(1), "now the editor lights pattern 2");
    assert!(
        !caught_up.iter().any(|note| note.on && note.key == 50),
        "pattern 2 holds no pad step, so nothing struck at beat 4: {caught_up:#?}"
    );
    // The kick moved onto the odd beats, which is pattern 2's content.
    let kick_strikes: Vec<f64> =
        caught_up.iter().filter(|note| note.on && note.key == 36).map(|note| note.beat).collect();
    assert_eq!(kick_strikes, vec![3.0, 5.0], "the kick plays pattern 2's odd beats");

    rig.stop_and_assert_silent("one pattern switch");

    // ---- the sweep ----
    // A new pattern on every block, cycling 1..=8 the way a Steps modulator
    // steps through its own grid.
    rig.locate(0.0);
    let per_block = 2048.0 / SAMPLES_PER_BEAT;
    let blocks = (16.0 / per_block).ceil() as usize;
    let mut swept: Vec<Note> = Vec::new();
    for index in 0..blocks {
        rig.host_moves_patsel((index % NUM_PATTERNS) as i32 + 1);
        swept.extend(rig.play(1));
    }
    assert!(!swept.is_empty(), "the sweep played something");
    // Every rate in the patch is 1/4 or 1/1, and every gate half of that, so
    // a switch arriving mid-step can move *which* step plays but never where
    // one lands.
    for note in &swept {
        let on_a_grid = (note.beat * 2.0).fract() == 0.0;
        assert!(on_a_grid, "a swept note landed off the 1/4 grid and its gate: {note:?}");
    }
    // One pattern held for a full cycle of both lanes, and everything has
    // caught up — the state a modulator leaves behind when it stops.
    rig.host_moves_patsel(5);
    rig.play_beats(8.0);
    assert_eq!(rig.engine.active_pattern(0), 4, "the kick settled on the held pattern");
    assert_eq!(rig.engine.active_pattern(1), 4, "and so did the pad");
    assert_eq!(rig.playhead.pattern(), Some(4));

    rig.stop_and_assert_silent("a pattern-select sweep");
}

/// Phase 6: save, close, reopen — everything intact, hidden steps included.
///
/// The walkthrough's last step is the one that would lose a user's work, so
/// it is checked three ways: the snapshot is identical after a round trip
/// through JSON *and* through a fresh instance's `load_snapshot` (which is
/// what a host handing back project state does), the hidden steps are
/// spot-checked by hand in the reloaded data, and the reopened instance
/// plays the same notes the saved one did.
///
/// Every probability in the replayed part of the patch is 0 or 100, so the
/// comparison is exact: a step in `1..=99` would depend on the block layout.
/// One *is* saved — in the last
/// pattern bank, which nothing plays — because the field still has to
/// survive the file.
#[test]
fn the_walkthrough_patch_survives_save_close_and_reopen() {
    let mut rig = Rig::new(4096);
    let ids = rig.lane_ids();
    let (kick, snare, hat, perc) = (ids[0], ids[1], ids[2], ids[3]);

    // The patch the walkthrough builds, in the order its phases build it:
    // the lane identities first...
    assert!(rig.model.rename_lane(kick, "Kick 808"));
    assert!(rig.model.set_colour(kick, 0x12_34_56));
    assert!(rig.model.set_key(kick, 35));
    assert!(rig.model.set_channel(kick, 9));
    assert!(rig.model.set_mute(perc, true));
    let long = rig.model.add_lane("Long", 0x65_43_21, 51, 0).expect("room for a fifth lane");

    // ...then the content: polymeter, a ratcheted and panned step, a
    // probability-0 step, and a long lane whose tail is hidden.
    let patches: [(u32, Patch); 5] = [
        (
            kick,
            Patch {
                key: 35,
                rate: Rate::Sixteenth,
                length: 15,
                steps: &[
                    (0, Step::pack(true, 100, Step::PAN_MAX, 4, 100)),
                    (8, Step::pack(true, 90, Step::PAN_MIN, 1, 100)),
                    // Hidden past the 15-step length until a drag brings it
                    // back — and it has to survive the file either way.
                    (20, Step::pack(true, 7, Step::PAN_CENTRE, 8, 100)),
                ],
            },
        ),
        (
            snare,
            Patch {
                key: 38,
                rate: Rate::Eighth,
                length: 13,
                steps: &[(4, Step::default_on()), (9, Step::pack(true, 64, 40, 2, 100))],
            },
        ),
        (
            hat,
            Patch {
                key: 42,
                rate: Rate::SixteenthTriplet,
                length: 11,
                steps: &[(0, Step::default_on()), (5, Step::pack(true, 80, 90, 1, 0))],
            },
        ),
        (
            perc,
            Patch {
                key: 45,
                rate: Rate::Quarter,
                length: 4,
                steps: &[(0, Step::default_on())],
            },
        ),
        (
            long,
            Patch {
                key: 51,
                rate: Rate::Sixteenth,
                length: 100,
                steps: &[
                    (0, Step::default_on()),
                    (37, Step::default_on()),
                    (101, Step::default_on()),
                    (127, Step::default_on()),
                ],
            },
        ),
    ];
    for (id, patch) in &patches {
        patch.write(&rig.model, 0, *id);
    }
    // The long lane is written at capacity 128 and then shortened, so steps
    // 101 and 127 are hidden — exactly the steps a load must keep rather
    // than trim to the length.
    assert!(rig.model.set_length(0, long, 100));
    // The last bank is not empty, which is what proves all eight are saved
    // rather than only the one being played — and it is where the one
    // probability in `1..=99` lives.
    Patch {
        key: 42,
        rate: Rate::QuarterDotted,
        length: 6,
        steps: &[(2, Step::pack(true, 111, 20, 3, 55))],
    }
    .write(&rig.model, NUM_PATTERNS - 1, hat);

    // What the saved instance plays, over a window long enough to cross
    // every one of those lanes' cycles at least twice.
    let (before, window) = rig.play_beats(16.0);
    rig.stop_and_assert_silent("the walkthrough patch, before saving");

    // ---- save ----
    let saved = rig.model.snapshot();
    let file = serde_json::to_string(&saved).expect("a snapshot serialises");

    // ---- close and reopen: a fresh instance, then the host's state ----
    let reopened = Arc::new(SeqModel::new(state::default_data()));
    let loaded: SeqSnapshot = serde_json::from_str(&file).expect("the file parses");
    assert_eq!(loaded, saved, "the file is the snapshot");
    reopened.load_snapshot(loaded);
    assert_eq!(reopened.snapshot(), saved, "the reopened sequence is the saved one");

    // Spot-checked by hand as well, because an equality on two snapshots
    // would also pass if both had lost the same thing.
    {
        let data = reopened.shared();
        let lane = data.lane(0).expect("the kick survived");
        assert_eq!((lane.name.as_str(), lane.colour, lane.key(), lane.channel()), (
            "Kick 808", 0x12_34_56, 35, 9
        ));
        assert!(data.lane(3).expect("the perc lane").is_muted(), "mute survived");
        let kick_data = data.lane_data(0, 0).expect("the kick's content");
        assert_eq!((kick_data.length(), kick_data.rate()), (15, Rate::Sixteenth));
        assert_eq!(kick_data.step(0), Step::pack(true, 100, Step::PAN_MAX, 4, 100));
        assert_eq!(
            kick_data.step(20),
            Step::pack(true, 7, Step::PAN_CENTRE, 8, 100),
            "a step hidden past the length came back with the file"
        );
        let long_data = data.lane_data(0, 4).expect("the long lane's content");
        assert_eq!((long_data.length(), long_data.capacity()), (100, 128));
        assert!(long_data.step(101).is_on() && long_data.step(127).is_on(), "its hidden tail too");
        let last = data.lane_data(NUM_PATTERNS - 1, 2).expect("the last bank's hat");
        assert_eq!(last.step(2), Step::pack(true, 111, 20, 3, 55), "bank 8 survived as well");
    }

    // ---- and it plays the same ----
    let mut played_again = Rig::from_model(reopened, 4096);
    let (after, again) = played_again.play_beats(16.0);
    assert_eq!(again, window, "the same window, so the comparison is like for like");
    assert_plays(
        after,
        before,
        "the reopened project plays note for note what the saved one did",
    );
    played_again.stop_and_assert_silent("the reopened walkthrough patch");
}
