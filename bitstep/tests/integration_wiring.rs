//! The whole plugin as one wired instance, driven without a host — a check
//! that the real plugin factory builds an instance whose parts are wired.
//!
//! The unit tests either side of this one each prove their own layer:
//! `lib.rs` that a host transport maps onto a block and that a generated
//! event becomes the right pair of nice-plug events, `engine.rs` that the
//! block driver stitches and splits, `state.rs` that a snapshot survives
//! clamping. What none of them can show is that the *composition* is wired:
//! that the sequence the host persists is the sequence the audio thread
//! reads, that an edit made between two blocks reaches the second one, that
//! the `patsel` parameter actually moves the pattern being played, and that a
//! project loaded back through the persist field plays.
//!
//! So this test assembles exactly what `Bitstep::default()` assembles — one
//! `SeqModel`, params over it, an `Engine`, a `Playhead` and one reused
//! `BlockOutput` — and drives a scripted session through it: play, edit while
//! playing, switch patterns from the parameter, loop, stop, restart, and load
//! a saved project mid-session. Every phase's notes are written out in full.
//!
//! Display-free and host-free by construction: the only parts of the shell
//! left out are reading the host's `Transport` into a `BlockCtx` and handing
//! the generated events back to it, both of which are `lib.rs`'s own unit
//! tests. Everything from `BlockCtx` inwards is this one's.

use std::collections::BTreeMap;
use std::sync::Arc;

use nice_assert_no_alloc::assert_no_alloc;
use nice_plug::params::persist::PersistentField;
use nice_plug::params::{InternalParamMut, Params};
use nice_plug::prelude::Plugin;

use bitstep::engine::{BlockCtx, BlockOutput, Engine};
use bitstep::model::{MAX_LANES, NUM_PATTERNS, Playhead, SeqModel, Step};
use bitstep::sequencer::{BeatRange, BlockRanges, Rate, SeqEvent, TimedEvent};
use bitstep::state::{self, SeqSnapshot, StepSnapshot};
use bitstep::voices::FIRST_VOICE_ID;
use bitstep::{Bitstep, BitstepParams};

/// 120 BPM at 48 kHz, i.e. 24 000 samples per beat — every beat this test
/// writes down is an exact binary fraction, so the stitched block starts are
/// bit-identical and the expected sample offsets are whole numbers.
const TEMPO: f64 = 120.0;
const SAMPLE_RATE: f32 = 48_000.0;
const SAMPLES_PER_BEAT: u32 = 24_000;
/// Half a beat per block: short enough that gates routinely close in a later
/// block than the one that opened them, long enough that the session is a
/// readable number of blocks.
const BLOCK: usize = SAMPLES_PER_BEAT as usize / 2;

// ---- the instance under test ----

/// The plugin's composition, assembled the way `Bitstep::default()` assembles
/// it, plus the crude host that drives it.
///
/// Not `Bitstep` itself: its fields are private, and an integration test
/// reaching into them would be a unit test in the wrong file. What matters is
/// that this *is* the same composition, which
/// [`the_plugin_factory_builds_independently_wired_instances`] pins by
/// comparing the two instances' persisted state.
struct Session {
    params: BitstepParams,
    /// The one sequence every writer writes and the audio thread reads — the
    /// same `Arc` the params struct persists.
    model: Arc<SeqModel>,
    playhead: Arc<Playhead>,
    engine: Engine,
    /// Allocated once here, as the shell allocates it once per instance.
    out: BlockOutput,
    /// Where the host will report the next block from.
    pos: f64,
    /// The host's cycle region, when it is looping.
    loop_range: Option<(f64, f64)>,
}

impl Session {
    fn new() -> Session {
        let model = Arc::new(SeqModel::new(state::default_data()));
        Session {
            params: BitstepParams::new(Arc::clone(&model)),
            model,
            playhead: Arc::new(Playhead::new()),
            engine: Engine::new(),
            out: BlockOutput::new(),
            pos: 0.0,
            loop_range: None,
        }
    }

    /// The transport a host would report for the next block.
    fn ctx(&self) -> BlockCtx {
        let ctx = BlockCtx::new(self.pos, TEMPO, SAMPLE_RATE, BLOCK)
            .with_pattern_select(self.params.pattern_select());
        match self.loop_range {
            Some((start, end)) => ctx.with_loop(start, end),
            None => ctx,
        }
    }

    /// One block, exactly as `process()` runs one: the parameter read into a
    /// [`BlockCtx`], **one** `shared()` for the whole block, `run_block`, and
    /// the generated events read back out.
    ///
    /// Under the no-alloc guard, because that is the rule the real one lives
    /// by: a composition that allocates on the way into the engine would pass
    /// every unit test and still be unusable in a host.
    fn block(&mut self, ctx: &BlockCtx) -> Vec<TimedEvent> {
        let Session { model, playhead, engine, out, .. } = self;
        let playhead: &Playhead = playhead;
        assert_no_alloc(|| {
            let data = model.shared();
            engine.run_block(ctx, &data, playhead, out);
            // `data` is dropped in here deliberately: handing a retired
            // `SeqData` back to the collector is part of the audio thread's
            // work, and it has to be allocation-free too.
        });
        check_block(ctx, self.out.events());
        self.out.events().to_vec()
    }

    /// Plays `blocks` consecutive blocks, advancing the host's position the
    /// way the host itself would — which after a loop wrap is the wrapped
    /// position, not the block's own end.
    fn play(&mut self, blocks: usize) -> Vec<Vec<TimedEvent>> {
        (0..blocks)
            .map(|_| {
                let ctx = self.ctx();
                let events = self.block(&ctx);
                self.pos = ranges_of(&ctx).resume_beat();
                events
            })
            .collect()
    }

    /// The host relocating: the next block starts at `beats` with no
    /// continuity from the last one, which the engine must read as a jump.
    fn locate(&mut self, beats: f64) {
        self.pos = beats;
    }

    /// A block reported while the transport sits still. The position does not
    /// advance — a stopped host stays where it is, and keeps reporting it.
    fn stop(&mut self) -> Vec<TimedEvent> {
        let ctx = BlockCtx { playing: false, ..self.ctx() };
        self.block(&ctx)
    }
}

/// The timeline `ctx` covers, through the engine's own splitter.
///
/// Used for two things: where the host reports the next block from, and which
/// segment an event's sample offset should have been measured against. Both
/// go through [`BlockRanges`] rather than a second copy of the arithmetic — a
/// resume position even one ULP off the engine's would turn every block into
/// a jump and quietly stop testing the stitching path.
fn ranges_of(ctx: &BlockCtx) -> BlockRanges {
    let timing = ctx.timing();
    let range = BeatRange::from_span(ctx.pos_beats, timing.span_beats());
    BlockRanges::split(range, ctx.loop_region(), timing)
}

/// The invariants one block's output has to satisfy whatever the patch is:
///
/// * the host receives the events in ascending sample offset, with note-offs
///   before note-ons at equal offsets;
/// * every offset lands inside the buffer the block was handed;
/// * every offset is the one its beat maps to *within the segment it fell
///   in*, which after a loop wrap is not the one it would map to within the
///   block;
/// * a flush note-off — a stop, or a jump — goes out at offset 0 whatever
///   beat it is stamped with.
fn check_block(ctx: &BlockCtx, events: &[TimedEvent]) {
    let ranges = ranges_of(ctx);
    let mut previous = (0u32, 0u8);
    for timed in events {
        // Note-offs rank before note-ons, which is what makes the voice
        // table's retrigger rule audible rather than a stuck note.
        let order = (timed.offset, u8::from(timed.event.is_note_on()));
        assert!(order >= previous, "{timed:?} came after {previous:?}");
        previous = order;
        assert!(
            (timed.offset as usize) < ctx.num_samples,
            "offset {} is outside a {}-sample block",
            timed.offset,
            ctx.num_samples
        );

        let beat = timed.event.beat();
        match ranges.segments().iter().find(|segment| segment.range.contains(beat)) {
            Some(segment) => assert_eq!(timed.offset, segment.offset_of(beat), "{timed:?}"),
            None => assert_eq!(timed.offset, 0, "a flush goes out at offset 0: {timed:?}"),
        }
    }
    assert_eq!(ctx.num_samples, BLOCK, "the host always hands us the same buffer");
}

/// The host moving `patsel`, as an automation event or a pattern-button
/// gesture does.
///
/// `IntParam`'s setters are `unsafe` because only a wrapper may call them,
/// and for this test the wrapper is us — the same thing the editor's own fake
/// host does in `editor/mod.rs`. Going through the parameter rather than
/// handing the engine a pattern index is the whole point: `patsel` is the
/// wiring under test.
fn host_moves_patsel(params: &BitstepParams, pattern: i32) {
    // SAFETY: nothing else holds this parameter — no wrapper, no smoother, no
    // editor — so there is no concurrent reader for a value to tear under.
    let changed = unsafe { params.pattern_select._internal_set_plain_value(pattern) };
    assert!(changed, "patsel was already {pattern}");
    assert_eq!(params.pattern_select(), pattern as u8);
}

// ---- reading the output ----

/// One generated note as this test compares them: the exact beat it was
/// generated for, which way it went, and the lane's key and channel.
///
/// Sample offsets are checked separately ([`check_block`]) because they are
/// per block, while beats are absolute — and they are compared *exactly*,
/// since the stitching path is built to be bit-identical.
#[derive(Clone, Copy, PartialEq, Debug)]
struct Note {
    beat: f64,
    on: bool,
    key: u8,
    channel: u8,
}

const fn on(beat: f64, key: u8) -> Note {
    Note { beat, on: true, key, channel: 0 }
}

const fn off(beat: f64, key: u8) -> Note {
    Note { beat, on: false, key, channel: 0 }
}

/// The same on another MIDI channel — the added lane is the only one in this
/// session that does not send on channel 0.
const fn on_ch(beat: f64, key: u8, channel: u8) -> Note {
    Note { beat, on: true, key, channel }
}

const fn off_ch(beat: f64, key: u8, channel: u8) -> Note {
    Note { beat, on: false, key, channel }
}

/// Every note a run of blocks produced, in the order the host received them.
///
/// Not re-sorted: `EventBuf::sort` is a total order, so emission order within
/// a block is fully determined, and beats only increase from block to block.
fn notes(blocks: &[Vec<TimedEvent>]) -> Vec<Note> {
    blocks.iter().flatten().map(note_of).collect()
}

/// [`notes`] for a single block.
fn block_notes(events: &[TimedEvent]) -> Vec<Note> {
    events.iter().map(note_of).collect()
}

fn note_of(timed: &TimedEvent) -> Note {
    Note {
        beat: timed.event.beat(),
        on: timed.event.is_note_on(),
        key: timed.event.key(),
        channel: timed.event.channel(),
    }
}

/// Which step each live lane is showing a playhead on — what the GUI draws
/// from.
fn playing_steps(playhead: &Playhead) -> Vec<(usize, u32)> {
    (0..MAX_LANES).filter_map(|pos| playhead.step(pos).map(|step| (pos, step))).collect()
}

/// The published lane ids, in grid order — what the GUI edits through.
fn lane_ids(model: &SeqModel) -> Vec<u32> {
    let data = model.shared();
    (0..data.lane_count()).filter_map(|pos| data.lane(pos).map(|lane| lane.id)).collect()
}

// ---- the patches ----

/// The patch the scripted session plays, built through `SeqModel`'s writer
/// API because that is the path a GUI edit takes: a patch assembled as a
/// `SeqData` by hand would prove nothing about the writers the editor calls.
///
/// Pattern 1 is a four-on-the-floor bar — a 1/4 kick on beats 1 and 3, a 1/16
/// snare on 2 and 4, a 1/8 hat on every beat — plus a perc lane with content
/// that is muted, so the scan's audibility rule is in the session too.
/// Pattern 2 is the kick alone, on the two beats pattern 1 leaves empty, so a
/// switch between them is unmistakable in the output.
fn build_patch(model: &SeqModel, lanes: &[u32]) {
    let (kick, snare, hat, perc) = (lanes[0], lanes[1], lanes[2], lanes[3]);

    // A drum kit's keys, set here rather than taken from the plugin's
    // defaults, so the session below keeps hearing the same notes if those
    // change.
    for (lane, key) in [(kick, 36), (snare, 38), (hat, 42), (perc, 45)] {
        assert!(model.set_key(lane, key));
    }

    assert!(model.set_rate(0, kick, Rate::Quarter));
    assert!(model.set_length(0, kick, 4));
    for index in [0, 2] {
        assert!(model.set_step(0, kick, index, Step::default_on()));
    }
    // The snare plays 1/16 over 16 steps: three different rates in one
    // pattern is what makes the per-lane step grids worth checking. The
    // length is set, like the keys, so a change to the default length does
    // not move it.
    assert!(model.set_length(0, snare, 16));
    for index in [4, 12] {
        assert!(model.set_step(0, snare, index, Step::default_on()));
    }
    assert!(model.set_rate(0, hat, Rate::Eighth));
    assert!(model.set_length(0, hat, 8));
    for index in [0, 2, 4, 6] {
        assert!(model.set_step(0, hat, index, Step::default_on()));
    }
    // Muted, not emptied: mute is a playback rule applied by the scan every
    // block, not an edit to the steps.
    assert!(model.set_rate(0, perc, Rate::Quarter));
    assert!(model.set_length(0, perc, 4));
    assert!(model.set_step(0, perc, 0, Step::default_on()));
    assert!(model.set_mute(perc, true));

    assert!(model.set_rate(1, kick, Rate::Quarter));
    assert!(model.set_length(1, kick, 4));
    for index in [1, 3] {
        assert!(model.set_step(1, kick, index, Step::default_on()));
    }
}

/// The saved project the persist tests round-trip and the session loads: a
/// sequence no default could produce, and one whose playback is still
/// predictable to the note.
///
/// Every probability here is 0 or 100. A probability in `1..=99` would make
/// the output depend on where the host split the timeline (the generator is
/// drawn from in scan order, block by block), which is a property of the
/// sequencer, not something a wiring test should be asserting around.
fn a_project() -> SeqSnapshot {
    let model = SeqModel::new(state::default_data());
    let ids = lane_ids(&model);
    let (kick, snare, perc) = (ids[0], ids[1], ids[3]);

    // A lane renamed, recoloured and moved onto another instrument — the
    // first things anyone does to a fresh instance.
    assert!(model.rename_lane(kick, "Kick 808"));
    assert!(model.set_colour(kick, 0x12_34_56));
    assert!(model.set_key(kick, 35));
    assert!(model.set_channel(kick, 9));
    // Solo and mute, which between them leave the kick the only audible lane.
    assert!(model.set_solo(kick, true));
    assert!(model.set_mute(snare, true));

    // A polymeter lane with steps hidden past its length, and a ratcheted
    // first step carrying a velocity and a pan nothing defaults to.
    assert!(model.ensure_capacity(0, kick, 20));
    assert!(model.set_rate(0, kick, Rate::Eighth));
    assert!(model.set_length(0, kick, 12));
    assert!(model.set_step(0, kick, 0, Step::pack(true, 100, Step::PAN_MAX, 4, 100)));
    assert!(model.set_step(0, kick, 20, Step::pack(true, 7, Step::PAN_MIN, 8, 100)));
    // Content in a silenced lane: the save is not just what is audible.
    assert!(model.toggle_step(0, snare, 0));

    // The lane list is no longer the default one...
    assert!(model.remove_lane(perc));
    // ...and the last pattern bank is not empty, which is what proves all
    // eight are saved rather than only the one being played.
    let clap = model.add_lane("Clap", 0x65_43_21, 39, 3).expect("room for another lane");
    assert!(model.toggle_step(NUM_PATTERNS - 1, clap, 5));
    assert!(model.set_rate(NUM_PATTERNS - 1, clap, Rate::QuarterDotted));

    model.snapshot()
}

// ---- the tests ----

/// What a host gets when it instantiates the plugin: one automatable
/// parameter under its stable id, the default fresh sequence behind the
/// persist field, nothing sounding and nothing dropped — and a second
/// instance that shares none of it.
#[test]
fn the_plugin_factory_builds_independently_wired_instances() {
    let first = Bitstep::default();
    let second = Bitstep::default();

    assert_eq!((first.send_failures(), first.dropped_events()), (0, 0));

    let params = first.params();
    assert_eq!(
        params.param_map().into_iter().map(|(id, _, _)| id).collect::<Vec<String>>(),
        vec![String::from("patsel")]
    );

    // The fresh sequence is `state::default_data()` — and it is the
    // *persist field* that says so, i.e. the very model the instance plays
    // from (`lib.rs`'s own test pins that the two are one `Arc`).
    let fresh = params.serialize_fields();
    assert_eq!(fresh.keys().collect::<Vec<&String>>(), vec!["seq"]);
    let stored: SeqSnapshot = serde_json::from_str(&fresh["seq"]).expect("the blob is JSON");
    assert_eq!(stored, SeqSnapshot::from_data(&state::default_data()));
    assert_eq!(
        stored.lanes.iter().map(|lane| lane.name.as_str()).collect::<Vec<&str>>(),
        ["Lane 1", "Lane 2", "Lane 3", "Lane 4", "Lane 5", "Lane 6", "Lane 7", "Lane 8"]
    );

    // Two instances in one project share nothing: a load into one leaves the
    // other on its defaults.
    let blob = serde_json::to_string(&a_project()).expect("a snapshot serialises");
    params.deserialize_fields(&BTreeMap::from([(String::from("seq"), blob)]));
    assert_ne!(params.serialize_fields(), fresh);
    assert_eq!(second.params().serialize_fields(), fresh);

    // ...and the harness the session test drives composes the same instance
    // this factory does, rather than a composition of its own.
    assert_eq!(Session::new().params.serialize_fields(), fresh);
}

/// The host's project round trip through `#[persist = "seq"]`: out as the JSON
/// blob a project file stores, back in on a fresh instance, identical.
///
/// Driven through the derive-generated `serialize_fields` /
/// `deserialize_fields` — the functions the wrapper calls — rather than
/// through `PersistentField`'s methods by hand, which is what the session
/// test uses.
#[test]
fn a_non_trivial_project_round_trips_through_the_persist_field() {
    let project = a_project();

    // Non-trivial in the ways the format has to carry: a lane list that is
    // not the default one, a polymeter length, steps hidden past that length,
    // per-step pan/ratchet/velocity, mute and solo, and content in the last
    // of the eight pattern banks.
    assert_eq!(project.version, state::CURRENT_VERSION);
    assert_eq!(
        project.lanes.iter().map(|lane| lane.name.as_str()).collect::<Vec<&str>>(),
        ["Kick 808", "Lane 2", "Lane 3", "Lane 5", "Lane 6", "Lane 7", "Lane 8", "Clap"]
    );
    assert_eq!((project.lanes[0].key, project.lanes[0].channel), (35, 9));
    assert_eq!(project.lanes[0].colour, 0x12_34_56);
    assert!(project.lanes[0].solo && project.lanes[1].mute);
    assert_eq!(project.patterns.len(), NUM_PATTERNS, "the last pattern has notes");

    let kick = &project.patterns[0].lanes[0];
    assert_eq!((kick.length, kick.rate), (12, Rate::Eighth.to_u8()));
    assert_eq!(
        kick.steps[0],
        StepSnapshot {
            on: true,
            velocity: 100,
            pan: Step::PAN_MAX,
            ratchet: 4,
            probability: 100,
            pitch: 0,
        }
    );
    assert!(kick.steps.len() > 20, "the lane's full capacity is stored");
    assert!(kick.steps[20].on, "a step hidden past `length` is stored too");
    let clap = project.lanes.len() - 1;
    assert!(project.patterns[NUM_PATTERNS - 1].lanes[clap].steps[5].on, "the last bank is saved");

    // Out of one instance...
    let saving = Bitstep::default();
    let blob = serde_json::to_string(&project).expect("a snapshot serialises");
    saving.params().deserialize_fields(&BTreeMap::from([(String::from("seq"), blob)]));
    let saved = saving.params().serialize_fields();
    assert_eq!(
        serde_json::from_str::<SeqSnapshot>(&saved["seq"]).expect("the blob is JSON"),
        project,
        "what the host stores is what the instance holds"
    );

    // ...and into another, with nothing lost on either leg.
    let loading = Bitstep::default();
    loading.params().deserialize_fields(&saved);
    assert_eq!(loading.params().serialize_fields(), saved);
}

/// The session: play, edit while playing, switch patterns from the parameter,
/// loop, stop, restart, and load a project mid-flight — with every note the
/// instance produces written out.
///
/// One long test on purpose. Each phase depends on the state the last one
/// left behind (what is sounding, where the stitch is, which pattern each
/// lane reads), and that carried-over state is most of what a wiring test has
/// to check: a phase started from a fresh engine would be an `engine.rs` unit
/// test with extra steps.
#[test]
fn a_scripted_session_plays_edits_loops_and_stops() {
    let mut session = Session::new();
    let model = Arc::clone(&session.model);
    let lanes = lane_ids(&model);
    build_patch(&model, &lanes);
    // Only the two lanes phase 2 edits are named here; the rest are
    // `build_patch`'s business.
    let (snare, hat) = (lanes[1], lanes[2]);

    // ---- phase 1: play from the top, beats [0, 4) ----
    let played = session.play(8);
    assert_eq!(
        notes(&played),
        vec![
            on(0.0, 36), on(0.0, 42), off(0.25, 42), off(0.5, 36),
            on(1.0, 38), on(1.0, 42), off(1.125, 38), off(1.25, 42),
            on(2.0, 36), on(2.0, 42), off(2.25, 42), off(2.5, 36),
            on(3.0, 38), on(3.0, 42), off(3.125, 38), off(3.25, 42),
        ],
        "one bar of the pattern, every gate closed in the block that holds it"
    );
    // The downbeat is the block's first sample, and its 1/8 hat gate closes
    // halfway through it — the one hand-written offset check of the phase.
    assert_eq!(played[0].iter().map(|timed| timed.offset).collect::<Vec<u32>>(), [0, 0, 6_000]);
    assert!(played[3].is_empty(), "nothing happens in [1.5, 2.0)");
    assert_eq!(session.engine.prev_end(), Some(4.0), "four beats, stitched throughout");
    assert!(session.engine.voices().is_empty(), "nothing is left sounding");

    // The GUI's feedback: every lane on the last step of its own grid (a 1/4
    // lane of 4, a 1/16 lane of 16, a 1/8 lane of 8), and the pattern being
    // played is the one `patsel` names.
    // Lanes 4–7 are the untouched defaults: 1/16 over 32, halfway through.
    assert_eq!(
        playing_steps(&session.playhead),
        vec![(0, 3), (1, 15), (2, 7), (3, 3), (4, 15), (5, 15), (6, 15), (7, 15)]
    );
    assert_eq!(session.playhead.pattern(), Some(0));

    // ---- phase 2: edit while playing, beats [4, 8) ----
    // Three kinds of edit between two blocks, all of which have to be in the
    // next one: a value edit (a step drawn in), a shape edit (a lane grown
    // past its capacity) and a structural one (a lane added).
    assert!(model.toggle_step(0, snare, 8));
    assert!(model.ensure_capacity(0, hat, 20));
    // Polymeter: ten 1/8 steps is a five-beat lane against everyone else's
    // four, so the hat's own cycle now drifts against the bar.
    assert!(model.set_length(0, hat, 10));
    let clap = model.add_lane("Clap", 0x65_43_21, 39, 3).expect("room for a fifth lane");
    assert!(model.set_length(0, clap, 16));
    assert!(model.set_step(0, clap, 8, Step::default_on()));

    let edited = session.play(8);
    assert_eq!(
        notes(&edited),
        vec![
            on(4.0, 36), off(4.5, 36),
            on(5.0, 38), on(5.0, 42), off(5.125, 38), off(5.25, 42),
            on(6.0, 36), on(6.0, 38), on_ch(6.0, 39, 3), on(6.0, 42),
            off(6.125, 38), off_ch(6.125, 39, 3), off(6.25, 42), off(6.5, 36),
            on(7.0, 38), on(7.0, 42), off(7.125, 38), off(7.25, 42),
        ],
        "the drawn step, the added lane on its own channel, and the hat on its new cycle"
    );
    // The hat's step 8 is the one the five-beat cycle lands on at beat 4, and
    // it is off — had the length edit not reached the engine, beat 4 would
    // have been its step 0 and sounded.
    assert_eq!(
        playing_steps(&session.playhead),
        vec![(0, 3), (1, 15), (2, 5), (3, 3), (4, 31), (5, 31), (6, 31), (7, 31), (8, 15)]
    );

    // ---- phase 3: the host moves `patsel`, beats [8, 12) ----
    host_moves_patsel(&session.params, 2);
    let switched = session.play(8);
    assert_eq!(
        notes(&switched),
        vec![on(9.0, 36), off(9.5, 36), on(11.0, 36), off(11.5, 36)],
        "pattern 2's kick alone, on the beats pattern 1 left empty"
    );
    // Beat 8 is a step boundary for every lane, so the switch landed there
    // and the GUI is already showing the new bank: a pattern change waits
    // for each lane's next step boundary.
    assert!(switched[0].is_empty(), "the switch itself makes no sound");
    assert_eq!(session.playhead.pattern(), Some(1));
    assert_eq!(session.engine.active_pattern(0), 1);

    // ---- phase 4: the host loops, wrapping inside a block ----
    host_moves_patsel(&session.params, 1);
    session.loop_range = Some((0.0, 2.0));
    // A relocation, which the engine reads as a jump: nothing is mid-step
    // across one, so the pattern change takes effect immediately rather than
    // waiting for a boundary.
    session.locate(1.75);

    let looped = session.play(3);
    assert_eq!(
        notes(&looped),
        vec![
            on(0.0, 36), on(0.0, 42),
            off(0.25, 42), off(0.5, 36),
            on(1.0, 38), on(1.0, 42), off(1.125, 38),
        ],
        "the block that wrapped carried the loop's first beat, not the song's"
    );
    // The wrap: [1.75, 2.0) had nothing in it, and the quarter beat after the
    // loop point starts a quarter beat (6 000 samples) into the buffer.
    assert_eq!(looped[0].iter().map(|timed| timed.offset).collect::<Vec<u32>>(), [6_000, 6_000]);
    // ...and the host's next block starts from the wrapped position, which is
    // where the engine expects to stitch.
    assert_eq!(session.engine.prev_end(), Some(1.25));
    // The GUI follows the piece *after* the wrap, so the kick is back on its
    // first step rather than its last.
    assert_eq!(session.playhead.step(0), Some(1));

    // ---- phase 5: stop mid-note ----
    // The hat struck at beat 1 is still sounding: its gate closes at 1.25,
    // which is exactly where the transport stopped.
    assert_eq!(session.engine.voices().len(), 1);
    let stopped = session.stop();
    assert_eq!(
        block_notes(&stopped),
        vec![off(1.25, 42)],
        "what was sounding is released, stamped where playback broke off"
    );
    assert_eq!(stopped[0].offset, 0, "a flush goes out at the top of the block");
    assert!(session.engine.voices().is_empty(), "no stuck notes");
    assert_eq!(session.engine.prev_end(), None, "continuity is forgotten");
    assert_eq!(session.playhead.pattern(), None);
    assert!(playing_steps(&session.playhead).is_empty(), "the GUI shows nothing playing");
    assert!(session.stop().is_empty(), "a second stopped block has nothing left to release");

    // ---- phase 6: play again ----
    session.loop_range = None;
    session.locate(0.0);
    assert_eq!(
        notes(&session.play(1)),
        vec![on(0.0, 36), on(0.0, 42), off(0.25, 42)],
        "the pattern restarts from its first step, nothing doubled"
    );

    // ---- phase 7: the host loads a project mid-session ----
    // Through `PersistentField::set` — the method the wrapper calls when the
    // user reopens a project — onto the model the audio thread is reading.
    let project = a_project();
    PersistentField::set(&session.params.seq, project.clone());
    assert_eq!(session.model.snapshot(), project, "the load landed on the live model");
    assert_eq!(session.params.seq.map(Clone::clone), project);

    // The kick struck before the load is still sounding, and it is released
    // on time even though the lane that struck it now plays another key
    // entirely: the voice table owns the note, not the lane.
    assert_eq!(
        notes(&session.play(1)),
        vec![off(0.5, 36)],
        "the old sequence's note ends where it was always going to"
    );

    // ...and the loaded project plays: one soloed lane (the muted and the
    // un-soloed ones silent), on its own key and channel, its first step
    // ratcheted four times inside a single block.
    session.locate(0.0);
    let burst = session.play(1).remove(0);
    assert_eq!(
        block_notes(&burst),
        vec![
            on_ch(0.0, 35, 9), off_ch(0.0625, 35, 9),
            on_ch(0.125, 35, 9), off_ch(0.1875, 35, 9),
            on_ch(0.25, 35, 9), off_ch(0.3125, 35, 9),
            on_ch(0.375, 35, 9), off_ch(0.4375, 35, 9),
        ],
        "four ratchets of a 1/8 step, each gated to half its subdivision"
    );
    // Hand-computed from 24 000 samples per beat: the ratchets fall every
    // 0.125 beats and each gate closes 0.0625 beats later.
    assert_eq!(
        burst.iter().map(|timed| timed.offset).collect::<Vec<u32>>(),
        [0, 1_500, 3_000, 4_500, 6_000, 7_500, 9_000, 10_500]
    );

    // The payload a host receives, end to end from the saved step's packed
    // bits: velocity 100 of 127, hard right, on the lane's own channel, and
    // retuned to the sample root as a fresh lane is a sample pad.
    let first = burst[0].event;
    let expected = SeqEvent::note_on(0.0, 0, 35, 9, 100, Step::PAN_MAX, first.voice_id());
    assert_eq!(first, expected.with_sample_pad(true));
    assert!(first.voice_id() >= FIRST_VOICE_ID, "generated notes have their own id range");
    // Each ratchet's note-off carries its own note-on's voice id, which is
    // what pairs them in a host when four notes share one key.
    for pair in burst.chunks(2) {
        assert_eq!(pair[0].event.voice_id(), pair[1].event.voice_id());
    }
    let mut ids: Vec<i32> = burst.iter().map(|timed| timed.event.voice_id()).collect();
    ids.dedup();
    assert_eq!(ids.len(), 4, "four separate voices, not one re-used");

    // Nothing leaked over the whole session: with the last `shared()` handle
    // gone, the only `SeqData` basedrop still holds is the published one.
    session.model.collect();
    assert_eq!(session.model.live_allocs(), 1, "retired SeqData piled up");
}
