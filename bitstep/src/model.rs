//! The shared pattern store and its building blocks.
//!
//! This module is deliberately free of nice-plug types so it can be
//! unit-tested without the framework.

use std::mem::ManuallyDrop;
use std::sync::{Mutex, MutexGuard};
use std::sync::atomic::{AtomicBool, AtomicI8, AtomicU8, AtomicU32, AtomicU64, Ordering::Relaxed};

use basedrop::{Collector, Handle, Shared, SharedCell};

use crate::sequencer::{Prng, Rate};
use crate::state::SeqSnapshot;
use crate::undo::History;

/// One step of one lane, packed into a single `u32` so a step lives in one
/// `AtomicU32` and the audio thread can read it with a relaxed load.
///
/// Layout:
///
/// ```text
/// bit  0       on          (1 = active)
/// bits 1–7     velocity    1..=127
/// bits 8–14    pan         0..=126, 63 = centre
/// bits 15–17   ratchet     stored n-1 → effective 1..=8
/// bits 18–24   probability 0..=100 (%)
/// bits 25–30   pitch       -24..=24 semitones, 6-bit two's complement
/// bit  31      reserved    (future: gate, micro-timing, accent)
/// ```
///
/// Pitch is signed so that all-zero bits read as "no pitch change": a step
/// saved before pitch existed, and a never-edited step, both come back
/// unpitched with no migration.
///
/// `pack` writes pitch and the reserved bit as zero; every `with_*` setter
/// preserves the bits it does not own, so a build that learns a new field can
/// be read by this one without losing it on a read-modify-write.
///
/// The getters clamp into the documented range instead of trusting the bits.
/// A step can come from a hostile state snapshot or from an edit by a future
/// version, and the audio path must never see e.g. velocity 0 (which is a
/// note-off on the wire) or ratchet 0 (which would make a zero-length
/// subdivision). Round-tripping a value that was in range is unaffected.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Step(pub u32);

impl Step {
    /// Lowest MIDI velocity that still sounds; 0 means note-off on the wire.
    pub const VELOCITY_MIN: u8 = 1;
    pub const VELOCITY_MAX: u8 = 127;
    pub const PAN_MIN: u8 = 0;
    /// Kept even so that [`Step::PAN_CENTRE`] is exactly halfway, which makes
    /// the GUI's centre detent and the [-1, 1] note-expression mapping agree.
    pub const PAN_MAX: u8 = 126;
    pub const PAN_CENTRE: u8 = 63;
    pub const RATCHET_MIN: u8 = 1;
    pub const RATCHET_MAX: u8 = 8;
    pub const PROBABILITY_MAX: u8 = 100;
    /// Pitch offset in semitones, sent as a per-note tuning expression — the
    /// note number (and so the Drum Machine pad) never changes.
    pub const PITCH_MIN: i8 = -24;
    pub const PITCH_MAX: i8 = 24;

    const ON_SHIFT: u32 = 0;
    const VELOCITY_SHIFT: u32 = 1;
    const PAN_SHIFT: u32 = 8;
    const RATCHET_SHIFT: u32 = 15;
    const PROBABILITY_SHIFT: u32 = 18;
    const PITCH_SHIFT: u32 = 25;

    const ON_MASK: u32 = 0b1 << Self::ON_SHIFT;
    const VELOCITY_MASK: u32 = 0x7F << Self::VELOCITY_SHIFT;
    const PAN_MASK: u32 = 0x7F << Self::PAN_SHIFT;
    const RATCHET_MASK: u32 = 0b111 << Self::RATCHET_SHIFT;
    const PROBABILITY_MASK: u32 = 0x7F << Self::PROBABILITY_SHIFT;
    const PITCH_MASK: u32 = 0x3F << Self::PITCH_SHIFT;

    /// The fields that say how a note sounds when it is played. All zero means
    /// the step has never been switched on (or was cleared), whatever its
    /// pitch: pitch can be drawn on an off step ahead of time, and that must
    /// not stop [`Step::turned_on`] from installing the defaults.
    const NOTE_MASK: u32 = Self::ON_MASK
        | Self::VELOCITY_MASK
        | Self::PAN_MASK
        | Self::RATCHET_MASK
        | Self::PROBABILITY_MASK;

    /// Bit 31, owned by no field yet.
    pub const RESERVED_MASK: u32 = 0x8000_0000;

    /// An empty step. All bits zero, so a freshly zeroed `[AtomicU32]` is
    /// already a lane of off steps — nothing has to initialise it.
    pub const OFF: Step = Step(0);

    /// The step a fresh edit turns on with: on, velocity 100, centre pan, no
    /// ratchet, always fires.
    pub const fn default_on() -> Step {
        Step::pack(true, 100, Self::PAN_CENTRE, 1, Self::PROBABILITY_MAX)
    }

    /// Packs all five fields, clamping each and zeroing the reserved bits.
    pub const fn pack(on: bool, velocity: u8, pan: u8, ratchet: u8, probability: u8) -> Step {
        let on = (on as u32) << Self::ON_SHIFT;
        let velocity = (clamp_u8(velocity, Self::VELOCITY_MIN, Self::VELOCITY_MAX) as u32)
            << Self::VELOCITY_SHIFT;
        let pan = (clamp_u8(pan, Self::PAN_MIN, Self::PAN_MAX) as u32) << Self::PAN_SHIFT;
        // Stored as n-1 so the three bits cover 1..=8 exactly.
        let ratchet = ((clamp_u8(ratchet, Self::RATCHET_MIN, Self::RATCHET_MAX) - 1) as u32)
            << Self::RATCHET_SHIFT;
        let probability =
            (clamp_u8(probability, 0, Self::PROBABILITY_MAX) as u32) << Self::PROBABILITY_SHIFT;
        Step(on | velocity | pan | ratchet | probability)
    }

    /// The raw packed bits, for storing into an `AtomicU32`.
    pub const fn bits(self) -> u32 {
        self.0
    }

    pub const fn is_on(self) -> bool {
        self.0 & Self::ON_MASK != 0
    }

    pub const fn velocity(self) -> u8 {
        let raw = ((self.0 & Self::VELOCITY_MASK) >> Self::VELOCITY_SHIFT) as u8;
        clamp_u8(raw, Self::VELOCITY_MIN, Self::VELOCITY_MAX)
    }

    pub const fn pan(self) -> u8 {
        let raw = ((self.0 & Self::PAN_MASK) >> Self::PAN_SHIFT) as u8;
        clamp_u8(raw, Self::PAN_MIN, Self::PAN_MAX)
    }

    /// The effective ratchet count, 1..=8 — the number of sub-hits the step
    /// fires (`for j in 0..step.ratchet()` in the sequencer).
    pub const fn ratchet(self) -> u8 {
        (((self.0 & Self::RATCHET_MASK) >> Self::RATCHET_SHIFT) as u8) + 1
    }

    pub const fn probability(self) -> u8 {
        let raw = ((self.0 & Self::PROBABILITY_MASK) >> Self::PROBABILITY_SHIFT) as u8;
        clamp_u8(raw, 0, Self::PROBABILITY_MAX)
    }

    /// Pitch offset in semitones, `-24..=24`; 0 for a step that has none.
    pub const fn pitch(self) -> i8 {
        let raw = ((self.0 & Self::PITCH_MASK) >> Self::PITCH_SHIFT) as u8;
        // Sign-extend the 6-bit field: shift it to the top of a byte and back.
        let signed = ((raw << 2) as i8) >> 2;
        clamp_i8(signed, Self::PITCH_MIN, Self::PITCH_MAX)
    }

    /// Whether the step has never been switched on (or was cleared): every
    /// field that says how a note sounds is zero. Pitch doesn't count.
    pub const fn is_untouched(self) -> bool {
        self.0 & Self::NOTE_MASK == 0
    }

    pub const fn with_on(self, on: bool) -> Step {
        Step((self.0 & !Self::ON_MASK) | ((on as u32) << Self::ON_SHIFT))
    }

    /// Switches a step on, filling in the defaults if it has never been
    /// edited.
    ///
    /// A never-edited step is all-zero bits, which would read back as
    /// hard-left and never-firing, so turning *that* one on installs
    /// [`Step::default_on`] (keeping the reserved bits) instead. A step that
    /// was switched off keeps the values the user drew.
    pub const fn turned_on(self) -> Step {
        self.with_note_defaults().with_on(true)
    }

    /// A never-edited step with [`Step::default_on`]'s values filled in but
    /// left as it was (off); any other step unchanged.
    ///
    /// What a value drawn on an empty step ahead of time is written on top
    /// of, so the step's other fields are the defaults rather than zeros (a
    /// 0% chance would never play once the step is switched on). Pitch and
    /// the reserved bit survive, as in [`Step::turned_on`].
    pub const fn with_note_defaults(self) -> Step {
        if self.is_untouched() {
            let defaults = Step::default_on().bits() & !Self::ON_MASK;
            Step(defaults | (self.0 & !Self::NOTE_MASK))
        } else {
            self
        }
    }

    /// Wipes every field (pitch included) back to [`Step::OFF`], keeping the
    /// reserved bit — a cleared step is indistinguishable from one that was
    /// never edited, so drawing on it again gets the defaults from
    /// [`Step::turned_on`].
    pub const fn cleared(self) -> Step {
        Step(self.0 & Self::RESERVED_MASK)
    }

    pub const fn with_velocity(self, velocity: u8) -> Step {
        let velocity = clamp_u8(velocity, Self::VELOCITY_MIN, Self::VELOCITY_MAX) as u32;
        Step((self.0 & !Self::VELOCITY_MASK) | (velocity << Self::VELOCITY_SHIFT))
    }

    pub const fn with_pan(self, pan: u8) -> Step {
        let pan = clamp_u8(pan, Self::PAN_MIN, Self::PAN_MAX) as u32;
        Step((self.0 & !Self::PAN_MASK) | (pan << Self::PAN_SHIFT))
    }

    /// `ratchet` is the effective count 1..=8; it is stored as n-1.
    pub const fn with_ratchet(self, ratchet: u8) -> Step {
        let stored = (clamp_u8(ratchet, Self::RATCHET_MIN, Self::RATCHET_MAX) - 1) as u32;
        Step((self.0 & !Self::RATCHET_MASK) | (stored << Self::RATCHET_SHIFT))
    }

    pub const fn with_probability(self, probability: u8) -> Step {
        let probability = clamp_u8(probability, 0, Self::PROBABILITY_MAX) as u32;
        Step((self.0 & !Self::PROBABILITY_MASK) | (probability << Self::PROBABILITY_SHIFT))
    }

    /// `pitch` in semitones, clamped to `-24..=24`.
    pub const fn with_pitch(self, pitch: i8) -> Step {
        let stored = (clamp_i8(pitch, Self::PITCH_MIN, Self::PITCH_MAX) as u8 as u32) & 0x3F;
        Step((self.0 & !Self::PITCH_MASK) | (stored << Self::PITCH_SHIFT))
    }
}

/// `u8::clamp` is not `const`, and the packing routines (plus the sequencer's
/// velocity/pan mapping) are.
pub(crate) const fn clamp_u8(value: u8, min: u8, max: u8) -> u8 {
    if value < min {
        min
    } else if value > max {
        max
    } else {
        value
    }
}

/// [`clamp_u8`] for the signed pitch field.
pub(crate) const fn clamp_i8(value: i8, min: i8, max: i8) -> i8 {
    if value < min {
        min
    } else if value > max {
        max
    } else {
        value
    }
}

impl std::fmt::Debug for Step {
    /// Fields, not the raw `u32` — a failing pack/unpack assert is unreadable
    /// as a hex blob.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Step")
            .field("on", &self.is_on())
            .field("velocity", &self.velocity())
            .field("pan", &self.pan())
            .field("ratchet", &self.ratchet())
            .field("probability", &self.probability())
            .field("pitch", &self.pitch())
            .field("reserved", &((self.0 & Step::RESERVED_MASK) >> 31))
            .finish()
    }
}


/// Hard ceiling on lanes in one instance. Not a design limit — it exists so a
/// typo in a state file or a GUI drag can't ask for gigabytes. Every write
/// path clamps against it.
pub const MAX_LANES: usize = 256;

/// Hard ceiling on a lane's step capacity.
pub const MAX_STEPS: usize = 4096;

/// Pattern banks. Every lane has one [`LaneData`] per pattern.
pub const NUM_PATTERNS: usize = 64;

/// Step capacity is allocated in multiples of this, so dragging a lane's
/// length a few steps longer stays a value edit instead of reallocating
/// (and republishing) the whole `SeqData`.
pub const CAPACITY_CHUNK: usize = 16;

/// The capacity a lane gets for a wanted step count: at least one chunk, at
/// most [`MAX_STEPS`], otherwise rounded up to the next [`CAPACITY_CHUNK`]
/// multiple.
///
/// Used when growing a lane. [`LaneData::with_capacity`] deliberately does
/// *not* round, because a state load has to reproduce the saved capacity
/// exactly — hidden steps live past `length`, and a snapshot written by a
/// build with a different chunk size must still round-trip.
pub const fn chunked_capacity(steps: usize) -> usize {
    if steps <= CAPACITY_CHUNK {
        CAPACITY_CHUNK
    } else if steps >= MAX_STEPS {
        // MAX_STEPS is itself a chunk multiple, so this is also the rounded value.
        MAX_STEPS
    } else {
        steps.div_ceil(CAPACITY_CHUNK) * CAPACITY_CHUNK
    }
}

/// One instrument: a row in the grid, shared by all patterns.
///
/// The identity fields (`id`, `name`, `colour`) are plain values — changing
/// them is a shape edit, which republishes the whole `SeqData`. Everything
/// playback reads live is an atomic, so the GUI can change it while the audio
/// thread holds a `&Lane`.
///
/// The atomics are public because the shape-edit path builds a `Lane`
/// directly, but prefer the accessors: they clamp, and they pin the ordering
/// to `Relaxed` (a value torn between two blocks is harmless, and nothing on
/// this path synchronises anything else).
#[derive(Debug)]
pub struct Lane {
    /// Stable across reorders and never reused within a session, so GUI
    /// selection survives a `move_lane`.
    pub id: u32,
    pub name: String,
    /// `0xRRGGBB`, from the theme palette.
    pub colour: u32,
    pub key: AtomicU8,
    pub channel: AtomicU8,
    pub mute: AtomicBool,
    pub solo: AtomicBool,
    /// The lane's volume in whole dB, `VOLUME_MIN..=VOLUME_MAX`, 0 = as
    /// played. Sent with every note as a per-note volume expression.
    pub volume: AtomicI8,
    /// Whether the lane plays a keytracked sample whose root key is
    /// [`Lane::SAMPLE_ROOT`]: every note is then retuned by the gap between
    /// the lane's key and that root, so the sample sounds at its own pitch
    /// (plus the step's pitch) without setting a root key on the pad.
    pub sample_pad: AtomicBool,
}

impl Lane {
    pub const KEY_MAX: u8 = 127;
    pub const CHANNEL_MAX: u8 = 15;
    pub const VOLUME_MIN: i8 = -36;
    /// +12 dB: the CLAP volume expression's ceiling (a gain of 4).
    pub const VOLUME_MAX: i8 = 12;
    /// The root key a sample pad is tuned against: C3 as Bitwig names it,
    /// the root a sampler assumes when a file names none.
    pub const SAMPLE_ROOT: u8 = 60;
    /// Whether a lane the user creates (a fresh instance's lanes, `+ Lane`)
    /// starts as a sample pad: bitstep is mostly used with drum samples.
    /// [`Lane::new`] itself starts off, so a saved lane loads as it was saved.
    pub const SAMPLE_PAD_DEFAULT: bool = true;

    /// A lane with no steps anywhere — the caller pairs it with one
    /// [`LaneData`] per pattern (see [`SeqData::from_lanes`]).
    ///
    /// `key` and `channel` are clamped; a snapshot or a future build can
    /// hand us anything.
    pub fn new(id: u32, name: impl Into<String>, colour: u32, key: u8, channel: u8) -> Lane {
        Lane {
            id,
            name: name.into(),
            colour,
            key: AtomicU8::new(key.min(Self::KEY_MAX)),
            channel: AtomicU8::new(channel.min(Self::CHANNEL_MAX)),
            mute: AtomicBool::new(false),
            solo: AtomicBool::new(false),
            volume: AtomicI8::new(0),
            sample_pad: AtomicBool::new(false),
        }
    }

    /// The MIDI key this lane plays, clamped to 0..=127 on read as well as on
    /// write — the field is public, and a key of 200 on the wire is worse
    /// than a wrong note.
    pub fn key(&self) -> u8 {
        self.key.load(Relaxed).min(Self::KEY_MAX)
    }

    pub fn set_key(&self, key: u8) {
        self.key.store(key.min(Self::KEY_MAX), Relaxed);
    }

    pub fn channel(&self) -> u8 {
        self.channel.load(Relaxed).min(Self::CHANNEL_MAX)
    }

    pub fn set_channel(&self, channel: u8) {
        self.channel.store(channel.min(Self::CHANNEL_MAX), Relaxed);
    }

    /// The lane's volume in dB, clamped on read as well as on write.
    pub fn volume_db(&self) -> i8 {
        self.volume.load(Relaxed).clamp(Self::VOLUME_MIN, Self::VOLUME_MAX)
    }

    pub fn set_volume_db(&self, db: i8) {
        self.volume.store(db.clamp(Self::VOLUME_MIN, Self::VOLUME_MAX), Relaxed);
    }

    pub fn is_sample_pad(&self) -> bool {
        self.sample_pad.load(Relaxed)
    }

    pub fn set_sample_pad(&self, on: bool) {
        self.sample_pad.store(on, Relaxed);
    }

    pub fn is_muted(&self) -> bool {
        self.mute.load(Relaxed)
    }

    pub fn set_mute(&self, mute: bool) {
        self.mute.store(mute, Relaxed);
    }

    pub fn is_soloed(&self) -> bool {
        self.solo.load(Relaxed)
    }

    pub fn set_solo(&self, solo: bool) {
        self.solo.store(solo, Relaxed);
    }
}

impl Clone for Lane {
    /// Copies the current values out of the atomics — the shape-edit path
    /// builds its replacement `SeqData` this way.
    /// The `id` comes along, so `duplicate_lane` has to overwrite it.
    fn clone(&self) -> Lane {
        Lane {
            id: self.id,
            name: self.name.clone(),
            colour: self.colour,
            key: AtomicU8::new(self.key.load(Relaxed)),
            channel: AtomicU8::new(self.channel.load(Relaxed)),
            mute: AtomicBool::new(self.mute.load(Relaxed)),
            solo: AtomicBool::new(self.solo.load(Relaxed)),
            volume: AtomicI8::new(self.volume.load(Relaxed)),
            sample_pad: AtomicBool::new(self.sample_pad.load(Relaxed)),
        }
    }
}

/// One lane's content in one pattern: its steps plus the loop length and rate
/// that play them.
///
/// `steps` is never resized in place — growing capacity is a shape edit that
/// builds a new `LaneData`. That is what lets the audio thread read
/// `steps[i]` through a shared reference with no lock: the slice it is looking
/// at can't move under it.
pub struct LaneData {
    /// Steps in the loop, `1..=capacity`. Shrinking it hides steps without
    /// erasing them; read it through [`LaneData::length`], which enforces the
    /// invariant even if the raw field was stored out of range.
    pub length: AtomicU32,
    /// A [`Rate`] discriminant; read it through [`LaneData::rate`].
    pub rate: AtomicU8,
    /// `len()` is the capacity. Every entry is a packed [`Step`].
    pub steps: Box<[AtomicU32]>,
}

impl LaneData {
    /// The loop length a fresh lane plays — two bars of 1/16 steps.
    pub const DEFAULT_LENGTH: u32 = 32;

    /// The capacity a fresh lane is allocated with: just enough for
    /// [`LaneData::DEFAULT_LENGTH`], in whole [`CAPACITY_CHUNK`]s.
    pub const DEFAULT_CAPACITY: usize = chunked_capacity(Self::DEFAULT_LENGTH as usize);

    /// A fresh lane's content: 32 off steps, length 32, rate 1/16.
    pub fn empty() -> LaneData {
        LaneData::with_capacity(Self::DEFAULT_CAPACITY)
    }

    /// `capacity` steps, all off, length `min(DEFAULT_LENGTH, capacity)`,
    /// rate 1/16.
    ///
    /// The capacity is clamped to `1..=MAX_STEPS` but otherwise used exactly
    /// as asked, so a state load reproduces the saved capacity (and with it
    /// every hidden step) verbatim. Zero is lifted to one: `length` has to
    /// stay ≥ 1 so the audio thread's `k.rem_euclid(len)` can never divide by
    /// zero.
    pub fn with_capacity(capacity: usize) -> LaneData {
        let capacity = capacity.clamp(1, MAX_STEPS);
        let steps: Vec<AtomicU32> =
            (0..capacity).map(|_| AtomicU32::new(Step::OFF.bits())).collect();
        LaneData {
            length: AtomicU32::new(Self::DEFAULT_LENGTH.min(capacity as u32)),
            rate: AtomicU8::new(Rate::DEFAULT.to_u8()),
            steps: steps.into_boxed_slice(),
        }
    }

    /// How many steps are allocated, hidden ones included.
    pub fn capacity(&self) -> usize {
        self.steps.len()
    }

    /// Whether this is exactly what [`LaneData::empty`] makes: the default
    /// capacity of untouched steps, default length and rate. A state save
    /// leaves out trailing patterns made only of these, since a load
    /// recreates them.
    pub fn is_pristine(&self) -> bool {
        self.capacity() == Self::DEFAULT_CAPACITY
            && self.length.load(Relaxed) == Self::DEFAULT_LENGTH
            && self.rate() == Rate::DEFAULT
            && self.steps.iter().all(|cell| cell.load(Relaxed) == Step::OFF.bits())
    }

    /// The loop length, clamped into `1..=capacity`.
    ///
    /// This is the `len = min(length, capacity)` the audio thread is required
    /// to compute, done once here so no caller can forget it.
    pub fn length(&self) -> u32 {
        let capacity = self.capacity().min(u32::MAX as usize) as u32;
        self.length.load(Relaxed).clamp(1, capacity.max(1))
    }

    /// Sets the loop length, clamped into `1..=capacity`. Growing past the
    /// capacity is a shape edit (`SeqModel::ensure_capacity`), not this.
    pub fn set_length(&self, length: u32) {
        self.length.store(self.length_clamped(length), Relaxed);
    }

    fn length_clamped(&self, length: u32) -> u32 {
        let capacity = self.capacity().min(u32::MAX as usize) as u32;
        length.clamp(1, capacity.max(1))
    }

    pub fn rate(&self) -> Rate {
        Rate::from_u8(self.rate.load(Relaxed))
    }

    pub fn set_rate(&self, rate: Rate) {
        self.rate.store(rate.to_u8(), Relaxed);
    }

    /// The step at `index`, or [`Step::OFF`] if `index` is past the capacity.
    ///
    /// Out of range reads as silence rather than panicking: the GUI can race
    /// a shape edit that shrank this lane, and a missed repaint must not take
    /// the host down.
    pub fn step(&self, index: usize) -> Step {
        match self.steps.get(index) {
            Some(cell) => Step(cell.load(Relaxed)),
            None => Step::OFF,
        }
    }

    /// Stores a step; a no-op if `index` is past the capacity.
    pub fn set_step(&self, index: usize, step: Step) {
        if let Some(cell) = self.steps.get(index) {
            cell.store(step.bits(), Relaxed);
        }
    }

    /// Read-modify-write of one step — the shape of every step edit that
    /// keeps existing fields (toggle, set velocity, …).
    ///
    /// Not a compare-exchange loop, and it doesn't need to be: all writers are
    /// serialised by `SeqModel`'s writer mutex and the audio thread only ever
    /// reads.
    pub fn update_step(&self, index: usize, f: impl FnOnce(Step) -> Step) {
        if let Some(cell) = self.steps.get(index) {
            let step = f(Step(cell.load(Relaxed)));
            cell.store(step.bits(), Relaxed);
        }
    }
}

impl Clone for LaneData {
    /// Copies every step, hidden ones included — a shape edit must not lose
    /// the steps past `length`.
    fn clone(&self) -> LaneData {
        let steps: Vec<AtomicU32> =
            self.steps.iter().map(|cell| AtomicU32::new(cell.load(Relaxed))).collect();
        LaneData {
            length: AtomicU32::new(self.length.load(Relaxed)),
            rate: AtomicU8::new(self.rate.load(Relaxed)),
            steps: steps.into_boxed_slice(),
        }
    }
}

impl std::fmt::Debug for LaneData {
    /// Shape and settings only. The derived form prints every one of up to
    /// 4096 atomics, which makes a failing assert unreadable.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LaneData")
            .field("length", &self.length())
            .field("rate", &self.rate())
            .field("capacity", &self.capacity())
            .field("on_steps", &(0..self.capacity()).filter(|&i| self.step(i).is_on()).count())
            .finish()
    }
}

// ---- song mode ----

/// The most entries a song can list.
pub const MAX_SONG_ENTRIES: usize = 1024;

/// The longest a pattern counts as in song mode, in bars. Only a lane of
/// thousands of whole notes gets near it; it keeps song positions small.
pub const MAX_PATTERN_BARS: u32 = 4096;

/// The bar length (in beats) assumed until the host reports a time
/// signature: 4/4.
pub const DEFAULT_BAR_BEATS: f64 = 4.0;

/// Song mode: a list of patterns the user types (`1-2-1-3x2`), played one
/// after another along the host's timeline from bar 1.
///
/// Each entry plays its whole pattern — as many bars as its longest lane
/// needs, see [`PatternBars`] — and a rest (`0`) is one silent bar. After
/// the last entry the song either starts again from the first (`looped`,
/// the default) or goes silent.
/// `entries` is always what [`parse_song`] makes of `text`. The text is kept
/// as typed so the field shows what the user wrote, and it is what a save
/// stores. Turning song mode or looping on or off is a value edit; changing
/// the list is a shape edit, since it reallocates `entries`.
#[derive(Debug)]
pub struct Song {
    pub on: AtomicBool,
    pub looped: AtomicBool,
    pub text: String,
    /// One entry per pattern played, in order: a pattern index, or
    /// [`Song::REST`].
    pub entries: Vec<u8>,
}

/// Where one song entry sits on the host's timeline.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SongPos {
    /// Which entry (0 = the first).
    pub entry: usize,
    /// The bar it starts on (0 = the host's first bar).
    pub start_bar: i64,
    /// The bar this pass through the song started on: 0 the first time, a
    /// multiple of the song's length once a looped song has come round.
    pub lap_start: i64,
    /// How many bars it lasts, at least 1.
    pub bars: u32,
    /// Its pattern, or `None` for a rest.
    pub pattern: Option<usize>,
}

impl SongPos {
    /// The bar after its last one.
    pub fn end_bar(&self) -> i64 {
        self.start_bar + i64::from(self.bars)
    }
}

impl Song {
    /// An entry that plays nothing for one bar (typed as `0`).
    pub const REST: u8 = u8::MAX;

    /// Off, looped, with an empty list.
    pub fn new() -> Song {
        Song::from_text(String::new(), false)
    }

    /// A song from typed text. Text that does not parse — only possible from
    /// a hostile or hand-edited project file — keeps the text and plays
    /// nothing, so the user sees what was there and can fix it.
    pub fn from_text(text: String, on: bool) -> Song {
        let entries = parse_song(&text).unwrap_or_default();
        Song { on: AtomicBool::new(on), looped: AtomicBool::new(true), text, entries }
    }

    /// The same song, looped or not.
    pub fn with_looped(self, looped: bool) -> Song {
        self.set_looped(looped);
        self
    }

    pub fn is_on(&self) -> bool {
        self.on.load(Relaxed)
    }

    pub fn set_on(&self, on: bool) {
        self.on.store(on, Relaxed);
    }

    /// Whether the song starts again after its last entry.
    pub fn is_looped(&self) -> bool {
        self.looped.load(Relaxed)
    }

    pub fn set_looped(&self, looped: bool) {
        self.looped.store(looped, Relaxed);
    }

    /// How many entries the song lists.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The pattern an entry plays, or `None` for a rest.
    pub fn pattern_of(entry: u8) -> Option<usize> {
        let pattern = usize::from(entry);
        (pattern < NUM_PATTERNS).then_some(pattern)
    }

    /// Every entry's place on the timeline, in order. Allocation-free, so
    /// the audio thread can walk it.
    pub fn positions<'a>(&'a self, lengths: &'a PatternBars) -> impl Iterator<Item = SongPos> + 'a {
        let mut start_bar = 0i64;
        self.entries.iter().enumerate().map(move |(entry, &value)| {
            let bars = lengths.of(value);
            let pattern = Song::pattern_of(value);
            let pos = SongPos { entry, start_bar, lap_start: 0, bars, pattern };
            start_bar = pos.end_bar();
            pos
        })
    }

    /// The entry playing at `bar`, or `None` before the start, or past the
    /// end of a song that does not loop. A looped song's later passes come
    /// back with their bars moved on by whole laps.
    pub fn locate(&self, bar: i64, lengths: &PatternBars) -> Option<SongPos> {
        if bar < 0 {
            return None;
        }
        let lap_start = match self.is_looped() {
            true => {
                let total = self.total_bars(lengths);
                (total > 0).then(|| bar - bar % total)?
            }
            false => 0,
        };
        let local = bar - lap_start;
        let pos = self.positions(lengths).find(|pos| local < pos.end_bar())?;
        Some(SongPos { start_bar: pos.start_bar + lap_start, lap_start, ..pos })
    }

    /// The whole song's length in bars.
    pub fn total_bars(&self, lengths: &PatternBars) -> i64 {
        self.positions(lengths).last().map_or(0, |pos| pos.end_bar())
    }
}

/// How many bars each pattern lasts in song mode: its longest lane, rounded
/// up to whole bars, and never less than one. Lanes shorter than that loop
/// inside it, as they do outside song mode. Lengths are live (a lane's
/// length and rate are value edits), so this is worked out afresh for each
/// block, and only for the patterns the song uses.
#[derive(Clone, Copy, Debug)]
pub struct PatternBars {
    bars: [u32; NUM_PATTERNS],
}

// `used` below is one bit per pattern.
const _: () = assert!(NUM_PATTERNS <= 64);

impl PatternBars {
    /// The lengths of the song's patterns in `bar_beats`-long bars.
    pub fn new(data: &SeqData, bar_beats: f64) -> PatternBars {
        let used = data
            .song
            .entries
            .iter()
            .filter_map(|&entry| Song::pattern_of(entry))
            .fold(0u64, |used, pattern| used | 1 << pattern);
        let bars = std::array::from_fn(|pattern| match used & 1 << pattern {
            0 => 1,
            _ => pattern_bars(&data.patterns[pattern], bar_beats),
        });
        PatternBars { bars }
    }

    /// How many bars an entry lasts: its pattern's length, or 1 for a rest.
    pub fn of(&self, entry: u8) -> u32 {
        self.bars.get(usize::from(entry)).copied().unwrap_or(1)
    }
}

/// Bars of `bar_beats` needed to hold a pattern's longest lane.
fn pattern_bars(lanes: &[LaneData], bar_beats: f64) -> u32 {
    let beats = lanes
        .iter()
        .map(|lane| f64::from(lane.length()) * lane.rate().rate_beats())
        .fold(0.0, f64::max);
    // Triplet steps are not exact binary fractions, so a lane that fills
    // its bars exactly can add up to a hair over; that must not cost a bar.
    let bars = (beats / bar_beats - 1e-9).ceil();
    match bars.is_finite() && bars >= 1.0 {
        true => bars.min(f64::from(MAX_PATTERN_BARS)) as u32,
        false => 1,
    }
}

impl Default for Song {
    fn default() -> Song {
        Song::new()
    }
}

impl Clone for Song {
    fn clone(&self) -> Song {
        Song {
            on: AtomicBool::new(self.is_on()),
            looped: AtomicBool::new(self.is_looped()),
            text: self.text.clone(),
            entries: self.entries.clone(),
        }
    }
}

/// Why a typed song order was refused.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SongError {
    /// A piece that is not a number or a `NxR` repeat.
    NotANumber(String),
    /// A pattern number past the last pattern.
    NoSuchPattern(u32),
    /// A repeat count of 0 (`3x0`).
    ZeroRepeat(String),
    /// More than [`MAX_SONG_ENTRIES`] entries.
    TooLong,
}

impl std::fmt::Display for SongError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SongError::NotANumber(token) => write!(f, "\"{token}\" is not a pattern number"),
            SongError::NoSuchPattern(number) => {
                write!(f, "There is no pattern {number} (1-{NUM_PATTERNS}, or 0 for silence)")
            }
            SongError::ZeroRepeat(token) => write!(f, "\"{token}\" repeats zero times"),
            SongError::TooLong => write!(f, "A song can list at most {MAX_SONG_ENTRIES} patterns"),
        }
    }
}

/// Reads a typed song order into its entries.
///
/// Pattern numbers are one-based, as on the pattern buttons, and `0` is a
/// silent bar. Pieces are separated by `-`, `,` or spaces, and `3x4` (or
/// `3*4`) is pattern 3 four times over. Empty text is an empty song.
pub fn parse_song(text: &str) -> Result<Vec<u8>, SongError> {
    let mut entries = Vec::new();
    let pieces = text.split(|c: char| c == '-' || c == ',' || c.is_whitespace());
    for token in pieces.filter(|token| !token.is_empty()) {
        let (number, repeat) = match token.split_once(['x', 'X', '*']) {
            Some((number, repeat)) => (number, Some(repeat)),
            None => (token, None),
        };
        let not_a_number = || SongError::NotANumber(token.to_owned());
        let number: u32 = number.parse().map_err(|_| not_a_number())?;
        let repeat: usize = match repeat {
            Some(repeat) => repeat.parse().map_err(|_| not_a_number())?,
            None => 1,
        };
        if repeat == 0 {
            return Err(SongError::ZeroRepeat(token.to_owned()));
        }
        let entry = match number {
            0 => Song::REST,
            n if n as usize <= NUM_PATTERNS => (n - 1) as u8,
            n => return Err(SongError::NoSuchPattern(n)),
        };
        if repeat > MAX_SONG_ENTRIES - entries.len() {
            return Err(SongError::TooLong);
        }
        entries.extend(std::iter::repeat_n(entry, repeat));
    }
    Ok(entries)
}

/// The whole sequence: the lane list plus, for each of the patterns, one
/// [`LaneData`] per lane, and the song.
///
/// `patterns[p][i]` always belongs to `lanes[i]` — [`SeqData::new`] is the
/// only constructor that accepts pattern vectors, and it repairs that
/// alignment, so no reader has to check it.
#[derive(Debug)]
pub struct SeqData {
    pub lanes: Vec<Lane>,
    pub patterns: [Vec<LaneData>; NUM_PATTERNS],
    pub song: Song,
}

impl SeqData {
    /// No lanes at all. The fresh-instance default (4 named drum lanes) is
    /// [`crate::state::default_data`], with the rest of the state defaults.
    pub fn empty() -> SeqData {
        SeqData {
            lanes: Vec::new(),
            patterns: std::array::from_fn(|_| Vec::new()),
            song: Song::new(),
        }
    }

    /// The given lanes, each with an empty [`LaneData`] in every pattern.
    /// Lanes past [`MAX_LANES`] are dropped.
    pub fn from_lanes(lanes: Vec<Lane>) -> SeqData {
        SeqData::new(lanes, std::array::from_fn(|_| Vec::new()))
    }

    /// Lanes and patterns as given, with the caps and the index alignment
    /// enforced: lanes past [`MAX_LANES`] are dropped, and every pattern is
    /// truncated or padded with empty [`LaneData`] to `lanes.len()`.
    ///
    /// Both repairs are silent on purpose — this is the single funnel for the
    /// shape-edit and state-load paths,
    /// and a project file that disagrees with itself should open with as much
    /// of the user's work as can be salvaged, not refuse to load.
    pub fn new(mut lanes: Vec<Lane>, mut patterns: [Vec<LaneData>; NUM_PATTERNS]) -> SeqData {
        lanes.truncate(MAX_LANES);
        for pattern in &mut patterns {
            pattern.truncate(lanes.len());
            while pattern.len() < lanes.len() {
                pattern.push(LaneData::empty());
            }
        }
        SeqData { lanes, patterns, song: Song::new() }
    }

    /// The same sequence with `song` as its song.
    pub fn with_song(mut self, song: Song) -> SeqData {
        self.song = song;
        self
    }

    pub fn lane_count(&self) -> usize {
        self.lanes.len()
    }

    /// The lane at a grid position, or `None` — the GUI may ask about a lane a
    /// shape edit has just removed.
    pub fn lane(&self, pos: usize) -> Option<&Lane> {
        self.lanes.get(pos)
    }

    /// The current position of a lane id, or `None` if it is gone.
    pub fn position_of(&self, id: u32) -> Option<usize> {
        self.lanes.iter().position(|lane| lane.id == id)
    }

    /// One lane's content in one pattern, or `None` if either index is out of
    /// range.
    pub fn lane_data(&self, pattern: usize, pos: usize) -> Option<&LaneData> {
        self.patterns.get(pattern)?.get(pos)
    }

    /// The highest lane id present, for restoring `next_lane_id` after a state
    /// load so new lanes can't collide with loaded ones.
    pub fn max_lane_id(&self) -> Option<u32> {
        self.lanes.iter().map(|lane| lane.id).max()
    }

    /// Whether any lane is soloed, which is what turns the per-lane scan's
    /// solo rule on. One pass here instead of one per lane.
    pub fn any_solo(&self) -> bool {
        self.lanes.iter().any(Lane::is_soloed)
    }
}

impl Clone for SeqData {
    /// A deep copy with every atomic's current value — the starting point of
    /// a shape edit.
    fn clone(&self) -> SeqData {
        SeqData {
            lanes: self.lanes.clone(),
            patterns: std::array::from_fn(|p| self.patterns[p].clone()),
            song: self.song.clone(),
        }
    }
}

/// The id the first lane of a fresh instance gets. Ids start at 1 so the GUI
/// can use 0 as "no lane selected".
pub const FIRST_LANE_ID: u32 = 1;

/// The one owner of the concurrency scheme: the audio thread reads the
/// published [`SeqData`] lock-free, every writer — GUI and host state load
/// alike — goes through here.
///
/// Two edit kinds share one writer mutex:
///
/// * **value edits** store into the atomics of the *currently published*
///   `SeqData`, so a reader that is mid-block sees them immediately and
///   nothing is allocated or freed;
/// * **shape edits** (anything that changes the lane list or a step slice's
///   length) build a replacement `SeqData`, publish it with
///   `SharedCell::set`, and leave the old one to [`SeqModel::collect`].
///
/// The mutex is what makes those two safe together: without it a value edit
/// could land on a `SeqData` that a concurrent shape edit is about to drop,
/// and the edit would silently vanish. The audio thread never takes it.
pub struct SeqModel {
    /// Dropped by hand in [`SeqModel`]'s `Drop` so the published `SeqData`
    /// reaches the collector's queue *before* the collector is torn down.
    cell: ManuallyDrop<SharedCell<SeqData>>,
    /// Allocates every replacement `SeqData`. Also dropped by hand, for the
    /// same reason: `Collector::try_cleanup` refuses while a handle is live.
    handle: ManuallyDrop<Handle>,
    /// Serialises all writers. Holds no data — the data it protects is the
    /// published `SeqData` and the `cell` pointer.
    writer: Mutex<()>,
    /// `Collector::collect` needs `&mut`, and the collector has to be moved
    /// out in `Drop` to be cleaned up, hence the `Option`.
    collector: Mutex<Option<Collector>>,
    next_lane_id: AtomicU32,
    /// Bumped by every write (see [`SeqModel::edit_lock`]), so the undo
    /// history can tell whether anything changed without snapshotting.
    edits: AtomicU64,
    /// Undo and redo, as whole-sequence snapshots. GUI and host-load side
    /// only; the audio thread never takes it. Always locked *before* the
    /// writer mutex, never while holding it.
    history: Mutex<History<SeqSnapshot>>,
}

impl SeqModel {
    /// Publishes `data` as the starting sequence.
    ///
    /// `next_lane_id` resumes above the highest id in `data`, so lanes added
    /// after a state load can't collide with loaded ones.
    pub fn new(data: SeqData) -> SeqModel {
        let collector = Collector::new();
        let handle = collector.handle();
        let next_lane_id = data.max_lane_id().map_or(FIRST_LANE_ID, |id| id.saturating_add(1));
        let cell = SharedCell::new(Shared::new(&handle, data));
        SeqModel {
            cell: ManuallyDrop::new(cell),
            handle: ManuallyDrop::new(handle),
            writer: Mutex::new(()),
            collector: Mutex::new(Some(collector)),
            next_lane_id: AtomicU32::new(next_lane_id),
            edits: AtomicU64::new(0),
            history: Mutex::new(History::new()),
        }
    }

    /// A model with no lanes. A fresh plugin instance starts from
    /// `SeqModel::new(crate::state::default_data())` instead.
    pub fn empty() -> SeqModel {
        SeqModel::new(SeqData::empty())
    }

    /// The audio thread's single entry point: one `get()` at the top of the
    /// block, then that reference for the whole block.
    ///
    /// Does not allocate, lock or block — it is a pair of atomic increments.
    pub fn shared(&self) -> Shared<SeqData> {
        self.cell.get()
    }

    /// Frees the `SeqData`s that shape edits replaced, once no reader holds
    /// them any more. Call it every GUI frame; shape edits also call it, so an
    /// instance with no editor open still reclaims.
    ///
    /// Never call this from the audio thread — it drops memory.
    pub fn collect(&self) {
        if let Some(collector) = self.collector_lock().as_mut() {
            collector.collect();
        }
    }

    /// The whole sequence in serialisable form, for the host's state save.
    ///
    /// Allocates, so main thread only. Takes the writer lock: a save that
    /// landed in the middle of someone else's edit could otherwise store a
    /// lane list from before a shape edit next to step values from after it.
    pub fn snapshot(&self) -> SeqSnapshot {
        let _writer = self.writer_lock();
        SeqSnapshot::from_data(&self.cell.get())
    }

    // ---- value edits: atomic stores on the published SeqData ----

    /// Replaces one step. The step is stored verbatim, so build it from the
    /// step you read (`step.with_velocity(..)`) to carry its reserved bits
    /// along.
    ///
    /// Returns `false` if the pattern, lane or step index does not exist — the
    /// GUI may be a frame behind a removal.
    pub fn set_step(&self, pattern: usize, lane_id: u32, index: usize, step: Step) -> bool {
        self.with_lane_data(pattern, lane_id, |lane_data| {
            if index < lane_data.capacity() {
                lane_data.set_step(index, step);
                true
            } else {
                false
            }
        })
    }

    /// Read-modify-write of one step under the writer lock — the shape of
    /// every edit that keeps the step's other fields.
    pub fn update_step(
        &self,
        pattern: usize,
        lane_id: u32,
        index: usize,
        f: impl FnOnce(Step) -> Step,
    ) -> bool {
        self.with_lane_data(pattern, lane_id, |lane_data| {
            if index < lane_data.capacity() {
                lane_data.update_step(index, f);
                true
            } else {
                false
            }
        })
    }

    /// Flips a step on or off, keeping its velocity/pan/ratchet/probability
    /// ([`Step::turned_on`] fills in a never-edited one).
    pub fn toggle_step(&self, pattern: usize, lane_id: u32, index: usize) -> bool {
        self.update_step(pattern, lane_id, index, |step| {
            if step.is_on() { step.with_on(false) } else { step.turned_on() }
        })
    }

    /// Sets a lane's loop length in one pattern, clamped to `1..=capacity`.
    /// Growing past the capacity needs [`SeqModel::ensure_capacity`] first.
    pub fn set_length(&self, pattern: usize, lane_id: u32, length: u32) -> bool {
        self.with_lane_data(pattern, lane_id, |lane_data| {
            lane_data.set_length(length);
            true
        })
    }

    pub fn set_rate(&self, pattern: usize, lane_id: u32, rate: Rate) -> bool {
        self.with_lane_data(pattern, lane_id, |lane_data| {
            lane_data.set_rate(rate);
            true
        })
    }

    pub fn set_key(&self, lane_id: u32, key: u8) -> bool {
        self.with_lane(lane_id, |lane| lane.set_key(key))
    }

    pub fn set_channel(&self, lane_id: u32, channel: u8) -> bool {
        self.with_lane(lane_id, |lane| lane.set_channel(channel))
    }

    pub fn set_mute(&self, lane_id: u32, mute: bool) -> bool {
        self.with_lane(lane_id, |lane| lane.set_mute(mute))
    }

    pub fn set_volume(&self, lane_id: u32, db: i8) -> bool {
        self.with_lane(lane_id, |lane| lane.set_volume_db(db))
    }

    pub fn set_sample_pad(&self, lane_id: u32, on: bool) -> bool {
        self.with_lane(lane_id, |lane| lane.set_sample_pad(on))
    }

    pub fn set_solo(&self, lane_id: u32, solo: bool) -> bool {
        self.with_lane(lane_id, |lane| lane.set_solo(solo))
    }

    /// Turns song mode on or off.
    pub fn set_song_on(&self, on: bool) {
        let _writer = self.edit_lock();
        self.cell.get().song.set_on(on);
    }

    /// Chooses whether the song starts again after its last entry.
    pub fn set_song_looped(&self, looped: bool) {
        let _writer = self.edit_lock();
        self.cell.get().song.set_looped(looped);
    }

    // ---- shape edits: build a new SeqData and publish it ----

    /// Appends a lane with an empty [`LaneData`] in every pattern and returns
    /// its fresh id, or `None` if the instance already holds [`MAX_LANES`].
    /// The new lane is a sample pad if [`Lane::SAMPLE_PAD_DEFAULT`] says so.
    pub fn add_lane(
        &self,
        name: impl Into<String>,
        colour: u32,
        key: u8,
        channel: u8,
    ) -> Option<u32> {
        let name = name.into();
        self.shape_edit(|data| {
            if data.lane_count() >= MAX_LANES {
                return None;
            }
            let id = self.take_lane_id();
            let lane = Lane::new(id, name, colour, key, channel);
            lane.set_sample_pad(Lane::SAMPLE_PAD_DEFAULT);
            data.lanes.push(lane);
            for pattern in &mut data.patterns {
                pattern.push(LaneData::empty());
            }
            Some(id)
        })
    }

    /// Copies every lane's steps, length and rate in pattern `from` over
    /// pattern `to`, hidden steps included. Lane settings are shared by all
    /// patterns, so they need no copying. `false` (and nothing published) if
    /// either pattern is out of range or they are the same.
    pub fn copy_pattern(&self, from: usize, to: usize) -> bool {
        if from == to || from >= NUM_PATTERNS || to >= NUM_PATTERNS {
            return false;
        }
        self.shape_edit(|data| {
            let copied = data.patterns[from].clone();
            data.patterns[to] = copied;
            Some(())
        })
        .is_some()
    }

    /// Replaces the song order with typed `text`. Text that does not parse
    /// changes nothing and says why. Returns the number of entries.
    pub fn set_song(&self, text: &str) -> Result<usize, SongError> {
        let entries = parse_song(text)?;
        let count = entries.len();
        self.shape_edit(|data| {
            data.song.text = text.to_owned();
            data.song.entries = entries;
            Some(())
        });
        Ok(count)
    }

    /// Drops a lane and its content in every pattern. `false` if the id is
    /// gone. Safe while the lane is sounding — note lifetimes belong to the
    /// voice table, not the lane.
    pub fn remove_lane(&self, lane_id: u32) -> bool {
        self.shape_edit(|data| {
            let pos = data.position_of(lane_id)?;
            data.lanes.remove(pos);
            for pattern in &mut data.patterns {
                if pos < pattern.len() {
                    pattern.remove(pos);
                }
            }
            Some(())
        })
        .is_some()
    }

    /// Moves a lane to `new_pos` (clamped to the last position), carrying its
    /// content in every pattern with it. Ids do not change, so GUI selection
    /// survives the reorder.
    pub fn move_lane(&self, lane_id: u32, new_pos: usize) -> bool {
        self.shape_edit(|data| {
            let pos = data.position_of(lane_id)?;
            let new_pos = new_pos.min(data.lane_count() - 1);
            let lane = data.lanes.remove(pos);
            data.lanes.insert(new_pos, lane);
            for pattern in &mut data.patterns {
                if pos < pattern.len() {
                    let lane_data = pattern.remove(pos);
                    pattern.insert(new_pos.min(pattern.len()), lane_data);
                }
            }
            Some(())
        })
        .is_some()
    }

    /// Inserts a copy of a lane — settings and every step in every pattern,
    /// hidden ones included — directly below it, with a fresh id.
    ///
    /// The name is copied as-is; the GUI offers rename right next to
    /// duplicate.
    pub fn duplicate_lane(&self, lane_id: u32) -> Option<u32> {
        self.shape_edit(|data| {
            let pos = data.position_of(lane_id)?;
            if data.lane_count() >= MAX_LANES {
                return None;
            }
            let id = self.take_lane_id();
            let mut lane = data.lanes[pos].clone();
            lane.id = id;
            data.lanes.insert(pos + 1, lane);
            for pattern in &mut data.patterns {
                let copy = pattern.get(pos).map_or_else(LaneData::empty, LaneData::clone);
                pattern.insert((pos + 1).min(pattern.len()), copy);
            }
            Some(id)
        })
    }

    pub fn rename_lane(&self, lane_id: u32, name: impl Into<String>) -> bool {
        let name = name.into();
        self.shape_edit(|data| {
            let pos = data.position_of(lane_id)?;
            data.lanes[pos].name = name;
            Some(())
        })
        .is_some()
    }

    /// Sets a lane's `0xRRGGBB` colour.
    pub fn set_colour(&self, lane_id: u32, colour: u32) -> bool {
        self.shape_edit(|data| {
            let pos = data.position_of(lane_id)?;
            data.lanes[pos].colour = colour;
            Some(())
        })
        .is_some()
    }

    /// Makes sure a lane can hold `steps` steps in one pattern, growing its
    /// capacity in [`CAPACITY_CHUNK`] chunks (and never past [`MAX_STEPS`]).
    ///
    /// Capacity belongs to the [`LaneData`], so this grows the one pattern
    /// asked for; the GUI's end-handle drag grows the pattern it is showing.
    /// Growing copies every existing step across, hidden ones included, and
    /// keeps `length` and `rate`. Already-big-enough is a cheap `true` with no
    /// republish, which is what keeps a length drag inside one chunk a value
    /// edit.
    ///
    /// Returns `false` only if the pattern or lane does not exist.
    pub fn ensure_capacity(&self, pattern: usize, lane_id: u32, steps: usize) -> bool {
        let wanted = steps.clamp(1, MAX_STEPS);
        let _writer = self.edit_lock();
        // Scoped so this method's reference to the current data is gone before
        // the replacement is published: while it lives, the retired copy's
        // reference count never reaches zero and `collect()` cannot reclaim it.
        let next = {
            let current = self.cell.get();
            let Some(pos) = current.position_of(lane_id) else {
                return false;
            };
            let Some(old) = current.lane_data(pattern, pos) else {
                return false;
            };
            if old.capacity() >= wanted {
                return true;
            }

            let grown = LaneData::with_capacity(chunked_capacity(wanted));
            for index in 0..old.capacity() {
                grown.set_step(index, old.step(index));
            }
            grown.set_length(old.length());
            grown.set_rate(old.rate());

            let mut next = (*current).clone();
            let Some(slot) = next.patterns.get_mut(pattern).and_then(|lanes| lanes.get_mut(pos))
            else {
                return false;
            };
            *slot = grown;
            next
        };
        self.publish(next);
        true
    }

    /// Replaces the whole sequence with a loaded one — the host handing back
    /// a saved project.
    ///
    /// A shape edit like any other: `SeqSnapshot::into_data` applies every
    /// load rule (caps, clamping, pattern realignment, id repair) while
    /// building the replacement *outside* the published data, which is then
    /// swapped in with one `SharedCell::set`. An audio thread mid-block keeps
    /// reading the sequence it started the block with and picks the new one up
    /// at the next `shared()`.
    ///
    /// The writer mutex is what makes this safe next to a GUI edit: a value
    /// edit either lands on the outgoing data (and is discarded with it — the
    /// load wins, as the user asked) or waits and lands on the loaded data.
    /// Never half on each.
    ///
    /// The undo history is cleared: what the host loaded is a new starting
    /// point, not an edit to step back out of.
    pub fn load_snapshot(&self, snapshot: SeqSnapshot) {
        // Built before the lock is taken: it allocates, and nothing in it
        // touches the published data.
        let data = snapshot.into_data();
        let mut history = self.history_lock();
        self.install(data);
        history.clear();
    }

    /// Publishes a whole replacement sequence: the shared half of a host load
    /// and an undo.
    fn install(&self, data: SeqData) {
        let resume = data.max_lane_id().map_or(FIRST_LANE_ID, |id| id.saturating_add(1));
        let _writer = self.edit_lock();
        // `fetch_max`, not `store`: ids are never reused within a session, so
        // a snapshot carrying lower ids than this instance has already handed
        // out must not let the next lane take an id some GUI state still
        // means by an older lane.
        self.next_lane_id.fetch_max(resume, Relaxed);
        self.publish(data);
    }

    // ---- undo and redo ----
    //
    // Every write bumps `edits`; the editor calls `commit_edits` whenever the
    // user is between gestures, and whatever changed since the last commit
    // becomes one undo step (see `crate::undo`). Mute and solo are left out:
    // they are switched while playing, and undoing a step edit must not
    // un-mute a lane. They are blanked in every stored state and kept as they
    // are on a restore.

    /// Records the edits made since the last call as one undo step. Cheap
    /// when nothing changed (one atomic load); a snapshot otherwise. GUI
    /// thread, between gestures.
    pub fn commit_edits(&self) {
        let mut history = self.history_lock();
        self.commit_into(&mut history);
    }

    /// Steps back one undo step. `false` if there was nothing to undo.
    pub fn undo(&self) -> bool {
        self.step_history(History::undo)
    }

    /// Steps forward again after an undo. `false` if there was nothing to
    /// redo.
    pub fn redo(&self) -> bool {
        self.step_history(History::redo)
    }

    /// Whether [`SeqModel::undo`] has anything to do, as of the last commit.
    pub fn can_undo(&self) -> bool {
        self.history_lock().can_undo()
    }

    /// Whether [`SeqModel::redo`] has anything to do.
    pub fn can_redo(&self) -> bool {
        self.history_lock().can_redo()
    }

    fn commit_into(&self, history: &mut History<SeqSnapshot>) {
        history.commit(self.edits.load(Relaxed), || self.history_snapshot());
    }

    fn step_history(
        &self,
        step: impl FnOnce(&mut History<SeqSnapshot>) -> Option<SeqSnapshot>,
    ) -> bool {
        let mut history = self.history_lock();
        self.commit_into(&mut history);
        let Some(target) = step(&mut history) else {
            return false;
        };
        self.restore(target);
        history.rebase(self.edits.load(Relaxed));
        true
    }

    /// The sequence as the history stores it: mute and solo blanked, so
    /// switching them is never an undo step.
    fn history_snapshot(&self) -> SeqSnapshot {
        let mut snapshot = self.snapshot();
        for lane in &mut snapshot.lanes {
            lane.mute = false;
            lane.solo = false;
        }
        snapshot
    }

    /// Puts a stored state back, keeping each lane's current mute and solo.
    fn restore(&self, mut snapshot: SeqSnapshot) {
        {
            let current = self.cell.get();
            for lane in &mut snapshot.lanes {
                if let Some(live) = current.position_of(lane.id).and_then(|pos| current.lane(pos)) {
                    lane.mute = live.is_muted();
                    lane.solo = live.is_soloed();
                }
            }
        }
        self.install(snapshot.into_data());
    }

    // ---- bulk ops: value edits over one lane's steps in one pattern ----
    //
    // All four work on `0..length` only. Steps past `length` are hidden, not
    // gone: a user who shortened a lane and then clears or rotates it must
    // still get the tail back by dragging the length out again.

    /// Wipes every visible step of one lane, keeping its length, rate and
    /// hidden steps. See [`Step::cleared`] for the reserved bits.
    pub fn clear_lane(&self, pattern: usize, lane_id: u32) -> bool {
        self.with_lane_data(pattern, lane_id, |lane_data| {
            for index in 0..lane_data.length() as usize {
                lane_data.update_step(index, Step::cleared);
            }
            true
        })
    }

    /// Flips every visible step of one lane on or off, keeping the values of
    /// the steps it switches off.
    pub fn invert_lane(&self, pattern: usize, lane_id: u32) -> bool {
        self.with_lane_data(pattern, lane_id, |lane_data| {
            for index in 0..lane_data.length() as usize {
                lane_data.update_step(index, |step| {
                    if step.is_on() { step.with_on(false) } else { step.turned_on() }
                });
            }
            true
        })
    }

    /// Rolls the dice on every visible step: `density` is the chance of a step
    /// coming on, `0.0` (clear the lane) to `1.0` (fill it). Out-of-range and
    /// NaN densities clamp into that range.
    ///
    /// Only the on bit is randomised — the velocity/pan/ratchet/probability a
    /// step already carries survive, and a never-edited step that comes on
    /// gets the defaults ([`Step::turned_on`]).
    ///
    /// `rng` is the caller's free-running [`Prng`], so a fixed seed makes the
    /// whole sequence of randomise calls reproducible.
    pub fn randomise_lane(
        &self,
        pattern: usize,
        lane_id: u32,
        density: f32,
        rng: &mut Prng,
    ) -> bool {
        // Compared against a u32 draw rather than a float, so the ends are
        // exact: threshold 0 can never fire and 2^32 always does.
        // NaN clamps to NaN, which casts to 0 — a NaN density clears the lane.
        const DRAWS: f64 = (u32::MAX as u64 + 1) as f64;
        let threshold = (f64::from(density.clamp(0.0, 1.0)) * DRAWS) as u64;
        self.fill_lane(pattern, lane_id, |_| u64::from(rng.next_u32()) < threshold)
    }

    /// Sets every visible step of one lane on or off by `on(index)`, asked
    /// once per step in order. Like [`SeqModel::randomise_lane`], only the on
    /// bit changes: the values a step carries survive, and a never-edited
    /// step that comes on gets the defaults ([`Step::turned_on`]).
    pub fn fill_lane(
        &self,
        pattern: usize,
        lane_id: u32,
        mut on: impl FnMut(usize) -> bool,
    ) -> bool {
        self.with_lane_data(pattern, lane_id, |lane_data| {
            for index in 0..lane_data.length() as usize {
                let on = on(index);
                lane_data.update_step(index, |step| {
                    if on { step.turned_on() } else { step.with_on(false) }
                });
            }
            true
        })
    }

    /// Rotates a lane's visible steps by `delta` steps, wrapping within
    /// `length`: `+1` moves every step one later, `-1` one earlier. Values
    /// travel with their step; a `delta` beyond the length wraps around.
    pub fn nudge_lane(&self, pattern: usize, lane_id: u32, delta: i32) -> bool {
        self.with_lane_data(pattern, lane_id, |lane_data| {
            let length = lane_data.length() as usize;
            // `length` is at most MAX_STEPS, so the cast can't wrap.
            let shift = delta.rem_euclid(length as i32) as usize;
            if shift == 0 {
                return true;
            }
            // Read the whole loop out first: writing in place would overwrite
            // steps still to be moved. This allocates, which is fine on a
            // writer path — the audio thread only reads, and the worst it can
            // see is one block drawn mid-rotation.
            let before: Vec<Step> = (0..length).map(|index| lane_data.step(index)).collect();
            for (index, step) in before.into_iter().enumerate() {
                lane_data.set_step((index + shift) % length, step);
            }
            true
        })
    }

    // ---- internals ----

    /// The writer mutex, poisoning ignored: it guards atomics that are always
    /// in a valid state, so a panicking writer must not wedge every later
    /// edit (which in a plugin means a dead GUI).
    fn writer_lock(&self) -> MutexGuard<'_, ()> {
        self.writer.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The writer lock for a write: counts the edit for the undo history. A
    /// write that turns out to change nothing still counts, which costs one
    /// snapshot at the next commit and records no step.
    fn edit_lock(&self) -> MutexGuard<'_, ()> {
        let guard = self.writer_lock();
        self.edits.fetch_add(1, Relaxed);
        guard
    }

    fn history_lock(&self) -> MutexGuard<'_, History<SeqSnapshot>> {
        self.history.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn collector_lock(&self) -> MutexGuard<'_, Option<Collector>> {
        self.collector.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn take_lane_id(&self) -> u32 {
        self.next_lane_id.fetch_add(1, Relaxed)
    }

    /// Runs a value edit on one lane's settings with the writer lock held.
    fn with_lane(&self, lane_id: u32, f: impl FnOnce(&Lane)) -> bool {
        let _writer = self.edit_lock();
        let data = self.cell.get();
        match data.position_of(lane_id).and_then(|pos| data.lane(pos)) {
            Some(lane) => {
                f(lane);
                true
            }
            None => false,
        }
    }

    /// Runs a value edit on one lane's content in one pattern with the writer
    /// lock held.
    fn with_lane_data(
        &self,
        pattern: usize,
        lane_id: u32,
        f: impl FnOnce(&LaneData) -> bool,
    ) -> bool {
        let _writer = self.edit_lock();
        let data = self.cell.get();
        match data.position_of(lane_id).and_then(|pos| data.lane_data(pattern, pos)) {
            Some(lane_data) => f(lane_data),
            None => false,
        }
    }

    /// Runs a shape edit: `f` gets a deep copy of the published `SeqData` with
    /// every current atomic value already in it. Returning `Some` publishes
    /// the copy; returning `None` abandons the edit, leaving the old data
    /// published and nothing allocated for the collector.
    fn shape_edit<R>(&self, f: impl FnOnce(&mut SeqData) -> Option<R>) -> Option<R> {
        let _writer = self.edit_lock();
        let mut next = (*self.cell.get()).clone();
        let result = f(&mut next)?;
        self.publish(next);
        Some(result)
    }

    /// Publishes a replacement `SeqData` and reclaims whatever that retired,
    /// so an instance whose editor is closed does not pile up old copies.
    /// Caller holds the writer lock.
    fn publish(&self, data: SeqData) {
        self.cell.set(Shared::new(&self.handle, data));
        self.collect();
    }

    /// How many `SeqData`s basedrop still has alive: the published one plus
    /// every retired one a reader has not let go of yet. A steady state is
    /// `1`, so this is the only way to see that [`SeqModel::collect`] really
    /// reclaims.
    ///
    /// Public for the tests only — the plugin itself has no use for it, but
    /// the concurrency stress test (`tests/concurrency_stress.rs`) lives
    /// outside this crate and the leak check is the point of it. Takes the
    /// collector mutex, so never call it from the audio thread.
    pub fn live_allocs(&self) -> usize {
        self.collector_lock().as_ref().map_or(0, Collector::alloc_count)
    }
}

impl Default for SeqModel {
    fn default() -> SeqModel {
        SeqModel::empty()
    }
}

impl Drop for SeqModel {
    /// `Collector` has no `Drop` of its own — dropping one leaks every
    /// allocation it ever handed out, which in a host means a whole `SeqData`
    /// per plugin instance. So tear down in order: release the published data
    /// and the handle, drain the queue, then free the collector itself.
    ///
    /// If someone still holds a `Shared<SeqData>` from [`SeqModel::shared`],
    /// `try_cleanup` refuses and we leak instead of freeing memory that is
    /// still being read — the safe direction.
    fn drop(&mut self) {
        // SAFETY: `drop` runs once, and neither field is touched afterwards.
        unsafe {
            ManuallyDrop::drop(&mut self.cell);
            ManuallyDrop::drop(&mut self.handle);
        }
        if let Some(mut collector) = self.collector_lock().take() {
            collector.collect();
            let _ = collector.try_cleanup();
        }
    }
}

/// Playback position, audio thread → GUI.
///
/// Indexed by **lane position**, not id, and deliberately *outside*
/// [`SeqData`]: a shape edit republishes the data, and the playhead must not
/// be republished with it (nor allocate on the audio path). The audio thread
/// stores each lane's current step once per block; the GUI reads it to draw
/// the playhead and the active pattern.
pub struct Playhead {
    steps: [AtomicU32; MAX_LANES],
    pattern: AtomicU32,
    song_entry: AtomicU32,
    song_bar: AtomicU32,
    /// The host's bar length in beats, as `f64` bits.
    bar_beats: AtomicU64,
}

impl Playhead {
    /// "Not playing" / "no step". A real step index can never reach it —
    /// lanes hold at most [`MAX_STEPS`] steps.
    pub const NONE: u32 = u32::MAX;

    pub fn new() -> Playhead {
        Playhead {
            steps: std::array::from_fn(|_| AtomicU32::new(Self::NONE)),
            pattern: AtomicU32::new(Self::NONE),
            song_entry: AtomicU32::new(Self::NONE),
            song_bar: AtomicU32::new(Self::NONE),
            bar_beats: AtomicU64::new(DEFAULT_BAR_BEATS.to_bits()),
        }
    }

    /// The step index lane `pos` is playing, or `None` when stopped (or when
    /// `pos` is past [`MAX_LANES`]).
    pub fn step(&self, pos: usize) -> Option<u32> {
        match self.steps.get(pos)?.load(Relaxed) {
            Self::NONE => None,
            step => Some(step),
        }
    }

    /// Publishes lane `pos`'s current step. A no-op past [`MAX_LANES`], which
    /// cannot happen while `SeqData` is capped there but costs one bounds
    /// check to never corrupt.
    pub fn set_step(&self, pos: usize, step: u32) {
        if let Some(cell) = self.steps.get(pos) {
            cell.store(step, Relaxed);
        }
    }

    /// The pattern the audio thread is actually playing, or `None` when
    /// stopped. May lag the `pattern_select` param: a change only takes
    /// effect at a step boundary.
    pub fn pattern(&self) -> Option<u32> {
        match self.pattern.load(Relaxed) {
            Self::NONE => None,
            pattern => Some(pattern),
        }
    }

    pub fn set_pattern(&self, pattern: u32) {
        self.pattern.store(pattern, Relaxed);
    }

    /// The song bar playing (0 = the first), or `None` when song mode is off
    /// or the transport is stopped. Can be past the song's end.
    pub fn song_bar(&self) -> Option<u32> {
        match self.song_bar.load(Relaxed) {
            Self::NONE => None,
            bar => Some(bar),
        }
    }

    pub fn set_song_bar(&self, bar: u32) {
        self.song_bar.store(bar, Relaxed);
    }

    /// The song entry playing (0 = the first), or `None` when song mode is
    /// off, the transport is stopped, or the song is before its start or
    /// past its end.
    pub fn song_entry(&self) -> Option<u32> {
        match self.song_entry.load(Relaxed) {
            Self::NONE => None,
            entry => Some(entry),
        }
    }

    pub fn set_song_entry(&self, entry: u32) {
        self.song_entry.store(entry, Relaxed);
    }

    /// The bar length the audio thread last saw, in beats: what the GUI
    /// needs to place song entries on bars. Kept while stopped.
    pub fn bar_beats(&self) -> f64 {
        f64::from_bits(self.bar_beats.load(Relaxed))
    }

    pub fn set_bar_beats(&self, beats: f64) {
        self.bar_beats.store(beats.to_bits(), Relaxed);
    }

    /// Back to "not playing": every lane and the pattern to the sentinel.
    /// Called from the audio thread when the transport stops, so it only
    /// stores — [`MAX_LANES`] relaxed stores, no allocation.
    pub fn clear(&self) {
        for cell in &self.steps {
            cell.store(Self::NONE, Relaxed);
        }
        self.pattern.store(Self::NONE, Relaxed);
        self.song_entry.store(Self::NONE, Relaxed);
        self.song_bar.store(Self::NONE, Relaxed);
    }
}

impl Default for Playhead {
    fn default() -> Playhead {
        Playhead::new()
    }
}

impl std::fmt::Debug for Playhead {
    /// Only the lanes that are playing — the derived form prints 256 atomics,
    /// nearly all of them the sentinel.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Playhead")
            .field("pattern", &self.pattern())
            .field(
                "steps",
                &(0..MAX_LANES)
                    .filter_map(|pos| self.step(pos).map(|step| (pos, step)))
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::state::{LaneSnapshot, PatternSnapshot};

    /// Every field, at every value it can legally hold, survives a pack and
    /// read back unchanged — with the other fields pinned to non-default
    /// values so a mask that reaches into a neighbour shows up here.
    #[test]
    fn pack_round_trips_each_field_over_its_full_range() {
        for velocity in Step::VELOCITY_MIN..=Step::VELOCITY_MAX {
            let step = Step::pack(true, velocity, 11, 5, 42);
            assert_eq!(step.velocity(), velocity, "{step:?}");
            assert_eq!((step.pan(), step.ratchet(), step.probability()), (11, 5, 42));
        }
        for pan in Step::PAN_MIN..=Step::PAN_MAX {
            let step = Step::pack(true, 99, pan, 5, 42);
            assert_eq!(step.pan(), pan, "{step:?}");
            assert_eq!((step.velocity(), step.ratchet(), step.probability()), (99, 5, 42));
        }
        for ratchet in Step::RATCHET_MIN..=Step::RATCHET_MAX {
            let step = Step::pack(true, 99, 11, ratchet, 42);
            assert_eq!(step.ratchet(), ratchet, "{step:?}");
            assert_eq!((step.velocity(), step.pan(), step.probability()), (99, 11, 42));
        }
        for probability in 0..=Step::PROBABILITY_MAX {
            let step = Step::pack(true, 99, 11, 5, probability);
            assert_eq!(step.probability(), probability, "{step:?}");
            assert_eq!((step.velocity(), step.pan(), step.ratchet()), (99, 11, 5));
        }
        for on in [false, true] {
            let step = Step::pack(on, 99, 11, 5, 42);
            assert_eq!(step.is_on(), on, "{step:?}");
            assert_eq!(
                (step.velocity(), step.pan(), step.ratchet(), step.probability()),
                (99, 11, 5, 42)
            );
        }
    }

    /// The same round-trip across combinations of all five fields (strided, so
    /// it stays well under a second in a debug build) — catches a mask error
    /// that only appears for particular bit patterns.
    #[test]
    fn pack_round_trips_field_combinations() {
        for velocity in (Step::VELOCITY_MIN..=Step::VELOCITY_MAX).step_by(5) {
            for pan in (Step::PAN_MIN..=Step::PAN_MAX).step_by(3) {
                for ratchet in Step::RATCHET_MIN..=Step::RATCHET_MAX {
                    for probability in (0..=Step::PROBABILITY_MAX).step_by(7) {
                        for on in [false, true] {
                            let step = Step::pack(on, velocity, pan, ratchet, probability);
                            assert_eq!(
                                (
                                    step.is_on(),
                                    step.velocity(),
                                    step.pan(),
                                    step.ratchet(),
                                    step.probability()
                                ),
                                (on, velocity, pan, ratchet, probability)
                            );
                        }
                    }
                }
            }
        }
    }

    /// The `with_*` setters are the GUI's edit path: each one changes its own
    /// field and leaves the other four alone.
    #[test]
    fn with_setters_change_only_their_own_field() {
        let base = Step::pack(true, 99, 11, 5, 42);

        let step = base.with_velocity(7);
        assert_eq!(
            (step.is_on(), step.velocity(), step.pan(), step.ratchet(), step.probability()),
            (true, 7, 11, 5, 42)
        );

        let step = base.with_pan(120);
        assert_eq!(
            (step.is_on(), step.velocity(), step.pan(), step.ratchet(), step.probability()),
            (true, 99, 120, 5, 42)
        );

        let step = base.with_ratchet(8);
        assert_eq!(
            (step.is_on(), step.velocity(), step.pan(), step.ratchet(), step.probability()),
            (true, 99, 11, 8, 42)
        );

        let step = base.with_probability(0);
        assert_eq!(
            (step.is_on(), step.velocity(), step.pan(), step.ratchet(), step.probability()),
            (true, 99, 11, 5, 0)
        );

        let step = base.with_on(false);
        assert_eq!(
            (step.is_on(), step.velocity(), step.pan(), step.ratchet(), step.probability()),
            (false, 99, 11, 5, 42)
        );
    }

    /// Setters, like `pack`, clamp rather than wrapping or bleeding into the
    /// next field — both the GUI and a hostile snapshot go through them.
    #[test]
    fn setters_clamp_out_of_range_values() {
        // velocity 0 is a note-off on the wire, so the floor is 1.
        assert_eq!(Step::pack(true, 0, 63, 1, 100).velocity(), 1);
        assert_eq!(Step::pack(true, 128, 63, 1, 100).velocity(), 127);
        assert_eq!(Step::pack(true, u8::MAX, 63, 1, 100).velocity(), 127);

        assert_eq!(Step::pack(true, 100, 127, 1, 100).pan(), 126);
        assert_eq!(Step::pack(true, 100, u8::MAX, 1, 100).pan(), 126);

        // A zero ratchet would mean a zero-length subdivision in the engine.
        assert_eq!(Step::pack(true, 100, 63, 0, 100).ratchet(), 1);
        assert_eq!(Step::pack(true, 100, 63, 9, 100).ratchet(), 8);
        assert_eq!(Step::pack(true, 100, 63, u8::MAX, 100).ratchet(), 8);

        assert_eq!(Step::pack(true, 100, 63, 1, 101).probability(), 100);
        assert_eq!(Step::pack(true, 100, 63, 1, u8::MAX).probability(), 100);

        let base = Step::default_on();
        assert_eq!(base.with_velocity(0).velocity(), 1);
        assert_eq!(base.with_velocity(u8::MAX).velocity(), 127);
        assert_eq!(base.with_pan(u8::MAX).pan(), 126);
        assert_eq!(base.with_ratchet(0).ratchet(), 1);
        assert_eq!(base.with_ratchet(u8::MAX).ratchet(), 8);
        assert_eq!(base.with_probability(u8::MAX).probability(), 100);

        // Clamping must not spill into a neighbouring field.
        let clamped = Step::pack(true, u8::MAX, u8::MAX, u8::MAX, u8::MAX);
        assert_eq!(clamped.bits() & Step::RESERVED_MASK, 0);
        assert_eq!(
            (clamped.is_on(), clamped.velocity(), clamped.pan(), clamped.ratchet()),
            (true, 127, 126, 8)
        );
    }

    /// Bits 25–31 belong to a future version. A setter must carry them
    /// through untouched, so a project saved by a newer build keeps its extra
    /// data after this build edits a step.
    #[test]
    fn reserved_bits_are_preserved_by_every_setter() {
        let marked = Step(Step::pack(true, 99, 11, 5, 42).bits() | Step::RESERVED_MASK);

        for step in [
            marked.with_on(false),
            marked.with_on(true),
            marked.with_velocity(1),
            marked.with_velocity(127),
            marked.with_pan(0),
            marked.with_pan(126),
            marked.with_ratchet(1),
            marked.with_ratchet(8),
            marked.with_probability(0),
            marked.with_probability(100),
        ] {
            assert_eq!(
                step.bits() & Step::RESERVED_MASK,
                Step::RESERVED_MASK,
                "setter dropped reserved bits: {step:?}"
            );
        }

        // And the known fields still read correctly with those bits set.
        assert_eq!(
            (
                marked.is_on(),
                marked.velocity(),
                marked.pan(),
                marked.ratchet(),
                marked.probability()
            ),
            (true, 99, 11, 5, 42)
        );

        // `pack` owns the whole word, so it writes the reserved bits as zero.
        assert_eq!(Step::pack(true, 99, 11, 5, 42).bits() & Step::RESERVED_MASK, 0);
    }

    /// A step read back from a snapshot or a newer build can hold any bit
    /// pattern; the getters must still report in-range values.
    #[test]
    fn getters_clamp_hostile_bit_patterns() {
        let all_ones = Step(u32::MAX);
        assert!(all_ones.is_on());
        assert_eq!(all_ones.velocity(), 127);
        assert_eq!(all_ones.pan(), 126);
        assert_eq!(all_ones.ratchet(), 8);
        assert_eq!(all_ones.probability(), 100);

        // Raw pan 127 and raw probability 101..=127 are reachable in the bits
        // but out of range by design.
        let raw_pan_127 = Step(Step::pack(true, 100, 0, 1, 100).bits() | (127 << 8));
        assert_eq!(raw_pan_127.pan(), 126);
        let raw_prob_127 = Step(Step::pack(true, 100, 63, 1, 0).bits() | (127 << 18));
        assert_eq!(raw_prob_127.probability(), 100);
    }

    #[test]
    fn off_is_all_zero_so_a_zeroed_lane_is_silent() {
        assert_eq!(Step::OFF.bits(), 0);
        assert!(!Step::OFF.is_on());
        assert_eq!(Step::default(), Step::OFF);
        // Even on the zero word the getters report legal values.
        assert_eq!(Step::OFF.velocity(), Step::VELOCITY_MIN);
        assert_eq!(Step::OFF.ratchet(), Step::RATCHET_MIN);
        assert_eq!(Step::OFF.pan(), 0);
        assert_eq!(Step::OFF.probability(), 0);
    }

    #[test]
    fn default_on_is_a_centred_always_firing_step() {
        let step = Step::default_on();
        assert!(step.is_on());
        assert_eq!(step.velocity(), 100);
        assert_eq!(step.pan(), Step::PAN_CENTRE);
        assert_eq!(step.ratchet(), 1);
        assert_eq!(step.probability(), 100);
        assert_eq!(step.bits() & Step::RESERVED_MASK, 0);
        // Turning it off and on again is lossless.
        assert_eq!(step.with_on(false).with_on(true), step);
        assert!(!step.with_on(false).is_on());
    }

    /// Pins the wire format: the documented bit positions (see `Step`), not
    /// just "some reversible encoding". A snapshot written today has to be
    /// readable by later builds.
    #[test]
    fn bit_layout_matches_the_spec() {
        assert_eq!(Step::pack(true, 1, 0, 1, 0).bits(), 0b1 | (1 << 1));
        assert_eq!(Step::pack(false, 127, 0, 1, 0).bits(), 127 << 1);
        assert_eq!(Step::pack(false, 1, 126, 1, 0).bits(), (1 << 1) | (126 << 8));
        // Ratchet is stored n-1: 1 → 0b000, 8 → 0b111.
        assert_eq!(Step::pack(false, 1, 0, 1, 0).bits(), 1 << 1);
        assert_eq!(Step::pack(false, 1, 0, 8, 0).bits(), (1 << 1) | (7 << 15));
        assert_eq!(Step::pack(false, 1, 0, 1, 100).bits(), (1 << 1) | (100 << 18));

        // The six field masks tile bits 0..=30 exactly once, and the
        // reserved mask covers the rest of the word.
        let masks = [
            Step::ON_MASK,
            Step::VELOCITY_MASK,
            Step::PAN_MASK,
            Step::RATCHET_MASK,
            Step::PROBABILITY_MASK,
            Step::PITCH_MASK,
        ];
        let mut union = 0u32;
        for mask in masks {
            assert_eq!(union & mask, 0, "field masks overlap");
            union |= mask;
        }
        assert_eq!(union, 0x7FFF_FFFF);
        assert_eq!(union | Step::RESERVED_MASK, u32::MAX);
        assert_eq!(union & Step::RESERVED_MASK, 0);
    }

    /// The packing exists so a step can live in one relaxed atomic.
    #[test]
    fn step_survives_an_atomic_round_trip() {
        use std::sync::atomic::{AtomicU32, Ordering};

        let cell = AtomicU32::new(Step::OFF.bits());
        let step = Step::default_on().with_pan(10).with_ratchet(4).with_probability(75);
        cell.store(step.bits(), Ordering::Relaxed);
        assert_eq!(Step(cell.load(Ordering::Relaxed)), step);
    }

    /// What the audio thread does with a `&SeqData` every block: read each
    /// lane's live settings and the step under the playhead. Used by the
    /// visibility test below, so "visible" means visible the way `process()`
    /// would see it.
    fn read_block(data: &SeqData, pattern: usize, step_index: usize) -> Vec<(u8, u8, bool, u32, Rate, Step)> {
        (0..data.lane_count())
            .map(|pos| {
                let lane = data.lane(pos).expect("lane in range");
                let lane_data = data.lane_data(pattern, pos).expect("lane data in range");
                (
                    lane.key(),
                    lane.channel(),
                    lane.is_muted(),
                    lane_data.length(),
                    lane_data.rate(),
                    lane_data.step(step_index),
                )
            })
            .collect()
    }

    fn two_lanes() -> SeqData {
        SeqData::from_lanes(vec![
            Lane::new(1, "Kick", 0xFF0000, 36, 0),
            Lane::new(2, "Snare", 0x00FF00, 38, 1),
        ])
    }

    /// The point of the atomics: a value edit lands on the `SeqData` the
    /// reader is already holding, with no republish and no lock. Here the
    /// writer only ever has a `&SeqData` — through a second `Arc` handle, from
    /// another thread — and the reader sees every change.
    #[test]
    fn value_edits_are_visible_through_a_shared_reference() {
        use std::sync::Arc;

        let data = Arc::new(two_lanes());
        let before = read_block(&data, 0, 3);
        assert_eq!(
            before,
            vec![
                (36, 0, false, 32, Rate::Sixteenth, Step::OFF),
                (38, 1, false, 32, Rate::Sixteenth, Step::OFF),
            ]
        );

        let writer_handle = Arc::clone(&data);
        let edited = Step::default_on().with_velocity(90).with_pan(20);
        std::thread::spawn(move || {
            // A `&SeqData` is all a writer needs for a value edit.
            let data: &SeqData = &writer_handle;
            data.lane(0).unwrap().set_key(40);
            data.lane(0).unwrap().set_channel(9);
            data.lane(1).unwrap().set_mute(true);
            data.lane_data(0, 0).unwrap().set_step(3, edited);
            data.lane_data(0, 0).unwrap().set_length(8);
            data.lane_data(0, 1).unwrap().set_rate(Rate::EighthTriplet);
        })
        .join()
        .expect("writer thread panicked");

        // Same `SeqData`, no republish: the reader's own handle sees it all.
        assert_eq!(
            read_block(&data, 0, 3),
            vec![
                (40, 9, false, 8, Rate::Sixteenth, edited),
                (38, 1, true, 32, Rate::EighthTriplet, Step::OFF),
            ]
        );
        // And an in-place read-modify-write is visible the same way.
        data.lane_data(0, 0).unwrap().update_step(3, |step| step.with_on(!step.is_on()));
        assert_eq!(data.lane_data(0, 0).unwrap().step(3), edited.with_on(false));
    }

    /// Only the addressed pattern changes: every pattern owns its own
    /// `LaneData` for every lane.
    #[test]
    fn patterns_hold_independent_lane_data() {
        let data = two_lanes();
        assert!(data.patterns.iter().all(|pattern| pattern.len() == data.lane_count()));

        data.lane_data(0, 0).unwrap().set_step(0, Step::default_on());
        data.lane_data(0, 0).unwrap().set_length(4);
        data.lane_data(0, 0).unwrap().set_rate(Rate::Quarter);

        for pattern in 1..NUM_PATTERNS {
            let other = data.lane_data(pattern, 0).unwrap();
            assert_eq!(other.step(0), Step::OFF, "pattern {pattern} shares step storage");
            assert_eq!(other.length(), LaneData::DEFAULT_LENGTH);
            assert_eq!(other.rate(), Rate::DEFAULT);
        }
    }

    #[test]
    fn a_fresh_lane_data_is_two_bars_of_sixteenths() {
        let lane_data = LaneData::empty();
        assert_eq!(lane_data.capacity(), 32);
        assert_eq!(lane_data.capacity(), LaneData::DEFAULT_CAPACITY);
        assert_eq!(lane_data.capacity() % CAPACITY_CHUNK, 0, "whole chunks");
        assert_eq!(lane_data.length(), 32);
        assert_eq!(lane_data.rate(), Rate::Sixteenth);
        assert_eq!(lane_data.rate().rate_beats(), 0.25);
        assert!((0..lane_data.capacity()).all(|i| !lane_data.step(i).is_on()));
    }

    /// `length <= capacity`, whatever is asked for and whatever is in the raw
    /// field — the audio thread indexes `steps[k % length]`, so a length past
    /// the capacity would be an out-of-bounds read.
    #[test]
    fn length_is_always_within_one_and_capacity() {
        let lane_data = LaneData::with_capacity(32);

        lane_data.set_length(32);
        assert_eq!(lane_data.length(), 32);
        lane_data.set_length(33);
        assert_eq!(lane_data.length(), 32);
        lane_data.set_length(u32::MAX);
        assert_eq!(lane_data.length(), 32);
        // Zero would make `k.rem_euclid(len)` divide by zero.
        lane_data.set_length(0);
        assert_eq!(lane_data.length(), 1);

        // Even a raw store that bypasses the setter reads back in range.
        lane_data.length.store(u32::MAX, Relaxed);
        assert_eq!(lane_data.length(), 32);
        lane_data.length.store(0, Relaxed);
        assert_eq!(lane_data.length(), 1);

        // A smaller capacity than the default length still holds.
        let short = LaneData::with_capacity(4);
        assert_eq!((short.capacity(), short.length()), (4, 4));
        // ...and a lane built by hand with no steps at all does not panic.
        let degenerate = LaneData { steps: Box::new([]), ..LaneData::with_capacity(1) };
        assert_eq!(degenerate.capacity(), 0);
        assert_eq!(degenerate.length(), 1);
        assert_eq!(degenerate.step(0), Step::OFF);
    }

    /// Shrinking `length` hides steps; it must not erase them, because
    /// growing back has to restore them (and a save keeps them).
    #[test]
    fn shrinking_length_hides_steps_without_erasing_them() {
        let lane_data = LaneData::with_capacity(32);
        let hidden = Step::default_on().with_velocity(7);
        lane_data.set_step(31, hidden);

        lane_data.set_length(4);
        assert_eq!(lane_data.step(31), hidden, "a hidden step was cleared");

        lane_data.set_length(32);
        assert_eq!(lane_data.step(31), hidden);
    }

    /// An index past the capacity is a race the GUI can lose, not a bug:
    /// reads give silence and writes do nothing.
    #[test]
    fn step_accessors_ignore_out_of_range_indices() {
        let lane_data = LaneData::with_capacity(16);
        assert_eq!(lane_data.step(16), Step::OFF);
        assert_eq!(lane_data.step(usize::MAX), Step::OFF);

        lane_data.set_step(16, Step::default_on());
        lane_data.set_step(usize::MAX, Step::default_on());
        lane_data.update_step(16, |_| Step::default_on());
        assert!((0..lane_data.capacity()).all(|i| !lane_data.step(i).is_on()));
    }

    /// Lane settings go on the wire, so they clamp on write *and* on read —
    /// the atomics are public and a state load can store anything.
    #[test]
    fn lane_settings_clamp_to_midi_ranges() {
        let lane = Lane::new(7, "Hat", 0x112233, 200, 99);
        assert_eq!((lane.id, lane.name.as_str(), lane.colour), (7, "Hat", 0x112233));
        assert_eq!((lane.key(), lane.channel()), (127, 15));
        assert!(!lane.is_muted() && !lane.is_soloed());

        lane.set_key(u8::MAX);
        assert_eq!(lane.key(), 127);
        lane.set_key(36);
        assert_eq!(lane.key(), 36);
        lane.set_channel(u8::MAX);
        assert_eq!(lane.channel(), 15);
        lane.set_channel(9);
        assert_eq!(lane.channel(), 9);

        assert_eq!(lane.volume_db(), 0, "a new lane plays as recorded");
        lane.set_volume_db(i8::MAX);
        assert_eq!(lane.volume_db(), Lane::VOLUME_MAX);
        lane.set_volume_db(i8::MIN);
        assert_eq!(lane.volume_db(), Lane::VOLUME_MIN);
        lane.volume.store(i8::MIN, Relaxed);
        assert_eq!(lane.volume_db(), Lane::VOLUME_MIN, "clamped on read too");
        lane.set_volume_db(5);
        assert_eq!(lane.clone().volume_db(), 5, "a clone keeps it");

        assert!(!lane.is_sample_pad(), "a bare lane is not a sample pad");
        lane.set_sample_pad(true);
        assert!(lane.clone().is_sample_pad(), "a clone keeps it");

        lane.key.store(u8::MAX, Relaxed);
        lane.channel.store(u8::MAX, Relaxed);
        assert_eq!((lane.key(), lane.channel()), (127, 15));

        lane.set_mute(true);
        lane.set_solo(true);
        assert!(lane.is_muted() && lane.is_soloed());
    }

    #[test]
    fn any_solo_sees_a_solo_on_any_lane() {
        let data = two_lanes();
        assert!(!data.any_solo());
        data.lane(1).unwrap().set_solo(true);
        assert!(data.any_solo());
        data.lane(1).unwrap().set_solo(false);
        assert!(!data.any_solo());
    }

    /// The caps exist so a bad number can't allocate gigabytes: an oversize
    /// request is cut down to the cap instead of being honoured.
    #[test]
    fn caps_cut_oversize_construction_down() {
        assert_eq!(LaneData::with_capacity(MAX_STEPS).capacity(), MAX_STEPS);
        assert_eq!(LaneData::with_capacity(MAX_STEPS + 1).capacity(), MAX_STEPS);
        assert_eq!(LaneData::with_capacity(1_000_000_000).capacity(), MAX_STEPS);
        assert_eq!(LaneData::with_capacity(usize::MAX).capacity(), MAX_STEPS);
        // A full-capacity lane's length still fits inside it.
        let full = LaneData::with_capacity(usize::MAX);
        full.set_length(u32::MAX);
        assert_eq!(full.length(), MAX_STEPS as u32);
        // Zero capacity is lifted to one, not left empty.
        assert_eq!(LaneData::with_capacity(0).capacity(), 1);

        let oversize: Vec<Lane> =
            (0..MAX_LANES as u32 + 5).map(|i| Lane::new(i, format!("L{i}"), 0, 36, 0)).collect();
        let data = SeqData::from_lanes(oversize);
        assert_eq!(data.lane_count(), MAX_LANES);
        // The surviving lanes are the first ones, and the patterns still line
        // up with them.
        assert_eq!(data.lane(0).unwrap().id, 0);
        assert_eq!(data.lane(MAX_LANES - 1).unwrap().id, MAX_LANES as u32 - 1);
        assert_eq!(data.max_lane_id(), Some(MAX_LANES as u32 - 1));
        assert!(data.patterns.iter().all(|pattern| pattern.len() == MAX_LANES));
        assert!(data.lane(MAX_LANES).is_none());
        assert!(data.lane_data(0, MAX_LANES).is_none());
        assert!(data.lane_data(NUM_PATTERNS, 0).is_none());
    }

    #[test]
    fn chunked_capacity_rounds_up_within_the_caps() {
        assert_eq!(chunked_capacity(0), CAPACITY_CHUNK);
        assert_eq!(chunked_capacity(1), CAPACITY_CHUNK);
        assert_eq!(chunked_capacity(16), 16);
        assert_eq!(chunked_capacity(17), 32);
        assert_eq!(chunked_capacity(31), 32);
        assert_eq!(chunked_capacity(32), 32);
        assert_eq!(chunked_capacity(33), 48);
        assert_eq!(chunked_capacity(MAX_STEPS - 1), MAX_STEPS);
        assert_eq!(chunked_capacity(MAX_STEPS), MAX_STEPS);
        assert_eq!(chunked_capacity(MAX_STEPS + 1), MAX_STEPS);
        assert_eq!(chunked_capacity(usize::MAX), MAX_STEPS);
        for steps in 1..=200 {
            let capacity = chunked_capacity(steps);
            assert!(capacity >= steps, "{steps} does not fit in {capacity}");
            assert_eq!(capacity % CAPACITY_CHUNK, 0);
            assert!(capacity - steps < CAPACITY_CHUNK, "{steps} rounded too far");
        }
    }

    /// `patterns[p][i]` belongs to `lanes[i]` — the one invariant every reader
    /// relies on. A snapshot that disagrees is repaired, not trusted.
    #[test]
    fn new_realigns_patterns_with_the_lane_list() {
        let lanes = vec![
            Lane::new(1, "a", 0, 36, 0),
            Lane::new(2, "b", 0, 38, 0),
            Lane::new(3, "c", 0, 42, 0),
        ];
        let marked = Step::default_on().with_velocity(64);
        let patterns: [Vec<LaneData>; NUM_PATTERNS] = std::array::from_fn(|p| {
            // Pattern 0 is too short, pattern 1 too long, the rest empty.
            let count = match p {
                0 => 2,
                1 => 5,
                _ => 0,
            };
            (0..count)
                .map(|_| {
                    let lane_data = LaneData::with_capacity(32);
                    lane_data.set_step(0, marked);
                    lane_data
                })
                .collect()
        });

        let data = SeqData::new(lanes, patterns);
        assert_eq!(data.lane_count(), 3);
        for pattern in 0..NUM_PATTERNS {
            assert_eq!(data.patterns[pattern].len(), 3, "pattern {pattern} misaligned");
        }
        // The supplied entries kept their values; the padding is empty.
        assert_eq!(data.lane_data(0, 1).unwrap().step(0), marked);
        assert_eq!(data.lane_data(0, 2).unwrap().capacity(), LaneData::DEFAULT_CAPACITY);
        assert_eq!(data.lane_data(0, 2).unwrap().step(0), Step::OFF);
        assert_eq!(data.lane_data(1, 2).unwrap().step(0), marked);
        assert_eq!(data.lane_data(7, 0).unwrap().step(0), Step::OFF);
    }

    #[test]
    fn empty_has_no_lanes_but_all_eight_patterns() {
        let data = SeqData::empty();
        assert_eq!(data.lane_count(), 0);
        assert_eq!(data.patterns.len(), NUM_PATTERNS);
        assert!(data.patterns.iter().all(Vec::is_empty));
        assert!(!data.any_solo());
        assert_eq!(data.max_lane_id(), None);
        assert!(data.lane(0).is_none());
        assert!(data.lane_data(0, 0).is_none());
        assert!(data.position_of(1).is_none());
    }

    #[test]
    fn position_of_finds_a_lane_by_its_stable_id() {
        let data = two_lanes();
        assert_eq!(data.position_of(1), Some(0));
        assert_eq!(data.position_of(2), Some(1));
        assert_eq!(data.position_of(99), None);
    }

    /// A shape edit starts by copying the live values out of the atomics, so
    /// the clone has to carry everything — including steps hidden past
    /// `length`, in every pattern.
    #[test]
    fn clone_copies_every_value_including_hidden_steps() {
        let lanes = vec![Lane::new(4, "Perc", 0xABCDEF, 45, 3)];
        let patterns: [Vec<LaneData>; NUM_PATTERNS] =
            std::array::from_fn(|_| vec![LaneData::with_capacity(48)]);
        let data = SeqData::new(lanes, patterns);

        data.lane(0).unwrap().set_key(50);
        data.lane(0).unwrap().set_channel(9);
        data.lane(0).unwrap().set_mute(true);
        data.lane(0).unwrap().set_solo(true);
        for pattern in 0..NUM_PATTERNS {
            let lane_data = data.lane_data(pattern, 0).unwrap();
            lane_data.set_length(8);
            lane_data.set_rate(Rate::QuarterDotted);
            lane_data.set_step(0, Step::default_on().with_velocity(pattern as u8 + 1));
            // Past `length`: invisible to playback, still part of the project.
            lane_data.set_step(47, Step::default_on().with_pan(pattern as u8));
        }

        let copy = data.clone();
        let lane = copy.lane(0).unwrap();
        assert_eq!((lane.id, lane.name.as_str(), lane.colour), (4, "Perc", 0xABCDEF));
        assert_eq!((lane.key(), lane.channel()), (50, 9));
        assert!(lane.is_muted() && lane.is_soloed());
        for pattern in 0..NUM_PATTERNS {
            let lane_data = copy.lane_data(pattern, 0).unwrap();
            assert_eq!(lane_data.capacity(), 48);
            assert_eq!(lane_data.length(), 8);
            assert_eq!(lane_data.rate(), Rate::QuarterDotted);
            assert_eq!(lane_data.step(0).velocity(), pattern as u8 + 1);
            assert_eq!(lane_data.step(47).pan(), pattern as u8, "hidden step lost");
        }

        // A deep copy: editing the clone leaves the original alone.
        copy.lane(0).unwrap().set_key(60);
        copy.lane_data(0, 0).unwrap().set_step(0, Step::OFF);
        assert_eq!(data.lane(0).unwrap().key(), 50);
        assert!(data.lane_data(0, 0).unwrap().step(0).is_on());
    }

    // ---- SeqModel ----

    /// Everything a lane owns, in every pattern, steps past `length`
    /// included — exactly what a shape edit has to carry across untouched.
    #[derive(Clone, Debug, PartialEq)]
    struct LaneFingerprint {
        id: u32,
        name: String,
        colour: u32,
        key: u8,
        channel: u8,
        mute: bool,
        solo: bool,
        /// One entry per pattern: length, rate, every step's raw bits.
        patterns: Vec<(u32, Rate, Vec<u32>)>,
    }

    fn fingerprint(data: &SeqData, pos: usize) -> LaneFingerprint {
        let lane = data.lane(pos).expect("lane in range");
        LaneFingerprint {
            id: lane.id,
            name: lane.name.clone(),
            colour: lane.colour,
            key: lane.key(),
            channel: lane.channel(),
            mute: lane.is_muted(),
            solo: lane.is_soloed(),
            patterns: (0..NUM_PATTERNS)
                .map(|pattern| {
                    let lane_data = data.lane_data(pattern, pos).expect("lane data in range");
                    let steps =
                        (0..lane_data.capacity()).map(|i| lane_data.step(i).bits()).collect();
                    (lane_data.length(), lane_data.rate(), steps)
                })
                .collect(),
        }
    }

    fn fingerprints(data: &SeqData) -> Vec<LaneFingerprint> {
        (0..data.lane_count()).map(|pos| fingerprint(data, pos)).collect()
    }

    /// Fills one lane with a value unique to every (lane, pattern, step), and
    /// with capacity well past `length`, so a shape edit that loses a hidden
    /// step or swaps two patterns shows up in its fingerprint.
    fn stamp_lane(model: &SeqModel, lane_id: u32, salt: u32) {
        assert!(model.set_key(lane_id, (20 + salt) as u8));
        assert!(model.set_channel(lane_id, (salt % 16) as u8));
        assert!(model.set_mute(lane_id, salt.is_multiple_of(2)));
        assert!(model.set_solo(lane_id, salt.is_multiple_of(3)));
        for pattern in 0..NUM_PATTERNS {
            assert!(model.ensure_capacity(pattern, lane_id, 48));
            assert!(model.set_length(pattern, lane_id, 8 + salt));
            let rate = Rate::ALL[(salt as usize + pattern) % Rate::ALL.len()];
            assert!(model.set_rate(pattern, lane_id, rate));
            for index in 0..48 {
                let n = salt * 31 + pattern as u32 * 7 + index as u32;
                let step = Step::pack(
                    index % 3 == 0,
                    (n % 127 + 1) as u8,
                    (n % 127) as u8,
                    (n % 8 + 1) as u8,
                    (n % 101) as u8,
                );
                assert!(model.set_step(pattern, lane_id, index, step));
            }
        }
    }

    /// Three stamped lanes — the fixture the shape-edit tests start from.
    fn stamped_model() -> SeqModel {
        let model = SeqModel::empty();
        for (salt, name) in ["Kick", "Snare", "Hat"].into_iter().enumerate() {
            let colour = 0x10_20_30 + salt as u32;
            let id = model.add_lane(name, colour, 36 + salt as u8, 0).expect("lane added");
            stamp_lane(&model, id, salt as u32);
        }
        model
    }

    /// The point of the atomics: a value edit lands on the `SeqData` the
    /// audio thread is already holding — no republish, so a reader mid-block
    /// sees the edit and nothing is retired behind its back.
    #[test]
    fn value_edits_land_on_the_published_data_without_republishing() {
        let model = stamped_model();
        let before = model.shared();
        let id = before.lane(1).expect("lane").id;

        assert!(model.set_step(2, id, 5, Step::default_on()));
        assert!(model.set_length(2, id, 12));
        assert!(model.set_rate(2, id, Rate::ThirtySecondTriplet));
        assert!(model.set_key(id, 99));
        assert!(model.set_channel(id, 9));
        assert!(model.set_mute(id, true));
        assert!(model.set_solo(id, true));

        assert!(
            std::ptr::eq(&*before, &*model.shared()),
            "a value edit must not publish a new SeqData"
        );
        let lane_data = before.lane_data(2, 1).expect("lane data");
        assert_eq!(lane_data.step(5), Step::default_on());
        assert_eq!(lane_data.length(), 12);
        assert_eq!(lane_data.rate(), Rate::ThirtySecondTriplet);
        let lane = before.lane(1).expect("lane");
        assert_eq!((lane.key(), lane.channel()), (99, 9));
        assert!(lane.is_muted() && lane.is_soloed());
    }

    /// A shape edit publishes a *new* `SeqData`; the one a reader is already
    /// holding stays valid and unchanged until it drops it.
    #[test]
    fn a_shape_edit_publishes_new_data_and_leaves_the_old_readable() {
        let model = stamped_model();
        let before = model.shared();
        let expected = fingerprints(&before);

        let new_id = model.add_lane("Perc", 0xABCDEF, 45, 3).expect("lane added");

        let after = model.shared();
        assert!(!std::ptr::eq(&*before, &*after), "a shape edit must republish");
        assert_eq!(before.lane_count(), 3, "the reader's data is frozen");
        assert_eq!(fingerprints(&before), expected);
        assert_eq!(after.lane_count(), 4);
        assert_eq!(&fingerprints(&after)[..3], &expected[..]);
        assert_eq!(after.lane(3).expect("new lane").id, new_id);
    }

    #[test]
    fn add_lane_appends_one_empty_lane_data_per_pattern() {
        let model = SeqModel::empty();
        let id = model.add_lane("Kick", 0x112233, 36, 2).expect("lane added");

        let data = model.shared();
        assert_eq!(data.lane_count(), 1);
        let lane = data.lane(0).expect("lane");
        assert_eq!((lane.id, lane.name.as_str(), lane.colour), (id, "Kick", 0x112233));
        assert_eq!((lane.key(), lane.channel()), (36, 2));
        assert!(!lane.is_muted() && !lane.is_soloed());
        assert_eq!(lane.is_sample_pad(), Lane::SAMPLE_PAD_DEFAULT);
        for pattern in 0..NUM_PATTERNS {
            let lane_data = data.lane_data(pattern, 0).expect("lane data");
            assert_eq!(lane_data.capacity(), LaneData::DEFAULT_CAPACITY);
            assert_eq!(lane_data.length(), LaneData::DEFAULT_LENGTH);
            assert_eq!(lane_data.rate(), Rate::DEFAULT);
            assert!((0..lane_data.capacity()).all(|i| lane_data.step(i) == Step::OFF));
        }
    }

    /// The lane cap is a refusal, not a clamp-and-corrupt: the instance keeps
    /// exactly `MAX_LANES` lanes and the refused add doesn't even burn an id.
    #[test]
    fn add_lane_refuses_past_the_lane_cap() {
        let model = SeqModel::empty();
        for i in 0..MAX_LANES {
            assert!(model.add_lane(format!("L{i}"), 0, 36, 0).is_some(), "lane {i}");
        }
        assert_eq!(model.shared().lane_count(), MAX_LANES);

        assert_eq!(model.add_lane("one too many", 0, 36, 0), None);
        let last = model.shared().lane(MAX_LANES - 1).expect("last lane").id;
        assert_eq!(model.duplicate_lane(last), None, "duplicating is capped too");
        assert_eq!(model.shared().lane_count(), MAX_LANES);

        assert!(model.remove_lane(last));
        assert_eq!(model.add_lane("room again", 0, 36, 0), Some(last + 1));
    }

    #[test]
    fn remove_lane_drops_only_that_lane_in_every_pattern() {
        let model = stamped_model();
        let before = fingerprints(&model.shared());
        let id = before[1].id;

        assert!(model.remove_lane(id));

        assert_eq!(fingerprints(&model.shared()), vec![before[0].clone(), before[2].clone()]);
        assert_eq!(model.shared().position_of(id), None);
        assert!(!model.remove_lane(id), "removing a gone lane is a no-op");
    }

    /// Reordering moves the content of every pattern with the lane, and ids
    /// stay put — that is what lets the GUI hold a selection by id across a
    /// drag.
    #[test]
    fn move_lane_reorders_every_pattern_without_changing_ids() {
        let model = stamped_model();
        let before = fingerprints(&model.shared());
        let id = before[0].id;

        assert!(model.move_lane(id, 2));
        assert_eq!(
            fingerprints(&model.shared()),
            vec![before[1].clone(), before[2].clone(), before[0].clone()]
        );
        assert_eq!(model.shared().position_of(id), Some(2));

        assert!(model.move_lane(id, 999), "past the end clamps to the last position");
        assert_eq!(model.shared().position_of(id), Some(2));

        assert!(model.move_lane(id, 0));
        assert_eq!(fingerprints(&model.shared()), before, "the round trip restores everything");
        assert!(!model.move_lane(4242, 0));
    }

    #[test]
    fn duplicate_lane_copies_every_pattern_under_a_fresh_id() {
        let model = stamped_model();
        let before = fingerprints(&model.shared());
        let source = before[0].id;

        let copy = model.duplicate_lane(source).expect("duplicated");
        assert_ne!(copy, source);

        let data = model.shared();
        assert_eq!(data.lane_count(), 4);
        assert_eq!(data.position_of(copy), Some(1), "inserted below its source");
        let after = fingerprints(&data);
        assert_eq!((&after[0], &after[2], &after[3]), (&before[0], &before[1], &before[2]));
        let mut expected = before[0].clone();
        expected.id = copy;
        assert_eq!(after[1], expected, "only the id differs");

        // Separate storage, not a shared slice: step 40 is a hidden step of
        // the copy, and erasing it must not touch the source.
        assert!(model.set_step(3, copy, 40, Step::OFF));
        assert_eq!(fingerprints(&model.shared())[0], before[0]);
    }

    #[test]
    fn rename_and_recolour_keep_every_value() {
        let model = stamped_model();
        let before = fingerprints(&model.shared());
        let id = before[2].id;

        assert!(model.rename_lane(id, "Clap"));
        assert!(model.set_colour(id, 0x00FF99));

        let after = fingerprints(&model.shared());
        let mut expected = before[2].clone();
        expected.name = "Clap".to_string();
        expected.colour = 0x00FF99;
        assert_eq!(after[2], expected);
        assert_eq!(&after[..2], &before[..2]);
        assert!(!model.rename_lane(4242, "nobody"));
        assert!(!model.set_colour(4242, 0));
    }

    /// Growing capacity rounds up to a [`CAPACITY_CHUNK`] boundary, keeps
    /// every existing step (hidden ones included) plus length and rate, and
    /// touches nothing else — not the other patterns, not the other lanes.
    #[test]
    fn ensure_capacity_grows_in_chunks_and_keeps_every_step() {
        let model = stamped_model();
        let id = model.shared().lane(1).expect("lane").id;
        let before = fingerprints(&model.shared());

        let published = model.shared();
        assert!(model.ensure_capacity(0, id, 48));
        assert!(
            std::ptr::eq(&*published, &*model.shared()),
            "already big enough: no republish, so a length drag inside a chunk stays cheap"
        );

        assert!(model.ensure_capacity(0, id, 49));
        let data = model.shared();
        assert_eq!(data.lane_data(0, 1).expect("lane data").capacity(), 64);

        let after = fingerprints(&data);
        assert_eq!((&after[0], &after[2]), (&before[0], &before[2]), "other lanes untouched");
        assert_eq!(after[1].patterns[1..], before[1].patterns[1..], "other patterns untouched");
        let (length, rate, steps) = &after[1].patterns[0];
        let (was_length, was_rate, was_steps) = &before[1].patterns[0];
        assert_eq!((length, rate), (was_length, was_rate));
        assert_eq!(&steps[..48], &was_steps[..]);
        assert!(steps[48..].iter().all(|&bits| bits == Step::OFF.bits()), "new tail is off");
    }

    #[test]
    fn ensure_capacity_clamps_to_the_step_cap() {
        let model = SeqModel::empty();
        let id = model.add_lane("Kick", 0, 36, 0).expect("lane added");

        assert!(model.ensure_capacity(0, id, 1_000_000_000));

        assert_eq!(model.shared().lane_data(0, 0).expect("lane data").capacity(), MAX_STEPS);
        assert!(model.set_length(0, id, u32::MAX));
        assert_eq!(model.shared().lane_data(0, 0).expect("lane data").length(), MAX_STEPS as u32);
    }

    /// The GUI can always be a frame behind a removal, and a snapshot can name
    /// a pattern or step that doesn't exist. Every such edit reports `false`
    /// and changes nothing.
    #[test]
    fn edits_to_a_missing_lane_pattern_or_step_are_no_ops() {
        let model = stamped_model();
        let before = fingerprints(&model.shared());
        let id = before[0].id;

        assert!(!model.set_step(0, 4242, 0, Step::default_on()));
        assert!(!model.set_step(NUM_PATTERNS, id, 0, Step::default_on()));
        assert!(!model.set_step(0, id, 48, Step::default_on()), "past the capacity");
        assert!(!model.toggle_step(0, id, 999));
        assert!(!model.update_step(0, 4242, 0, |step| step.with_velocity(1)));
        assert!(!model.set_length(NUM_PATTERNS, id, 4));
        assert!(!model.set_rate(NUM_PATTERNS, id, Rate::Whole));
        assert!(!model.set_key(4242, 60));
        assert!(!model.set_channel(4242, 1));
        assert!(!model.set_mute(4242, true));
        assert!(!model.set_solo(4242, true));
        assert!(!model.ensure_capacity(0, 4242, 64));
        assert!(!model.ensure_capacity(NUM_PATTERNS, id, 64));
        assert_eq!(model.duplicate_lane(4242), None);

        assert_eq!(fingerprints(&model.shared()), before);
    }

    /// Toggling is how the grid is drawn on, so a never-edited step has to
    /// come on usable: an all-zero step would otherwise read back as
    /// hard-left and 0% probability.
    #[test]
    fn toggling_a_fresh_step_gives_it_the_default_values() {
        let model = SeqModel::empty();
        let id = model.add_lane("Kick", 0, 36, 0).expect("lane added");

        assert!(model.toggle_step(0, id, 0));
        assert_eq!(model.shared().lane_data(0, 0).expect("lane data").step(0), Step::default_on());

        // Switching a step off keeps its values, so toggling back on restores
        // the velocity the user drew.
        let drawn = Step::pack(true, 42, 10, 3, 55);
        assert!(model.set_step(0, id, 1, drawn));
        assert!(model.toggle_step(0, id, 1));
        let step = model.shared().lane_data(0, 0).expect("lane data").step(1);
        assert_eq!(step, drawn.with_on(false));
        assert!(model.toggle_step(0, id, 1));
        assert_eq!(model.shared().lane_data(0, 0).expect("lane data").step(1), drawn);

        // Reserved bits from a future build survive the defaults being
        // installed.
        assert!(model.set_step(0, id, 2, Step(Step::RESERVED_MASK)));
        assert!(model.toggle_step(0, id, 2));
        let step = model.shared().lane_data(0, 0).expect("lane data").step(2);
        assert_eq!(step.bits(), Step::default_on().bits() | Step::RESERVED_MASK);
    }

    #[test]
    fn update_step_reads_modifies_and_writes_one_step() {
        let model = SeqModel::empty();
        let id = model.add_lane("Kick", 0, 36, 0).expect("lane added");
        assert!(model.set_step(4, id, 3, Step::default_on()));

        assert!(model.update_step(4, id, 3, |step| step.with_velocity(7)));

        let step = model.shared().lane_data(4, 0).expect("lane data").step(3);
        assert_eq!((step.is_on(), step.velocity(), step.pan()), (true, 7, Step::PAN_CENTRE));
    }

    // ---- bulk lane ops ----

    /// A lane whose capacity is well past its length, with a distinct step in
    /// every slot: the hidden tail is what the bulk ops must not touch.
    fn bulk_model() -> (SeqModel, u32) {
        let model = SeqModel::empty();
        let id = model.add_lane("Kick", 0, 36, 0).expect("lane added");
        for pattern in 0..NUM_PATTERNS {
            assert!(model.ensure_capacity(pattern, id, 32));
            assert!(model.set_length(pattern, id, 8));
            for index in 0..32 {
                let n = (pattern * 32 + index) as u32;
                let step = Step::pack(
                    index % 2 == 0,
                    (n % 127 + 1) as u8,
                    (n % 127) as u8,
                    (n % 8 + 1) as u8,
                    (n % 101) as u8,
                );
                assert!(model.set_step(pattern, id, index, step));
            }
        }
        (model, id)
    }

    /// Every step of one lane in one pattern, hidden ones included.
    fn lane_steps(model: &SeqModel, pattern: usize) -> Vec<Step> {
        let data = model.shared();
        let lane_data = data.lane_data(pattern, 0).expect("lane data");
        (0..lane_data.capacity()).map(|index| lane_data.step(index)).collect()
    }

    /// The bulk ops are value edits, so they land on the `SeqData` the audio
    /// thread already holds and the hidden tail survives untouched.
    #[test]
    fn bulk_ops_stay_inside_length_and_never_republish() {
        let (model, id) = bulk_model();
        let before = model.shared();
        let tail: Vec<Vec<Step>> =
            (0..NUM_PATTERNS).map(|p| lane_steps(&model, p)[8..].to_vec()).collect();

        assert!(model.clear_lane(0, id));
        assert!(model.invert_lane(1, id));
        assert!(model.randomise_lane(2, id, 0.5, &mut Prng::new(7)));
        assert!(model.nudge_lane(3, id, 1));
        assert!(model.nudge_lane(4, id, -3));

        assert!(
            std::ptr::eq(&*before, &*model.shared()),
            "a bulk op is a value edit: no new SeqData"
        );
        for (pattern, untouched) in tail.iter().enumerate() {
            assert_eq!(
                lane_steps(&model, pattern)[8..],
                untouched[..],
                "pattern {pattern}: steps past length must be untouched"
            );
            let data = model.shared();
            assert_eq!(data.lane_data(pattern, 0).expect("lane data").length(), 8);
        }
        // Only the pattern each op named changed.
        for pattern in 5..NUM_PATTERNS {
            assert_eq!(lane_steps(&model, pattern), lane_steps(&bulk_model().0, pattern));
        }
    }

    #[test]
    fn pitch_is_a_signed_field_that_leaves_the_others_alone() {
        let base = Step::pack(true, 99, 11, 5, 42);
        for pitch in Step::PITCH_MIN..=Step::PITCH_MAX {
            let step = base.with_pitch(pitch);
            assert_eq!(step.pitch(), pitch);
            assert_eq!(step.with_pitch(0), base, "pitch {pitch} touched another field");
        }
        assert_eq!(base.with_pitch(100).pitch(), Step::PITCH_MAX, "clamped");
        assert_eq!(base.with_pitch(-100).pitch(), Step::PITCH_MIN, "clamped");
        assert_eq!(Step::OFF.pitch(), 0, "all-zero bits are no pitch");
        // Raw bits outside ±24 (e.g. from a future build) read clamped.
        assert_eq!(Step(31 << 25).pitch(), Step::PITCH_MAX);
    }

    #[test]
    fn note_defaults_fill_a_never_edited_step_and_leave_it_off() {
        let empty = Step::OFF.with_pitch(-7);
        let filled = empty.with_note_defaults();
        assert!(!filled.is_on());
        assert!(!filled.is_untouched());
        assert_eq!(filled.velocity(), 100);
        assert_eq!(filled.pan(), Step::PAN_CENTRE);
        assert_eq!(filled.ratchet(), 1);
        assert_eq!(filled.probability(), Step::PROBABILITY_MAX);
        assert_eq!(filled.pitch(), -7, "pitch survives");
        assert_eq!(filled.with_on(true), empty.turned_on());

        // A step that was edited keeps its values.
        let edited = Step::default_on().with_velocity(20).with_on(false);
        assert_eq!(edited.with_note_defaults(), edited);
    }

    #[test]
    fn a_pitch_drawn_on_a_never_edited_step_survives_switching_it_on() {
        let pitched = Step::OFF.with_pitch(-7);
        assert!(pitched.is_untouched(), "pitch alone does not count as edited");
        let on = pitched.turned_on();
        assert_eq!(on.with_pitch(0), Step::default_on(), "the defaults go in");
        assert_eq!(on.pitch(), -7, "and the pitch stays");
        assert_eq!(Step::default_on().with_pitch(5).cleared(), Step::OFF, "clear wipes pitch");
    }

    #[test]
    fn clear_lane_wipes_the_visible_steps_back_to_off() {
        let (model, id) = bulk_model();
        assert!(model.clear_lane(0, id));

        let steps = lane_steps(&model, 0);
        assert!(steps[..8].iter().all(|step| *step == Step::OFF), "{steps:?}");

        // The reserved bit is the one thing a clear keeps (pitch goes too),
        // and a cleared step draws back on with the defaults.
        let marked = Step::pack(true, 9, 9, 2, 9).with_pitch(5).bits() | Step::RESERVED_MASK;
        assert!(model.set_step(0, id, 0, Step(marked)));
        assert!(model.clear_lane(0, id));
        assert_eq!(lane_steps(&model, 0)[0].bits(), Step::RESERVED_MASK);
        assert!(model.toggle_step(0, id, 0));
        assert_eq!(
            lane_steps(&model, 0)[0].bits(),
            Step::default_on().bits() | Step::RESERVED_MASK
        );
    }

    #[test]
    fn invert_lane_flips_the_visible_steps_and_keeps_their_values() {
        let (model, id) = bulk_model();
        let before = lane_steps(&model, 0);

        assert!(model.invert_lane(0, id));
        let once = lane_steps(&model, 0);
        for index in 0..8 {
            assert_eq!(once[index].is_on(), !before[index].is_on(), "step {index}");
            assert_eq!(once[index].with_on(false), before[index].with_on(false), "step {index}");
        }

        // Inverting twice is the identity on steps that already had values.
        assert!(model.invert_lane(0, id));
        assert_eq!(lane_steps(&model, 0), before);

        // A never-edited step comes on usable, like a toggle.
        let fresh = SeqModel::empty();
        let fresh_id = fresh.add_lane("Hat", 0, 42, 0).expect("lane added");
        assert!(fresh.invert_lane(0, fresh_id));
        let data = fresh.shared();
        let lane_data = data.lane_data(0, 0).expect("lane data");
        for index in 0..lane_data.length() as usize {
            assert_eq!(lane_data.step(index), Step::default_on(), "step {index}");
        }
    }

    #[test]
    fn randomise_lane_honours_the_density_ends_and_a_fixed_seed() {
        let (model, id) = bulk_model();

        // Density 0 switches every visible step off; density 1 switches every
        // one on. Both keep the values already in the steps.
        let before = lane_steps(&model, 0);
        assert!(model.randomise_lane(0, id, 0.0, &mut Prng::new(1)));
        let off = lane_steps(&model, 0);
        assert!(off[..8].iter().all(|step| !step.is_on()), "{off:?}");
        assert!(model.randomise_lane(0, id, 1.0, &mut Prng::new(1)));
        let on = lane_steps(&model, 0);
        assert!(on[..8].iter().all(|step| step.is_on()), "{on:?}");
        for index in 0..8 {
            assert_eq!(on[index].with_on(false), before[index].with_on(false), "step {index}");
        }

        // Out of range and NaN clamp rather than panicking or filling at random.
        assert!(model.randomise_lane(0, id, -5.0, &mut Prng::new(1)));
        assert!(lane_steps(&model, 0)[..8].iter().all(|step| !step.is_on()));
        assert!(model.randomise_lane(0, id, 9.0, &mut Prng::new(1)));
        assert!(lane_steps(&model, 0)[..8].iter().all(|step| step.is_on()));
        assert!(model.randomise_lane(0, id, f32::NAN, &mut Prng::new(1)));
        assert!(lane_steps(&model, 0)[..8].iter().all(|step| !step.is_on()));

        // Same seed, same lane: identical result. A half density on a long
        // lane lands somewhere in between.
        let pattern_of = |seed: u64| {
            let (model, id) = bulk_model();
            assert!(model.set_length(0, id, 32));
            assert!(model.randomise_lane(0, id, 0.5, &mut Prng::new(seed)));
            lane_steps(&model, 0).iter().map(|step| step.is_on()).collect::<Vec<bool>>()
        };
        let a = pattern_of(12345);
        assert_eq!(a, pattern_of(12345));
        assert_ne!(a, pattern_of(999));
        let lit = a.iter().filter(|on| **on).count();
        assert!((4..=28).contains(&lit), "half density lit {lit} of 32");
    }

    /// A typed song order: numbers one-based, `0` a rest, `NxR` repeats,
    /// any of `-`, `,` and spaces between, and every mistake named.
    #[test]
    fn a_song_order_parses_into_one_entry_per_bar() {
        assert_eq!(parse_song(""), Ok(vec![]));
        assert_eq!(parse_song(" - , "), Ok(vec![]), "separators alone are an empty song");
        assert_eq!(parse_song("1-2-1-3"), Ok(vec![0, 1, 0, 2]));
        assert_eq!(parse_song("1, 2  3"), Ok(vec![0, 1, 2]));
        assert_eq!(parse_song("4x3-1"), Ok(vec![3, 3, 3, 0]));
        assert_eq!(parse_song("2X2 5*1"), Ok(vec![1, 1, 4]));
        assert_eq!(parse_song("1-0-64"), Ok(vec![0, Song::REST, 63]));

        assert_eq!(parse_song("1-a"), Err(SongError::NotANumber("a".into())));
        assert_eq!(parse_song("3x"), Err(SongError::NotANumber("3x".into())));
        assert_eq!(parse_song("-1"), Ok(vec![0]), "a leading dash is a separator");
        assert_eq!(parse_song("65"), Err(SongError::NoSuchPattern(65)));
        assert_eq!(parse_song("2x0"), Err(SongError::ZeroRepeat("2x0".into())));
        assert_eq!(parse_song("1x1024").map(|entries| entries.len()), Ok(MAX_SONG_ENTRIES));
        assert_eq!(parse_song("1x1024-2"), Err(SongError::TooLong));
        assert_eq!(parse_song("1x99999999999999999999"), Err(SongError::NotANumber(
            "1x99999999999999999999".into()
        )));
        assert!(SongError::NoSuchPattern(65).to_string().contains("1-64"));
    }

    /// Each entry lasts its pattern's longest lane in whole bars: rests are
    /// one bar, and bars before the start or past the end are in no entry.
    #[test]
    fn a_song_places_each_entry_on_its_bars() {
        let model = SeqModel::empty();
        model.add_lane("kick", 0, 36, 0).expect("room for a lane");
        model.add_lane("snare", 0, 38, 0).expect("room for a lane");
        let data = model.shared();
        // Every lane starts as one bar of sixteenths.
        for pattern in &data.patterns[..3] {
            pattern.iter().for_each(|lane_data| lane_data.set_length(16));
        }
        // Pattern 2: 16 quarters is four bars, whatever the other lane does.
        data.patterns[1][0].set_rate(Rate::Quarter);
        // Pattern 3: 12 triplet eighths fill one bar exactly.
        data.patterns[2][1].set_rate(Rate::EighthTriplet);
        data.patterns[2][1].set_length(12);
        let data = (*data).clone().with_song(Song::from_text("2-0-3-1".into(), true));
        let lengths = PatternBars::new(&data, DEFAULT_BAR_BEATS);
        let song = &data.song;
        assert!(song.is_on());
        assert_eq!(song.len(), 4);
        assert_eq!(song.total_bars(&lengths), 7);
        let pos = |entry, start_bar, bars, pattern| {
            SongPos { entry, start_bar, lap_start: 0, bars, pattern }
        };
        assert_eq!(
            song.positions(&lengths).collect::<Vec<_>>(),
            [
                pos(0, 0, 4, Some(1)),
                pos(1, 4, 1, None),
                pos(2, 5, 1, Some(2)),
                pos(3, 6, 1, Some(0)),
            ]
        );
        song.set_looped(false);
        let entries = (-1..=7).map(|bar| song.locate(bar, &lengths).map(|pos| pos.entry));
        assert_eq!(
            entries.collect::<Vec<_>>(),
            [None, Some(0), Some(0), Some(0), Some(0), Some(1), Some(2), Some(3), None]
        );
        // Looped, bar 7 is the first bar again, a lap of 7 bars on.
        song.set_looped(true);
        assert_eq!(song.locate(-1, &lengths), None, "the count-in stays silent");
        let again = SongPos { entry: 0, start_bar: 7, lap_start: 7, bars: 4, pattern: Some(1) };
        assert_eq!(song.locate(7, &lengths), Some(again));
        assert_eq!(song.locate(10, &lengths), Some(again));
        let far = song.locate(7 * 1000 + 5, &lengths).expect("looped forever");
        assert_eq!((far.entry, far.start_bar, far.lap_start), (2, 7005, 7000));
        assert_eq!(Song::new().locate(0, &lengths), None, "an empty song never plays");
        // In 3/4 the same four-bar lane needs six bars.
        assert_eq!(PatternBars::new(&data, 3.0).of(1), 6);
        assert_eq!(PatternBars::new(&data, f64::NAN).of(1), 1, "nonsense bars");
        assert_eq!(lengths.of(Song::REST), 1);

        let hostile = Song::from_text("1-banana".into(), false);
        assert!(hostile.is_empty(), "text that does not parse plays nothing");
        assert_eq!(hostile.text, "1-banana", "but is kept for the user to fix");
        assert!(!Song::default().is_on());
    }

    /// The song through the writer: text that parses is published, text
    /// that does not changes nothing, and the switch is a value edit.
    #[test]
    fn the_song_is_set_through_the_model() {
        let model = SeqModel::empty();
        assert_eq!(model.set_song("1-2x2"), Ok(3));
        assert_eq!(model.shared().song.entries, [0, 1, 1]);
        assert_eq!(model.shared().song.text, "1-2x2");
        assert_eq!(model.set_song("1-99"), Err(SongError::NoSuchPattern(99)));
        assert_eq!(model.shared().song.text, "1-2x2", "a bad order leaves the song alone");

        let before = model.shared();
        model.set_song_on(true);
        assert!(before.song.is_on(), "switched on in place, no republish");
        assert!(before.song.is_looped(), "a song loops unless told not to");
        model.set_song_looped(false);
        assert!(!before.song.is_looped(), "also in place");
        // A shape edit carries the song and the switch along.
        let id = model.add_lane("kick", 0, 36, 0).expect("room for a lane");
        assert!(model.shared().song.is_on());
        assert!(!model.shared().song.is_looped());
        assert_eq!(model.shared().song.entries, [0, 1, 1]);
        assert!(model.remove_lane(id));
    }

    /// Only exactly what a new lane starts with counts as pristine.
    #[test]
    fn pristine_lane_data_is_what_a_new_lane_starts_with() {
        assert!(LaneData::empty().is_pristine());
        let touched = LaneData::empty();
        touched.set_step(3, Step::OFF.with_pitch(2));
        assert!(!touched.is_pristine(), "a pitch on an off step is still an edit");
        let longer = LaneData::empty();
        longer.set_length(8);
        assert!(!longer.is_pristine());
        let slower = LaneData::empty();
        slower.set_rate(Rate::Eighth);
        assert!(!slower.is_pristine());
        assert!(!LaneData::with_capacity(48).is_pristine(), "grown");
        assert!(!LaneData::with_capacity(16).is_pristine(), "a 16-step lane from an old save");
    }

    #[test]
    fn copy_pattern_overwrites_the_target_with_the_source() {
        let (model, id) = bulk_model();
        assert!(model.set_length(1, id, 5));
        assert!(model.set_rate(1, id, Rate::Eighth));
        assert!(model.ensure_capacity(1, id, 64));
        let source = lane_steps(&model, 1);
        let untouched = lane_steps(&model, 0);

        assert!(model.copy_pattern(1, 6));
        let data = model.shared();
        let copy = data.lane_data(6, 0).expect("lane data");
        assert_eq!((copy.length(), copy.rate(), copy.capacity()), (5, Rate::Eighth, 64));
        assert_eq!(lane_steps(&model, 6), source, "every step, hidden ones too");
        assert_eq!(lane_steps(&model, 1), source, "the source is unchanged");
        assert_eq!(lane_steps(&model, 0), untouched, "other patterns are unchanged");

        // A copy is independent of its source.
        assert!(model.set_step(6, id, 0, Step::OFF));
        assert_eq!(lane_steps(&model, 1), source);

        assert!(!model.copy_pattern(2, 2), "onto itself");
        assert!(!model.copy_pattern(0, NUM_PATTERNS), "out of range");
        assert!(!model.copy_pattern(NUM_PATTERNS, 0), "out of range");
    }

    #[test]
    fn fill_lane_sets_visible_steps_and_keeps_their_values() {
        let (model, id) = bulk_model();
        let before = lane_steps(&model, 0);

        let mut asked = Vec::new();
        assert!(model.fill_lane(0, id, |index| {
            asked.push(index);
            index % 4 == 0
        }));
        assert_eq!(asked, (0..8).collect::<Vec<usize>>(), "once per visible step, in order");

        let after = lane_steps(&model, 0);
        for index in 0..8 {
            assert_eq!(after[index].is_on(), index % 4 == 0, "step {index}");
            assert_eq!(after[index].with_on(false), before[index].with_on(false), "step {index}");
        }
        // Hidden steps past the length are left alone.
        assert_eq!(after[8..], before[8..]);
        // Another pattern is untouched, and an unknown lane is refused.
        assert_eq!(lane_steps(&model, 1)[..8], bulk_model_steps(1)[..8]);
        assert!(!model.fill_lane(0, id + 1, |_| true));
    }

    /// [`bulk_model`]'s steps for `pattern`, from a fresh copy.
    fn bulk_model_steps(pattern: usize) -> Vec<Step> {
        let (model, _) = bulk_model();
        lane_steps(&model, pattern)
    }

    #[test]
    fn nudge_lane_rotates_within_length_and_wraps() {
        let (model, id) = bulk_model();
        let before = lane_steps(&model, 0);

        assert!(model.nudge_lane(0, id, 1));
        let later = lane_steps(&model, 0);
        for index in 0..8 {
            assert_eq!(later[(index + 1) % 8], before[index], "step {index} moved one later");
        }

        assert!(model.nudge_lane(0, id, -1));
        assert_eq!(lane_steps(&model, 0), before, "one back undoes one forward");

        // A full turn, no turn, and a wrapping delta all behave.
        assert!(model.nudge_lane(0, id, 8));
        assert_eq!(lane_steps(&model, 0), before);
        assert!(model.nudge_lane(0, id, 0));
        assert_eq!(lane_steps(&model, 0), before);
        assert!(model.nudge_lane(0, id, 11));
        let wrapped = lane_steps(&model, 0);
        assert!(model.nudge_lane(0, id, -3));
        assert_eq!(lane_steps(&model, 0), before, "delta 11 == delta 3 on a length of 8");
        assert_ne!(wrapped, before);

        // Extremes can't panic, and a one-step lane has nowhere to rotate.
        assert!(model.nudge_lane(0, id, i32::MIN));
        assert!(model.nudge_lane(0, id, i32::MAX));
        assert!(model.set_length(0, id, 1));
        let single = lane_steps(&model, 0);
        assert!(model.nudge_lane(0, id, 1));
        assert_eq!(lane_steps(&model, 0), single);
    }

    #[test]
    fn bulk_ops_on_a_missing_lane_or_pattern_are_no_ops() {
        let (model, id) = bulk_model();
        let before = fingerprints(&model.shared());

        assert!(!model.clear_lane(0, 4242));
        assert!(!model.clear_lane(NUM_PATTERNS, id));
        assert!(!model.invert_lane(0, 4242));
        assert!(!model.invert_lane(NUM_PATTERNS, id));
        assert!(!model.randomise_lane(0, 4242, 1.0, &mut Prng::new(1)));
        assert!(!model.randomise_lane(NUM_PATTERNS, id, 1.0, &mut Prng::new(1)));
        assert!(!model.nudge_lane(0, 4242, 1));
        assert!(!model.nudge_lane(NUM_PATTERNS, id, 1));

        assert_eq!(fingerprints(&model.shared()), before);
    }

    /// Ids are handed out monotonically and never reused, so a GUI selection
    /// or a saved reference can't silently come to mean a different lane.
    #[test]
    fn lane_ids_are_unique_and_resume_above_loaded_data() {
        let model = SeqModel::new(SeqData::from_lanes(vec![
            Lane::new(7, "Kick", 0, 36, 0),
            Lane::new(41, "Snare", 0, 38, 0),
        ]));

        assert_eq!(model.add_lane("Hat", 0, 42, 0), Some(42));
        assert!(model.remove_lane(42));
        assert_eq!(model.add_lane("Hat", 0, 42, 0), Some(43));
        assert_eq!(model.duplicate_lane(7), Some(44));

        assert_eq!(SeqModel::empty().add_lane("Kick", 0, 36, 0), Some(FIRST_LANE_ID));
    }

    // ---- state load ----

    /// A load is lossless in both directions: everything `snapshot()` reads
    /// out of one instance — lane identity and flags, per-pattern
    /// length/rate/capacity, every step including the ones past `length` —
    /// comes back in the instance that loads it.
    #[test]
    fn load_snapshot_replaces_the_whole_sequence() {
        let saved = stamped_model();
        let expected = fingerprints(&saved.shared());

        let loaded = SeqModel::empty();
        loaded.load_snapshot(saved.snapshot());

        assert_eq!(fingerprints(&loaded.shared()), expected);

        // And loading an instance's own snapshot back into it is the
        // identity, which is what a host save/load cycle amounts to.
        loaded.load_snapshot(loaded.snapshot());
        assert_eq!(fingerprints(&loaded.shared()), expected);
    }

    /// A load is a shape edit: it publishes a *new* `SeqData` and leaves the
    /// one a reader is already holding intact until that reader is done with
    /// it, so an audio thread mid-block never has the sequence pulled out
    /// from under it.
    #[test]
    fn a_state_load_publishes_new_data_and_leaves_the_old_readable() {
        let model = stamped_model();
        let reader = model.shared();
        let before = fingerprints(&reader);
        assert_eq!(model.live_allocs(), 1);

        model.load_snapshot(SeqSnapshot::from_data(&SeqData::from_lanes(vec![Lane::new(
            9, "Loaded", 0x00_FF_00, 64, 2,
        )])));

        assert!(!std::ptr::eq(&*reader, &*model.shared()), "a load must publish new data");
        assert_eq!(fingerprints(&reader), before, "the retired data stays readable");
        assert_eq!(model.live_allocs(), 2, "a reader's data must not be freed under it");

        let now = model.shared();
        assert_eq!(now.lane_count(), 1);
        assert_eq!((now.lane(0).expect("lane").id, now.lane(0).expect("lane").key()), (9, 64));

        drop(reader);
        drop(now);
        model.collect();
        assert_eq!(model.live_allocs(), 1, "the retired data is reclaimed once nobody reads it");
    }

    /// The writer mutex's job, exercised as the sequential interleavings it
    /// serialises edits into: an edit before a load is discarded with the
    /// data the load retires, and an edit after one lands on the loaded data.
    /// What must never happen is a surviving mixture of the two.
    #[test]
    fn value_edits_around_a_load_stay_consistent() {
        let model = stamped_model();
        let id = model.shared().lane(1).expect("lane").id;
        let saved = model.snapshot();

        // Edit, then load: the load wins — the user asked for the saved
        // sequence, and this edit belonged to the one being replaced.
        assert!(model.set_step(0, id, 0, Step::default_on()));
        assert!(model.set_key(id, 7));
        let stale = model.shared();
        model.load_snapshot(saved.clone());

        let after_load = model.shared();
        assert_eq!(fingerprints(&after_load), fingerprints(&saved.clone().into_data()));
        assert_ne!(after_load.lane(1).expect("lane").key(), 7, "the load replaced the edit");

        // Edit, then read: the edit lands on the loaded data (in place, no
        // republish) and not on the retired copy the stale reference holds.
        assert!(model.set_key(id, 77));
        assert!(model.set_step(0, id, 1, Step::default_on()));
        assert!(std::ptr::eq(&*after_load, &*model.shared()), "a value edit must not republish");
        assert_eq!(after_load.lane(1).expect("lane").key(), 77);
        assert_eq!(stale.lane(1).expect("lane").key(), 7, "the retired data is untouched");
    }

    /// Edits aimed at a lane the loaded sequence does not have are no-ops,
    /// not panics: the GUI can be a frame behind a host state load.
    #[test]
    fn edits_for_a_lane_a_load_removed_are_no_ops() {
        let model = stamped_model();
        let gone = model.shared().lane(2).expect("lane").id;

        model.load_snapshot(SeqSnapshot::default());

        assert_eq!(model.shared().lane_count(), 0);
        assert!(model.shared().patterns.iter().all(Vec::is_empty));
        assert!(!model.set_step(0, gone, 0, Step::default_on()));
        assert!(!model.set_length(0, gone, 4));
        assert!(!model.set_key(gone, 60));
        assert!(!model.remove_lane(gone));
        assert!(!model.ensure_capacity(0, gone, 32));
        assert!(!model.clear_lane(0, gone));
    }

    /// Lane ids stay unique across a load in both directions: above the ids
    /// the file brought in, and above the ids this session already handed
    /// out (a snapshot with low ids must not make the next lane reuse one).
    #[test]
    fn load_snapshot_resumes_lane_ids_above_every_id_seen() {
        let model = SeqModel::empty();

        model.load_snapshot(SeqSnapshot::from_data(&SeqData::from_lanes(vec![
            Lane::new(7, "Kick", 0, 36, 0),
            Lane::new(41, "Snare", 0, 38, 0),
        ])));
        assert_eq!(model.add_lane("Hat", 0, 42, 0), Some(42));

        model.load_snapshot(SeqSnapshot::from_data(&SeqData::from_lanes(vec![Lane::new(
            1, "Kick", 0, 36, 0,
        )])));
        assert_eq!(model.add_lane("Hat", 0, 42, 0), Some(43), "ids are never reused in a session");

        model.load_snapshot(SeqSnapshot::default());
        assert_eq!(model.add_lane("Hat", 0, 42, 0), Some(44));
    }

    /// Hostile state reaches the model as an ordinary load: the rules in
    /// `SeqSnapshot::into_data` cut it down to the caps, and the published
    /// sequence is one the audio thread can read.
    #[test]
    fn a_hostile_snapshot_loads_clamped_rather_than_panicking() {
        let model = stamped_model();
        let snapshot = SeqSnapshot {
            version: 0,
            lanes: vec![
                LaneSnapshot { id: 0, key: 200, channel: 99, ..Default::default() };
                300
            ],
            patterns: vec![PatternSnapshot { lanes: Vec::new() }; 20],
            song: Default::default(),
        };

        model.load_snapshot(snapshot);

        let data = model.shared();
        assert_eq!(data.lane_count(), MAX_LANES);
        for pattern in 0..NUM_PATTERNS {
            assert_eq!(data.patterns[pattern].len(), MAX_LANES, "pattern {pattern}");
        }
        assert!(data.lanes.iter().all(|lane| lane.key() == 127 && lane.channel() == 15));
        assert_eq!(data.lanes.iter().filter(|lane| lane.id == 0).count(), 0);
        assert_eq!(model.add_lane("Hat", 0, 42, 0), None, "the lane cap is full");
    }

    /// `collect()` is the whole reason shape edits are safe: the retired
    /// `SeqData` is freed only once no reader holds it.
    #[test]
    fn collect_reclaims_the_data_shape_edits_retire() {
        let model = stamped_model();
        assert_eq!(model.live_allocs(), 1, "steady state: just the published data");

        let reader = model.shared();
        let id = model.add_lane("Perc", 0, 45, 0).expect("lane added");
        model.collect();
        assert_eq!(model.live_allocs(), 2, "a reader's data must not be freed under it");

        drop(reader);
        model.collect();
        assert_eq!(model.live_allocs(), 1);

        let mut id = id;
        for _ in 0..8 {
            assert!(model.remove_lane(id));
            id = model.add_lane("Perc", 0, 45, 0).expect("lane added");
        }
        assert_eq!(model.live_allocs(), 1, "shape edits with no reader don't pile up");
    }

    /// Dropping the model while the audio thread still holds a block's data
    /// must not free that data (basedrop's collector refuses to clean up
    /// while an allocation is live, which is the safe direction).
    #[test]
    fn a_reader_outliving_the_model_still_reads_its_data() {
        let reader = {
            let model = stamped_model();
            model.shared()
        };

        assert_eq!(reader.lane_count(), 3);
        assert_eq!(reader.lane(0).expect("lane").name, "Kick");
        assert_eq!(reader.lane_data(7, 2).expect("lane data").capacity(), 48);
    }

    /// A reader on another thread never sees a half-built `SeqData`: lanes and
    /// all patterns stay index-aligned through every shape edit, because the
    /// replacement is only published once it is complete. The real audio-path
    /// stress test is `tests/concurrency_stress.rs`; this pins the invariant
    /// cheaply.
    #[test]
    fn a_concurrent_reader_only_ever_sees_aligned_data() {
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;

        let model = Arc::new(stamped_model());
        let stop = Arc::new(AtomicBool::new(false));
        let reader = {
            let model = Arc::clone(&model);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                while !stop.load(Relaxed) {
                    // One `get()` per "block", then only that reference.
                    let data = model.shared();
                    for pattern in &data.patterns {
                        assert_eq!(pattern.len(), data.lane_count());
                    }
                    for pos in 0..data.lane_count() {
                        let lane_data = data.lane_data(0, pos).expect("aligned");
                        assert!(lane_data.length() as usize <= lane_data.capacity());
                        let _ = data.lane(pos).expect("aligned").key();
                    }
                }
            })
        };

        for round in 0..64 {
            let id = model.add_lane(format!("L{round}"), 0, 36, 0).expect("lane added");
            stamp_lane(&model, id, round % 7);
            assert!(model.move_lane(id, 0));
            assert!(model.remove_lane(id));
            model.collect();
        }

        stop.store(true, Relaxed);
        reader.join().expect("the reader never saw misaligned data");
    }

    // ---- undo and redo ----

    /// A model with one lane and a committed starting point, the way the
    /// editor leaves it after its first frame.
    fn committed_model() -> (SeqModel, u32) {
        let model = SeqModel::empty();
        let id = model.add_lane("Kick", 0x10_20_30, 36, 0).expect("lane added");
        model.commit_edits();
        (model, id)
    }

    fn step_on(model: &SeqModel, pattern: usize, lane_id: u32, index: usize) -> bool {
        let data = model.shared();
        let pos = data.position_of(lane_id).expect("lane");
        data.lane_data(pattern, pos).expect("pattern").step(index).is_on()
    }

    #[test]
    fn undo_and_redo_step_through_committed_edits() {
        let (model, id) = committed_model();
        assert!(!model.can_undo());

        // One gesture: three cells painted, then the button comes up.
        for index in 0..3 {
            assert!(model.toggle_step(0, id, index));
        }
        model.commit_edits();
        assert!(model.set_key(id, 40));
        model.commit_edits();
        assert!(model.can_undo() && !model.can_redo());

        assert!(model.undo());
        assert_eq!(model.shared().lane(0).expect("lane").key(), 36, "the key came back");
        assert!(step_on(&model, 0, id, 2), "the steps are a step further back");
        assert!(model.undo());
        assert!((0..3).all(|index| !step_on(&model, 0, id, index)), "the paint came back as one");
        assert!(!model.undo(), "nothing before the starting point");

        assert!(model.redo());
        assert!((0..3).all(|index| step_on(&model, 0, id, index)));
        assert!(model.redo());
        assert_eq!(model.shared().lane(0).expect("lane").key(), 40);
        assert!(!model.redo());
    }

    #[test]
    fn undo_puts_a_deleted_lane_back_with_its_id_and_steps() {
        let (model, id) = committed_model();
        assert!(model.toggle_step(5, id, 7));
        model.commit_edits();
        assert!(model.remove_lane(id));
        model.commit_edits();
        assert_eq!(model.shared().lane_count(), 0);

        assert!(model.undo());
        assert_eq!(model.shared().lane(0).map(|lane| lane.id), Some(id));
        assert!(step_on(&model, 5, id, 7), "its steps in another pattern too");

        // A lane added now must not reuse an id the redo would bring back.
        assert!(model.redo());
        let fresh = model.add_lane("Snare", 0, 38, 0).expect("lane added");
        assert_ne!(fresh, id);
    }

    #[test]
    fn undo_covers_the_song_and_pattern_copies() {
        let (model, id) = committed_model();
        assert!(model.toggle_step(0, id, 0));
        model.commit_edits();
        assert!(model.copy_pattern(0, 3));
        model.commit_edits();
        model.set_song_on(true);
        assert!(model.set_song("1-4").is_ok());
        model.commit_edits();

        assert!(model.undo());
        assert!(!model.shared().song.is_on(), "song switch and text were one step");
        assert!(step_on(&model, 3, id, 0));
        assert!(model.undo());
        assert!(!step_on(&model, 3, id, 0), "the copy is undone");
        assert!(step_on(&model, 0, id, 0), "but not the edit before it");
    }

    #[test]
    fn mute_and_solo_are_not_undo_steps_and_survive_an_undo() {
        let (model, id) = committed_model();
        assert!(model.set_mute(id, true));
        model.commit_edits();
        assert!(!model.can_undo(), "muting is not an edit to undo");

        assert!(model.toggle_step(0, id, 0));
        model.commit_edits();
        assert!(model.set_solo(id, true));
        model.commit_edits();
        assert!(model.undo());
        let data = model.shared();
        let lane = data.lane(0).expect("lane");
        assert!(!step_on(&model, 0, id, 0), "the step edit was undone");
        assert!(lane.is_muted() && lane.is_soloed(), "mute and solo stayed as they are");
    }

    #[test]
    fn an_edit_after_undo_clears_redo() {
        let (model, id) = committed_model();
        assert!(model.toggle_step(0, id, 0));
        model.commit_edits();
        assert!(model.undo());
        assert!(model.can_redo());
        assert!(model.toggle_step(0, id, 1));
        model.commit_edits();
        assert!(!model.can_redo());
        assert!(!model.redo());
    }

    #[test]
    fn a_host_load_clears_the_history() {
        let (model, id) = committed_model();
        assert!(model.toggle_step(0, id, 0));
        model.commit_edits();
        assert!(model.can_undo());

        let saved = model.snapshot();
        model.load_snapshot(saved);
        assert!(!model.can_undo() && !model.can_redo());
        assert!(!model.undo(), "nothing to step back into from before the load");
        assert!(step_on(&model, 0, id, 0));

        // Editing after the load records against the loaded state.
        model.commit_edits();
        assert!(model.toggle_step(0, id, 0));
        model.commit_edits();
        assert!(model.undo());
        assert!(step_on(&model, 0, id, 0));
    }

    #[test]
    fn a_write_of_the_same_value_is_not_an_undo_step() {
        let (model, id) = committed_model();
        assert!(model.set_key(id, 36));
        model.commit_edits();
        assert!(!model.can_undo());
    }

    #[test]
    fn undo_does_not_leak_the_sequences_it_replaces() {
        let (model, id) = committed_model();
        for index in 0..10 {
            assert!(model.toggle_step(0, id, index));
            model.commit_edits();
        }
        while model.undo() {}
        while model.redo() {}
        model.collect();
        assert_eq!(model.live_allocs(), 1);
    }

    // ---- Playhead ----

    #[test]
    fn playhead_starts_stopped_and_round_trips_every_lane() {
        let playhead = Playhead::new();
        assert_eq!(playhead.pattern(), None);
        assert!((0..MAX_LANES).all(|pos| playhead.step(pos).is_none()));

        playhead.set_step(0, 15);
        playhead.set_step(3, 0);
        playhead.set_step(MAX_LANES - 1, (MAX_STEPS - 1) as u32);
        playhead.set_pattern(7);

        assert_eq!(playhead.step(0), Some(15));
        assert_eq!(playhead.step(3), Some(0), "step 0 is not 'stopped'");
        assert_eq!(playhead.step(MAX_LANES - 1), Some((MAX_STEPS - 1) as u32));
        assert_eq!(playhead.step(1), None);
        assert_eq!(playhead.pattern(), Some(7));

        // Past the cap is a no-op, not a panic.
        playhead.set_step(MAX_LANES, 3);
        assert_eq!(playhead.step(MAX_LANES), None);

        playhead.clear();
        assert_eq!(playhead.pattern(), None);
        assert!((0..MAX_LANES).all(|pos| playhead.step(pos).is_none()));
    }
}
