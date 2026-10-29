//! bitstep — a notes-only CLAP step sequencer.
//!
//! This module is the nice-plug glue only: plugin declaration, host
//! parameters, and the per-block `process()` entry point. All sequencing
//! logic lives in the pure modules (`model`, `sequencer`, `voices`, `state`)
//! and in `engine`, which is driven by unit tests without a host.

pub mod editor;
pub mod engine;
pub mod model;
pub mod sequencer;
pub mod state;
pub mod voices;

use std::sync::Arc;

use nice_plug::midi::{Channel, Key, VoiceID};
use nice_plug::params::persist::PersistentField;
use nice_plug::prelude::*;
use nice_plug_egui::{EguiEditor, EguiEditorState, RepaintNotifier};

use crate::editor::BitstepEditor;
use crate::engine::{BlockCtx, BlockOutput, Engine};
use crate::model::{Playhead, SeqModel};
use crate::sequencer::{SeqEvent, TimedEvent};
use crate::state::SeqSnapshot;

/// The whole sequence as a persistent field, saved beside `patsel` in the
/// host's project.
///
/// A newtype around the shared [`SeqModel`] rather than an impl on
/// `Arc<SeqModel>` directly: the orphan rule forbids that, and this is also
/// where the two worlds meet — `model.rs` and `state.rs` import no nice-plug
/// types, so the trait that hands their snapshots to the framework belongs
/// here in the shell.
///
/// The wrapped `Arc` is the same one [`Bitstep::model`] reads every block, so
/// a load through [`PersistentField::set`] is visible to the audio thread at
/// its next `shared()` — there is one sequence, not a saved copy of one.
pub struct SeqModelField(Arc<SeqModel>);

impl SeqModelField {
    /// Wraps the model the rest of the plugin already shares.
    pub fn new(model: Arc<SeqModel>) -> SeqModelField {
        SeqModelField(model)
    }

    /// The model behind the field, for the GUI and for tests.
    pub fn model(&self) -> &Arc<SeqModel> {
        &self.0
    }
}

/// Both directions are main-thread work that allocates freely; neither is
/// reachable from `process()`.
impl PersistentField<'_, SeqSnapshot> for SeqModelField {
    /// The host handing back a saved project. A shape edit through the writer
    /// mutex, so it can neither interleave with a GUI edit nor tear a block
    /// the audio thread is already generating. Every load rule — caps,
    /// clamping, pattern realignment — is applied by `SeqSnapshot::into_data`
    /// on the way in, so a hostile project file cannot panic the host.
    fn set(&self, new_value: SeqSnapshot) {
        self.0.load_snapshot(new_value);
    }

    /// The host asking for state: read the live atomics into a snapshot and
    /// let the framework serialise it.
    fn map<F, R>(&self, f: F) -> R
    where
        F: Fn(&SeqSnapshot) -> R,
    {
        f(&self.0.snapshot())
    }
}

/// The plugin's host-visible parameters.
///
/// CLAP parameter lists are fixed-size, so with unlimited lanes the only
/// per-lane control is the GUI; v1 exposes exactly one host parameter.
/// Post-v1 "Macros" are out of scope — do not scaffold them here.
#[derive(Params)]
pub struct BitstepParams {
    /// Which pattern bank the lanes read from, one-based. A change takes
    /// effect per lane at that lane's next step boundary, not immediately.
    /// In song mode the song picks the pattern, and this only picks the one
    /// the editor shows.
    #[id = "patsel"]
    pub pattern_select: IntParam,
    /// The sequence itself. Not a parameter — a persistent field, which the
    /// host stores as an opaque blob beside the parameter values and hands
    /// straight back on load.
    #[persist = "seq"]
    pub seq: SeqModelField,
}

impl BitstepParams {
    /// Parameters sharing `model` with the rest of the plugin.
    ///
    /// There is deliberately no `Default`: a params struct built around a
    /// model nobody else holds would save and load a sequence the audio
    /// thread never sees, and nothing would point out the mistake.
    pub fn new(model: Arc<SeqModel>) -> BitstepParams {
        BitstepParams {
            pattern_select: IntParam::new(
                "Pattern Select",
                i32::from(BlockCtx::PATTERN_SELECT_MIN),
                IntRange::Linear {
                    min: i32::from(BlockCtx::PATTERN_SELECT_MIN),
                    max: i32::from(BlockCtx::PATTERN_SELECT_MAX),
                },
            ),
            seq: SeqModelField::new(model),
        }
    }
}

impl BitstepParams {
    /// `patsel` as the engine wants it: a `u8` inside `1..=NUM_PATTERNS`.
    ///
    /// nice-plug clamps the parameter whenever a host drives it through a
    /// normalized value, but the plain `i32` is what the parameter actually
    /// stores, so the shell clamps here rather than trusting it — an `as u8`
    /// cast would quietly wrap a nonsense value into a valid-looking pattern.
    pub fn pattern_select(&self) -> u8 {
        clamp_pattern_select(self.pattern_select.value())
    }
}

impl Bitstep {
    /// Events the host's output buffer refused since this instance was
    /// created — a full buffer, or a host with no buffer at all. Diagnostic
    /// only: the events are gone either way.
    pub fn send_failures(&self) -> u32 {
        self.send_failures
    }

    /// Generated events that did not fit in a block's output buffer. Reaching
    /// this needs an absurd patch ([`BlockOutput::CAPACITY`] events in one
    /// block), so a non-zero count means the patch, not the host.
    pub fn dropped_events(&self) -> u32 {
        self.dropped_events
    }
}

/// Folds any `i32` into the `patsel` range, saturating at both ends.
fn clamp_pattern_select(raw: i32) -> u8 {
    raw.clamp(i32::from(BlockCtx::PATTERN_SELECT_MIN), i32::from(BlockCtx::PATTERN_SELECT_MAX))
        as u8
}

/// Reads the host's transport into the engine's view of the block.
///
/// `num_samples` comes from the buffer rather than from the transport, since
/// that is what the block's events are timed against.
///
/// The engine cannot place a single step without a position *and* a tempo, so
/// a transport missing either of them — a host that reports no position while
/// the transport sits still, or a free-running one with no tempo at all —
/// reads as stopped, which releases whatever is sounding and leaves the
/// host's own input the only thing this block forwards. A missing loop range
/// is not that case: it simply means the host is not looping.
///
/// Nothing here validates the numbers themselves — a nonsense tempo or
/// position is [`BlockCtx`]'s business, and it resolves them to "nothing to
/// sequence" rather than to a panic.
fn block_ctx(transport: &Transport, num_samples: usize, pattern_select: u8) -> BlockCtx {
    // The `pos_beats()`/`loop_range_beats()` accessors, not the raw fields:
    // CLAP hosts are free to report the position in samples or seconds only,
    // and these reconstruct beats from whichever of the three arrived.
    let (Some(pos_beats), Some(tempo)) = (transport.pos_beats(), transport.tempo) else {
        return BlockCtx::STOPPED.with_pattern_select(pattern_select);
    };

    let mut ctx = BlockCtx {
        playing: transport.playing,
        ..BlockCtx::new(pos_beats, tempo, transport.sample_rate, num_samples)
            .with_pattern_select(pattern_select)
    };
    // Song mode's bar length. A host that sends no signature plays 4/4.
    if let (Some(numerator), Some(denominator)) =
        (transport.time_sig_numerator, transport.time_sig_denominator)
    {
        ctx = ctx.with_time_signature(numerator, denominator);
    }
    match transport.loop_range_beats() {
        Some((start, end)) => ctx.with_loop(start, end),
        None => ctx,
    }
}

/// The most host events one generated event turns into: the note itself, a
/// `PolyPan`, a `PolyTuning` and a `PolyVolume`.
const MAX_EVENTS_PER_NOTE: usize = 4;

/// One generated event as the host's note events: the note itself, then for a
/// note-on the `PolyPan` expression that carries the step's pan and, when the
/// step has a pitch, the `PolyTuning` expression that carries it.
///
/// The expressions ride on separate events because that is how CLAP
/// expresses them — `NoteOn` has no pan or tuning field — and they are
/// emitted right after their note-on, at the same timing and voice id, so a
/// host reading the stream in order has them before the voice makes a sound.
/// Tuning is in semitones and leaves the key alone, so a pitched step still
/// lands on its own Drum Machine pad; the pad's instrument decides whether it
/// follows the expression. An ignored expression costs nothing.
///
/// Pan is behind the `pan-expression` feature, which is **in `default`**: it
/// sends each step's pan as a per-note pan expression, and a build without it
/// sends no pan and draws no pan control, while the step's pan bits stay in
/// the model and in the saved project either way.
///
/// Note-offs carry velocity 0: the variant demands a velocity, and a
/// release velocity is not something a step sequencer has an opinion about.
///
/// Returns a fixed array and how many of it to send, so this stays
/// allocation-free on the audio thread.
fn note_events(timed: TimedEvent) -> ([PluginNoteEvent<Bitstep>; MAX_EVENTS_PER_NOTE], usize) {
    let TimedEvent { offset: timing, event, .. } = timed;
    // Every generated note carries its own voice id, which is what pairs the
    // note-off — and the expressions — with the right note-on when two lanes
    // share a key.
    let voice_id = VoiceID::ID(event.voice_id());
    let channel = Channel::Number(event.channel());
    let key = Key::Number(event.key());

    match event {
        SeqEvent::NoteOn { velocity, pan, volume_db, .. } => {
            let note = NoteEvent::NoteOn { timing, voice_id, channel, key, velocity };
            let mut events = [note; MAX_EVENTS_PER_NOTE];
            let mut count = 1;
            #[cfg(feature = "pan-expression")]
            {
                events[count] = NoteEvent::PolyPan { timing, voice_id, channel, key, pan };
                count += 1;
            }
            // Without the feature the pan is read off the step and dropped
            // here, which is the whole of the difference: the bits are still
            // stored, still saved, and still there if it is turned back on.
            #[cfg(not(feature = "pan-expression"))]
            let _ = pan;
            // Only a pitched step (or a sample pad off its root) sends
            // tuning: a fresh voice starts untuned, so a zero would be one
            // more event saying nothing.
            let tuning = event.tuning();
            if tuning != 0 {
                // At most ±127, so the conversion is exact.
                let tuning = tuning as f32;
                events[count] = NoteEvent::PolyTuning { timing, voice_id, channel, key, tuning };
                count += 1;
            }
            // The lane's volume, likewise only when it is not 0 dB.
            if volume_db != 0 {
                let gain = db_to_gain(volume_db);
                events[count] = NoteEvent::PolyVolume { timing, voice_id, channel, key, gain };
                count += 1;
            }
            (events, count)
        }
        SeqEvent::NoteOff { .. } => {
            let off = NoteEvent::NoteOff { timing, voice_id, channel, key, velocity: 0.0 };
            ([off; MAX_EVENTS_PER_NOTE], 1)
        }
    }
}

/// A lane volume in dB as the linear gain the CLAP volume expression takes
/// (1.0 = unchanged; +12 dB is just under its ceiling of 4).
fn db_to_gain(db: i8) -> f32 {
    10f32.powf(f32::from(db) / 20.0)
}

pub struct Bitstep {
    params: Arc<BitstepParams>,
    /// The sequence itself, shared with every writer (GUI, state load) and
    /// read by the audio thread through one `shared()` per block. A fresh
    /// instance starts from `state::default_data`; a host state load replaces
    /// it wholesale.
    model: Arc<SeqModel>,
    /// Where playback is, for the GUI to draw. Written here, read there.
    playhead: Arc<Playhead>,
    /// Per-block driver state: stitch position, sounding voices, pattern
    /// switching, probability rolls.
    engine: Engine,
    /// The block's generated events. Owned by the shell and reused every
    /// block so that `process()` never allocates one.
    out: BlockOutput,
    /// Events the host's output buffer refused, and events that did not fit
    /// in `out`. Both are counted rather than reacted to: dropping a note is
    /// the only real-time-safe answer, and the counters are what a debug
    /// build complains about.
    send_failures: u32,
    dropped_events: u32,
    /// The editor window's size, kept here rather than inside the editor
    /// because the host may close and reopen the window many times over one
    /// instance and the size should survive that.
    editor_state: Arc<EguiEditorState>,
    /// The editor's wake-up handle. Lives alongside `editor_state` for the
    /// same reason, and is cheap to clone (one `Arc<AtomicBool>`).
    repaint_notifier: RepaintNotifier,
}

impl Default for Bitstep {
    fn default() -> Self {
        // One model, two handles: the params struct persists it, `process()`
        // reads it. Cloning the `Arc` here is what keeps a host state load
        // and the audio thread looking at the same sequence.
        let model = Arc::new(SeqModel::new(state::default_data()));
        Self {
            params: Arc::new(BitstepParams::new(Arc::clone(&model))),
            model,
            playhead: Arc::new(Playhead::new()),
            engine: Engine::new(),
            out: BlockOutput::new(),
            send_failures: 0,
            dropped_events: 0,
            editor_state: editor::default_state(),
            repaint_notifier: RepaintNotifier::new(),
        }
    }
}

impl Plugin for Bitstep {
    const NAME: &'static str = "bitstep";
    const VENDOR: &'static str = "pywkt";
    const URL: &'static str = "https://github.com/pywkt/bitstep";
    const EMAIL: &'static str = "bitstep@pywkt.com";
    const VERSION: &'static str = env!("CARGO_PKG_VERSION");

    /// No audio at all: this plugin consumes and emits note events only.
    const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = &[];

    /// `MidiCCs`, not `Basic`, on both sides: with `Basic`, nice-plug drops
    /// incoming CC, pitch bend, channel pressure and program change before
    /// `process()` sees them, and the host stops passing input through once
    /// the plugin declares note output — so a sustain pedal or mod wheel
    /// played into the track would never reach the instrument behind it.
    const MIDI_INPUT: MidiConfig = MidiConfig::MidiCCs;
    const MIDI_OUTPUT: MidiConfig = MidiConfig::MidiCCs;

    type Editor = EguiEditor<BitstepEditor>;
    type SysExMessage = ();
    type BackgroundTask = ();

    fn params(&self) -> Arc<dyn Params> {
        self.params.clone()
    }

    /// The egui editor, over the same handles `process()` uses.
    ///
    /// Assembling it opens no window — the host does that later through
    /// `Editor::spawn()` — so this is safe to call with no display, which is
    /// how it is tested here. The background-task executor goes unused: the
    /// GUI reads the model directly and has nothing to hand off
    /// (`BackgroundTask = ()`).
    fn editor(&mut self, _async_executor: AsyncExecutor<Self>) -> Option<Self::Editor> {
        editor::create(
            Arc::clone(&self.params),
            Arc::clone(&self.model),
            Arc::clone(&self.playhead),
            Arc::clone(&self.editor_state),
            self.repaint_notifier.clone(),
        )
    }

    /// Clears everything that outlives a block. Called from the audio thread,
    /// so it only sets the flag the next `process()` turns into note-offs —
    /// events exist nowhere else.
    fn reset(&mut self) {
        self.engine.request_flush();
    }

    /// The host is putting the plugin to sleep. Same story as
    /// [`Plugin::reset`]: ask, and let the next block (if there is one)
    /// release the notes.
    fn deactivate(&mut self) {
        self.engine.request_flush();
    }

    /// One block: forward the host's events, generate ours, send them.
    ///
    /// Thin on purpose — the timeline logic is [`Engine::run_block`]'s, and
    /// it is unit-tested without a host. What is left here is the shell's
    /// own: reading the transport, taking exactly one `SeqData` snapshot for
    /// the whole block, and turning generated events into the host's.
    ///
    /// Allocation-free throughout, which the `assert_process_allocs` feature
    /// enforces in debug and test builds: `shared()` is two atomic
    /// increments, [`Self::out`] was allocated at instantiation, and
    /// conversion is by value.
    fn process(
        &mut self,
        buffer: &mut Buffer,
        _aux: &mut AuxiliaryBuffers,
        context: &mut impl ProcessContext<Self>,
    ) -> ProcessStatus {
        // Declaring note output makes the host stop passing input through on
        // its own, so everything that comes in goes straight back out
        // unchanged — this is a note *effect*, and anything played into it
        // must still reach the instrument behind it.
        let mut refused: u32 = 0;
        while let Some(event) = context.next_event() {
            if context.try_send_event(event).is_err() {
                refused = refused.saturating_add(1);
            }
        }

        let ctx = block_ctx(context.transport(), buffer.samples(), self.params.pattern_select());
        // One `get()` per block: the GUI may publish a replacement mid-block,
        // and the whole block has to be generated from one consistent shape.
        let data = self.model.shared();
        self.engine.run_block(&ctx, &data, &self.playhead, &mut self.out);

        for &timed in self.out.events() {
            let (events, count) = note_events(timed);
            // Stops at the first refusal on purpose: an expression for a note
            // the host never received is noise.
            let sent = events[..count].iter().all(|&event| context.try_send_event(event).is_ok());
            if !sent {
                refused = refused.saturating_add(1);
            }
        }

        // Both counters are per-instance totals; the assertions are per block,
        // so a host that refuses one event does not then complain every block
        // for the rest of the session. Neither does anything in release.
        let dropped = self.out.overflows();
        self.dropped_events = self.dropped_events.saturating_add(dropped);
        self.send_failures = self.send_failures.saturating_add(refused);
        nice_debug_assert_eq!(dropped, 0, "{} generated events did not fit in one block", dropped);
        nice_debug_assert_eq!(refused, 0, "the host refused {} event(s) this block", refused);

        // `KeepAlive`, not `Normal`: nice-plug reports `Normal` to a CLAP host
        // as "continue if not quiet", and with no audio outputs this plugin is
        // always quiet — so a host that sleeps quiet plugins could stop calling
        // `process()` on a track with no incoming notes, and the sequencer
        // would go silent mid-song.
        ProcessStatus::KeepAlive
    }
}

impl ClapPlugin for Bitstep {
    const CLAP_ID: &'static str = "com.bitstep.bitstep";
    const CLAP_DESCRIPTION: Option<&'static str> = Some(env!("CARGO_PKG_DESCRIPTION"));
    const CLAP_MANUAL_URL: Option<&'static str> = Some(Self::URL);
    const CLAP_SUPPORT_URL: Option<&'static str> = Some("https://github.com/pywkt/bitstep/issues");
    const CLAP_FEATURES: &'static [ClapFeature] = &[ClapFeature::NoteEffect, ClapFeature::Utility];
}

nice_export_clap!(Bitstep);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Step;
    use crate::sequencer::Rate;
    use std::collections::BTreeMap;

    /// Parameters over a fresh default sequence, as a new instance has them.
    fn test_params() -> BitstepParams {
        BitstepParams::new(Arc::new(SeqModel::new(state::default_data())))
    }

    /// Harness smoke test: proves `cargo test --workspace` links the lib and
    /// runs it. Asserts the two shell facts the whole design rests on — this
    /// is a note effect with no audio ports.
    #[test]
    fn plugin_shell_is_notes_only() {
        assert!(Bitstep::AUDIO_IO_LAYOUTS.is_empty());
        assert_eq!(Bitstep::CLAP_ID, "com.bitstep.bitstep");
    }

    /// Controller data played into the track (sustain, mod wheel, pitch bend)
    /// has to pass through to the instrument; `Basic` would drop it.
    #[test]
    fn controller_messages_pass_through() {
        assert!(Bitstep::MIDI_INPUT >= MidiConfig::MidiCCs);
        assert!(Bitstep::MIDI_OUTPUT >= MidiConfig::MidiCCs);
    }

    /// The host asking for a GUI gets one, over the handles this instance
    /// already plays from.
    ///
    /// Display-free: `Plugin::editor()` only assembles the editor, and the
    /// window is not created until the host calls `Editor::spawn()` (what the
    /// window then looks like can only be checked by eye, in a host). The
    /// executor is a pair of no-op closures because `BackgroundTask = ()` —
    /// nothing is ever posted to it.
    #[test]
    fn the_host_gets_an_editor_sharing_this_instances_sequence() {
        let mut plugin = Bitstep::default();
        let executor = AsyncExecutor::new(Arc::new(|()| {}), Arc::new(|()| {}));
        let editor = plugin.editor(executor).expect("an editor is always available");

        assert!(editor.resize_hint().can_resize, "the window is resizable");

        // Same `Arc`, not a copy of the sequence: the GUI's model, the
        // persistent field's model and the one `process()` reads are one.
        assert!(Arc::ptr_eq(&plugin.model, plugin.params.seq.model()));
        // Three handles to that one model while the editor is alive — this
        // field, the persistent field, and the editor's. The editor taking a
        // handle is the thing being checked: it cannot be read back out of
        // `BitstepEditor`, whose fields are private to `editor`.
        assert_eq!(Arc::strong_count(&plugin.model), 3);
        drop(editor);
        assert_eq!(Arc::strong_count(&plugin.model), 2);
    }

    /// Everything a host finds inside `target/bundled/bitstep.clap` reaches it
    /// through the `clap_entry` symbol that `nice_export_clap!` generates, so
    /// the bundle's contract is checked here rather than by inspecting the
    /// built `.clap` file: entry point present, plugin factory reachable under
    /// the CLAP factory ID, and exactly one plugin described in it.
    ///
    /// Deliberately stops short of `create_plugin()` — that needs a real
    /// `clap_host`, which is what `clap-validator validate` on the bundle is
    /// for.
    #[test]
    fn clap_entry_exposes_one_note_effect_plugin() {
        use nice_plug::wrapper::clap::{CLAP_PLUGIN_FACTORY_ID, clap_plugin_factory};
        use std::ffi::CStr;

        assert_eq!(clap_entry.clap_version.major, 1);

        let get_factory = clap_entry.get_factory.expect("no `get_factory` in `clap_entry`");
        let factory =
            unsafe { get_factory(CLAP_PLUGIN_FACTORY_ID.as_ptr()) } as *const clap_plugin_factory;
        assert!(!factory.is_null(), "plugin factory not exposed");

        // SAFETY: `factory` is the macro's own `static CLAP_PLUGIN_FACTORY`,
        // and its function pointers take that same pointer back.
        unsafe {
            let count = (*factory).get_plugin_count.expect("no `get_plugin_count`")(factory);
            assert_eq!(count, 1, "the bundle must expose exactly one plugin");

            let descriptor =
                (*factory).get_plugin_descriptor.expect("no `get_plugin_descriptor`")(factory, 0);
            assert!(!descriptor.is_null());
            assert_eq!(
                CStr::from_ptr((*descriptor).id).to_str().unwrap(),
                Bitstep::CLAP_ID
            );
            assert_eq!(
                CStr::from_ptr((*descriptor).name).to_str().unwrap(),
                Bitstep::NAME
            );

            // The feature list is an environ-style null-terminated array; a
            // note effect that does not advertise `note-effect` is invisible in
            // Bitwig's note-FX browser.
            let mut features = Vec::new();
            let mut ptr = (*descriptor).features;
            while !(*ptr).is_null() {
                features.push(CStr::from_ptr(*ptr).to_str().unwrap());
                ptr = ptr.add(1);
            }
            assert_eq!(features, vec!["note-effect", "utility"]);
        }
    }

    /// The host-visible parameter list: exactly one entry, at the top level,
    /// under the stable id that host automation lanes and saved projects key
    /// on. Renaming `patsel` silently breaks every existing project, so the
    /// id is pinned here rather than only in the attribute.
    #[test]
    fn param_map_exposes_patsel_alone() {
        let params = test_params();
        let exposed: Vec<(String, String)> =
            params.param_map().into_iter().map(|(id, _, group)| (id, group)).collect();
        assert_eq!(exposed, vec![("patsel".to_string(), String::new())]);
    }

    /// Range and default of `patsel`, as also asserted by `clap-validator`'s
    /// `param-default-values` test on the bundle: one-based `1..=64`,
    /// starting on pattern 1.
    #[test]
    fn pattern_select_param_is_one_based() {
        let params = test_params();

        assert_eq!(params.pattern_select.name(), "Pattern Select");
        assert_eq!(params.pattern_select.default_plain_value(), 1);
        assert_eq!(params.pattern_select.value(), 1);
        assert_eq!(params.pattern_select.preview_plain(0.0), 1);
        assert_eq!(params.pattern_select.preview_plain(1.0), 64);

        // 64 discrete values, so 63 steps between them — this is what the
        // host advertises as a stepped parameter and what its generic UI
        // walks through. Every step must land on a distinct pattern.
        assert_eq!(params.pattern_select.step_count(), Some(63));
        let walked: Vec<i32> = (0..=63)
            .map(|step| params.pattern_select.preview_plain(step as f32 / 63.0))
            .collect();
        assert_eq!(walked, (1..=64).collect::<Vec<i32>>());

        // ...and the range is the engine's, so there is one definition of how
        // many patterns exist.
        assert_eq!(BlockCtx::PATTERN_SELECT_MAX as usize, model::NUM_PATTERNS);
        assert_eq!(params.pattern_select(), BlockCtx::PATTERN_SELECT_MIN);
    }

    const SAMPLE_RATE: f32 = 48_000.0;
    const BLOCK: usize = 512;

    /// The transport a host reports while playing at `pos_beats`, with
    /// everything the engine reads present.
    fn playing_at(pos_beats: f64) -> Transport {
        let mut transport = Transport::new(SAMPLE_RATE);
        transport.playing = true;
        transport.tempo = Some(120.0);
        transport.pos_beats = Some(pos_beats);
        transport
    }

    /// The host's position and tempo are the two fields the engine cannot do
    /// without, and the loop range is the one it can: a playing host with all
    /// three maps straight onto the block the engine sequences.
    #[test]
    fn a_playing_transport_maps_onto_a_playing_block() {
        let mut transport = playing_at(8.5);
        transport.loop_range_beats = Some((4.0, 12.0));

        let ctx = block_ctx(&transport, BLOCK, 3);
        assert_eq!(
            ctx,
            BlockCtx::new(8.5, 120.0, SAMPLE_RATE, BLOCK)
                .with_loop(4.0, 12.0)
                .with_pattern_select(3)
        );
    }

    /// No loop is not missing data — it is the normal, non-looping case, and
    /// the only thing it changes is that the block is never split.
    #[test]
    fn a_host_that_is_not_looping_still_plays() {
        let ctx = block_ctx(&playing_at(0.0), BLOCK, 1);
        assert!(ctx.playing);
        assert_eq!(ctx.loop_range, None);
        assert_eq!(ctx.loop_region(), None);
    }

    /// A stopped transport keeps its position: the flush note-offs are
    /// stamped with where playback broke off, which is what makes them
    /// legible.
    #[test]
    fn a_stopped_transport_keeps_the_position_it_reports() {
        let mut transport = playing_at(16.0);
        transport.playing = false;

        let ctx = block_ctx(&transport, BLOCK, 1);
        assert!(!ctx.playing);
        assert_eq!(ctx.pos_beats, 16.0);
        assert_eq!(ctx.flush_beat(), 16.0);
    }

    /// The missing-transport rule: a host that reports no position cannot be
    /// sequenced, so the block reads as stopped however loudly the transport
    /// claims to be playing. Same for a missing tempo — without one there is
    /// no way to know how long the block even is in beats.
    #[test]
    fn missing_transport_data_reads_as_stopped() {
        let no_position = Transport { pos_beats: None, ..playing_at(4.0) };
        let no_tempo = Transport { tempo: None, ..playing_at(4.0) };

        for transport in [no_position, no_tempo] {
            let ctx = block_ctx(&transport, BLOCK, 5);
            assert!(!ctx.playing, "{transport:?} must read as stopped");
            // Still the selected pattern: a stopped block resets the pattern
            // state to what `patsel` asks for, so it has to arrive even here.
            assert_eq!(ctx.pattern_select, 5);
            assert_eq!(BlockCtx { pattern_select: 1, ..ctx }, BlockCtx::STOPPED);
        }
    }

    /// CLAP hosts may report the position in samples or seconds instead of
    /// beats, so the mapping goes through `Transport`'s accessors rather than
    /// its raw fields — a plugin that read `transport.pos_beats` directly
    /// would sit silent under such a host.
    #[test]
    fn a_position_reported_in_samples_is_still_a_position() {
        // 2 beats at 120 BPM = 1 second = 48000 samples.
        let mut transport = playing_at(0.0);
        transport.pos_beats = None;
        transport.pos_samples = Some(48_000);

        let ctx = block_ctx(&transport, BLOCK, 1);
        assert!(ctx.playing);
        assert_eq!(ctx.pos_beats, 2.0);

        // ...and the loop range the same way: 1 s to 2 s is beats 2 to 4.
        let mut in_seconds = playing_at(2.0);
        in_seconds.loop_range_seconds = Some((1.0, 2.0));
        assert_eq!(block_ctx(&in_seconds, BLOCK, 1).loop_range, Some((2.0, 4.0)));
    }

    /// [`note_events`] for an unpitched note: the note, and the expression
    /// right behind it (its pan, in the default build) if there is one.
    fn note_and_pan(
        timed: TimedEvent,
    ) -> (PluginNoteEvent<Bitstep>, Option<PluginNoteEvent<Bitstep>>) {
        let (events, count) = note_events(timed);
        assert!(count <= 2, "an unpitched note sends at most a pan: {events:?}");
        (events[0], (count == 2).then_some(events[1]))
    }

    /// A pitched step's note-on carries a `PolyTuning` in semitones, at the
    /// same timing and voice id, and keeps its key — so it still lands on its
    /// own Drum Machine pad.
    #[test]
    fn a_pitched_note_on_carries_its_tuning() {
        let pitched =
            SeqEvent::note_on(0.0, 0, 36, 0, 100, Step::PAN_CENTRE, 1 << 20).with_pitch(-7);
        let (events, count) = note_events(TimedEvent::new(12, pitched));
        let tuning = events[..count]
            .iter()
            .find_map(|event| match *event {
                NoteEvent::PolyTuning { timing, voice_id, key, tuning, .. } => {
                    Some((timing, voice_id, key, tuning))
                }
                _ => None,
            })
            .expect("a pitched note sends a tuning expression");
        assert_eq!(tuning, (12, VoiceID::ID(1 << 20), Key::Number(36), -7.0));
        assert!(matches!(events[0], NoteEvent::NoteOn { .. }), "the note goes first");
    }

    /// A lane with a volume sends it as a per-note volume (linear gain);
    /// one at 0 dB sends none.
    #[test]
    fn a_lane_volume_goes_out_as_a_per_note_volume() {
        let plain = SeqEvent::note_on(0.0, 0, 42, 0, 100, Step::PAN_CENTRE, 1 << 20);
        let gain_of = |event: SeqEvent| {
            let (events, count) = note_events(TimedEvent::new(5, event));
            assert!(matches!(events[0], NoteEvent::NoteOn { .. }), "the note goes first");
            events[..count].iter().find_map(|event| match *event {
                NoteEvent::PolyVolume { timing, voice_id, key, gain, .. } => {
                    assert_eq!((timing, voice_id, key), (5, VoiceID::ID(1 << 20), Key::Number(42)));
                    Some(gain)
                }
                _ => None,
            })
        };
        assert_eq!(gain_of(plain), None, "0 dB sends nothing");
        let loud = gain_of(plain.with_volume(12).with_pitch(3)).expect("+12 dB is sent");
        assert!((loud - 3.981).abs() < 1e-3 && loud <= 4.0, "{loud}");
        let quiet = gain_of(plain.with_volume(-6)).expect("-6 dB is sent");
        assert!((quiet - 0.501).abs() < 1e-3, "{quiet}");
        // Note, pan, tuning and volume all fit.
        let (_, count) = note_events(TimedEvent::new(0, plain.with_volume(3).with_pitch(1)));
        assert_eq!(count, if cfg!(feature = "pan-expression") { 4 } else { 3 });
    }

    /// A sample pad's note is retuned to the sample root on top of its
    /// pitch, and one sitting on the root sends no tuning.
    #[test]
    fn a_sample_pad_note_on_is_retuned_to_the_root() {
        let tuning_of = |event: SeqEvent| {
            let (events, count) = note_events(TimedEvent::new(0, event));
            events[..count].iter().find_map(|event| match *event {
                NoteEvent::PolyTuning { tuning, .. } => Some(tuning),
                _ => None,
            })
        };
        let kick = SeqEvent::note_on(0.0, 0, 36, 0, 100, Step::PAN_CENTRE, 1 << 20);
        assert_eq!(tuning_of(kick.with_sample_pad(true)), Some(24.0));
        assert_eq!(tuning_of(kick.with_sample_pad(true).with_pitch(-3)), Some(21.0));
        let on_root = SeqEvent::note_on(0.0, 0, 60, 0, 100, Step::PAN_CENTRE, 1 << 20);
        assert_eq!(tuning_of(on_root.with_sample_pad(true)), None);
    }

    /// An unpitched note sends no tuning at all.
    #[test]
    fn an_unpitched_note_on_sends_no_tuning() {
        let plain = SeqEvent::note_on(0.0, 0, 36, 0, 100, Step::PAN_CENTRE, 1 << 20);
        let (events, count) = note_events(TimedEvent::new(0, plain));
        assert!(!events[..count].iter().any(|event| matches!(event, NoteEvent::PolyTuning { .. })));
    }

    /// A note-on becomes two events: the note, then its pan at the same
    /// timing and voice id. The packed velocity and pan have already been
    /// mapped by `SeqEvent`, so what arrives here is what the host receives.
    ///
    /// With the `pan-expression` feature, i.e. the default build; the other
    /// side of the switch is
    /// [`a_note_on_sends_no_pan_without_the_feature`].
    #[test]
    #[cfg(feature = "pan-expression")]
    fn a_note_on_carries_its_pan_right_behind_it() {
        let hard_right = SeqEvent::note_on(1.5, 2, 36, 9, 127, Step::PAN_MAX, 1 << 20);

        let (note, expression) = note_and_pan(TimedEvent::new(240, hard_right));
        assert_eq!(
            note,
            NoteEvent::NoteOn {
                timing: 240,
                voice_id: VoiceID::ID(1 << 20),
                channel: Channel::Number(9),
                key: Key::Number(36),
                velocity: 1.0,
            }
        );
        assert_eq!(
            expression,
            Some(NoteEvent::PolyPan {
                timing: 240,
                voice_id: VoiceID::ID(1 << 20),
                channel: Channel::Number(9),
                key: Key::Number(36),
                pan: 1.0,
            })
        );
    }

    /// Centre pan is a clean `0.0`, and a mid velocity keeps its `/127`
    /// scaling — the two values a host hears as "unchanged".
    #[test]
    #[cfg(feature = "pan-expression")]
    fn a_centred_note_pans_to_zero() {
        let centred = SeqEvent::note_on(0.0, 0, 60, 0, 64, Step::PAN_CENTRE, 1 << 20);

        let (note, expression) = note_and_pan(TimedEvent::new(0, centred));
        let NoteEvent::NoteOn { velocity, .. } = note else { panic!("not a note-on: {note:?}") };
        assert_eq!(velocity, 64.0 / 127.0);
        let Some(NoteEvent::PolyPan { pan, .. }) = expression else {
            panic!("no pan: {expression:?}")
        };
        assert_eq!(pan, 0.0);
    }

    /// The other side of the `pan-expression` switch: built without it, a
    /// note-on goes out **alone**. Everything else about it is unchanged —
    /// the timing, voice id, channel, key and velocity are the same event the
    /// default build sends — and the step still *holds* its pan, which is
    /// what makes the switch reversible without touching a saved project.
    #[test]
    #[cfg(not(feature = "pan-expression"))]
    fn a_note_on_sends_no_pan_without_the_feature() {
        let hard_right = SeqEvent::note_on(1.5, 2, 36, 9, 127, Step::PAN_MAX, 1 << 20);

        let (note, expression) = note_and_pan(TimedEvent::new(240, hard_right));
        assert_eq!(
            note,
            NoteEvent::NoteOn {
                timing: 240,
                voice_id: VoiceID::ID(1 << 20),
                channel: Channel::Number(9),
                key: Key::Number(36),
                velocity: 1.0,
            }
        );
        assert_eq!(expression, None, "no expression without the feature");

        // The bits the GUI can no longer reach are still the bits the model
        // stores and `state.rs` saves.
        assert_eq!(Step::default_on().with_pan(Step::PAN_MAX).pan(), Step::PAN_MAX);
    }

    /// A note-off is one event — pan is a property of a sounding voice, and
    /// re-sending it as the voice ends would only confuse a host. The voice
    /// id is what pairs it with its note-on, so two lanes on one key release
    /// the right notes.
    #[test]
    fn a_note_off_is_sent_alone() {
        let off = SeqEvent::note_off(2.0, 42, 0, (1 << 20) + 7);

        let (note, expression) = note_and_pan(TimedEvent::new(96, off));
        assert_eq!(
            note,
            NoteEvent::NoteOff {
                timing: 96,
                voice_id: VoiceID::ID((1 << 20) + 7),
                channel: Channel::Number(0),
                key: Key::Number(42),
                velocity: 0.0,
            }
        );
        assert_eq!(expression, None);
    }

    /// One real block's whole event stream, converted the way `process()`
    /// converts it: every generated note-on carries its own `PolyPan` right
    /// behind it, and nothing else carries one.
    ///
    /// Whether Bitwig's Drum Machine *honours* the expression is something
    /// only a listening test in the host can answer, and that answer is only
    /// attributable if the stream it receives is known to be well formed —
    /// which is what this test pins down. The two tests above convert one
    /// event each; what this one adds is the shape of a block
    /// that has been through the sort — a four-ratchet step, a second strike
    /// two beats later, and a second lane on the *same key*, so note-offs,
    /// note-ons and pans from six different voices are interleaved and the
    /// pairing cannot be read off the key.
    ///
    /// Runs in **both** builds: the walk below is the invariant either way
    /// (a pan may only ever appear immediately behind its own note-on), and
    /// how many pans to expect comes from `cfg!(feature = "pan-expression")`
    /// — six with the feature, none without it, and the notes themselves
    /// identical.
    #[test]
    fn every_generated_note_on_in_a_block_carries_one_pan() {
        use crate::model::{Lane, LaneData, SeqData, chunked_capacity};
        use crate::voices::FIRST_VOICE_ID;

        // Lane 1: 1/16, 16 steps — step 0 hard left with a ratchet of 4,
        // step 8 hard right. Lane 2: 1/4, 4 steps — step 0 at the default
        // centre pan, on key 36 as well.
        let lanes = vec![
            Lane::new(1, String::from("left"), 0, 36, 0),
            Lane::new(2, String::from("centre"), 0, 36, 1),
        ];
        let patterns = std::array::from_fn(|_| {
            let sixteenths = LaneData::with_capacity(chunked_capacity(16));
            sixteenths.set_length(16);
            sixteenths.set_rate(Rate::Sixteenth);
            sixteenths.set_step(0, Step::pack(true, 100, Step::PAN_MIN, 4, 100));
            sixteenths.set_step(8, Step::pack(true, 100, Step::PAN_MAX, 1, 100));

            let quarters = LaneData::with_capacity(chunked_capacity(4));
            quarters.set_length(4);
            quarters.set_rate(Rate::Quarter);
            quarters.set_step(0, Step::default_on());

            vec![sixteenths, quarters]
        });
        let data = SeqData::new(lanes, patterns);

        // Three beats from song zero in one block, which covers the ratchet
        // burst at beat 0, the quarter lane's strike at beat 0, the hard-right
        // step at beat 2.0, and every gate those open.
        let mut engine = Engine::new();
        let mut out = BlockOutput::new();
        let ctx = BlockCtx::new(0.0, 120.0, SAMPLE_RATE, 3 * 24_000);
        engine.run_block(&ctx, &data, &Playhead::new(), &mut out);

        // Exactly `process()`'s loop: the note, then its expression if it has
        // one, in the order the host receives them.
        let mut stream: Vec<PluginNoteEvent<Bitstep>> = Vec::new();
        for &timed in out.events() {
            let (events, count) = note_events(timed);
            stream.extend_from_slice(&events[..count]);
        }

        // Walk it: a pan may only appear immediately after a note-on, and
        // must name that same voice. `pans` collects what each voice was
        // panned to, which is what the host is being asked to honour.
        let mut pans: Vec<(VoiceID, f32)> = Vec::new();
        let mut note_ons = 0;
        let mut note_offs = 0;
        let mut previous: Option<PluginNoteEvent<Bitstep>> = None;
        for event in &stream {
            match *event {
                NoteEvent::NoteOn { .. } => note_ons += 1,
                NoteEvent::NoteOff { .. } => note_offs += 1,
                NoteEvent::PolyPan { timing, voice_id, channel, key, pan } => {
                    let Some(NoteEvent::NoteOn {
                        timing: on_timing,
                        voice_id: on_voice,
                        channel: on_channel,
                        key: on_key,
                        ..
                    }) = previous
                    else {
                        panic!("a pan that does not follow a note-on: {previous:?}");
                    };
                    assert_eq!(
                        (timing, voice_id, channel, key),
                        (on_timing, on_voice, on_channel, on_key),
                        "the pan must name the note-on in front of it"
                    );
                    assert!((-1.0..=1.0).contains(&pan), "pan {pan} is outside [-1, 1]");
                    pans.push((voice_id, pan));
                }
                ref other => panic!("the shell generates notes and pans only: {other:?}"),
            }
            previous = Some(*event);
        }

        // Six strikes: four ratchet hits, the hard-right step, the quarter
        // lane's own note on the same key — whatever the feature does to the
        // expressions, this is the patch and these are its notes.
        assert_eq!(note_ons, 6, "the patch's strikes: {stream:?}");
        assert_eq!(note_offs, note_ons, "every gate closes inside this block");

        if !cfg!(feature = "pan-expression") {
            assert!(pans.is_empty(), "no expressions without the feature: {pans:?}");
            return;
        }

        // One pan per note-on, each on its own voice id — so a host applying
        // a pan per voice has six voices to apply it to.
        assert_eq!(pans.len(), note_ons, "one pan per note-on, no more");
        let voices: std::collections::BTreeSet<i32> = pans
            .iter()
            .map(|&(voice, _)| match voice {
                VoiceID::ID(id) => id,
                other => panic!("a generated note needs its own voice id: {other:?}"),
            })
            .collect();
        assert_eq!(voices.len(), 6, "six distinct voice ids: {voices:?}");
        assert!(voices.iter().all(|&id| id >= FIRST_VOICE_ID), "generated ids: {voices:?}");

        // ...and the values: four hard-left ratchets, one hard-right strike,
        // one centred quarter note. Sorted, because which voice got which id
        // is the voice table's business.
        let mut values: Vec<f32> = pans.iter().map(|&(_, pan)| pan).collect();
        values.sort_by(f32::total_cmp);
        assert_eq!(values, vec![-1.0, -1.0, -1.0, -1.0, 0.0, 1.0], "{pans:?}");
    }

    /// A fresh instance has nothing sounding, nothing selected but pattern 1,
    /// and no events to report — the state the host finds it in before the
    /// first `process()` call.
    #[test]
    fn a_fresh_instance_starts_silent() {
        let plugin = Bitstep::default();

        assert_eq!(plugin.params.pattern_select(), 1);
        assert!(plugin.engine.voices().is_empty());
        assert_eq!(plugin.engine.prev_end(), None);
        assert!(plugin.out.is_empty());
        assert_eq!(plugin.out.overflows(), 0);
        assert_eq!(plugin.send_failures(), 0);
        assert_eq!(plugin.dropped_events(), 0);
        assert_eq!(plugin.model.shared().lane_count(), state::DEFAULT_LANES.len());
        assert_eq!(plugin.playhead.pattern(), None);
    }

    /// `reset()` runs where no event can be sent, so all it may do is ask the
    /// next block to release what is sounding; same for `deactivate()`.
    #[test]
    fn reset_and_deactivate_only_ask_for_a_flush() {
        for interrupt in [Bitstep::reset as fn(&mut Bitstep), Bitstep::deactivate] {
            let mut plugin = Bitstep::default();
            assert!(!plugin.engine.flush_pending());
            interrupt(&mut plugin);
            assert!(plugin.engine.flush_pending());
            assert!(plugin.out.is_empty(), "no events may be produced outside process()");
        }
    }

    /// `IntParam` stores a plain `i32` and only clamps values a host hands it
    /// in normalized form, so the accessor the engine reads through clamps.
    /// Note the 256 case: a bare `as u8` cast would turn it into pattern 0,
    /// which is not even a legal parameter value.
    #[test]
    fn pattern_select_accessor_clamps_hostile_values() {
        assert_eq!(clamp_pattern_select(i32::MIN), 1);
        assert_eq!(clamp_pattern_select(0), 1);
        assert_eq!(clamp_pattern_select(1), 1);
        assert_eq!(clamp_pattern_select(4), 4);
        assert_eq!(clamp_pattern_select(64), 64);
        assert_eq!(clamp_pattern_select(65), 64);
        assert_eq!(clamp_pattern_select(256), 64);
        assert_eq!(clamp_pattern_select(i32::MAX), 64);
    }

    // ---- persistence: the `#[persist = "seq"]` field ----

    /// Edits a model into something no default could produce: a renamed and
    /// recoloured lane, a fifth lane, a polymeter length and rate, steps with
    /// every per-step field set, a step hidden past `length`, and an edit in
    /// a pattern other than the first.
    fn fill_with_a_project(model: &SeqModel) {
        let first = model.shared().lane(0).expect("default lanes").id;
        assert!(model.rename_lane(first, "Kick 808"));
        assert!(model.set_colour(first, 0x12_34_56));
        assert!(model.set_key(first, 35));
        assert!(model.set_channel(first, 9));
        assert!(model.set_mute(first, true));

        // Polymeter: 12 steps of 1/8T against everyone else's 16 of 1/16.
        assert!(model.ensure_capacity(0, first, 24));
        assert!(model.set_length(0, first, 12));
        assert!(model.set_rate(0, first, Rate::EighthTriplet));
        assert!(model.set_step(
            0,
            first,
            0,
            Step::pack(true, 100, Step::PAN_MAX, 4, 75),
        ));
        // Past `length`, so it only survives if hidden steps are saved too.
        assert!(model.set_step(0, first, 20, Step::pack(true, 7, 0, 8, 1)));

        // A second pattern bank, so the save is not just pattern 1.
        let added = model.add_lane("Clap", 0x65_43_21, 39, 3).expect("room for a fifth lane");
        assert!(model.set_solo(added, true));
        assert!(model.toggle_step(7, added, 5));
        assert!(model.set_rate(7, added, Rate::QuarterDotted));
    }

    /// `map` serialises whatever the model holds *now*, not a copy taken when
    /// the field was built — the host asking for state twice around an edit
    /// must get two different blobs.
    #[test]
    fn map_reads_the_live_sequence() {
        let params = test_params();

        let before: SeqSnapshot = params.seq.map(Clone::clone);
        assert_eq!(before, params.seq.model().snapshot());

        fill_with_a_project(params.seq.model());
        let after: SeqSnapshot = params.seq.map(Clone::clone);
        assert_ne!(after, before);
        assert_eq!(after, params.seq.model().snapshot());
    }

    /// `set` is a shape edit on the shared model, so the `Arc` the audio
    /// thread reads sees the loaded sequence — the whole point of the field
    /// wrapping the live model rather than owning a copy.
    #[test]
    fn set_publishes_onto_the_shared_model() {
        let plugin = Bitstep::default();
        let saved = {
            let source = SeqModel::new(state::default_data());
            fill_with_a_project(&source);
            source.snapshot()
        };

        PersistentField::set(&plugin.params.seq, saved.clone());

        // `plugin.model` and `params.seq` are the same `Arc`, and `shared()`
        // is what `process()` reads.
        assert_eq!(plugin.model.snapshot(), saved);
        assert_eq!(plugin.model.shared().lane_count(), saved.lanes.len());
        assert!(Arc::ptr_eq(&plugin.model, plugin.params.seq.model()));
    }

    /// The persistent-field trait contract: `map` after `set(snapshot)`
    /// returns an equal snapshot, hidden steps and all eight patterns included.
    #[test]
    fn set_then_map_returns_an_equal_snapshot() {
        let source = SeqModel::new(state::default_data());
        fill_with_a_project(&source);
        let saved = source.snapshot();

        let params = test_params();
        PersistentField::set(&params.seq, saved.clone());

        assert_eq!(params.seq.map(Clone::clone), saved);
    }

    /// The host's round trip end to end: `serialize_fields` produces the blob
    /// a project file stores under `seq`, and `deserialize_fields` on a
    /// fresh instance rebuilds the same sequence. This is the derive-generated
    /// path — `serde_json` either side of the trait methods — not the trait
    /// methods called by hand.
    #[test]
    fn the_seq_field_round_trips_through_the_project_file() {
        let saved_params = test_params();
        fill_with_a_project(saved_params.seq.model());
        let saved = saved_params.seq.model().snapshot();

        let stored = saved_params.serialize_fields();
        assert_eq!(stored.keys().collect::<Vec<_>>(), vec!["seq"]);
        // Stored as JSON with explicit field names, never the packed bits.
        assert_eq!(
            serde_json::from_str::<SeqSnapshot>(&stored["seq"]).expect("stored blob is JSON"),
            saved
        );
        assert!(stored["seq"].contains("\"probability\""));

        let loaded_params = test_params();
        assert_ne!(loaded_params.seq.map(Clone::clone), saved);
        loaded_params.deserialize_fields(&stored);

        assert_eq!(loaded_params.seq.map(Clone::clone), saved);
        // ...and what the audio thread would read matches too, hidden step
        // beyond `length` included.
        let data = loaded_params.seq.model().shared();
        let lane_data = data.lane_data(0, 0).expect("the first lane in pattern 1");
        assert_eq!(lane_data.length(), 12);
        assert_eq!(lane_data.rate(), Rate::EighthTriplet);
        assert_eq!(lane_data.step(20), Step::pack(true, 7, 0, 8, 1));
    }

    /// A project that never stored a `seq` key — an older build, or a host
    /// handing back parameters only — leaves the sequence alone rather than
    /// wiping it to an empty one.
    #[test]
    fn a_project_without_the_seq_key_keeps_the_current_sequence() {
        let params = test_params();
        fill_with_a_project(params.seq.model());
        let before = params.seq.model().snapshot();

        params.deserialize_fields(&BTreeMap::from([
            (String::from("something_else"), String::from("\"ignored\"")),
        ]));

        assert_eq!(params.seq.model().snapshot(), before);
    }

    /// Garbage under the `seq` key never reaches the model: the framework
    /// deserialises before it calls `set`, so a blob that is not a snapshot
    /// is dropped with an error and the current sequence stands.
    ///
    /// Driven through `deserialize_field` — the exact function the `Params`
    /// derive calls — rather than through `deserialize_fields`, because the
    /// generated code answers a failed parse with `nice_debug_assert_failure!`,
    /// which is a hard panic in `cfg(test)` builds and a logged warning
    /// everywhere else. The branch under test is the one before that.
    #[test]
    fn garbage_under_the_seq_key_keeps_the_current_sequence() {
        use nice_plug::params::persist::deserialize_field;

        let params = test_params();
        fill_with_a_project(params.seq.model());
        let before = params.seq.model().snapshot();

        for blob in ["", "not json at all", "{", "[1, 2, 3]", "\"seq\"", "null"] {
            let parsed = deserialize_field::<SeqSnapshot>(blob);
            assert!(parsed.is_err(), "{blob:?} must not parse as a snapshot");
            // Nothing was handed to `set`, so nothing moved.
            assert_eq!(params.seq.model().snapshot(), before);
        }
    }

    /// JSON that *does* parse is loaded, however little of it there is or
    /// however hostile: `{}` is an empty sequence, and a file claiming 10 000
    /// lanes with impossible field values opens clamped instead of panicking
    /// inside the host. The rules themselves are `state.rs`'s; what this
    /// pins is that the persist path runs them.
    #[test]
    fn hostile_but_parseable_state_loads_clamped() {
        let params = test_params();

        params.deserialize_fields(&BTreeMap::from([(String::from("seq"), String::from("{}"))]));
        let empty = params.seq.map(Clone::clone);
        assert!(empty.lanes.is_empty());
        // Patterns nobody touched are not saved, and a load pads them back.
        assert!(empty.patterns.is_empty());

        let lanes: Vec<String> = (0..10_000)
            .map(|id| {
                format!(
                    "{{\"id\":{id},\"name\":\"x\",\"colour\":0,\"key\":250,\
                      \"channel\":99,\"mute\":false,\"solo\":false}}"
                )
            })
            .collect();
        let hostile = format!(
            "{{\"version\":9999,\"lanes\":[{}],\"patterns\":[]}}",
            lanes.join(",")
        );
        params.deserialize_fields(&BTreeMap::from([(String::from("seq"), hostile)]));

        let loaded = params.seq.map(Clone::clone);
        assert_eq!(loaded.lanes.len(), model::MAX_LANES);
        assert!(loaded.patterns.is_empty(), "every pattern loaded empty");
        assert!(loaded.lanes.iter().all(|lane| lane.key <= model::Lane::KEY_MAX));
        assert!(loaded.lanes.iter().all(|lane| lane.channel <= model::Lane::CHANNEL_MAX));
        // Re-saving the clamped state writes the current version, so the next
        // load is no longer a from-the-future file.
        assert_eq!(loaded.version, state::CURRENT_VERSION);
    }
}
