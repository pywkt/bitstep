//! The concurrency scheme under real contention.
//!
//! An integration test rather than a unit test because of the allocator: with
//! the `assert_process_allocs` feature on, nice-plug installs
//! `nice_assert_no_alloc`'s `AllocDisabler` as the *global* allocator for
//! whatever binary links it, but keeps its own `assert_no_alloc` wrapper
//! crate-private — so the forbid guard is called straight from the crate
//! nice-plug shares with us (see `bitstep/Cargo.toml` on why the version is
//! pinned). Inside that guard any allocation, on this thread only, aborts the
//! test binary with a backtrace.
//!
//! The thread shape is the plugin's own:
//!
//! * the **audio thread** does one `shared()` and one `run_block` per block
//!   and touches nothing else, exactly as `process()` does;
//! * the **GUI thread** hammers value edits, shape edits and bulk ops and
//!   calls `collect()`;
//! * the **host thread** saves state and hands snapshots back — a writer in
//!   its own right (`PersistentField::set` runs off the GUI thread), and the
//!   second half of what the writer mutex exists for.
//!
//! Nothing here asserts *what* was played: with lanes appearing and vanishing
//! between blocks the event stream is deliberately unpredictable. What it
//! asserts is that no thread panics, that the audio thread allocates nothing,
//! that the published sequence stays structurally coherent, and that nothing
//! is leaked once the dust settles.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::thread;
use std::time::{Duration, Instant};

use nice_assert_no_alloc::assert_no_alloc;

use bitstep::engine::{BlockCtx, BlockOutput, Engine};
use bitstep::model::{MAX_LANES, NUM_PATTERNS, Playhead, SeqModel, Step};
use bitstep::sequencer::{Prng, Rate};
use bitstep::state::{SeqSnapshot, default_data};

/// How long the three threads run: a bounded ~5 s, so the suite stays well
/// under its two-minute budget.
const RUN_FOR: Duration = Duration::from_secs(5);

const TEMPO: f64 = 128.0;
const SAMPLE_RATE: f32 = 48_000.0;

/// Fixed seeds, one per thread: which edits are made and which blocks are
/// driven is reproducible, even though their interleaving — the thing under
/// test — cannot be.
const AUDIO_SEED: u64 = 0x5713_0001;
const GUI_SEED: u64 = 0x5713_0002;
const HOST_SEED: u64 = 0x5713_0003;

/// The lane count the GUI thread hovers around: enough lanes that a block has
/// real work to do, few enough that `snapshot()` stays cheap.
const LANE_TARGET: usize = 8;

/// A crude host transport, driving every path `run_block` has: stitched
/// blocks of varying size, loop wraps, relocations, stops, pattern changes and
/// the odd `reset()`-style flush.
struct Transport {
    rng: Prng,
    /// The song position the host will report for the next block.
    pos: f64,
    looping: bool,
    pattern: u8,
}

impl Transport {
    /// Buffer sizes a host might hand us, from a small low-latency buffer up
    /// to a large one.
    const BLOCK_SIZES: [usize; 5] = [64, 128, 256, 512, 2048];
    /// Two bars of 4/4, the abuse test's loop.
    const LOOP: (f64, f64) = (0.0, 8.0);

    fn new(seed: u64) -> Transport {
        Transport { rng: Prng::new(seed), pos: 0.0, looping: false, pattern: 1 }
    }

    /// The next block's transport, plus whether a flush was requested just
    /// before it (what `Plugin::reset()` leaves behind for the next block).
    fn next(&mut self) -> (BlockCtx, bool) {
        let draw = self.rng.next_u32();
        let roll = draw % 1000;
        let num_samples = Self::BLOCK_SIZES[(draw >> 10) as usize % Self::BLOCK_SIZES.len()];

        if roll < 15 {
            // The transport stopped: everything sounding is released, and the
            // next playing block reads as a jump.
            return (BlockCtx::STOPPED, roll < 5);
        }
        if roll < 40 {
            self.looping = !self.looping;
        }
        if roll < 60 {
            // A scrub: far enough from `pos` that the stitch tolerance can't
            // absorb it, so the block is a genuine relocation.
            self.pos = f64::from(draw >> 20) * 0.125;
        }
        if roll < 120 {
            self.pattern = (self.pattern % BlockCtx::PATTERN_SELECT_MAX) + 1;
        }

        let block = BlockCtx::new(self.pos, TEMPO, SAMPLE_RATE, num_samples);
        let mut ctx = block.with_pattern_select(self.pattern);
        if self.looping {
            ctx = ctx.with_loop(Self::LOOP.0, Self::LOOP.1);
        }
        self.pos = resume_beat(&ctx);
        (ctx, roll < 65)
    }
}

/// Where a host reports the block *after* `ctx` starting: its own end, or the
/// wrapped position when the block ran into the loop end.
///
/// This mirrors `BlockRanges::resume_beat` on purpose, and through the
/// engine's own `timing()`/`loop_region()` rather than a second copy of the
/// arithmetic — a position even one ULP off the engine's would make every
/// block a jump and quietly stop testing the stitching path.
fn resume_beat(ctx: &BlockCtx) -> f64 {
    let end = ctx.pos_beats + ctx.timing().span_beats();
    match ctx.loop_region() {
        Some(region) if ctx.pos_beats < region.end() && end >= region.end() => {
            (region.start() + (end - region.end())).min(region.end())
        }
        _ => end,
    }
}

/// The audio thread: `shared()` + `run_block` per block, under the no-alloc
/// guard, until `stop` is set. Returns the blocks driven.
fn audio_thread(model: &SeqModel, playhead: &Playhead, stop: &AtomicBool) -> u64 {
    let mut engine = Engine::with_seed(AUDIO_SEED);
    // The one allocation this path is allowed, made before the loop just as
    // `Bitstep::new` makes it before `process()` ever runs.
    let mut out = BlockOutput::new();
    let mut transport = Transport::new(AUDIO_SEED);
    let mut blocks = 0;
    while !stop.load(Relaxed) {
        let (ctx, flush) = transport.next();
        assert_no_alloc(|| {
            if flush {
                engine.request_flush();
            }
            let data = model.shared();
            engine.run_block(&ctx, &data, playhead, &mut out);
            // `data` is dropped in here deliberately: handing a retired
            // `SeqData` to the collector is part of the audio thread's work,
            // and it has to be allocation-free too.
        });
        blocks += 1;
    }
    blocks
}

/// The GUI thread: every `SeqModel` writer method, in a seeded order, against
/// lane ids re-read every iteration — the GUI is always a frame behind a
/// removal, and every writer has to say `false` rather than panic when it is.
fn gui_thread(model: &SeqModel, stop: &AtomicBool) -> u64 {
    let mut rng = Prng::new(GUI_SEED);
    let mut edits = 0;
    while !stop.load(Relaxed) {
        let ids = lane_ids(model);
        let draw = rng.next_u32();
        let bits = (draw >> 10) as usize;
        let pattern = bits % NUM_PATTERNS;
        let index = (bits >> 3) % 96;
        // 0 belongs to no lane (ids start at `FIRST_LANE_ID`), so an empty
        // lane list leaves every edit below a no-op rather than a panic.
        let id = ids.get((bits >> 10) % ids.len().max(1)).copied().unwrap_or(0);
        let crowded = ids.len() >= LANE_TARGET;

        match draw % 1000 {
            0..=249 => {
                let step = Step::pack(
                    bits & 1 == 0,
                    (bits >> 1) as u8 & 0x7F,
                    (bits >> 8) as u8 & 0x7F,
                    (bits >> 15) as u8 & 0x0F,
                    (bits >> 19) as u8 & 0x7F,
                );
                model.set_step(pattern, id, index, step);
            }
            250..=349 => {
                model.toggle_step(pattern, id, index);
            }
            350..=389 => {
                model.set_length(pattern, id, 1 + (draw >> 20) % 64);
            }
            390..=419 => {
                model.set_rate(pattern, id, Rate::ALL[bits % Rate::ALL.len()]);
            }
            420..=439 => {
                model.set_key(id, (bits % 128) as u8);
            }
            440..=454 => {
                model.set_channel(id, (bits % 16) as u8);
            }
            455..=469 => {
                model.set_mute(id, bits & 1 == 0);
            }
            470..=484 => {
                model.set_solo(id, bits & 2 == 0);
            }
            485..=514 => {
                model.clear_lane(pattern, id);
            }
            515..=544 => {
                model.invert_lane(pattern, id);
            }
            545..=574 => {
                model.randomise_lane(pattern, id, 0.4, &mut rng);
            }
            575..=604 => {
                model.nudge_lane(pattern, id, if bits & 1 == 0 { 1 } else { -1 });
            }
            // Shape edits from here down: each one republishes the whole
            // `SeqData` under the audio thread.
            605..=664 => {
                model.ensure_capacity(pattern, id, 16 + (bits >> 4) % 128);
            }
            665..=744 if !crowded => {
                model.add_lane("stress", 0x00_FF_80, (bits % 128) as u8, (bits % 16) as u8);
            }
            665..=744 => {
                model.remove_lane(id);
            }
            745..=794 if !crowded => {
                model.duplicate_lane(id);
            }
            745..=794 => {
                model.remove_lane(id);
            }
            795..=844 => {
                model.move_lane(id, bits % ids.len().max(1));
            }
            845..=879 => {
                model.rename_lane(id, "renamed under load");
            }
            880..=914 => {
                model.set_colour(id, draw & 0x00_FF_FF_FF);
            }
            _ => {
                // What the editor does every frame; shape edits reclaim too,
                // so this is the path for an instance with no editor open.
                model.collect();
            }
        }
        edits += 1;
    }
    edits
}

/// The host thread: project saves, and loads handing the sequence straight
/// back. A `load_snapshot` is a shape edit built entirely outside the
/// published data, so it races the GUI thread's edits for the writer mutex —
/// exactly the case that mutex exists for.
fn host_thread(model: &SeqModel, stop: &AtomicBool) -> u64 {
    let mut rng = Prng::new(HOST_SEED);
    let mut saves = 0;
    while !stop.load(Relaxed) {
        let saved = model.snapshot();
        match rng.next_u32() % 8 {
            0..=2 => model.load_snapshot(saved),
            // A load of something that is *not* what is published: the lane
            // ids the GUI thread is editing go away under it.
            3 => model.load_snapshot(SeqSnapshot::from_data(&default_data())),
            _ => {}
        }
        model.collect();
        saves += 1;
        // Snapshotting is the most expensive thing in this test; without
        // this the host thread can hold the writer mutex most of the time and
        // the GUI thread barely gets an edit in.
        thread::yield_now();
    }
    saves
}

/// The published lane ids, in grid order — what the GUI reads to draw a frame.
fn lane_ids(model: &SeqModel) -> Vec<u32> {
    let data = model.shared();
    (0..data.lane_count()).filter_map(|pos| data.lane(pos).map(|lane| lane.id)).collect()
}

#[test]
fn audio_thread_reads_while_writers_hammer_the_model() {
    let model = Arc::new(SeqModel::new(default_data()));
    let playhead = Arc::new(Playhead::new());
    let stop = Arc::new(AtomicBool::new(false));

    let audio = {
        let (model, playhead, stop) = (model.clone(), playhead.clone(), stop.clone());
        thread::spawn(move || audio_thread(&model, &playhead, &stop))
    };
    let gui = {
        let (model, stop) = (model.clone(), stop.clone());
        thread::spawn(move || gui_thread(&model, &stop))
    };
    let host = {
        let (model, stop) = (model.clone(), stop.clone());
        thread::spawn(move || host_thread(&model, &stop))
    };

    let started = Instant::now();
    while started.elapsed() < RUN_FOR {
        thread::sleep(Duration::from_millis(25));
    }
    stop.store(true, Relaxed);

    // `expect` rather than `unwrap`: a panic in any of the three is the
    // failure this test is looking for, so name which one it was.
    let blocks = audio.join().expect("the audio thread panicked");
    let edits = gui.join().expect("the GUI thread panicked");
    let saves = host.join().expect("the host thread panicked");

    // Floors, not throughput measurements (a test must pass the same way on
    // any machine): they only catch a loop that fell out immediately, which
    // would make every assertion below vacuous. Even a loaded machine manages
    // orders of magnitude more than these in five seconds.
    assert!(blocks > 1_000, "the audio thread drove only {blocks} blocks");
    assert!(edits > 1_000, "the GUI thread made only {edits} edits");
    assert!(saves > 100, "the host thread made only {saves} saves");

    // The sequence survived: still within the caps, and still index-aligned
    // across all 8 patterns, which is the invariant every shape edit and
    // every load has to maintain.
    let data = model.shared();
    assert!(data.lane_count() <= MAX_LANES);
    for pattern in 0..NUM_PATTERNS {
        assert_eq!(
            data.patterns[pattern].len(),
            data.lane_count(),
            "pattern {pattern} lost its alignment with the lane list"
        );
    }
    for pos in 0..data.lane_count() {
        let lane = data.lane(pos).expect("lane in range");
        assert!(lane.key() <= 127 && lane.channel() <= 15, "lane {pos} holds an unclamped value");
        let lane_data = data.lane_data(0, pos).expect("lane data in range");
        assert!(
            (1..=lane_data.capacity() as u32).contains(&lane_data.length()),
            "lane {pos} holds a length outside its capacity"
        );
    }

    // And nothing leaked: once the readers are gone and the last `collect()`
    // has run, the only `SeqData` basedrop still holds is the published one.
    drop(data);
    model.collect();
    assert_eq!(model.live_allocs(), 1, "retired SeqData piled up");
}
