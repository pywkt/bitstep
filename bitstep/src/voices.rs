//! The active-note table: who is sounding, until when, and under which voice
//! id.
//!
//! The lane scan ([`crate::sequencer::scan_lane`]) only reports *when a step
//! fires and for how long*; everything about a note's lifetime lives here,
//! because:
//!
//! - a note-off routinely falls in a later block than its note-on, so someone
//!   has to remember it across blocks;
//! - re-striking a key that is still sounding has to close the old note first,
//!   and the lane that struck it cannot know another lane (or an earlier
//!   ratchet) is holding that key;
//! - a lane deleted mid-note still has to release it — the table owns the
//!   note, not the lane.
//!
//! Like `model` and `sequencer`, this module imports no nice-plug types and
//! allocates nothing once constructed.

use crate::sequencer::{BeatRange, SeqEvent, StepHit, StepSink};

/// How many notes may sound at once.
///
/// The table is a flat array of this many slots, scanned linearly: 512 is
/// small enough that a scan is a few cache lines and large enough that a
/// patch would need 512 simultaneously held notes to steal — far past
/// anything a step sequencer produces, since a lane holds at most one note at
/// a time.
pub const VOICE_CAPACITY: usize = 512;

/// The first voice id this plugin generates.
///
/// Generated notes live in their own high id range so they can never be
/// confused with the host's ids on forwarded input: input events are passed
/// through untouched and matched by key and channel only, because Bitwig
/// sends no voice id on an input note-off.
pub const FIRST_VOICE_ID: i32 = 1 << 20;

/// How close (in beats) a strike must land to a held voice's start on the
/// same key and channel to count as *the same note* and be merged into it.
///
/// Two lanes on one key hitting one beat — a duplicated lane, or a 1/4 and a
/// 1/16 lane meeting on the downbeat — would otherwise open a note and close
/// it again at the same instant, which plays the sample twice and gives the
/// host a zero-length note whose off can sort ahead of its on. Different
/// rates can compute "the same" beat a few ulps apart (every triplet), so the
/// test can't be exact equality. A billionth of a beat is far below one
/// sample at any tempo, and far above float noise anywhere in a song.
pub const MERGE_WINDOW_BEATS: f64 = 1e-9;

/// One sounding note.
///
/// `end_beat` is the exact beat its gate closes at, in the same song-position
/// timeline as everything else, which is what lets [`VoiceTable::drain_ending`]
/// find it from a later block.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Voice {
    key: u8,
    channel: u8,
    voice_id: i32,
    /// The beat the note-on was struck at, for [`MERGE_WINDOW_BEATS`].
    start_beat: f64,
    end_beat: f64,
    /// Allocation order, for the oldest-first steal. A separate counter
    /// rather than the voice id because ids wrap back to [`FIRST_VOICE_ID`]
    /// eventually, and a wrapped id would make the newest voice look oldest;
    /// a `u64` incremented once per note cannot wrap in any session.
    serial: u64,
}

impl Voice {
    /// The id the matching note-off must carry.
    pub const fn voice_id(self) -> i32 {
        self.voice_id
    }

    /// The beat the note's gate closes at.
    pub const fn end_beat(self) -> f64 {
        self.end_beat
    }

    /// The note-off that releases this voice at `beat` — its own `end_beat`
    /// when it ends normally, the striking note's beat when it is cut short
    /// by a retrigger or a steal.
    pub const fn note_off_at(self, beat: f64) -> SeqEvent {
        SeqEvent::note_off(beat, self.key, self.channel, self.voice_id)
    }
}

/// Where the voice table sends the events it decides on.
///
/// A trait rather than a returned collection for the same reason
/// [`StepSink`] is one: this runs on the audio thread and must not allocate.
/// The engine's sink turns each [`SeqEvent`]'s beat into a sample offset (via
/// [`crate::sequencer::BlockSegment::offset_of`]) and pushes it into the
/// block's [`crate::sequencer::EventBuf`]; tests pass a closure.
///
/// `emit` returns nothing on purpose: whether the event *fit* is the event
/// buffer's business, and the table's bookkeeping must stay correct either
/// way — a dropped note-off is one stuck note, but a table that believed a
/// dropped note-on never happened would leak the slot forever.
pub trait EventSink {
    fn emit(&mut self, event: SeqEvent);
}

impl<F: FnMut(SeqEvent)> EventSink for F {
    fn emit(&mut self, event: SeqEvent) {
        self(event)
    }
}

/// The fixed-capacity table of sounding notes.
///
/// Storage is [`VOICE_CAPACITY`] slots allocated once in [`VoiceTable::new`]
/// and never resized, so every method here is safe to call from `process()`.
/// Lookups are linear scans over those slots; the occupied count lets them
/// stop early while the table is mostly empty, which is the normal case.
#[derive(Clone, Debug)]
pub struct VoiceTable {
    slots: Vec<Option<Voice>>,
    held: usize,
    next_voice_id: i32,
    next_serial: u64,
    steals: u32,
}

impl VoiceTable {
    /// An empty table with all [`VOICE_CAPACITY`] slots' worth of storage
    /// already allocated — the one allocation this type ever makes, off the
    /// audio thread.
    pub fn new() -> VoiceTable {
        VoiceTable {
            slots: vec![None; VOICE_CAPACITY],
            held: 0,
            next_voice_id: FIRST_VOICE_ID,
            next_serial: 0,
            steals: 0,
        }
    }

    /// How many notes are sounding.
    pub const fn len(&self) -> usize {
        self.held
    }

    pub const fn is_empty(&self) -> bool {
        self.held == 0
    }

    /// Whether the next [`VoiceTable::strike`] on a fresh key would have to
    /// steal.
    pub const fn is_full(&self) -> bool {
        self.held >= VOICE_CAPACITY
    }

    pub const fn capacity(&self) -> usize {
        VOICE_CAPACITY
    }

    /// How many voices have been stolen since the table was created — a
    /// debug counter, the voice-table counterpart of [`crate::sequencer::EventBuf::overflows`].
    /// Non-zero means notes were cut short.
    pub const fn steals(&self) -> u32 {
        self.steals
    }

    /// The sounding voices, in slot order (which is neither age nor pitch
    /// order — slots are reused as notes end).
    pub fn iter(&self) -> impl Iterator<Item = &Voice> {
        self.slots.iter().flatten()
    }

    /// The voice holding `key` on `channel`, if any. At most one can exist:
    /// that is exactly what the retrigger rule enforces.
    /// Returned by value because [`Voice`] is `Copy` and the borrow would
    /// otherwise keep the whole table frozen.
    pub fn find(&self, key: u8, channel: u8) -> Option<Voice> {
        self.iter()
            .copied()
            .find(|v| v.key == key && v.channel == channel)
    }

    /// Opens the note a [`StepHit`] calls for, emitting into `sink` the
    /// events that implies, **in the order a host must receive them**:
    ///
    /// 1. the **retrigger** note-off, if that key+channel is already held —
    ///    at the new note's own beat, so the old note ends exactly where the
    ///    new one starts rather than overlapping it;
    /// 2. otherwise the **steal** note-off, if the table is full — the oldest
    ///    voice, likewise cut at the new note's beat;
    /// 3. the new note-on.
    ///
    /// Returns the new voice's id. A retrigger frees a slot before the insert,
    /// so it never also steals.
    ///
    /// **Merge:** a strike within [`MERGE_WINDOW_BEATS`] of the start of the
    /// voice already holding its key+channel is the same note, struck twice
    /// at once. It emits nothing; the held voice keeps its velocity and pan
    /// (the first strike's — strikes arrive in time order, then lane order),
    /// takes the longer of the two gates, and its id is returned.
    ///
    /// Strikes must arrive in time order (the engine sorts each piece's hits
    /// by beat before striking them): a strike earlier than the voice it
    /// retriggers would release that voice before its own note-on.
    ///
    /// Both note-offs are *never later than the released voice's own
    /// `end_beat`* ([`VoiceTable::released_at`]): the engine drains closed
    /// gates once per block segment, so within one segment a strike can
    /// arrive after a gate it is holding has already closed — and a note-off
    /// at the strike's beat would then stretch that note by however much
    /// timeline the host happened to put in one block.
    ///
    /// A genuine retrigger's note-off lands at the same beat as the note-on,
    /// hence the same sample offset; [`crate::sequencer::EventBuf::sort`]'s
    /// note-offs-first tie-break at an equal beat is what keeps that order
    /// once the block's events are sorted.
    pub fn strike(&mut self, hit: StepHit, sink: &mut impl EventSink) -> i32 {
        self.open(hit.beat, hit.end_beat, hit.key, hit.channel, sink, |id| hit.note_on(id))
    }

    /// [`VoiceTable::strike`] without a [`StepHit`]: the caller supplies the
    /// note-on it wants emitted once the id is known. Kept separate so the
    /// hit's velocity, pan and lane position travel with the hit and are
    /// unpacked in exactly one place.
    fn open(
        &mut self,
        beat: f64,
        end_beat: f64,
        key: u8,
        channel: u8,
        sink: &mut impl EventSink,
        note_on: impl FnOnce(i32) -> SeqEvent,
    ) -> i32 {
        // One pass over the slots collects everything the decision needs:
        // the voice already holding this key, the first free slot, and the
        // oldest voice to steal if there is no free slot.
        let mut held_slot = None;
        let mut free_slot = None;
        let mut oldest: Option<(usize, u64)> = None;
        for (index, slot) in self.slots.iter().enumerate() {
            match slot {
                Some(voice) => {
                    if voice.key == key && voice.channel == channel {
                        held_slot = Some(index);
                        break;
                    }
                    if oldest.is_none_or(|(_, serial)| voice.serial < serial) {
                        oldest = Some((index, voice.serial));
                    }
                }
                None if free_slot.is_none() => free_slot = Some(index),
                None => {}
            }
        }

        if let Some(index) = held_slot
            && let Some(held) = self.slots[index].as_mut()
            // `abs`: after a loop wrap a strike can sit *before* the held
            // voice's start, and that is a retrigger, not the same note.
            && (beat - held.start_beat).abs() < MERGE_WINDOW_BEATS
        {
            held.end_beat = held.end_beat.max(end_beat);
            return held.voice_id;
        }

        let index = match (held_slot, free_slot) {
            // Retrigger: close the old note at the new note's timing, then
            // reuse its slot.
            (Some(index), _) => {
                let voice = self.take(index).expect("held slot holds a voice");
                sink.emit(voice.note_off_at(Self::released_at(voice, beat)));
                index
            }
            (None, Some(index)) => index,
            // Full: steal the oldest voice, closing it at the new note's
            // timing too.
            (None, None) => {
                let (index, _) = oldest.expect("a full table has an oldest voice");
                let voice = self.take(index).expect("occupied slot holds a voice");
                sink.emit(voice.note_off_at(Self::released_at(voice, beat)));
                self.steals = self.steals.saturating_add(1);
                index
            }
        };

        let voice_id = self.next_voice_id();
        let serial = self.next_serial();
        self.slots[index] =
            Some(Voice { key, channel, voice_id, start_beat: beat, end_beat, serial });
        self.held += 1;
        sink.emit(note_on(voice_id));
        voice_id
    }

    /// Releases every voice whose gate closes inside `range`, emitting each
    /// note-off at its own `end_beat`. Returns how many it released.
    ///
    /// The range is half-open like every other beat range here, so a voice
    /// ending exactly at `range.end` is left for the next block — the same
    /// rule that stops a note-on from firing twice across a stitched edge.
    ///
    /// The engine must run this **after** the scan as well as before it: a
    /// gate is a fraction of a subdivision, so most notes open and close
    /// inside one range, and a drain that only ran first would never see
    /// them.
    pub fn drain_ending(&mut self, range: BeatRange, sink: &mut impl EventSink) -> usize {
        self.release_matching(sink, |voice| {
            range.contains(voice.end_beat).then_some(voice.end_beat)
        })
    }

    /// Releases *every* voice, emitting the note-offs at `beat` — used on
    /// stop, on a transport jump, on `reset()` and on deactivate, where the
    /// engine schedules them at sample offset 0. Returns how many it released.
    ///
    /// `beat` is only the stamp the events carry (it tells two offsets that
    /// round together apart, and the tests read it); the table is emptied
    /// regardless of how it compares to any voice's `end_beat`.
    pub fn flush_all(&mut self, beat: f64, sink: &mut impl EventSink) -> usize {
        self.release_matching(sink, |_| Some(beat))
    }

    /// The shared body of the two release paths: walks the slots in order and
    /// frees each voice the predicate returns a note-off beat for.
    fn release_matching(
        &mut self,
        sink: &mut impl EventSink,
        at: impl Fn(&Voice) -> Option<f64>,
    ) -> usize {
        let mut released = 0;
        for index in 0..self.slots.len() {
            if self.held == released {
                break;
            }
            let Some(voice) = self.slots[index] else {
                continue;
            };
            let Some(beat) = at(&voice) else { continue };
            self.slots[index] = None;
            sink.emit(voice.note_off_at(beat));
            released += 1;
        }
        self.held -= released;
        released
    }

    /// When a voice cut short by a strike at `beat` is released: at `beat`
    /// itself, or at its own `end_beat` when that is already past.
    ///
    /// The second case is the one that keeps the output block-size
    /// invariant. A gate is released by [`VoiceTable::drain_ending`], which
    /// the engine runs once per block segment, so a strike that lands later
    /// in the *same* segment than a gate it is holding sees a voice that is
    /// musically already over; chopping the block differently would have
    /// drained it first. Taking the earlier of the two beats makes both
    /// orders emit the same note-off.
    fn released_at(voice: Voice, beat: f64) -> f64 {
        if voice.end_beat < beat { voice.end_beat } else { beat }
    }

    /// Empties the slot, keeping `held` in step with it.
    fn take(&mut self, index: usize) -> Option<Voice> {
        let voice = self.slots[index].take();
        if voice.is_some() {
            self.held -= 1;
        }
        voice
    }

    /// The next generated id, wrapping back to [`FIRST_VOICE_ID`] rather than
    /// overflowing into the negatives (which would take ~2 billion notes, but
    /// a panicking `process()` is not an option).
    fn next_voice_id(&mut self) -> i32 {
        let voice_id = self.next_voice_id;
        self.next_voice_id = if voice_id == i32::MAX {
            FIRST_VOICE_ID
        } else {
            voice_id.wrapping_add(1)
        };
        voice_id
    }

    fn next_serial(&mut self) -> u64 {
        let serial = self.next_serial;
        self.next_serial = serial.wrapping_add(1);
        serial
    }

    /// Borrows the table as a [`StepSink`], so the lane scan can be run
    /// straight through it:
    /// `scan_lanes(data, range, pattern, rng, &mut table.striking(&mut sink))`.
    pub fn striking<'a, S: EventSink>(&'a mut self, sink: &'a mut S) -> Striker<'a, S> {
        Striker { table: self, sink }
    }
}

impl Default for VoiceTable {
    fn default() -> Self {
        VoiceTable::new()
    }
}

/// The [`StepSink`] view of a [`VoiceTable`] writing into an [`EventSink`],
/// as returned by [`VoiceTable::striking`] — the glue that turns the scan's
/// step hits into note events.
pub struct Striker<'a, S: EventSink> {
    table: &'a mut VoiceTable,
    sink: &'a mut S,
}

impl<S: EventSink> StepSink for Striker<'_, S> {
    fn strike(&mut self, hit: StepHit) {
        self.table.strike(hit, self.sink);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Step;

    /// A hit of the shape the scan produces: a 50 %-gated note on `key`.
    fn hit(beat: f64, end_beat: f64, key: u8, channel: u8) -> StepHit {
        StepHit {
            beat,
            end_beat,
            lane_pos: 0,
            step_index: 0,
            key,
            channel,
            volume_db: 0,
            sample_pad: false,
            step: Step::default_on(),
        }
    }

    /// Collects the events a table emits, so the tests can assert on their
    /// exact order.
    #[derive(Default)]
    struct Recorder(Vec<SeqEvent>);

    impl EventSink for Recorder {
        fn emit(&mut self, event: SeqEvent) {
            self.0.push(event);
        }
    }

    impl Recorder {
        fn events(&self) -> &[SeqEvent] {
            &self.0
        }

        fn take(&mut self) -> Vec<SeqEvent> {
            std::mem::take(&mut self.0)
        }
    }

    /// `(is_note_on, beat, key, channel, voice_id)` — the shape the ordering
    /// assertions read most easily.
    fn shape(event: &SeqEvent) -> (bool, f64, u8, u8, i32) {
        (
            event.is_note_on(),
            event.beat(),
            event.key(),
            event.channel(),
            event.voice_id(),
        )
    }

    #[test]
    fn a_fresh_table_is_empty_and_allocates_its_slots_once() {
        let table = VoiceTable::new();
        assert!(table.is_empty());
        assert_eq!(table.len(), 0);
        assert!(!table.is_full());
        assert_eq!(table.capacity(), VOICE_CAPACITY);
        assert_eq!(table.slots.len(), VOICE_CAPACITY);
        assert_eq!(table.slots.capacity(), VOICE_CAPACITY);
        assert_eq!(table.steals(), 0);
        assert_eq!(table.iter().count(), 0);
    }

    /// The id range is part of the contract with the host: generated voices
    /// must never land on an id the input stream could carry.
    #[test]
    fn generated_voice_ids_start_at_one_shifted_by_twenty_and_increment() {
        let mut table = VoiceTable::new();
        let mut sink = Recorder::default();

        let ids: Vec<i32> = (0..4)
            .map(|i| table.strike(hit(i as f64, i as f64 + 0.5, 60 + i, 0), &mut sink))
            .collect();

        assert_eq!(
            ids,
            [
                FIRST_VOICE_ID,
                FIRST_VOICE_ID + 1,
                FIRST_VOICE_ID + 2,
                FIRST_VOICE_ID + 3
            ]
        );
        assert!(ids.iter().all(|&id| id >= FIRST_VOICE_ID));
        // Each note-on carries the id it was given.
        let on_ids: Vec<i32> = sink.events().iter().map(|e| e.voice_id()).collect();
        assert_eq!(on_ids, ids);
    }

    /// Ids wrap instead of overflowing `i32`, and they stay inside the
    /// generated range when they do.
    #[test]
    fn voice_ids_wrap_back_into_the_generated_range() {
        let mut table = VoiceTable::new();
        let mut sink = Recorder::default();
        table.next_voice_id = i32::MAX;

        assert_eq!(table.strike(hit(0.0, 0.5, 60, 0), &mut sink), i32::MAX);
        assert_eq!(
            table.strike(hit(0.0, 0.5, 61, 0), &mut sink),
            FIRST_VOICE_ID
        );
    }

    /// The retrigger rule: two lanes (or two overlapping ratchets) on one
    /// key+channel must produce off-then-on at one timing, never two stacked
    /// note-ons.
    #[test]
    fn retriggering_a_held_key_emits_its_note_off_before_the_new_note_on() {
        let mut table = VoiceTable::new();
        let mut sink = Recorder::default();

        let first = table.strike(hit(0.0, 1.0, 60, 0), &mut sink);
        sink.take();
        let second = table.strike(hit(0.25, 1.25, 60, 0), &mut sink);

        assert_ne!(first, second, "the new note needs its own voice id");
        assert_eq!(
            sink.events().iter().map(shape).collect::<Vec<_>>(),
            [
                // The old note is cut at the new note's beat, not at its own
                // end_beat (1.0), so the two never overlap.
                (false, 0.25, 60, 0, first),
                (true, 0.25, 60, 0, second),
            ]
        );
        // One key held means one voice, with the *new* gate.
        assert_eq!(table.len(), 1);
        assert_eq!(
            table.find(60, 0).map(|v| (v.voice_id(), v.end_beat())),
            Some((second, 1.25))
        );
        assert_eq!(table.steals(), 0, "a retrigger is not a steal");
    }

    /// Key and channel together identify a voice: the same key on another
    /// channel is a different note and must not retrigger.
    #[test]
    fn the_same_key_on_another_channel_is_a_separate_voice() {
        let mut table = VoiceTable::new();
        let mut sink = Recorder::default();

        let a = table.strike(hit(0.0, 1.0, 60, 0), &mut sink);
        sink.take();
        let b = table.strike(hit(0.0, 1.0, 60, 3), &mut sink);

        assert_eq!(
            sink.events().iter().map(shape).collect::<Vec<_>>(),
            [(true, 0.0, 60, 3, b)]
        );
        assert_eq!(table.len(), 2);
        assert_eq!(table.find(60, 0).map(Voice::voice_id), Some(a));
        assert_eq!(table.find(60, 3).map(Voice::voice_id), Some(b));
    }

    /// A key released by a drain is free again: striking it emits only the
    /// note-on.
    #[test]
    fn a_second_strike_at_the_same_beat_merges_into_the_held_voice() {
        let mut table = VoiceTable::new();
        let mut sink = Recorder::default();

        let first = table.strike(hit(1.0, 1.5, 60, 0), &mut sink);
        // A few ulps away, as a triplet lane computes "the same" beat.
        let second = table.strike(hit(1.0 + 1e-15, 2.0, 60, 0), &mut sink);

        assert_eq!(second, first, "the merged strike reports the held voice");
        let emitted: Vec<_> = sink.events().iter().map(shape).collect();
        assert_eq!(emitted, [(true, 1.0, 60, 0, first)]);
        assert_eq!(table.len(), 1);
        assert_eq!(table.find(60, 0).map(Voice::end_beat), Some(2.0), "the longer gate wins");
    }

    #[test]
    fn a_strike_before_the_held_voice_s_start_is_a_retrigger_not_a_merge() {
        // What a loop wrap looks like to the table: the strike's beat is the
        // loop start, earlier than the voice opened just before the loop end.
        let mut table = VoiceTable::new();
        let mut sink = Recorder::default();
        let held = table.strike(hit(3.9, 3.95, 60, 0), &mut sink);
        sink.take();

        let new = table.strike(hit(0.0, 0.05, 60, 0), &mut sink);
        assert_ne!(new, held);
        assert_eq!(sink.events().len(), 2, "a note-off for the held voice, then the new note-on");
        assert!(sink.events()[0].is_note_off());
    }

    #[test]
    fn striking_a_key_that_was_already_released_does_not_emit_a_note_off() {
        let mut table = VoiceTable::new();
        let mut sink = Recorder::default();

        table.strike(hit(0.0, 0.5, 60, 0), &mut sink);
        table.drain_ending(BeatRange::new(0.5, 1.0), &mut sink);
        sink.take();

        let voice_id = table.strike(hit(1.0, 1.5, 60, 0), &mut sink);
        assert_eq!(
            sink.events().iter().map(shape).collect::<Vec<_>>(),
            [(true, 1.0, 60, 0, voice_id)]
        );
        assert_eq!(table.len(), 1);
    }

    /// A strike landing *after* the held note's gate has closed releases it
    /// at that gate end, not at the strike — the engine drains gates once per
    /// block segment, so how much timeline one block covers must not be able
    /// to stretch a note.
    #[test]
    fn a_strike_past_a_held_gate_releases_it_at_the_gate_end() {
        let mut table = VoiceTable::new();
        let mut sink = Recorder::default();

        let first = table.strike(hit(0.0, 0.5, 60, 0), &mut sink);
        sink.take();
        // Same key, struck half a beat after the first note was due to end:
        // exactly what one big block looks like next to two small ones.
        let second = table.strike(hit(1.0, 1.5, 60, 0), &mut sink);

        assert_eq!(
            sink.events().iter().map(shape).collect::<Vec<_>>(),
            [(false, 0.5, 60, 0, first), (true, 1.0, 60, 0, second)]
        );
        // The same two strikes with a drain in between — the small-block
        // reading — produce exactly the same events.
        let mut split = VoiceTable::new();
        let mut split_sink = Recorder::default();
        split.strike(hit(0.0, 0.5, 60, 0), &mut split_sink);
        split_sink.take();
        split.drain_ending(BeatRange::new(0.25, 0.75), &mut split_sink);
        split.strike(hit(1.0, 1.5, 60, 0), &mut split_sink);
        assert_eq!(
            split_sink.events().iter().map(shape).collect::<Vec<_>>(),
            sink.events().iter().map(shape).collect::<Vec<_>>()
        );
        assert_eq!(table.steals(), 0);
        assert_eq!(table.len(), 1);
    }

    /// Overflow steals the *oldest* voice and emits its note-off first, so
    /// the host never loses track of it.
    #[test]
    fn overflow_steals_the_oldest_voice_and_emits_its_note_off() {
        let mut table = VoiceTable::new();
        let mut sink = Recorder::default();

        // Fill every slot with a distinct key+channel pair, so none of these
        // retrigger each other.
        let mut ids = Vec::new();
        for i in 0..VOICE_CAPACITY {
            let key = (i % 128) as u8;
            let channel = (i / 128) as u8;
            ids.push(table.strike(hit(i as f64 * 0.01, 100.0, key, channel), &mut sink));
        }
        assert!(table.is_full());
        assert_eq!(table.len(), VOICE_CAPACITY);
        sink.take();

        let stolen = ids[0];
        let fresh = table.strike(hit(9.0, 9.5, 127, 7), &mut sink);

        assert_eq!(
            sink.events().iter().map(shape).collect::<Vec<_>>(),
            [
                // Key 0 / channel 0 was struck first, so it is the oldest.
                (false, 9.0, 0, 0, stolen),
                (true, 9.0, 127, 7, fresh),
            ]
        );
        assert_eq!(
            table.len(),
            VOICE_CAPACITY,
            "the stolen slot was reused, not added to"
        );
        assert_eq!(table.steals(), 1);
        assert!(
            table.find(0, 0).is_none(),
            "the stolen voice is gone from the table"
        );
        assert_eq!(table.find(127, 7).map(Voice::voice_id), Some(fresh));

        // The next steal takes the second-oldest, not the same slot again.
        sink.take();
        table.strike(hit(10.0, 10.5, 126, 7), &mut sink);
        assert_eq!(sink.events()[0].voice_id(), ids[1]);
        assert_eq!(table.steals(), 2);
    }

    /// A retrigger on a full table reuses the retriggered voice's own slot,
    /// so it must not also steal somebody else's note.
    #[test]
    fn a_retrigger_on_a_full_table_does_not_steal() {
        let mut table = VoiceTable::new();
        let mut sink = Recorder::default();
        for i in 0..VOICE_CAPACITY {
            table.strike(
                hit(i as f64 * 0.01, 100.0, (i % 128) as u8, (i / 128) as u8),
                &mut sink,
            );
        }
        sink.take();

        let again = table.strike(hit(9.0, 9.5, 0, 0), &mut sink);

        assert_eq!(
            sink.events().len(),
            2,
            "exactly one note-off and one note-on"
        );
        assert!(sink.events()[0].is_note_off());
        assert_eq!(sink.events()[0].key(), 0);
        assert_eq!(sink.events()[1].voice_id(), again);
        assert_eq!(table.steals(), 0);
        assert_eq!(table.len(), VOICE_CAPACITY);
    }

    /// `drain_ending` releases exactly the voices whose gate closes in the
    /// half-open range, at their own `end_beat`.
    #[test]
    fn drain_ending_releases_only_the_voices_ending_in_the_half_open_range() {
        let mut table = VoiceTable::new();
        let mut sink = Recorder::default();

        // end_beat: below the range, at its start, inside, at its end, past it.
        let ends = [0.5, 1.0, 1.5, 2.0, 3.0];
        let ids: Vec<i32> = ends
            .iter()
            .enumerate()
            .map(|(i, &end)| table.strike(hit(0.0, end, 60 + i as u8, 0), &mut sink))
            .collect();
        sink.take();

        let released = table.drain_ending(BeatRange::new(1.0, 2.0), &mut sink);

        assert_eq!(released, 2);
        assert_eq!(
            sink.events().iter().map(shape).collect::<Vec<_>>(),
            [
                // Each note-off carries its own gate end, not the range's.
                (false, 1.0, 61, 0, ids[1]),
                (false, 1.5, 62, 0, ids[2]),
            ]
        );
        assert_eq!(table.len(), 3);
        assert!(table.find(61, 0).is_none() && table.find(62, 0).is_none());
        // Ending exactly at `range.end` waits for the next block, where it
        // lands on `range.start` — the half-open rule, so no double-off and
        // no lost off.
        sink.take();
        assert_eq!(table.drain_ending(BeatRange::new(2.0, 3.0), &mut sink), 1);
        assert_eq!(
            sink.events().iter().map(shape).collect::<Vec<_>>(),
            [(false, 2.0, 63, 0, ids[3])]
        );
    }

    /// A drain that matches nothing (the common case for a block with no
    /// gates closing in it) touches neither the table nor the sink.
    #[test]
    fn drain_ending_is_a_no_op_when_nothing_ends_in_the_range() {
        let mut table = VoiceTable::new();
        let mut sink = Recorder::default();
        table.strike(hit(0.0, 4.0, 60, 0), &mut sink);
        sink.take();

        assert_eq!(table.drain_ending(BeatRange::new(1.0, 2.0), &mut sink), 0);
        assert!(sink.events().is_empty());
        assert_eq!(table.len(), 1);
        assert_eq!(table.drain_ending(BeatRange::new(1.0, 1.0), &mut sink), 0);
    }

    /// A note that opens and closes inside one range is released by the drain
    /// the engine runs *after* the scan — the ordering the doc comment on
    /// `drain_ending` calls for.
    #[test]
    fn a_gate_closing_inside_its_own_range_is_drained_after_the_scan() {
        let mut table = VoiceTable::new();
        let mut sink = Recorder::default();
        let range = BeatRange::new(0.0, 1.0);

        assert_eq!(
            table.drain_ending(range, &mut sink),
            0,
            "nothing is held yet"
        );
        let voice_id = table.strike(hit(0.0, 0.125, 60, 0), &mut sink);
        assert_eq!(table.drain_ending(range, &mut sink), 1);

        assert_eq!(
            sink.events().iter().map(shape).collect::<Vec<_>>(),
            [
                (true, 0.0, 60, 0, voice_id),
                (false, 0.125, 60, 0, voice_id)
            ]
        );
        assert!(table.is_empty());
    }

    /// `flush_all` is what stop, jump, `reset()` and deactivate rely on: the
    /// table ends up empty and every held note has been released.
    #[test]
    fn flush_all_empties_the_table_and_releases_every_voice() {
        let mut table = VoiceTable::new();
        let mut sink = Recorder::default();

        let ids: Vec<i32> = (0..5)
            .map(|i| table.strike(hit(i as f64, 100.0 + i as f64, 60 + i as u8, 0), &mut sink))
            .collect();
        sink.take();

        let released = table.flush_all(7.5, &mut sink);

        assert_eq!(released, 5);
        assert!(table.is_empty());
        assert_eq!(table.len(), 0);
        assert_eq!(table.iter().count(), 0);
        // All at the flush beat, whatever each gate's own end was.
        assert_eq!(
            sink.events().iter().map(shape).collect::<Vec<_>>(),
            (0..5)
                .map(|i| (false, 7.5, 60 + i as u8, 0u8, ids[i]))
                .collect::<Vec<_>>()
        );

        // Flushing an empty table emits nothing, so a stopped transport does
        // not spray note-offs every block.
        sink.take();
        assert_eq!(table.flush_all(8.0, &mut sink), 0);
        assert!(sink.events().is_empty());
    }

    /// Slots freed by a flush are reusable, and the table keeps handing out
    /// fresh ids afterwards.
    #[test]
    fn the_table_is_reusable_after_a_flush() {
        let mut table = VoiceTable::new();
        let mut sink = Recorder::default();
        for i in 0..VOICE_CAPACITY {
            table.strike(hit(0.0, 1.0, (i % 128) as u8, (i / 128) as u8), &mut sink);
        }
        table.flush_all(1.0, &mut sink);
        sink.take();

        assert!(table.is_empty());
        let voice_id = table.strike(hit(2.0, 2.5, 60, 0), &mut sink);
        assert_eq!(voice_id, FIRST_VOICE_ID + VOICE_CAPACITY as i32);
        assert_eq!(table.len(), 1);
        assert_eq!(table.steals(), 0);
    }

    /// The [`StepSink`] adapter is how the engine will drive the table: hits
    /// in, note events out, retrigger rule included.
    #[test]
    fn the_striking_adapter_feeds_step_hits_through_the_table() {
        let mut table = VoiceTable::new();
        let mut sink = Recorder::default();

        {
            let mut striker = table.striking(&mut sink);
            striker.strike(hit(0.0, 0.5, 60, 0));
            striker.strike(hit(0.25, 0.75, 60, 0));
            striker.strike(hit(0.25, 0.75, 64, 0));
        }

        assert_eq!(table.len(), 2);
        let kinds: Vec<(bool, f64, u8)> = sink
            .events()
            .iter()
            .map(|e| (e.is_note_on(), e.beat(), e.key()))
            .collect();
        assert_eq!(
            kinds,
            [
                (true, 0.0, 60),
                (false, 0.25, 60),
                (true, 0.25, 60),
                (true, 0.25, 64)
            ]
        );
    }

    /// The note-on the table emits carries the hit's own payload — nothing
    /// here re-derives velocity or pan.
    #[test]
    fn the_note_on_carries_the_hits_step_payload() {
        let mut table = VoiceTable::new();
        let mut sink = Recorder::default();
        let mut hit = hit(1.5, 1.75, 42, 9);
        hit.lane_pos = 3;
        hit.step = Step::default_on().with_velocity(100).with_pan(0);

        let voice_id = table.strike(hit, &mut sink);

        assert_eq!(sink.events(), [hit.note_on(voice_id)]);
        assert_eq!(sink.events()[0].lane_pos(), Some(3));
    }

    /// Nothing on the hot path may allocate: all three mutating entry points
    /// leave the slot storage exactly as `new()` made it, even after the
    /// table has been filled, stolen from, drained and flushed.
    #[test]
    fn the_slot_storage_never_grows() {
        let mut table = VoiceTable::new();
        let mut sink = |_: SeqEvent| {};
        let ptr = table.slots.as_ptr();

        for i in 0..VOICE_CAPACITY * 3 {
            table.strike(
                hit(
                    i as f64 * 0.01,
                    i as f64 * 0.01 + 0.5,
                    (i % 128) as u8,
                    (i / 128) as u8,
                ),
                &mut sink,
            );
        }
        table.drain_ending(BeatRange::new(0.0, 100.0), &mut sink);
        table.flush_all(0.0, &mut sink);

        assert_eq!(table.slots.len(), VOICE_CAPACITY);
        assert_eq!(table.slots.capacity(), VOICE_CAPACITY);
        assert_eq!(
            table.slots.as_ptr(),
            ptr,
            "the slot storage was reallocated"
        );
        assert!(table.is_empty());
    }
}
