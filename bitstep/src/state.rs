//! The serialisable form of the sequence: the `SeqSnapshot` family and the
//! conversions to and from [`SeqData`].
//!
//! The snapshot stores **plain values, never the packed step bits**, so the
//! `AtomicU32` layout in `model.rs` can change without breaking saved
//! projects. It is also the one place that treats host state as hostile
//! input: `into_data` clamps and repairs everything rather than trusting a
//! project file, because the alternative is a panic inside the host.
//!
//! Like `model`, `sequencer` and `voices`, this module imports no nice-plug
//! types — the `PersistentField` wiring that hands these structs to the
//! framework lives in `lib.rs`.

use serde::{Deserialize, Serialize};

use crate::model::{
    FIRST_LANE_ID, Lane, LaneData, MAX_LANES, NUM_PATTERNS, SeqData, Song, Step,
};
use crate::sequencer::Rate;

/// The format version written by this build.
///
/// A snapshot that carries a *higher* version still loads: serde ignores
/// fields it does not know and the load rules clamp the rest, which salvages
/// more of the user's work than refusing would. A *lower* version would run a
/// migration — there are none yet, v1 being the first format.
pub const CURRENT_VERSION: u32 = 1;

/// The whole sequence in serialisable form: the lane list plus one
/// [`PatternSnapshot`] per pattern bank.
///
/// Every struct in this family is `#[serde(default)]` at the container level,
/// so a field missing from the stored JSON falls back to this type's
/// `Default` instead of failing the whole deserialise. That is what makes a
/// snapshot written by a different build (one field more, one field fewer)
/// loadable at all.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(default)]
pub struct SeqSnapshot {
    pub version: u32,
    pub lanes: Vec<LaneSnapshot>,
    /// Up to [`NUM_PATTERNS`] entries: [`SeqSnapshot::from_data`] leaves out
    /// the trailing patterns nobody has touched, and
    /// [`SeqSnapshot::into_data`] pads or truncates to that many.
    pub patterns: Vec<PatternSnapshot>,
    /// Song mode; off and empty in every save from before it existed.
    pub song: SongSnapshot,
}

/// Song mode as saved: the switches and the order as typed.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(default)]
pub struct SongSnapshot {
    pub on: bool,
    /// Starts again after the last entry. Saves from before the choice
    /// existed stopped there, but load looped: it is what a song should do.
    pub looped: bool,
    pub text: String,
}

impl Default for SongSnapshot {
    fn default() -> SongSnapshot {
        SongSnapshot { on: false, looped: true, text: String::new() }
    }
}

/// One lane's identity and live settings. Its steps live in the patterns.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug, Default)]
#[serde(default)]
pub struct LaneSnapshot {
    /// The stable lane id. Preserved across a save/load so GUI selection (and
    /// later, macro assignments) survive; repaired on load if a file carries
    /// duplicates or the reserved id 0.
    pub id: u32,
    pub name: String,
    pub colour: u32,
    pub key: u8,
    pub channel: u8,
    pub mute: bool,
    pub solo: bool,
    /// The lane's volume in dB; 0 (and so every save from before it
    /// existed) means as played.
    pub volume_db: i8,
    /// Whether the lane is a sample pad; off in every save from before it
    /// existed.
    pub sample_pad: bool,
}

/// One pattern bank: one [`LaneDataSnapshot`] per lane, index-aligned with
/// [`SeqSnapshot::lanes`].
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug, Default)]
#[serde(default)]
pub struct PatternSnapshot {
    pub lanes: Vec<LaneDataSnapshot>,
}

/// One lane's content in one pattern.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug, Default)]
#[serde(default)]
pub struct LaneDataSnapshot {
    pub length: u32,
    /// A [`Rate`] discriminant; an unknown one loads as [`Rate::DEFAULT`].
    pub rate: u8,
    /// The lane's **full capacity**, hidden steps past `length` included — a
    /// user who shortened a lane expects the tail back when they lengthen it
    /// again, including after a project reload.
    pub steps: Vec<StepSnapshot>,
}

/// One step as plain values instead of packed bits.
///
/// The reserved bit of [`Step`] is deliberately *not* stored: it has no
/// meaning in this format version, and a future build that gives it one will
/// add a named field here instead.
///
/// `pitch` arrived after the first saved projects. `#[serde(default)]` loads
/// a step saved without it as pitch 0, so older projects open unchanged.
///
/// Fields at their zero default are left out of the saved form: with 64
/// patterns most steps are untouched, and they store as `{}`. Loading fills a
/// missing field with the same zero, so nothing changes on the way back.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(default)]
pub struct StepSnapshot {
    #[serde(skip_serializing_if = "is_zero")]
    pub on: bool,
    #[serde(skip_serializing_if = "is_zero")]
    pub velocity: u8,
    #[serde(skip_serializing_if = "is_zero")]
    pub pan: u8,
    #[serde(skip_serializing_if = "is_zero")]
    pub ratchet: u8,
    #[serde(skip_serializing_if = "is_zero")]
    pub probability: u8,
    /// Semitones, `-24..=24`. Clamped on load.
    #[serde(skip_serializing_if = "is_zero")]
    pub pitch: i8,
}

/// Whether a field holds its type's default, for `skip_serializing_if`.
fn is_zero<T: Default + PartialEq>(value: &T) -> bool {
    *value == T::default()
}

impl SeqSnapshot {
    /// Reads the live sequence out of the atomics — every lane, every
    /// pattern, every step up to each lane's capacity.
    ///
    /// Allocates freely: this runs on the main thread when the host asks for
    /// state, never in `process()`.
    pub fn from_data(data: &SeqData) -> SeqSnapshot {
        // Patterns past the last one anybody touched load back as exactly
        // what they are, so they are not written.
        let used = data
            .patterns
            .iter()
            .rposition(|pattern| !pattern.iter().all(LaneData::is_pristine))
            .map_or(0, |last| last + 1);
        SeqSnapshot {
            version: CURRENT_VERSION,
            lanes: data.lanes.iter().map(LaneSnapshot::from_lane).collect(),
            patterns: data.patterns[..used]
                .iter()
                .map(|pattern| PatternSnapshot::from_lane_data(pattern))
                .collect(),
            song: SongSnapshot {
                on: data.song.is_on(),
                looped: data.song.is_looped(),
                text: data.song.text.clone(),
            },
        }
    }

    /// Builds a fresh [`SeqData`] from a snapshot, applying every load rule:
    /// caps, per-field clamping, pattern realignment and lane-id repair.
    ///
    /// Never fails and never panics. A file that disagrees with itself opens
    /// with as much of the user's work as can be salvaged, because the only
    /// other options inside a host are losing the project or taking the DAW
    /// down with us.
    pub fn into_data(mut self) -> SeqData {
        // Before anything else, so dropped lanes neither claim a lane id nor
        // leave pattern entries to realign.
        self.lanes.truncate(MAX_LANES);
        repair_lane_ids(&mut self.lanes);

        let lanes: Vec<Lane> = self.lanes.iter().map(LaneSnapshot::to_lane).collect();
        // `SeqData::new` truncates and pads each pattern to `lanes.len()`, so
        // a misaligned file only needs the pattern *count* fixed here.
        self.patterns.truncate(NUM_PATTERNS);
        let mut patterns = self.patterns.into_iter();
        let patterns = std::array::from_fn(|_| match patterns.next() {
            Some(pattern) => pattern.to_lane_data(),
            None => Vec::new(),
        });
        let song = Song::from_text(self.song.text, self.song.on).with_looped(self.song.looped);
        SeqData::new(lanes, patterns).with_song(song)
    }
}

impl Default for SeqSnapshot {
    /// An empty sequence at the current version — notably *not* `version: 0`,
    /// so a stored object with no `version` field is read as current rather
    /// than as a pre-v1 file needing migration.
    fn default() -> SeqSnapshot {
        SeqSnapshot {
            version: CURRENT_VERSION,
            lanes: Vec::new(),
            patterns: Vec::new(),
            song: SongSnapshot::default(),
        }
    }
}

impl LaneSnapshot {
    fn from_lane(lane: &Lane) -> LaneSnapshot {
        LaneSnapshot {
            id: lane.id,
            name: lane.name.clone(),
            colour: lane.colour,
            key: lane.key(),
            channel: lane.channel(),
            mute: lane.is_muted(),
            solo: lane.is_soloed(),
            volume_db: lane.volume_db(),
            sample_pad: lane.is_sample_pad(),
        }
    }

    /// `Lane::new` clamps `key` and `channel`; mute and solo are booleans and
    /// need no repair.
    fn to_lane(&self) -> Lane {
        let lane = Lane::new(self.id, self.name.clone(), self.colour, self.key, self.channel);
        lane.set_mute(self.mute);
        lane.set_solo(self.solo);
        lane.set_volume_db(self.volume_db);
        lane.set_sample_pad(self.sample_pad);
        lane
    }
}

impl PatternSnapshot {
    fn from_lane_data(lane_data: &[LaneData]) -> PatternSnapshot {
        PatternSnapshot {
            lanes: lane_data.iter().map(LaneDataSnapshot::from_lane_data).collect(),
        }
    }

    fn to_lane_data(&self) -> Vec<LaneData> {
        self.lanes.iter().map(LaneDataSnapshot::to_lane_data).collect()
    }
}

impl LaneDataSnapshot {
    fn from_lane_data(lane_data: &LaneData) -> LaneDataSnapshot {
        LaneDataSnapshot {
            length: lane_data.length(),
            rate: lane_data.rate().to_u8(),
            steps: (0..lane_data.capacity())
                .map(|index| StepSnapshot::from_step(lane_data.step(index)))
                .collect(),
        }
    }

    /// The capacity is whatever the file says, clamped to `1..=MAX_STEPS` by
    /// `LaneData::with_capacity` — not rounded to [`crate::model::CAPACITY_CHUNK`],
    /// so a snapshot from a build with a different chunk size still returns
    /// every step the user drew.
    fn to_lane_data(&self) -> LaneData {
        let lane_data = LaneData::with_capacity(self.steps.len());
        for (index, step) in self.steps.iter().take(lane_data.capacity()).enumerate() {
            lane_data.set_step(index, step.to_step());
        }
        lane_data.set_rate(Rate::from_u8(self.rate));
        // After the steps, because `set_length` clamps against the capacity
        // the steps just established.
        lane_data.set_length(self.length);
        lane_data
    }
}

impl StepSnapshot {
    /// Reads a step's five fields.
    ///
    /// A step that was never edited (or was cleared) is all-zero bits, and
    /// has to come back as all-zero bits: [`Step::turned_on`] keys the
    /// "install the defaults" behaviour off exactly that pattern, so storing
    /// the clamped getter values instead (velocity 1, hard-left pan, never
    /// fires) would turn a saved blank step into a silent broken one.
    /// `StepSnapshot::default()` is the stored form of that step — `velocity`
    /// and `ratchet` 0 are outside their live ranges and so cannot mean
    /// anything else.
    ///
    /// Pitch is stored either way: it can be drawn on a step that was never
    /// switched on, and has to survive a reload without making that step look
    /// edited.
    pub fn from_step(step: Step) -> StepSnapshot {
        if step.is_untouched() {
            return StepSnapshot { pitch: step.pitch(), ..StepSnapshot::default() };
        }
        StepSnapshot {
            on: step.is_on(),
            velocity: step.velocity(),
            pan: step.pan(),
            ratchet: step.ratchet(),
            probability: step.probability(),
            pitch: step.pitch(),
        }
    }

    /// The inverse of [`StepSnapshot::from_step`]. `Step::pack` clamps every
    /// field, so a hostile velocity of 200 or ratchet of 0 lands in range
    /// instead of corrupting a neighbouring field.
    pub fn to_step(&self) -> Step {
        if (StepSnapshot { pitch: 0, ..*self }) == StepSnapshot::default() {
            return Step::OFF.with_pitch(self.pitch);
        }
        Step::pack(self.on, self.velocity, self.pan, self.ratchet, self.probability)
            .with_pitch(self.pitch)
    }
}

// ---- fresh-instance defaults ----

/// The lane colours of a fresh instance, in order — `0xRRGGBB`, muted tones
/// that read against a dark Bitwig-like panel.
///
/// The palette lives here because the default state is the first thing that
/// needs it and this module is the one that may not depend on the editor;
/// `editor/theme.rs` turns these into egui colours for its swatches rather
/// than keeping a second list that could drift out of step with saved
/// projects.
pub const LANE_PALETTE: [u32; 10] = [
    0xE2_6E_6E, // red
    0xE2_9A_5E, // orange
    0xE2_C9_5E, // yellow
    0xA8_C9_6E, // lime
    0x6E_C9_8A, // green
    0x6E_C2_C9, // cyan
    0x6E_96_D6, // blue
    0x8E_7E_D6, // indigo
    0xC2_7E_D6, // purple
    0xD6_7E_A8, // pink
];

/// The lanes a fresh instance opens with: name and MIDI key. Eight
/// consecutive keys from C1 (36), because that is how Bitwig's Drum Machine
/// lays out its pads — one pad per note, counting up from C1 — so a fresh
/// instance in front of one plays its first eight pads in order. The names
/// match what `+ lane` would call them.
pub const DEFAULT_LANES: [(&str, u8); 8] = [
    ("Lane 1", 36),
    ("Lane 2", 37),
    ("Lane 3", 38),
    ("Lane 4", 39),
    ("Lane 5", 40),
    ("Lane 6", 41),
    ("Lane 7", 42),
    ("Lane 8", 43),
];

/// The MIDI channel every default lane sends on. Channel 1 in a host's
/// one-based display.
pub const DEFAULT_CHANNEL: u8 = 0;

/// The sequence a fresh plugin instance starts with: [`DEFAULT_LANES`] on
/// [`DEFAULT_CHANNEL`], palette colours in order, and an empty
/// [`LaneData`] — one bar of 16 off steps at 1/16 — in every pattern.
///
/// Ids are handed out from [`FIRST_LANE_ID`] upwards, matching what
/// [`crate::model::SeqModel::add_lane`] would have produced had the user
/// added the eight lanes by hand.
pub fn default_data() -> SeqData {
    let lanes = DEFAULT_LANES
        .iter()
        .enumerate()
        .map(|(index, &(name, key))| {
            let lane = Lane::new(
                FIRST_LANE_ID + index as u32,
                name,
                LANE_PALETTE[index % LANE_PALETTE.len()],
                key,
                DEFAULT_CHANNEL,
            );
            lane.set_sample_pad(Lane::SAMPLE_PAD_DEFAULT);
            lane
        })
        .collect();
    // `from_lanes` gives every lane an empty `LaneData` in every pattern.
    SeqData::from_lanes(lanes)
}

/// Gives every lane a unique, non-zero id, keeping the ids the file already
/// got right.
///
/// Ids are stored so that selection survives a reload, but nothing stops a
/// corrupt or hand-edited file from repeating one — and a duplicate id is
/// worse than a renumbered lane, because `SeqData::position_of` would then
/// route every edit for the second lane to the first. Id 0 is reserved for
/// "no lane selected", so it counts as missing.
fn repair_lane_ids(lanes: &mut [LaneSnapshot]) {
    let mut taken: Vec<u32> = Vec::with_capacity(lanes.len());
    // Pass one reserves every id that is usable as it stands — non-zero and
    // the first lane to claim that value. Reserving them all up front matters:
    // renumbering as we go would let a repaired lane take an id that a *later*
    // lane holds legitimately, renumbering that one too.
    let keeps: Vec<bool> = lanes
        .iter()
        .map(|lane| {
            let keep = lane.id != 0 && !taken.contains(&lane.id);
            if keep {
                taken.push(lane.id);
            }
            keep
        })
        .collect();
    for (lane, keep) in lanes.iter_mut().zip(keeps) {
        if !keep {
            lane.id = free_lane_id(&taken);
            taken.push(lane.id);
        }
    }
}

/// The lowest id ≥ 1 that is not in `taken`. `taken` holds at most
/// [`MAX_LANES`] ids, so the search always finds one inside the range it
/// scans.
fn free_lane_id(taken: &[u32]) -> u32 {
    (1..=MAX_LANES as u32 + 1).find(|id| !taken.contains(id)).unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::model::{CAPACITY_CHUNK, MAX_STEPS};

    /// Three lanes with every identity field, flag, rate, length and step
    /// field set to something distinguishable, and capacities that are *not*
    /// `CAPACITY_CHUNK` multiples so a conversion that quietly rounds shows
    /// up. Lengths sit well below capacity, so most steps are hidden.
    fn sample_data() -> SeqData {
        let lanes = vec![
            Lane::new(7, "Kick", 0x11_22_33, 36, 0),
            Lane::new(9, "Snare", 0x44_55_66, 38, 3),
            Lane::new(12, "Hat", 0xAA_BB_CC, 42, 15),
        ];
        lanes[1].set_mute(true);
        lanes[2].set_solo(true);
        let patterns = std::array::from_fn(|pattern| {
            (0..lanes.len()).map(|lane| sample_lane_data(pattern, lane)).collect()
        });
        SeqData::new(lanes, patterns)
    }

    fn sample_lane_data(pattern: usize, lane: usize) -> LaneData {
        let capacity = 17 + 13 * lane + pattern;
        let lane_data = LaneData::with_capacity(capacity);
        lane_data.set_rate(Rate::ALL[(pattern * 5 + lane) % Rate::ALL.len()]);
        lane_data.set_length((1 + pattern + lane) as u32);
        for index in (0..capacity).filter(|index| (index + pattern + lane).is_multiple_of(3)) {
            lane_data.set_step(index, sample_step(pattern, lane, index));
        }
        lane_data
    }

    /// Every field inside its legal range, so nothing here is clamped on the
    /// way out or back in.
    fn sample_step(pattern: usize, lane: usize, index: usize) -> Step {
        Step::pack(
            (index + lane).is_multiple_of(2),
            ((index * 7 + pattern * 5 + lane * 3) % 127 + 1) as u8,
            ((index * 3 + pattern) % 127) as u8,
            ((index + pattern) % 8 + 1) as u8,
            ((index * 11 + pattern) % 101) as u8,
        )
    }

    /// Every step of every lane in every pattern, as raw bits — the strictest
    /// view of a round trip, hidden steps included.
    fn all_step_bits(data: &SeqData) -> Vec<u32> {
        let mut bits = Vec::new();
        for pattern in 0..NUM_PATTERNS {
            for pos in 0..data.lane_count() {
                let lane_data = data.lane_data(pattern, pos).expect("aligned");
                bits.extend((0..lane_data.capacity()).map(|i| lane_data.step(i).bits()));
            }
        }
        bits
    }

    /// `SeqData` → snapshot → `SeqData` → snapshot is the identity: lane
    /// identity and flags, per-pattern length/rate/capacity, and every step
    /// including the ones past `length`.
    #[test]
    fn a_snapshot_round_trips_the_whole_sequence() {
        let data = sample_data();
        let snapshot = SeqSnapshot::from_data(&data);
        let reloaded = snapshot.clone().into_data();

        assert_eq!(SeqSnapshot::from_data(&reloaded), snapshot);
        assert_eq!(all_step_bits(&reloaded), all_step_bits(&data));
        assert_eq!(snapshot.version, CURRENT_VERSION);
        assert_eq!(snapshot.patterns.len(), NUM_PATTERNS);

        // Spot-check the parts the snapshot comparison would also pass with
        // both sides equally wrong.
        assert_eq!(
            reloaded.lanes.iter().map(|lane| (lane.id, lane.name.as_str())).collect::<Vec<_>>(),
            vec![(7, "Kick"), (9, "Snare"), (12, "Hat")]
        );
        assert_eq!(reloaded.lanes[0].colour, 0x11_22_33);
        assert_eq!((reloaded.lanes[1].key(), reloaded.lanes[1].channel()), (38, 3));
        assert!(reloaded.lanes[1].is_muted() && !reloaded.lanes[1].is_soloed());
        assert!(reloaded.lanes[2].is_soloed() && !reloaded.lanes[2].is_muted());
        for pattern in 0..NUM_PATTERNS {
            for pos in 0..3 {
                let before = data.lane_data(pattern, pos).expect("aligned");
                let after = reloaded.lane_data(pattern, pos).expect("aligned");
                assert_eq!(
                    (after.length(), after.rate(), after.capacity()),
                    (before.length(), before.rate(), before.capacity()),
                    "pattern {pattern} lane {pos}"
                );
            }
        }
    }

    /// Steps beyond `length` are saved and restored — shrinking a lane hides
    /// its tail, a reload must not erase it.
    #[test]
    fn hidden_steps_past_length_survive_a_round_trip() {
        let lane_data = LaneData::with_capacity(64);
        lane_data.set_length(4);
        let hidden = Step::pack(true, 99, 12, 6, 42);
        lane_data.set_step(40, hidden);
        let data = SeqData::new(vec![Lane::new(1, "L", 0, 60, 0)], std::array::from_fn(|p| {
            if p == 0 { vec![lane_data.clone()] } else { Vec::new() }
        }));

        let reloaded = SeqSnapshot::from_data(&data).into_data();

        let reloaded_data = reloaded.lane_data(0, 0).expect("aligned");
        assert_eq!(reloaded_data.length(), 4);
        assert_eq!(reloaded_data.capacity(), 64);
        assert_eq!(reloaded_data.step(40), hidden);
    }

    #[test]
    fn a_snapshot_round_trips_through_json() {
        let snapshot = SeqSnapshot::from_data(&sample_data());

        let json = serde_json::to_string(&snapshot).expect("serialises");
        let parsed: SeqSnapshot = serde_json::from_str(&json).expect("deserialises");

        assert_eq!(parsed, snapshot);
        assert_eq!(all_step_bits(&parsed.into_data()), all_step_bits(&sample_data()));
    }

    /// A never-edited step is all-zero bits and has to come back that way:
    /// `Step::turned_on` installs the defaults only for that exact pattern.
    #[test]
    fn an_untouched_step_comes_back_as_step_off() {
        assert_eq!(StepSnapshot::from_step(Step::OFF), StepSnapshot::default());
        assert_eq!(StepSnapshot::from_step(Step::OFF).to_step(), Step::OFF);
        assert_eq!(StepSnapshot::from_step(Step::default_on().cleared()).to_step(), Step::OFF);
        assert_eq!(
            StepSnapshot::from_step(Step::OFF).to_step().turned_on(),
            Step::default_on(),
            "a saved blank step must still draw with the defaults"
        );

        // An off step the user *did* draw on keeps its values, so switching
        // it back on restores them rather than the defaults.
        let drawn_then_off = Step::pack(true, 42, 10, 3, 55).with_on(false);
        let reloaded = StepSnapshot::from_step(drawn_then_off).to_step();
        assert_eq!(reloaded, drawn_then_off);
        assert_eq!(reloaded.turned_on().velocity(), 42);
    }

    /// The reserved bits have no meaning in version 1, so they are not
    /// stored — the five live fields still come back exactly.
    #[test]
    fn reserved_bits_are_not_persisted() {
        let step = Step(Step::pack(true, 77, 20, 4, 60).bits() | Step::RESERVED_MASK);

        let reloaded = StepSnapshot::from_step(step).to_step();

        assert_eq!(
            (
                reloaded.is_on(),
                reloaded.velocity(),
                reloaded.pan(),
                reloaded.ratchet(),
                reloaded.probability()
            ),
            (true, 77, 20, 4, 60)
        );
        assert_eq!(reloaded.bits() & Step::RESERVED_MASK, 0);
    }

    #[test]
    fn hostile_step_fields_are_clamped() {
        let too_low =
            StepSnapshot { on: true, velocity: 0, pan: 0, ratchet: 0, probability: 0, pitch: 0 };
        let too_high = StepSnapshot {
            on: true,
            velocity: 200,
            pan: 250,
            ratchet: 99,
            probability: 200,
            pitch: 0,
        };

        let low = too_low.to_step();
        let high = too_high.to_step();

        assert_eq!((low.velocity(), low.pan(), low.ratchet(), low.probability()), (1, 0, 1, 0));
        assert_eq!(
            (high.velocity(), high.pan(), high.ratchet(), high.probability()),
            (Step::VELOCITY_MAX, Step::PAN_MAX, Step::RATCHET_MAX, Step::PROBABILITY_MAX)
        );
        assert!(low.is_on() && high.is_on());
    }

    #[test]
    fn key_and_channel_are_clamped_on_load() {
        let snapshot = SeqSnapshot {
            version: CURRENT_VERSION,
            lanes: vec![LaneSnapshot {
                id: 1,
                name: "hostile".into(),
                colour: 0xFF_FF_FF,
                key: 200,
                channel: 99,
                mute: true,
                solo: true,
                volume_db: i8::MAX,
                sample_pad: true,
            }],
            patterns: Vec::new(),
            song: Default::default(),
        };

        let data = snapshot.into_data();

        assert_eq!((data.lanes[0].key(), data.lanes[0].channel()), (127, 15));
        assert_eq!(data.lanes[0].volume_db(), Lane::VOLUME_MAX, "volume is clamped too");
        assert!(data.lanes[0].is_muted() && data.lanes[0].is_soloed());
    }

    /// Builds a snapshot of `lane_count` lanes where pattern `p` holds
    /// `pattern_lanes(p)` lane-data entries, each of `steps` steps.
    fn snapshot_of(
        lane_count: usize,
        patterns: usize,
        pattern_lanes: impl Fn(usize) -> usize,
        steps: usize,
    ) -> SeqSnapshot {
        SeqSnapshot {
            version: CURRENT_VERSION,
            lanes: (0..lane_count)
                .map(|i| LaneSnapshot {
                    id: i as u32 + 1,
                    name: format!("lane {i}"),
                    colour: i as u32,
                    key: 60,
                    channel: 0,
                    mute: false,
                    solo: false,
                    volume_db: 0,
                    sample_pad: false,
                })
                .collect(),
            patterns: (0..patterns)
                .map(|p| PatternSnapshot {
                    lanes: (0..pattern_lanes(p))
                        .map(|_| LaneDataSnapshot {
                            length: 1,
                            rate: Rate::DEFAULT.to_u8(),
                            steps: vec![StepSnapshot::default(); steps],
                        })
                        .collect(),
                })
                .collect(),
            song: SongSnapshot::default(),
        }
    }

    #[test]
    fn an_oversize_snapshot_is_cut_down_to_the_caps() {
        let snapshot = snapshot_of(10_000, 20, |_| 10_000, 1);

        let data = snapshot.into_data();

        assert_eq!(data.lane_count(), MAX_LANES);
        assert_eq!(data.patterns.len(), NUM_PATTERNS);
        for pattern in 0..NUM_PATTERNS {
            assert_eq!(data.patterns[pattern].len(), MAX_LANES, "pattern {pattern}");
        }
        // The ids of the lanes that survived are the first MAX_LANES ones.
        assert_eq!(data.max_lane_id(), Some(MAX_LANES as u32));
    }

    #[test]
    fn a_million_steps_clamp_to_the_step_cap() {
        let mut snapshot = snapshot_of(1, 1, |_| 1, 0);
        snapshot.patterns[0].lanes[0] = LaneDataSnapshot {
            length: 999_999,
            rate: Rate::DEFAULT.to_u8(),
            steps: vec![StepSnapshot::default(); 1_000_000],
        };

        let data = snapshot.into_data();

        let lane_data = data.lane_data(0, 0).expect("aligned");
        assert_eq!(lane_data.capacity(), MAX_STEPS);
        assert_eq!(lane_data.length(), MAX_STEPS as u32);
    }

    /// Patterns that disagree with the lane list — too few, too many, or
    /// missing entirely — are realigned, keeping whatever was in position.
    #[test]
    fn misaligned_patterns_are_padded_and_truncated() {
        let mut snapshot = snapshot_of(3, 2, |p| if p == 0 { 1 } else { 9 }, 32);
        snapshot.patterns[0].lanes[0].length = 7;
        snapshot.patterns[1].lanes[2].rate = Rate::EighthTriplet.to_u8();

        let data = snapshot.into_data();

        assert_eq!(data.lane_count(), 3);
        for pattern in 0..NUM_PATTERNS {
            assert_eq!(data.patterns[pattern].len(), 3, "pattern {pattern}");
        }
        // Surviving entries keep their values…
        assert_eq!(data.lane_data(0, 0).expect("aligned").length(), 7);
        assert_eq!(data.lane_data(0, 0).expect("aligned").capacity(), 32);
        assert_eq!(data.lane_data(1, 2).expect("aligned").rate(), Rate::EighthTriplet);
        // …and the padding is a fresh empty lane.
        let padded = data.lane_data(0, 1).expect("aligned");
        assert_eq!((padded.length(), padded.rate(), padded.capacity()), (
            LaneData::DEFAULT_LENGTH,
            Rate::DEFAULT,
            CAPACITY_CHUNK
        ));
        let missing_pattern = data.lane_data(7, 0).expect("aligned");
        assert_eq!(missing_pattern.capacity(), CAPACITY_CHUNK);
    }

    #[test]
    fn a_bad_rate_or_length_is_repaired() {
        let mut snapshot = snapshot_of(3, NUM_PATTERNS, |_| 3, 24);
        snapshot.patterns[0].lanes[0].rate = 200;
        snapshot.patterns[0].lanes[1].length = 0;
        snapshot.patterns[0].lanes[2].length = 99_999;

        let data = snapshot.into_data();

        assert_eq!(data.lane_data(0, 0).expect("aligned").rate(), Rate::DEFAULT);
        assert_eq!(data.lane_data(0, 1).expect("aligned").length(), 1);
        assert_eq!(data.lane_data(0, 2).expect("aligned").length(), 24);
    }

    /// A lane whose steps are missing entirely still gets one step, because
    /// `length` can never be 0 — the audio thread's `rem_euclid(len)` would
    /// divide by zero.
    #[test]
    fn a_lane_with_no_steps_gets_exactly_one() {
        let snapshot = snapshot_of(1, NUM_PATTERNS, |_| 1, 0);

        let data = snapshot.into_data();

        let lane_data = data.lane_data(0, 0).expect("aligned");
        assert_eq!((lane_data.capacity(), lane_data.length()), (1, 1));
    }

    /// Duplicate or zero ids would route every edit for the later lane to the
    /// earlier one, so they are renumbered; the ids the file got right stay.
    #[test]
    fn duplicate_and_zero_lane_ids_are_repaired() {
        let mut snapshot = snapshot_of(5, NUM_PATTERNS, |_| 5, 16);
        let ids = [0, 5, 5, 0, 3];
        for (lane, id) in snapshot.lanes.iter_mut().zip(ids) {
            lane.id = id;
        }

        let data = snapshot.into_data();

        let loaded: Vec<u32> = data.lanes.iter().map(|lane| lane.id).collect();
        let mut unique = loaded.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), loaded.len(), "ids must be unique: {loaded:?}");
        assert!(!loaded.contains(&0), "0 is reserved for 'no lane selected'");
        assert_eq!((loaded[1], loaded[4]), (5, 3), "valid ids are kept in place");
    }

    #[test]
    fn an_empty_snapshot_loads_as_an_empty_sequence() {
        let data = SeqSnapshot::default().into_data();

        assert_eq!(data.lane_count(), 0);
        assert_eq!(data.patterns.len(), NUM_PATTERNS);
        assert!(data.patterns.iter().all(Vec::is_empty));
        assert_eq!(SeqSnapshot::from_data(&data).version, CURRENT_VERSION);
    }

    /// Stored JSON from another build: unknown fields are ignored, missing
    /// ones take this version's defaults, and a higher `version` still loads.
    #[test]
    fn json_from_another_build_loads_best_effort() {
        let json = r#"{
            "version": 99,
            "future_field": {"anything": [1, 2, 3]},
            "lanes": [{"id": 4, "name": "Kick", "key": 36}],
            "patterns": [{"lanes": [{"length": 2, "steps": [{"on": true}, {}]}]}]
        }"#;

        let snapshot: SeqSnapshot = serde_json::from_str(json).expect("deserialises");
        let data = snapshot.clone().into_data();

        assert_eq!(snapshot.version, 99);
        assert_eq!(snapshot.lanes[0].channel, 0, "a missing field takes its default");
        assert_eq!(data.lane_count(), 1);
        assert_eq!((data.lanes[0].id, data.lanes[0].key()), (4, 36));
        let lane_data = data.lane_data(0, 0).expect("aligned");
        assert_eq!((lane_data.capacity(), lane_data.length()), (2, 2));
        assert!(lane_data.step(0).is_on());
        assert_eq!(lane_data.step(1), Step::OFF);
        assert_eq!(data.lane_data(7, 0).expect("aligned").capacity(), CAPACITY_CHUNK);
    }

    /// A stored object with no `version` is this version, not version 0 — a
    /// missing field must not look like a pre-v1 file needing migration.
    #[test]
    fn json_without_a_version_is_read_as_current() {
        let snapshot: SeqSnapshot = serde_json::from_str("{}").expect("deserialises");

        assert_eq!(snapshot, SeqSnapshot::default());
        assert_eq!(snapshot.version, CURRENT_VERSION);
    }

    #[test]
    fn step_pitch_round_trips_including_on_a_never_edited_step() {
        for step in [
            Step::default_on().with_pitch(12),
            Step::pack(false, 40, 3, 2, 70).with_pitch(-24),
            Step::OFF.with_pitch(5),
            Step::OFF,
        ] {
            let snapshot = StepSnapshot::from_step(step);
            assert_eq!(snapshot.to_step(), step, "{step:?}");
        }
        // A pitched but never-switched-on step still turns on with defaults
        // after a reload.
        let reloaded = StepSnapshot::from_step(Step::OFF.with_pitch(5)).to_step();
        assert_eq!(reloaded.turned_on().velocity(), 100);
    }

    #[test]
    fn a_step_saved_before_pitch_existed_loads_unpitched() {
        let old: StepSnapshot = serde_json::from_str(
            r#"{"on":true,"velocity":90,"pan":63,"ratchet":1,"probability":100}"#,
        )
        .expect("an old step parses");
        assert_eq!(old.to_step(), Step::pack(true, 90, 63, 1, 100));
        assert_eq!(old.to_step().pitch(), 0);
    }

    /// A lane's volume survives a save and reload, and a lane saved before
    /// volume existed loads at 0 dB.
    #[test]
    fn lane_volume_round_trips_and_defaults_to_zero() {
        let data = default_data();
        data.lanes[2].set_volume_db(7);
        let json = serde_json::to_string(&SeqSnapshot::from_data(&data)).expect("saves");
        let back: SeqSnapshot = serde_json::from_str(&json).expect("loads");
        let back = back.into_data();
        assert_eq!(back.lanes[2].volume_db(), 7);
        assert_eq!(back.lanes[0].volume_db(), 0);

        let old: LaneSnapshot = serde_json::from_str(
            r#"{"id":1,"name":"Kick","colour":0,"key":36,"channel":0,"mute":false,"solo":false}"#,
        )
        .expect("an old lane parses");
        assert_eq!(old.volume_db, 0);
        assert!(!old.sample_pad, "an old lane is not a sample pad");
    }

    #[test]
    fn a_sample_pad_lane_round_trips() {
        let data = default_data();
        assert!(data.lanes.iter().all(Lane::is_sample_pad), "a fresh instance's lanes start on");
        data.lanes[1].set_sample_pad(false);
        let json = serde_json::to_string(&SeqSnapshot::from_data(&data)).expect("saves");
        let back: SeqSnapshot = serde_json::from_str(&json).expect("loads");
        let back = back.into_data();
        assert!(!back.lanes[1].is_sample_pad(), "turned off and saved, it loads off");
        assert!(back.lanes[0].is_sample_pad());
    }

    /// Song mode is saved and loaded, and a save from before it existed
    /// loads with it off and empty.
    #[test]
    fn the_song_round_trips() {
        let data = default_data().with_song(Song::from_text("1-2x3-0".into(), true));
        let json = serde_json::to_string(&SeqSnapshot::from_data(&data)).expect("saves");
        let back: SeqSnapshot = serde_json::from_str(&json).expect("loads");
        let back = back.into_data();
        assert!(back.song.is_on());
        assert_eq!(back.song.text, "1-2x3-0");
        assert_eq!(back.song.entries, [0, 1, 1, 1, Song::REST]);
        assert!(back.song.is_looped());

        data.song.set_looped(false);
        let json = serde_json::to_string(&SeqSnapshot::from_data(&data)).expect("saves");
        let back: SeqSnapshot = serde_json::from_str(&json).expect("loads");
        assert!(!back.into_data().song.is_looped(), "stopping at the end is kept");

        let old: SeqSnapshot =
            serde_json::from_str(r#"{"version":1,"lanes":[],"patterns":[]}"#).expect("loads");
        let old = old.into_data();
        assert!(!old.song.is_on() && old.song.is_empty());
        let older = r#"{"version":1,"lanes":[],"patterns":[],"song":{"on":true,"text":"1"}}"#;
        let older: SeqSnapshot = serde_json::from_str(older).expect("loads");
        assert!(older.into_data().song.is_looped(), "a song saved before the choice loops");
    }

    /// Patterns past the last one with anything in them are left out of a
    /// save and come back exactly as they were; untouched steps save as
    /// `{}`.
    #[test]
    fn untouched_patterns_and_steps_save_small() {
        let data = default_data();
        let fresh = SeqSnapshot::from_data(&data);
        assert!(fresh.patterns.is_empty(), "a fresh instance saves no patterns");

        data.patterns[2][0].set_step(5, Step::default_on());
        data.patterns[0][1].set_length(12);
        let snapshot = SeqSnapshot::from_data(&data);
        assert_eq!(snapshot.patterns.len(), 3, "up to the last pattern with an edit");

        let json = serde_json::to_string(&snapshot).expect("saves");
        assert!(json.contains("{},{}"), "untouched steps are empty objects");
        let back: SeqSnapshot = serde_json::from_str(&json).expect("loads");
        assert_eq!(back, snapshot);
        let back = back.into_data();
        assert_eq!(back.patterns.len(), NUM_PATTERNS);
        assert!(back.patterns[2][0].step(5).is_on());
        assert_eq!(back.patterns[0][1].length(), 12);
        assert!(back.patterns[3..].iter().flatten().all(LaneData::is_pristine));
        assert_eq!(SeqSnapshot::from_data(&back), snapshot, "and saves the same again");
    }

    // ---- fresh-instance defaults ----

    /// The state a new instance opens in, field by field: eight lanes on
    /// consecutive keys from C1, matching a Drum Machine's pads, on channel 0
    /// with palette colours, and one bar of sixteenths in every pattern.
    #[test]
    fn the_default_state_is_eight_lanes_on_consecutive_pads() {
        let data = default_data();

        assert_eq!(
            data.lanes
                .iter()
                .map(|lane| (lane.name.clone(), lane.key(), lane.channel(), lane.colour, lane.id))
                .collect::<Vec<_>>(),
            (0..8)
                .map(|i| {
                    (format!("Lane {}", i + 1), 36 + i as u8, 0, LANE_PALETTE[i], 1 + i as u32)
                })
                .collect::<Vec<_>>()
        );
        assert!(data.lanes.iter().all(|lane| !lane.is_muted() && !lane.is_soloed()));

        assert_eq!(data.patterns.len(), NUM_PATTERNS);
        for pattern in 0..NUM_PATTERNS {
            assert_eq!(data.patterns[pattern].len(), DEFAULT_LANES.len(), "pattern {pattern}");
            for pos in 0..DEFAULT_LANES.len() {
                let lane_data = data.lane_data(pattern, pos).expect("aligned");
                assert_eq!(
                    (lane_data.length(), lane_data.rate(), lane_data.capacity()),
                    (LaneData::DEFAULT_LENGTH, Rate::Sixteenth, CAPACITY_CHUNK),
                    "pattern {pattern} lane {pos}"
                );
                assert!(
                    (0..lane_data.capacity()).all(|i| lane_data.step(i) == Step::OFF),
                    "pattern {pattern} lane {pos} must start silent"
                );
            }
        }
    }

    /// The default state is an ordinary snapshot: saving a fresh instance and
    /// loading it back changes nothing.
    #[test]
    fn the_default_state_round_trips_like_any_other() {
        let snapshot = SeqSnapshot::from_data(&default_data());

        let reloaded = snapshot.clone().into_data();

        assert_eq!(SeqSnapshot::from_data(&reloaded), snapshot);
        assert_eq!(all_step_bits(&reloaded), all_step_bits(&default_data()));
    }

    /// Enough distinct colours for the palette to be worth having, none of
    /// them with bits outside `0xRRGGBB` (a stray alpha byte would reach the
    /// GUI as the wrong colour entirely).
    #[test]
    fn the_lane_palette_is_distinct_rgb_values() {
        let mut unique = LANE_PALETTE.to_vec();
        unique.sort_unstable();
        unique.dedup();

        assert_eq!(unique.len(), LANE_PALETTE.len(), "palette colours must differ");
        assert!(LANE_PALETTE.len() >= DEFAULT_LANES.len(), "every default lane needs a colour");
        assert!(LANE_PALETTE.iter().all(|colour| *colour <= 0xFF_FF_FF));
    }
}
