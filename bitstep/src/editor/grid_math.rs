//! Everything about the grid that can be worked out without drawing it.
//!
//! The step strip is one hand-painted widget: there is no egui widget per
//! cell, so every cell rectangle, every culling bound and every pointer
//! lookup is arithmetic this module does. Keeping it here — pure functions
//! over [`CellMetrics`], a viewport and a scroll offset — is what makes the
//! grid testable on a machine with no display, which is the only way it gets
//! tested at all.
//!
//! Four things live here:
//!
//! * [`CellMetrics`] — the pixel sizes, in *content* coordinates (step 0 of
//!   row 0 at the origin, scroll not yet applied).
//! * [`GridView`] — one frame's step strip: metrics plus where the content
//!   sits on screen and how much of it the scroll area shows. Rects, culling,
//!   hit-testing and the follow-mode scroll target.
//! * [`PaintDrag`] — the on/off paint gesture, as a state machine the editor
//!   feeds pointer positions to and gets steps to write back from.
//! * [`EditorLayout`] — the window split into top bar, lane headers and step
//!   strip, for deciding which of the three a pointer is over.
//!
//! Sizes come from [`theme`], never from numbers written here: a restyling
//! retunes the look in one file and this module follows.

use std::ops::{Range, RangeInclusive};

use egui::{Pos2, Rect, Vec2, pos2, vec2};

use super::theme;
use crate::engine::BlockCtx;
use crate::model::{Lane, MAX_STEPS, NUM_PATTERNS, Step};
use crate::sequencer::pan_to_f32;

/// How much clear space follow-mode tries to leave between the playhead's
/// step and the edge it is scrolling towards, in steps.
///
/// Behaviour, not look: it is what keeps the playhead from sitting against
/// the edge of the viewport where the next step to light up is off-screen.
pub const FOLLOW_MARGIN_STEPS: f32 = 2.0;

/// `value`, forced into `min..=max` — including when it is NaN.
///
/// `f32::clamp` would hand NaN straight back (and a NaN pitch turns every
/// rect in the grid into nothing, silently); `max` then `min` cannot, because
/// both discard a NaN operand in favour of the bound.
fn sane(value: f32, min: f32, max: f32) -> f32 {
    value.max(min).min(max)
}

/// `offset / pitch`, rounded down, as an index.
///
/// The `as` cast is deliberate: float→int in Rust saturates, so an infinite
/// offset (egui hands out [`Rect::EVERYTHING`] before a scroll area has been
/// laid out) becomes `usize::MAX` and a NaN one becomes 0, both of which the
/// callers clamp to the lane's length rather than panicking.
fn index_floor(offset: f32, pitch: f32) -> usize {
    (offset / pitch).floor().max(0.0) as usize
}

/// As [`index_floor`], rounded up — the exclusive end of a range.
fn index_ceil(offset: f32, pitch: f32) -> usize {
    (offset / pitch).ceil().max(0.0) as usize
}

/// The pixel geometry of the step grid: cell size, the gaps between cells,
/// and the grab width of a lane's end handle.
///
/// All of it in logical points and in content coordinates — the top-left of
/// step 0 of row 0 is the origin, so nothing here knows about scrolling or
/// where on screen the grid ended up. [`GridView`] adds that.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CellMetrics {
    cell: Vec2,
    gap: Vec2,
    handle_width: f32,
}

impl CellMetrics {
    /// The smallest a cell may be. A zero-width cell would divide by zero in
    /// every lookup below, so the metrics refuse to hold one however they are
    /// constructed.
    pub const MIN_CELL: f32 = 1.0;

    /// The largest any of these may be — a sanity cap in the spirit of
    /// `model.rs`'s, so a bad zoom factor cannot produce rects whose
    /// arithmetic overflows into infinities.
    pub const MAX_SIZE: f32 = 4096.0;

    /// Metrics with every field forced into range: cells at least
    /// [`Self::MIN_CELL`], gaps and handle width non-negative, nothing above
    /// [`Self::MAX_SIZE`], and no NaN anywhere.
    pub fn new(cell: Vec2, gap: Vec2, handle_width: f32) -> CellMetrics {
        CellMetrics {
            cell: vec2(
                sane(cell.x, Self::MIN_CELL, Self::MAX_SIZE),
                sane(cell.y, Self::MIN_CELL, Self::MAX_SIZE),
            ),
            gap: vec2(sane(gap.x, 0.0, Self::MAX_SIZE), sane(gap.y, 0.0, Self::MAX_SIZE)),
            handle_width: sane(handle_width, 0.0, Self::MAX_SIZE),
        }
    }

    /// The metrics the editor actually draws with, from [`theme`].
    pub fn from_theme() -> CellMetrics {
        CellMetrics::new(
            vec2(theme::CELL_WIDTH, theme::CELL_HEIGHT),
            vec2(theme::CELL_GAP, theme::ROW_GAP),
            theme::END_HANDLE_WIDTH,
        )
    }

    /// The drawn size of one cell, gaps excluded.
    pub fn cell_size(&self) -> Vec2 {
        self.cell
    }

    /// The gap between neighbouring cells: `x` between step columns, `y`
    /// between lane rows.
    pub fn gap(&self) -> Vec2 {
        self.gap
    }

    /// The grab width of a lane's end handle.
    pub fn handle_width(&self) -> f32 {
        self.handle_width
    }

    /// Distance from one step column to the next. Always `> 0`.
    pub fn step_pitch(&self) -> f32 {
        self.cell.x + self.gap.x
    }

    /// Distance from one lane row to the next. Always `> 0`.
    pub fn row_pitch(&self) -> f32 {
        self.cell.y + self.gap.y
    }

    /// Content-space `x` of a step column's left edge. `step_x(n)` for the
    /// step *past* the last one is also the left edge of the trailing gap,
    /// which is where the end handle goes.
    pub fn step_x(&self, step: usize) -> f32 {
        step as f32 * self.step_pitch()
    }

    /// Content-space `y` of a lane row's top edge.
    pub fn row_y(&self, row: usize) -> f32 {
        row as f32 * self.row_pitch()
    }

    /// The size of a grid of `steps` columns and `rows` rows, trailing gap
    /// included and with room on the right for the end handle of the longest
    /// lane — that handle sits past the last column, and a scroll area that
    /// did not account for it could not be scrolled far enough to grab it.
    pub fn content_size(&self, steps: usize, rows: usize) -> Vec2 {
        vec2(self.step_x(steps) + self.handle_width, self.row_y(rows))
    }

    /// Which step column a content-space `x` falls in, or `None` left of the
    /// grid.
    ///
    /// Columns are the full [`Self::step_pitch`], so the gap after a cell
    /// belongs to that cell: the grid has no dead pixels a click can fall
    /// into and do nothing.
    pub fn step_at_offset(&self, x: f32) -> Option<usize> {
        (x >= 0.0).then(|| index_floor(x, self.step_pitch()))
    }

    /// Which lane row a content-space `y` falls in, or `None` above the grid.
    /// Rows own the gap below them, as columns own the gap after them.
    pub fn row_at_offset(&self, y: f32) -> Option<usize> {
        (y >= 0.0).then(|| index_floor(y, self.row_pitch()))
    }
}

impl Default for CellMetrics {
    fn default() -> CellMetrics {
        CellMetrics::from_theme()
    }
}

/// What the pointer is over inside the step strip.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GridHit {
    /// A step cell of a lane row. `step` is always inside that lane's length.
    Cell { row: usize, step: usize },
    /// The handle after a lane's last active step, which drags its length.
    EndHandle { row: usize },
}

/// One frame's step strip: the metrics, where the content sits on screen, and
/// which part of it is visible.
///
/// Built from the scroll area's viewport and scroll offset, so every rect it
/// hands back is in screen coordinates and ready to paint or to compare a
/// pointer position against.
#[derive(Clone, Copy, Debug)]
pub struct GridView {
    metrics: CellMetrics,
    /// Screen position of the top-left corner of step 0 of row 0 — i.e. the
    /// content origin, which is above and left of [`Self::viewport`] once
    /// anything is scrolled.
    origin: Pos2,
    /// The visible part of the strip, in screen coordinates.
    viewport: Rect,
}

impl GridView {
    /// The strip showing `viewport`, scrolled by `scroll` (egui's scroll
    /// offset: how far the content has moved up and left).
    pub fn new(metrics: CellMetrics, viewport: Rect, scroll: Vec2) -> GridView {
        GridView { metrics, origin: viewport.min - scroll, viewport }
    }

    pub fn metrics(&self) -> &CellMetrics {
        &self.metrics
    }

    /// Screen position of the content origin — step 0 of row 0.
    pub fn origin(&self) -> Pos2 {
        self.origin
    }

    /// The visible part of the strip, in screen coordinates.
    pub fn viewport(&self) -> Rect {
        self.viewport
    }

    /// The scroll offset this view was built with.
    pub fn scroll(&self) -> Vec2 {
        self.viewport.min - self.origin
    }

    /// The drawn rect of one step's cell, in screen coordinates.
    ///
    /// Defined for any index, visible or not: the caller culls with
    /// [`Self::visible_steps`] and [`Self::visible_rows`] first.
    pub fn cell_rect(&self, row: usize, step: usize) -> Rect {
        let min = self.origin + vec2(self.metrics.step_x(step), self.metrics.row_y(row));
        Rect::from_min_size(min, self.metrics.cell_size())
    }

    /// The cell's rect grown to the full pitch — the cell plus the gaps it
    /// owns. This is the region [`Self::hit`] matches, and the region a
    /// playhead wash or a column highlight should cover so that the
    /// highlighted column reads as continuous.
    pub fn column_rect(&self, row: usize, step: usize) -> Rect {
        Rect::from_min_size(
            self.cell_rect(row, step).min,
            self.metrics.cell_size() + self.metrics.gap(),
        )
    }

    /// The grab rect of a lane's end handle, centred on the boundary after
    /// its last active step.
    ///
    /// A zero-length lane would put the handle half a gap left of the content
    /// origin, where it could not be grabbed, so it is held just inside the
    /// origin instead.
    pub fn end_handle_rect(&self, row: usize, length: u32) -> Rect {
        let m = &self.metrics;
        let boundary = self.origin.x + m.step_x(length as usize) - m.gap().x * 0.5;
        let centre_x = boundary.max(self.origin.x + m.handle_width() * 0.5);
        let top = self.origin.y + m.row_y(row);
        Rect::from_min_size(
            pos2(centre_x - m.handle_width() * 0.5, top),
            vec2(m.handle_width(), m.cell_size().y),
        )
    }

    /// The step columns that touch the viewport, clamped to `total` — the
    /// only ones worth drawing. A lane may be [`crate::model::MAX_STEPS`]
    /// long; a frame that drew all of them would not be a frame.
    ///
    /// Partly visible columns at both ends are included. The trailing end
    /// handle is not a column and is not covered by this range.
    pub fn visible_steps(&self, total: usize) -> Range<usize> {
        self.visible_range(
            self.viewport.min.x - self.origin.x,
            self.viewport.max.x - self.origin.x,
            self.metrics.step_pitch(),
            total,
        )
    }

    /// The lane rows that touch the viewport, clamped to `total`.
    pub fn visible_rows(&self, total: usize) -> Range<usize> {
        self.visible_range(
            self.viewport.min.y - self.origin.y,
            self.viewport.max.y - self.origin.y,
            self.metrics.row_pitch(),
            total,
        )
    }

    /// The indices whose pitch-wide slots overlap `near..far` (content
    /// coordinates), clamped to `0..total`.
    fn visible_range(&self, near: f32, far: f32, pitch: f32, total: usize) -> Range<usize> {
        let first = index_floor(near, pitch).min(total);
        // Never below `first`, so the range is well-formed even when the
        // viewport lies entirely off one end of the content.
        let end = index_ceil(far, pitch).clamp(first, total);
        first..end
    }

    /// Which lane row a screen position is in, or `None` outside the strip's
    /// rows.
    ///
    /// Only the vertical axis is read, so this also answers the question for
    /// the pinned lane headers, which scroll vertically with the strip and
    /// must stay row-for-row aligned with it.
    pub fn row_at(&self, pos: Pos2, rows: usize) -> Option<usize> {
        if pos.y < self.viewport.min.y || pos.y > self.viewport.max.y {
            return None;
        }
        let row = self.metrics.row_at_offset(pos.y - self.origin.y)?;
        (row < rows).then_some(row)
    }

    /// Which step column a screen position is over, or `None` left of the
    /// grid.
    ///
    /// Only the horizontal axis is read, as [`Self::row_at`] reads only the
    /// vertical one. That is what a running [`PaintDrag`] needs: it is locked
    /// to the row it started in (painting is a *horizontal* drag), so a
    /// gesture that wanders a few points into the row above must keep
    /// painting its own row's column under the pointer, not stop dead.
    ///
    /// The index is unbounded — a position far to the right names a step no
    /// lane has. Callers bound it by the row's length, which is the only
    /// thing that knows where the cells stop.
    pub fn step_at(&self, pos: Pos2) -> Option<usize> {
        self.metrics.step_at_offset(pos.x - self.origin.x)
    }

    /// What a screen position is over: a step cell, a lane's end handle, or
    /// nothing.
    ///
    /// `length_of` gives a row's current length, which decides both where its
    /// handle sits and how many of its cells exist — the strip draws a lane's
    /// steps up to its length and the handle after them, so a click past the
    /// handle is a miss rather than a cell.
    ///
    /// The handle wins over the cell it overlaps: it is the narrower target
    /// and it is drawn on top.
    pub fn hit(
        &self,
        pos: Pos2,
        rows: usize,
        length_of: impl Fn(usize) -> u32,
    ) -> Option<GridHit> {
        // A position outside the viewport is over the headers, the top bar or
        // another window — not over whatever cell happens to be scrolled
        // under it.
        if !self.viewport.contains(pos) {
            return None;
        }
        let row = self.row_at(pos, rows)?;
        let length = length_of(row);
        if self.end_handle_rect(row, length).contains(pos) {
            return Some(GridHit::EndHandle { row });
        }
        let step = self.step_at(pos)?;
        ((step as u64) < u64::from(length)).then_some(GridHit::Cell { row, step })
    }

    /// The largest horizontal scroll offset that still shows content, for a
    /// strip `total_steps` wide.
    pub fn max_scroll_x(&self, total_steps: usize) -> f32 {
        let content = self.metrics.content_size(total_steps, 0).x;
        sane(content - self.viewport.width(), 0.0, f32::MAX)
    }

    /// The largest vertical scroll offset that still shows a lane, for a
    /// strip of `total_rows` lanes.
    pub fn max_scroll_y(&self, total_rows: usize) -> f32 {
        let content = self.metrics.content_size(0, total_rows).y;
        sane(content - self.viewport.height(), 0.0, f32::MAX)
    }

    /// Both bounds at once: the far corner of the legal scroll offsets for a
    /// strip this wide and this tall.
    ///
    /// Zero on an axis whose content fits — there is nothing to scroll, and
    /// the grid parks at the origin rather than drifting into empty space
    /// below the last lane or right of the last step.
    pub fn max_scroll(&self, total_steps: usize, total_rows: usize) -> Vec2 {
        vec2(self.max_scroll_x(total_steps), self.max_scroll_y(total_rows))
    }

    /// `scroll`, forced into the offsets this strip actually has.
    ///
    /// The editor keeps its own offset across frames and the content moves
    /// underneath it — a lane deleted, a loop shortened, the window resized —
    /// so the offset is re-clamped every frame rather than only where it is
    /// changed. NaN and infinity (a wheel delta can be either before egui has
    /// laid a frame out) come back as `0.0` and the bound, never as an origin
    /// that puts every cell off screen.
    pub fn clamp_scroll(&self, scroll: Vec2, total_steps: usize, total_rows: usize) -> Vec2 {
        let max = self.max_scroll(total_steps, total_rows);
        vec2(sane(scroll.x, 0.0, max.x), sane(scroll.y, 0.0, max.y))
    }

    /// Where follow mode should scroll to so that `step` is visible — follow
    /// keeps the playhead in view.
    ///
    /// Scrolls as little as possible: a step already comfortably in view
    /// leaves the offset alone, so a playing pattern that fits on screen
    /// never moves. Otherwise the step is brought just inside the edge it
    /// went past, with [`FOLLOW_MARGIN_STEPS`] of lead-in where the viewport
    /// is wide enough to spare it. The result is always a legal offset for
    /// this strip.
    pub fn follow_scroll_x(&self, step: usize, total_steps: usize) -> f32 {
        let m = &self.metrics;
        let max = self.max_scroll_x(total_steps);
        let scroll = sane(self.scroll().x, 0.0, max);
        let view_width = self.viewport.width();

        let left = m.step_x(step);
        let right = left + m.cell_size().x;
        // The margin can only be what is left over once the cell itself has
        // its room, or a viewport barely one cell wide would chase its own
        // tail: scroll to show the margin, which pushes the cell out, which
        // asks to scroll back.
        let margin =
            sane((view_width - m.cell_size().x) * 0.5, 0.0, FOLLOW_MARGIN_STEPS * m.step_pitch());

        let target = if left - margin < scroll {
            left - margin
        } else if right + margin > scroll + view_width {
            right + margin - view_width
        } else {
            scroll
        };
        sane(target, 0.0, max)
    }
}

// ---- the drag state machines ----

/// A paint gesture in progress: the one lane row it owns, the on/off value it
/// latched when it started, and the span of steps it has already written.
///
/// Two behaviours come from one gesture — a click toggles, and a horizontal
/// drag paints the on/off value the press entered — and they are the same
/// machine: the press toggles the cell it landed on, and whatever
/// that cell *became* is what every further cell is set to. Latching matters;
/// toggling each cell as it is entered would undo the run as soon as the
/// pointer crossed a step that was already on, and a drag back over the trail
/// would erase it.
///
/// The machine holds a span rather than a set because the gesture can only
/// grow one: a pointer that jumps several columns in a frame (a fast drag at
/// 30 Hz easily does) fills in everything it flew over, so the run is
/// continuous rather than dotted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PaintDrag {
    row: usize,
    /// What the steps this drag touches are set to — the value the first cell
    /// toggled to.
    value: bool,
    /// The painted span, inclusive at both ends. Starts as the press's cell.
    lo: usize,
    hi: usize,
}

impl PaintDrag {
    /// Starts a drag on the cell under the press, which was `was_on`.
    ///
    /// The step is *not* written here — this only decides what writing it
    /// means. [`Self::painted`] is the span the caller then writes, which on
    /// this first call is that one cell: a press released without moving is a
    /// complete toggle.
    pub fn begin(row: usize, step: usize, was_on: bool) -> PaintDrag {
        PaintDrag { row, value: !was_on, lo: step, hi: step }
    }

    /// The lane row the gesture is locked to.
    pub fn row(&self) -> usize {
        self.row
    }

    /// The value every cell of the gesture is set to.
    pub fn value(&self) -> bool {
        self.value
    }

    /// The steps painted so far, inclusive at both ends.
    pub fn painted(&self) -> RangeInclusive<usize> {
        self.lo..=self.hi
    }

    /// The pointer has moved to `step` of the same row: the steps that still
    /// need [`Self::value`] written to them, in content order.
    ///
    /// Empty for a step already inside the painted span — a drag that wanders
    /// back over its own trail, or jitters inside one cell, writes nothing a
    /// second time. (The writes are idempotent, so this is about not taking
    /// the writer lock 30 times a second for nothing.)
    ///
    /// `step` must be one the row actually has: the returned range spans
    /// every index up to it, so feeding in an unbounded [`GridView::step_at`]
    /// would ask the caller to write millions of steps that do not exist.
    pub fn enter(&mut self, step: usize) -> Range<usize> {
        if step < self.lo {
            let fresh = step..self.lo;
            self.lo = step;
            fresh
        } else if step > self.hi {
            // `saturating_add` only to keep a pathological index (`step_at`
            // saturates on an infinite pointer position) from overflowing;
            // real indices are below `crate::model::MAX_STEPS`.
            let fresh = self.hi.saturating_add(1)..step.saturating_add(1);
            self.hi = step;
            fresh
        } else {
            step..step
        }
    }

    /// What one step becomes once this drag writes it.
    ///
    /// Only the on bit moves: a step keeps the velocity, pan, ratchet and
    /// probability the user gave it, so switching a run off and on again does
    /// not flatten it ([`Step::turned_on`] fills in the defaults for a step
    /// that never had any).
    pub fn apply(&self, step: Step) -> Step {
        if self.value { step.turned_on() } else { step.with_on(false) }
    }
}

/// How far a press has to travel before it is a drag rather than a click, in
/// points.
///
/// Behaviour, not look (like [`FOLLOW_MARGIN_STEPS`]): a press on an on step
/// can become either of the two gestures an on step carries — a horizontal
/// paint or a vertical velocity edit — or neither, and the
/// deadzone is what stops a hand that wobbles by a point from choosing one on
/// the user's behalf.
pub const DRAG_DEADZONE: f32 = 3.0;

/// The vertical travel that covers the whole velocity range, in points.
///
/// Behaviour as well: it sets how fine a velocity edit is (about 1.3 points
/// per velocity unit), not how anything looks. Deliberately several times a
/// cell's height — the fill the drag moves is only [`theme::CELL_HEIGHT`]
/// tall, and mapping 126 velocities onto that would make every point worth
/// five of them.
pub const VELOCITY_DRAG_SPAN: f32 = 160.0;

/// What a press that could be either gesture has turned into.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DragAxis {
    /// Still inside [`DRAG_DEADZONE`]: it may yet become either, or stay a
    /// click.
    Undecided,
    /// Mostly sideways: a paint drag.
    Horizontal,
    /// Mostly up or down: a velocity drag.
    Vertical,
}

/// Which gesture a pointer now `delta` from where it went down is making.
///
/// The larger component decides, so the gesture follows the direction the
/// hand committed to rather than the order the two axes happened to cross the
/// deadzone in. A tie goes to [`DragAxis::Horizontal`]: painting is the
/// gesture a user reaches for far more often, and it is the one that can be
/// undone by dragging back.
///
/// A non-finite delta — the window system can hand egui one — leaves the
/// gesture [`DragAxis::Undecided`] rather than resolving it at random.
pub fn resolve_axis(delta: Vec2) -> DragAxis {
    let (dx, dy) = (delta.x.abs(), delta.y.abs());
    // `sane` rather than a bare comparison: a NaN travel must read as "has
    // not moved", and `NaN < DRAG_DEADZONE` is false.
    if sane(dx.max(dy), 0.0, f32::INFINITY) < DRAG_DEADZONE {
        DragAxis::Undecided
    } else if dy > dx {
        DragAxis::Vertical
    } else {
        DragAxis::Horizontal
    }
}

/// The velocity a drag of `dy` points from the press lands on, from the
/// velocity the step had when the press happened.
///
/// Up is louder: egui's y grows downwards, so a negative `dy` raises the
/// velocity, which is the direction the fill height grows in
/// (velocity is drawn as fill height).
///
/// Absolute, not incremental — the whole gesture is measured from its origin,
/// so dragging back to where the press was restores the velocity it started
/// with even after the drag has run into an end of the range. Clamped to
/// `Step::VELOCITY_MIN..=Step::VELOCITY_MAX`: a velocity of 0 is a note-off on
/// the wire, so the drag's floor is 1, not silence.
pub fn drag_velocity(start: u8, dy: f32) -> u8 {
    let start = start.clamp(Step::VELOCITY_MIN, Step::VELOCITY_MAX);
    if !dy.is_finite() {
        // A pointer position nothing can be measured from: the step keeps the
        // velocity the press found rather than snapping to an end of the
        // range.
        return start;
    }
    let range = f32::from(Step::VELOCITY_MAX - Step::VELOCITY_MIN);
    let target = f32::from(start) - (dy / VELOCITY_DRAG_SPAN) * range;
    sane(
        target.round(),
        f32::from(Step::VELOCITY_MIN),
        f32::from(Step::VELOCITY_MAX),
    ) as u8
}

/// A velocity gesture in progress: the one step it edits, and the pointer
/// height and velocity the press fixed as its zero point.
///
/// One step, never a span: the horizontal axis belongs to painting, so a
/// velocity drag ignores where the pointer wanders to sideways — including
/// off the lane — and keeps editing the step it started on until the button
/// comes up.
///
/// Both halves of the zero point are latched at the press. The velocity
/// especially: reading it back from the model each frame would feed the
/// gesture's own writes into its next result, and the step would run away
/// from the pointer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VelocityDrag {
    row: usize,
    step: usize,
    start: u8,
    origin_y: f32,
    /// The velocity the step is believed to hold — the start, and then
    /// whatever [`Self::enter`] last handed out.
    written: u8,
}

impl VelocityDrag {
    /// Starts a drag on `step` of `row`, which had `velocity` when the button
    /// went down at `origin_y`.
    pub fn begin(row: usize, step: usize, velocity: u8, origin_y: f32) -> VelocityDrag {
        let start = velocity.clamp(Step::VELOCITY_MIN, Step::VELOCITY_MAX);
        VelocityDrag { row, step, start, origin_y, written: start }
    }

    /// The lane row the gesture is locked to.
    pub fn row(&self) -> usize {
        self.row
    }

    /// The step index the gesture edits.
    pub fn step(&self) -> usize {
        self.step
    }

    /// The velocity the step had at the press — where `dy` is measured from.
    pub fn start_velocity(&self) -> u8 {
        self.start
    }

    /// The velocity the pointer at screen height `y` means.
    pub fn velocity_at(&self, y: f32) -> u8 {
        drag_velocity(self.start, y - self.origin_y)
    }

    /// The pointer has moved to screen height `y`: the velocity that still
    /// needs writing, or `None` if the step already holds it.
    ///
    /// Nothing for a frame where the pointer moved too little to change the
    /// velocity — which, held still, is every frame of the gesture after the
    /// first. As with [`PaintDrag::enter`] the write would be idempotent; this
    /// is about not taking the writer mutex 30 times a second for nothing.
    pub fn enter(&mut self, y: f32) -> Option<u8> {
        let velocity = self.velocity_at(y);
        if velocity == self.written {
            return None;
        }
        self.written = velocity;
        Some(velocity)
    }

    /// What one step becomes at the `velocity` [`Self::enter`] handed out.
    ///
    /// Only the velocity moves: the step keeps its on bit, pan, ratchet and
    /// probability, so a velocity edit cannot switch a step off (and a step
    /// switched off under the gesture by a state load still comes back with
    /// the velocity the user drew).
    pub fn apply(&self, step: Step, velocity: u8) -> Step {
        step.with_velocity(velocity)
    }
}

/// The loop length a drag of `dx` points from the press means, from the
/// length the lane had then.
///
/// One step per column of travel, so the end handle keeps up with the hand
/// that is pulling it: the handle sits on the boundary after the last step
/// (`GridView::end_handle_rect`), and moving the pointer a whole column puts
/// that boundary on the next one. Rounded rather than truncated so the step
/// changes as the pointer passes the middle of a column, which is where the
/// handle is nearest the cell it is about to add.
///
/// Absolute, like [`drag_velocity`]: measured from the press's own origin, so
/// dragging back shortens the lane again to exactly what it was, even after
/// the gesture has run into an end of the range. Clamped to
/// `LengthEdit::MIN..=LengthEdit::MAX`, and a non-finite `dx` or `pitch`
/// (egui can hand out either) leaves the length alone rather than saturating
/// it to one end.
pub fn drag_length(start: u32, dx: f32, pitch: f32) -> u32 {
    let start = start.clamp(LengthEdit::MIN, LengthEdit::MAX);
    if !dx.is_finite() || !pitch.is_finite() || pitch <= 0.0 {
        return start;
    }

    // `f64` for the sum: a pathological `dx` is worth more columns than a
    // `u32` holds, and the clamp has to see which end it went past.
    let wanted = f64::from(start) + f64::from((dx / pitch).round());
    wanted.max(f64::from(LengthEdit::MIN)).min(f64::from(LengthEdit::MAX)) as u32
}

/// An end-handle gesture in progress: the one lane row it owns, and the
/// pointer position and length the press fixed as its zero point.
///
/// The handle after a lane's last step drags to change `length` directly,
/// which is the same edit the header's length field
/// makes — so the machine hands out a wanted length and the editor resolves
/// it through [`LengthEdit`], growing the lane's capacity first where the
/// drag has gone past it.
///
/// Measured in screen `x`, not content `x`: the length follows the hand. A
/// strip that scrolls mid-gesture (follow mode, with the transport running)
/// slides the handle out from under the pointer, and the alternative — the
/// length chasing the content position of a pointer that is holding still —
/// would move the handle nobody was dragging.
///
/// The length is latched at the press for the same reason [`VelocityDrag`]
/// latches its velocity: reading it back from the model each frame would feed
/// the gesture's own writes into its next result, and the lane would run away
/// from the pointer.
#[derive(Clone, Copy, Debug)]
pub struct EndHandleDrag {
    row: usize,
    start: u32,
    origin_x: f32,
    /// The step pitch the press happened at — see [`drag_length`]. Latched so
    /// that the machine answers [`Self::length_at`] on its own, as
    /// [`VelocityDrag`] does.
    pitch: f32,
    /// The length the lane is believed to hold — the start, and then whatever
    /// [`Self::enter`] last handed out.
    written: u32,
}

impl EndHandleDrag {
    /// Starts a drag on `row`'s handle, which was at `length` when the button
    /// went down at `origin_x`.
    pub fn begin(row: usize, length: u32, origin_x: f32, pitch: f32) -> EndHandleDrag {
        let start = length.clamp(LengthEdit::MIN, LengthEdit::MAX);
        EndHandleDrag { row, start, origin_x, pitch, written: start }
    }

    /// The lane row the gesture is locked to.
    pub fn row(&self) -> usize {
        self.row
    }

    /// The length the lane had at the press — where `dx` is measured from.
    pub fn start_length(&self) -> u32 {
        self.start
    }

    /// The length a pointer at screen `x` means.
    pub fn length_at(&self, x: f32) -> u32 {
        drag_length(self.start, x - self.origin_x, self.pitch)
    }

    /// The pointer has moved to screen `x`: the length that still needs
    /// writing, or `None` if the lane already holds it.
    ///
    /// Quiet in the same way as [`PaintDrag::enter`] and
    /// [`VelocityDrag::enter`]: a frame that did not move the length writes
    /// nothing. It matters more here than there, because a length past the
    /// lane's capacity is a *shape* edit — republishing the whole sequence 30
    /// times a second for a pointer holding still would be a waste of the
    /// writer mutex and of the collector.
    pub fn enter(&mut self, x: f32) -> Option<u32> {
        let length = self.length_at(x);
        if length == self.written {
            return None;
        }
        self.written = length;
        Some(length)
    }
}

/// The lane row a reorder drag of `dy` points from `from`'s row means, out of
/// `rows` lanes.
///
/// Absolute, like [`drag_length`] and [`drag_velocity`]: measured from the
/// press's own origin, so dragging back puts the lane where it started even
/// after the gesture has run into an end of the list. Rounded rather than
/// floored, so a lane changes places once the hand is past the halfway point
/// of the next row rather than the moment it leaves its own.
///
/// A non-finite `dy` or `pitch` leaves the lane where it is, and an empty
/// list answers 0 — the only position there is — rather than underflowing.
pub fn drag_rows(from: usize, dy: f32, pitch: f32, rows: usize) -> usize {
    let last = rows.saturating_sub(1);
    let from = from.min(last);
    if !dy.is_finite() || !pitch.is_finite() || pitch <= 0.0 {
        return from;
    }

    // `f64` for the sum, as in `drag_length`: a pathological `dy` is worth
    // more rows than a `usize` holds, and the clamp has to see which end it
    // went past.
    let wanted = from as f64 + f64::from((dy / pitch).round());
    wanted.max(0.0).min(last as f64) as usize
}

/// A lane-reorder gesture in progress: the row the press picked up, and the
/// pointer position it is measured from — the header's drag handle is how
/// lanes are reordered.
///
/// The machine hands out *positions*, which the editor turns into
/// `SeqModel::move_lane` calls; it never looks at the lane list itself, so the
/// row the lane has reached in between is none of its business — which is
/// exactly what makes it safe to move the lane under the hand as the drag
/// goes. The origin row is latched for [`VelocityDrag`]'s reason: reading the
/// lane's current position back each frame would feed the gesture's own moves
/// into its next result, and the row would run away down the list.
///
/// Measured in screen `y`, not content `y`, like [`EndHandleDrag`]: the lane
/// follows the hand, and a header column that scrolls mid-gesture must not
/// reorder anything on its own.
#[derive(Clone, Copy, Debug)]
pub struct LaneReorder {
    from: usize,
    origin_y: f32,
    /// The row pitch the press happened at, latched so the machine answers
    /// [`Self::target_at`] on its own.
    pitch: f32,
    /// The position the lane is believed to hold — the start, and then
    /// whatever [`Self::enter`] last handed out.
    written: usize,
}

impl LaneReorder {
    /// Starts a drag of the lane at `row`, picked up at `origin_y`.
    pub fn begin(row: usize, origin_y: f32, pitch: f32) -> LaneReorder {
        LaneReorder { from: row, origin_y, pitch, written: row }
    }

    /// The row the gesture picked the lane up from — where a cancelled drag
    /// puts the lane back.
    pub fn from(&self) -> usize {
        self.from
    }

    /// The position the lane is believed to hold now: the row it was picked
    /// up from until [`Self::enter`] has moved it.
    ///
    /// What a cancel compares against, so that a drag which never left its
    /// own row is undone without a `move_lane` that would republish the
    /// sequence to put a lane back where it already is.
    pub fn target(&self) -> usize {
        self.written
    }

    /// The position a pointer at screen `y` means, in a list of `rows` lanes.
    pub fn target_at(&self, y: f32, rows: usize) -> usize {
        drag_rows(self.from, y - self.origin_y, self.pitch, rows)
    }

    /// The pointer has moved to screen `y`: the position the lane still needs
    /// moving to, or `None` if it is already there.
    ///
    /// Quiet in the same way as [`EndHandleDrag::enter`], and for the sharper
    /// version of its reason: a move is a *shape* edit, so a frame that did
    /// not change the target must not republish the whole sequence.
    pub fn enter(&mut self, y: f32, rows: usize) -> Option<usize> {
        let target = self.target_at(y, rows);
        if target == self.written {
            return None;
        }
        self.written = target;
        Some(target)
    }
}

/// Which part of the editor window a position is in.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Zone {
    /// Pattern buttons, `+ lane`, follow toggle.
    TopBar,
    /// The pinned lane headers down the left.
    LaneHeader,
    /// The scrolling step strip.
    Steps,
    /// The parameter panel under the grid, header and bars alike.
    Panel,
    /// Not in the window at all.
    Outside,
}

/// The window split into its three regions: a top
/// bar across the whole width, then lane headers pinned left of the step
/// strip.
///
/// The split is what pointer handling needs (the drag that started in a
/// header is a lane drag, not a paint drag) and what the panels are sized
/// from; the regions are mutually exclusive and cover the window exactly.
#[derive(Clone, Copy, Debug)]
pub struct EditorLayout {
    full: Rect,
    top_bar: Rect,
    headers: Rect,
    steps: Rect,
    /// The parameter panel's left end, under the lane headers: what it edits
    /// and the parameter dropdown. Zero-height until [`Self::with_panel`].
    panel_header: Rect,
    /// The parameter panel's bars, under the step strip and scrolled with it.
    panel_strip: Rect,
}

impl EditorLayout {
    /// Splits `full`. A bar or header asking for more than there is gets what
    /// is left — the region past it is empty rather than negative, which is
    /// what keeps the zones exclusive in a window squeezed below
    /// [`super::MIN_SIZE`] by a host that ignored the hint.
    pub fn new(full: Rect, top_bar_height: f32, header_width: f32) -> EditorLayout {
        let width = sane(full.width(), 0.0, f32::MAX);
        let height = sane(full.height(), 0.0, f32::MAX);
        let bar = sane(top_bar_height, 0.0, height);
        let header = sane(header_width, 0.0, width);

        let left = full.min.x;
        let top = full.min.y;
        let split_x = left + header;
        let split_y = top + bar;
        let right = left + width;
        let bottom = top + height;

        EditorLayout {
            full: Rect::from_min_max(full.min, pos2(right, bottom)),
            top_bar: Rect::from_min_max(full.min, pos2(right, split_y)),
            headers: Rect::from_min_max(pos2(left, split_y), pos2(split_x, bottom)),
            steps: Rect::from_min_max(pos2(split_x, split_y), pos2(right, bottom)),
            panel_header: Rect::from_min_max(pos2(left, bottom), pos2(split_x, bottom)),
            panel_strip: Rect::from_min_max(pos2(split_x, bottom), pos2(right, bottom)),
        }
    }

    /// The same split with the parameter panel taking `height` points off the
    /// bottom of the lane headers and the step strip.
    ///
    /// Never more than [`PANEL_MAX_SHARE`] of what is below the top bar, so a
    /// short window keeps some grid to select a lane in.
    pub fn with_panel(mut self, height: f32) -> EditorLayout {
        let below_bar = (self.full.max.y - self.top_bar.max.y).max(0.0);
        let height = sane(height, 0.0, below_bar * PANEL_MAX_SHARE);
        let top = self.full.max.y - height;
        let bottom = self.full.max.y;
        self.headers.max.y = top;
        self.steps.max.y = top;
        self.panel_header =
            Rect::from_min_max(pos2(self.headers.min.x, top), pos2(self.headers.max.x, bottom));
        self.panel_strip =
            Rect::from_min_max(pos2(self.steps.min.x, top), pos2(self.steps.max.x, bottom));
        self
    }

    /// The split the editor actually draws, from [`theme`].
    pub fn from_theme(full: Rect) -> EditorLayout {
        EditorLayout::new(full, theme::TOP_BAR_HEIGHT, theme::HEADER_WIDTH)
    }

    /// The whole window, with any negative size squared up.
    pub fn full(&self) -> Rect {
        self.full
    }

    pub fn top_bar(&self) -> Rect {
        self.top_bar
    }

    /// The lane headers' column — the viewport they scroll vertically in.
    pub fn headers(&self) -> Rect {
        self.headers
    }

    /// The step strip's viewport.
    pub fn steps(&self) -> Rect {
        self.steps
    }

    /// The parameter panel's left end (empty when the panel is hidden).
    pub fn panel_header(&self) -> Rect {
        self.panel_header
    }

    /// The parameter panel's bars (empty when the panel is hidden).
    pub fn panel_strip(&self) -> Rect {
        self.panel_strip
    }

    /// The whole parameter panel, both ends.
    pub fn panel(&self) -> Rect {
        self.panel_header.union(self.panel_strip)
    }

    /// The parameter panel's bars as a [`GridView`]: the step strip's
    /// columns, scrolled with it horizontally and never vertically, so each
    /// bar sits exactly under its step.
    pub fn panel_view(&self, metrics: CellMetrics, scroll_x: f32) -> GridView {
        GridView::new(metrics, self.panel_strip, vec2(scroll_x, 0.0))
    }

    /// Which region a position is in.
    ///
    /// Boundaries belong to the region above or left of them, so every
    /// position in the window answers exactly one [`Zone`].
    pub fn zone(&self, pos: Pos2) -> Zone {
        if !self.full.contains(pos) {
            Zone::Outside
        } else if pos.y < self.top_bar.max.y {
            Zone::TopBar
        } else if self.panel_strip.height() > 0.0 && pos.y >= self.panel_strip.min.y {
            Zone::Panel
        } else if pos.x < self.headers.max.x {
            Zone::LaneHeader
        } else {
            Zone::Steps
        }
    }

    /// The lane header band for one row: the full width of the header
    /// column, at the height of that row's cells.
    ///
    /// The vertical geometry comes from `view` rather than from a second set
    /// of sums over [`CellMetrics`]: the headers scroll vertically with the
    /// strip, and a row whose header and cells
    /// disagreed by a pixel would read as a bug. Defined for any row, as
    /// [`GridView::cell_rect`] is — the caller culls with
    /// [`GridView::visible_rows`] first.
    pub fn header_row(&self, view: &GridView, row: usize) -> Rect {
        let cell = view.cell_rect(row, 0);
        Rect::from_min_max(
            pos2(self.headers.min.x, cell.min.y),
            pos2(self.headers.max.x, cell.max.y),
        )
    }

    /// The step strip as a [`GridView`], for a given scroll offset.
    pub fn grid_view(&self, metrics: CellMetrics, scroll: Vec2) -> GridView {
        GridView::new(metrics, self.steps, scroll)
    }

    /// The top bar inset by its padding — where the bar's widgets go.
    ///
    /// The inset gives way rather than inverting the rect in a window too
    /// small to hold it, as [`Self::new`] gives way to a squeezed window.
    pub fn bar_inner(&self) -> Rect {
        let pad = sane(theme::PANEL_PADDING, 0.0, self.top_bar.width().max(0.0) * 0.5)
            .min(self.top_bar.height().max(0.0) * 0.5);
        self.top_bar.shrink(pad)
    }

    /// The follow toggle's slot: the right end of the top bar.
    ///
    /// Placed from a rect rather than laid out, like [`HeaderFields`]: it is
    /// the one control in the bar that must stay put, and the pattern buttons
    /// share the row with it. Narrower than
    /// [`theme::FOLLOW_WIDTH`] only when the bar itself is, so it is always
    /// inside the bar and never inverted.
    pub fn follow_button(&self) -> Rect {
        let bar = self.bar_inner();
        let width = sane(theme::FOLLOW_WIDTH, 0.0, bar.width().max(0.0));
        Rect::from_min_max(pos2(bar.max.x - width, bar.min.y), bar.max)
    }

    /// The `+ lane` button's slot: directly left of the follow toggle.
    ///
    /// Placed from the right like [`Self::follow_button`], and for the same
    /// reason: the two controls that must stay put are the ones at the end of
    /// the bar, and the pattern buttons grow from the left into whatever
    /// [`Self::bar_rest`] is left. Squeezed down to nothing rather than past
    /// the left edge of the bar in a window too narrow to hold it.
    pub fn add_lane_button(&self) -> Rect {
        let bar = self.bar_inner();
        let right = (self.follow_button().min.x - theme::ITEM_SPACING).max(bar.min.x);
        let width = sane(theme::ADD_LANE_WIDTH, 0.0, (right - bar.min.x).max(0.0));
        Rect::from_min_max(pos2(right - width, bar.min.y), pos2(right, bar.max.y))
    }

    /// The parameter panel's show/hide toggle: directly left of `+ Lane`,
    /// placed from the right for the same reason.
    pub fn panel_button(&self) -> Rect {
        let bar = self.bar_inner();
        let right = (self.add_lane_button().min.x - theme::ITEM_SPACING).max(bar.min.x);
        let width = sane(theme::PANEL_TOGGLE_WIDTH, 0.0, (right - bar.min.x).max(0.0));
        Rect::from_min_max(pos2(right - width, bar.min.y), pos2(right, bar.max.y))
    }

    /// What is left of the top bar once the follow toggle, the `+ Lane`
    /// button and the panel toggle have their slots — the room the pattern
    /// buttons share with the status line.
    pub fn bar_rest(&self) -> Rect {
        let bar = self.bar_inner();
        let end = (self.panel_button().min.x - theme::ITEM_SPACING).max(bar.min.x);
        Rect::from_min_max(bar.min, pos2(end, bar.max.y))
    }

    /// How wide one pattern button is drawn: [`theme::PATTERN_WIDTH`], or an
    /// even share of [`Self::bar_rest`] when there is not that much room.
    ///
    /// Shrinking rather than scrolling or wrapping, because the bar is the
    /// one row in the window that cannot grow: all eight buttons have to stay
    /// reachable in a window squeezed to [`super::MIN_SIZE`], and eight
    /// narrow digits still read as eight patterns. Zero when even that does
    /// not fit, which is the signal the caller draws nothing from.
    fn pattern_button_width(&self) -> f32 {
        let gaps = theme::ITEM_SPACING * (PATTERN_BUTTONS - 1) as f32;
        let share = (self.bar_rest().width() - gaps).max(0.0) / PATTERN_BUTTONS as f32;
        sane(theme::PATTERN_WIDTH, 0.0, share)
    }

    /// The whole pattern strip: the left end of the top bar, as wide as its
    /// buttons and the gaps between them, and empty when the bar has no room
    /// for them at all.
    pub fn pattern_bar(&self) -> Rect {
        let rest = self.bar_rest();
        let width = self.pattern_button_width();
        let buttons = PATTERN_BUTTONS as f32;
        let gaps = theme::ITEM_SPACING * (buttons - 1.0);
        let total = match width > 0.0 {
            // Clamped to the room it was divided out of: the division is a
            // float one, so the eight shares and their gaps may add back up
            // to a hair more than they came from, and the strip has to stay
            // inside the bar for `bar_status` to be a rect at all.
            true => sane(width * buttons + gaps, 0.0, rest.width()),
            false => 0.0,
        };
        Rect::from_min_max(rest.min, pos2(rest.min.x + total, rest.max.y))
    }

    /// One pattern button's slot, by zero-based button index.
    ///
    /// Empty rather than absent for an index past the last button, as a
    /// squeezed bar's buttons are: every slot in this bar is placed from a
    /// rect, and the caller already skips the ones with no room
    /// (`Rect::is_positive`).
    pub fn pattern_button(&self, index: usize) -> Rect {
        let rest = self.bar_rest();
        let width = self.pattern_button_width();
        if width <= 0.0 || index >= PATTERN_BUTTONS {
            return Rect::from_min_max(rest.min, rest.min);
        }
        let left = rest.min.x + index as f32 * (width + theme::ITEM_SPACING);
        let right = (left + width).min(rest.max.x);
        Rect::from_min_max(pos2(left.min(right), rest.min.y), pos2(right, rest.max.y))
    }

    /// One of the two edit-mode buttons (0 = steps, 1 = pitch): the left end
    /// of [`Self::bar_status`], right after the pattern strip. Empty when
    /// there is no room, like a squeezed pattern button.
    pub fn mode_button(&self, index: usize) -> Rect {
        let room = self.bar_status();
        let left = room.min.x + index as f32 * (theme::MODE_WIDTH + theme::ITEM_SPACING);
        let right = (left + theme::MODE_WIDTH).min(room.max.x);
        if index >= MODE_BUTTONS || right <= left {
            return Rect::from_min_max(room.min, room.min);
        }
        Rect::from_min_max(pos2(left, room.min.y), pos2(right, room.max.y))
    }

    /// What is left of [`Self::bar_status`] after the edit-mode buttons —
    /// where the instance's status line is drawn.
    pub fn status_line(&self) -> Rect {
        let room = self.bar_status();
        let used = MODE_BUTTONS as f32 * (theme::MODE_WIDTH + theme::ITEM_SPACING);
        let start = (room.min.x + used).min(room.max.x);
        Rect::from_min_max(pos2(start, room.min.y), room.max)
    }

    /// What is left of [`Self::bar_rest`] right of the pattern strip — where
    /// the edit-mode buttons and the instance's status line go.
    pub fn bar_status(&self) -> Rect {
        let rest = self.bar_rest();
        let bar = self.pattern_bar();
        let start = match bar.width() > 0.0 {
            true => (bar.max.x + theme::ITEM_SPACING).min(rest.max.x),
            false => rest.min.x,
        };
        Rect::from_min_max(pos2(start, rest.min.y), rest.max)
    }
}

/// How many edit-mode buttons the top bar has: steps and pitch.
pub const MODE_BUTTONS: usize = 2;

/// The most of the space under the top bar the parameter panel may take.
pub const PANEL_MAX_SHARE: f32 = 0.5;

// ---- the pattern bar ----

/// How many buttons the pattern bar has: one per pattern a [`SeqData`][d]
/// holds, which is also the span of the `patsel` parameter.
///
/// [d]: crate::model::SeqData
pub const PATTERN_BUTTONS: usize = NUM_PATTERNS;

/// The `patsel` value button `index` writes.
///
/// The parameter is one-based and the buttons are not, so this is the one
/// place in the GUI that knows the offset — and it takes it from
/// [`BlockCtx::PATTERN_SELECT_MIN`] rather than writing `1`, like
/// [`super::BitstepEditor::shown_pattern`] on the way back. Saturating at the
/// last button, so a caller that counted wrong selects a pattern that exists
/// rather than one the parameter would clamp for it.
pub fn pattern_button_value(index: usize) -> i32 {
    let index = index.min(PATTERN_BUTTONS - 1) as i32;
    index + i32::from(BlockCtx::PATTERN_SELECT_MIN)
}

/// The button a `patsel` value belongs to — [`pattern_button_value`]
/// backwards — or `None` for a value outside the parameter's range.
///
/// `None` rather than a clamp: the values that reach this come from the
/// parameter, which has already clamped them, so one that is out of range
/// means the range moved and no button should light rather than the wrong
/// one.
pub fn pattern_button_index(value: i32) -> Option<usize> {
    let min = i32::from(BlockCtx::PATTERN_SELECT_MIN);
    let max = i32::from(BlockCtx::PATTERN_SELECT_MAX);
    (min..=max).contains(&value).then(|| (value - min) as usize)
}

/// The button a *pattern index* lights: what the playhead publishes
/// (zero-based) mapped to the bar, or `None` when the bar has no button for
/// it.
///
/// Separate from [`pattern_button_index`] because the two numbers are
/// different things — the playhead's index addresses `SeqData::patterns`,
/// the parameter's value is one more than that — and taking a `u32` is what
/// keeps the bounds check on [`Playhead::pattern`][p]'s own type.
///
/// [p]: crate::model::Playhead::pattern
pub fn pattern_index_button(index: u32) -> Option<usize> {
    usize::try_from(index).ok().filter(|index| *index < PATTERN_BUTTONS)
}

// ---- the lane header ----

/// Where one lane header's nine fields go, left to right: drag grip, colour
/// dot, name, key, length, rate, volume, mute, solo.
///
/// Absolute rects rather than an egui layout, for the same reason the grid is
/// hand-painted: every lane's header must line its columns up with every
/// other's, whatever a lane happens to be called, and a horizontal layout
/// would let a long name push the toggles out of line.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HeaderFields {
    pub grip: Rect,
    pub dot: Rect,
    pub name: Rect,
    pub key: Rect,
    pub length: Rect,
    pub rate: Rect,
    pub volume: Rect,
    pub mute: Rect,
    pub solo: Rect,
}

impl HeaderFields {
    /// The field widths, in drawing order, from [`theme`].
    pub const WIDTHS: [f32; 9] = [
        theme::HEADER_GRIP_WIDTH,
        theme::HEADER_DOT_WIDTH,
        theme::HEADER_NAME_WIDTH,
        theme::HEADER_KEY_WIDTH,
        theme::HEADER_LENGTH_WIDTH,
        theme::HEADER_RATE_WIDTH,
        theme::HEADER_VOLUME_WIDTH,
        theme::HEADER_TOGGLE_WIDTH,
        theme::HEADER_TOGGLE_WIDTH,
    ];

    /// The width the fields want: all of [`Self::WIDTHS`] plus the gaps
    /// between them.
    pub fn needed_width() -> f32 {
        let gaps = theme::HEADER_FIELD_GAP * (Self::WIDTHS.len() - 1) as f32;
        Self::WIDTHS.iter().sum::<f32>() + gaps
    }

    /// Splits a header row band ([`EditorLayout::header_row`], inset by
    /// whatever padding the caller wants) into the fields.
    ///
    /// Everything is scaled down together when the band is narrower than
    /// [`Self::needed_width`] — which is what a host that ignored
    /// [`super::MIN_SIZE`] leaves, since [`EditorLayout::new`] then trims the
    /// header column to the window. Scaling keeps the fields in order,
    /// non-overlapping and inside the band, so a squeezed header is cramped
    /// rather than drawn over the step strip.
    pub fn split(row: Rect) -> HeaderFields {
        let needed = Self::needed_width();
        let available = sane(row.width(), 0.0, f32::MAX);
        // `needed` is a sum of positive constants, so this cannot divide by
        // zero; the `min` is what keeps a roomy header at its tuned sizes
        // instead of stretching the dot across it.
        let scale = sane(available / needed, 0.0, 1.0);

        let mut fields = [Rect::ZERO; Self::WIDTHS.len()];
        let mut x = row.min.x;
        for (field, width) in fields.iter_mut().zip(Self::WIDTHS) {
            let width = width * scale;
            *field = Rect::from_min_max(pos2(x, row.min.y), pos2(x + width, row.max.y));
            x += width + theme::HEADER_FIELD_GAP * scale;
        }

        let [grip, dot, name, key, length, rate, volume, mute, solo] = fields;
        HeaderFields { grip, dot, name, key, length, rate, volume, mute, solo }
    }

    /// The row's *lane handle*: the grip, the colour dot and the name as one
    /// region, which is what a lane is dragged by and right-clicked on
    /// (the drag handle that reorders lanes, and the context menu beside
    /// it).
    ///
    /// Three fields rather than the grip alone because none of them is a
    /// widget — the grip and the dot are painted and the name is a label — so
    /// the whole left end of the row is free to answer the pointer, and a
    /// 10-point grip is a small thing to ask a hand to hit. It stops at the
    /// name's right edge: everything past it (key, length, rate, mute, solo)
    /// is a control of its own, and a drag starting on one of those belongs
    /// to that control.
    pub fn handle(&self) -> Rect {
        self.grip.union(self.dot).union(self.name)
    }
}

/// Where the dots of a lane header's drag grip go: two columns of three,
/// centred in the grip's field.
///
/// Painted rather than a glyph, so the grip needs no font that may not be
/// there (the editor ships no custom fonts) and lines up with the row at any
/// cell height. The dots are inset by [`theme::GRIP_DOT_RADIUS`] so that none
/// of them is clipped by the field's edge, and a field too small to hold them
/// collapses them onto its centre rather than placing them outside it.
pub fn grip_dots(grip: Rect) -> [Pos2; 6] {
    let radius = sane(theme::GRIP_DOT_RADIUS, 0.0, f32::MAX);
    // The field's own size, with a NaN one (which a header squeezed to
    // nothing can hand over) read as empty rather than carried into every dot
    // below.
    let width = sane(grip.width(), 0.0, f32::MAX);
    let height = sane(grip.height(), 0.0, f32::MAX);
    // Half the clear space the two columns and three rows of dots want,
    // never more than half the field: the dots are measured from the centre
    // outwards, so this is what keeps them inside it.
    let half_x = sane(radius * 1.6, 0.0, (width * 0.5 - radius).max(0.0));
    let half_y = sane(radius * 2.6, 0.0, (height * 0.5 - radius).max(0.0));
    let centre = grip.min + vec2(width, height) * 0.5;

    let mut dots = [centre; 6];
    for (index, dot) in dots.iter_mut().enumerate() {
        let column = if index % 2 == 0 { -half_x } else { half_x };
        // Rows at -1, 0, +1 of the vertical half-space.
        let row = (index / 2) as f32 - 1.0;
        *dot = centre + vec2(column, row * half_y);
    }
    dots
}

/// The twelve pitch classes, sharps only — the spelling the lane header
/// shows ("F#1", never "Gb1").
pub const PITCH_CLASSES: [&str; 12] =
    ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];

/// The octave MIDI key 0 is named in.
///
/// Bitwig (like Ableton) calls key 60 "C3", which puts key 0 two octaves
/// below zero. That is the convention the lane header is written in — a
/// kit's kick, snare and hat on keys 36, 38 and 42 read C1, D1 and F#1 — so
/// it is the one the GUI has to use, or every key in the
/// editor would read an octave off what the host shows.
pub const LOWEST_OCTAVE: i32 = -2;

/// A MIDI key as the note name the lane header shows, e.g. 36 → `"C1"`.
///
/// Keys above 127 cannot reach the sequence ([`Lane::key`] clamps on the way
/// in *and* out), so they are named as 127 rather than panicking on a field
/// some future version widened.
pub fn note_name(key: u8) -> String {
    let key = i32::from(key.min(Lane::KEY_MAX));
    format!("{}{}", PITCH_CLASSES[(key % 12) as usize], key / 12 + LOWEST_OCTAVE)
}

/// A note name back to a MIDI key, or `None` for anything that is not one —
/// the parser behind the key field's typed entry.
///
/// Deliberately more generous than [`note_name`]'s output: case is ignored
/// and flats are accepted ("bb2" is "Bb2" is key 46), because the user is
/// typing rather than choosing. A name outside 0..=127 is rejected instead of
/// clamped, so the field keeps the key it had rather than jumping to an end
/// of the range the user never asked for.
pub fn parse_note_name(text: &str) -> Option<u8> {
    let text = text.trim();
    let mut chars = text.chars();
    // Semitones from the C below, so the letters are in sounding order
    // rather than alphabetical.
    let class = match chars.next()?.to_ascii_uppercase() {
        'C' => 0,
        'D' => 2,
        'E' => 4,
        'F' => 5,
        'G' => 7,
        'A' => 9,
        'B' => 11,
        _ => return None,
    };

    // A leading 'b' here is a flat rather than a note: the note letter has
    // already been taken, and "Bb" has to mean one semitone below B.
    let rest = chars.as_str();
    let (accidental, octave) = match rest.strip_prefix(['#', 's', 'S']) {
        Some(octave) => (1, octave),
        None => match rest.strip_prefix(['b', 'B']) {
            Some(octave) => (-1, octave),
            None => (0, rest),
        },
    };

    let octave: i32 = octave.trim().parse().ok()?;
    // `checked_*`: an octave of 200_000_000 overflows before it is in range.
    let key = octave
        .checked_sub(LOWEST_OCTAVE)?
        .checked_mul(12)?
        .checked_add(class + accidental)?;
    (0..=i32::from(Lane::KEY_MAX)).contains(&key).then_some(key as u8)
}

/// A raw widget value as a MIDI key: rounded, clamped, NaN-safe.
///
/// The key field is an [`egui::DragValue`], which works in `f64` and hands
/// back whatever its own arithmetic produced. Clamping here as well as in its
/// range is the rule that values are clamped at every boundary:
/// [`Lane::set_key`] would clamp a 200 down to 127, but a NaN cast straight
/// to `u8` is 0 — the wrong end of the keyboard.
pub fn widget_key(value: f64) -> u8 {
    // `max` then `min`, as `sane` does for `f32`: `f64::clamp` would hand a
    // NaN straight back.
    value.round().max(0.0).min(f64::from(Lane::KEY_MAX)) as u8
}

/// The widget value a MIDI channel is shown as: one-based, the way every
/// host counts channels.
///
/// The model stores the channel the wire carries (0..=15,
/// [`Lane::CHANNEL_MAX`]) and the menu shows 1..=16, so the offset lives here
/// — with [`widget_channel`] as its inverse — rather than as a `+ 1` written
/// into the editor beside a `- 1`.
pub fn channel_widget(channel: u8) -> f64 {
    f64::from(channel.min(Lane::CHANNEL_MAX)) + 1.0
}

/// A raw widget value as a MIDI channel: one-based in, zero-based out,
/// rounded, clamped, NaN-safe.
///
/// Clamped here as well as in the field's range for [`widget_key`]'s reason:
/// a NaN cast straight to `u8` is 0, which is a valid channel and so would
/// pass unnoticed.
pub fn widget_channel(value: f64) -> u8 {
    let zero_based = value.round() - 1.0;
    zero_based.max(0.0).min(f64::from(Lane::CHANNEL_MAX)) as u8
}

/// A key moved by `semitones`, held inside the MIDI range.
///
/// The clamp is the whole point: the key field's drag and scroll both work in
/// deltas, and a gesture that ran off either end would otherwise wrap through
/// [`Lane::set_key`]'s own clamp and come back at the wrong note.
pub fn nudge_key(key: u8, semitones: i32) -> u8 {
    i32::from(key).saturating_add(semitones).clamp(0, i32::from(Lane::KEY_MAX)) as u8
}

/// Points of wheel scroll one semitone of a lane's key costs.
///
/// Feel, not look, so it lives here with [`DRAG_DEADZONE`]: egui spreads one
/// wheel notch over several frames of `smooth_scroll_delta`, so the editor
/// banks the points and [`take_scroll_steps`] spends them. Roughly a notch
/// per semitone on a mouse, and a deliberate flick on a trackpad.
pub const KEY_SCROLL_PITCH: f32 = 24.0;

/// The most semitones one frame of scroll may move a key — the whole MIDI
/// range, so no bank of points can do more than cross it.
const MAX_SCROLL_STEPS: f32 = 128.0;

/// The whole steps the scroll banked in `accumulated` has earned, leaving the
/// remainder there for the next frame.
///
/// Truncating rather than rounding, and the remainder kept: that is what
/// makes a slow drag of the wheel move one semitone per notch instead of
/// skipping or stalling, and what keeps a trackpad's stream of fractions from
/// moving the key at all until it adds up to a note.
///
/// A non-finite bank (egui has handed out an infinite scroll delta before a
/// scroll area was laid out) is thrown away rather than turned into
/// [`i32::MAX`] semitones.
pub fn take_scroll_steps(accumulated: &mut f32, pitch: f32) -> i32 {
    let banked = *accumulated;
    if !banked.is_finite() || !pitch.is_finite() || pitch <= 0.0 {
        *accumulated = 0.0;
        return 0;
    }

    let steps = (banked / pitch).trunc();
    *accumulated = banked - steps * pitch;
    sane(steps, -MAX_SCROLL_STEPS, MAX_SCROLL_STEPS) as i32
}

/// What a wanted loop length means for one lane in one pattern: the capacity
/// to make room for first, then the length to store.
///
/// Length is a value edit inside the lane's capacity and a shape edit past it
/// (the model's two kinds of edits), and the lane header's drag crosses
/// that line without the user knowing it is there. Working out which
/// writes it takes is arithmetic, so it happens here and the editor just
/// performs them: [`Self::grow`] through `SeqModel::ensure_capacity`, then
/// [`Self::length`] through `SeqModel::set_length`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LengthEdit {
    length: u32,
    grow: Option<usize>,
}

impl LengthEdit {
    /// The shortest a lane may be, as [`crate::model::LaneData::set_length`]
    /// also clamps it: a lane of no steps has no loop to play.
    pub const MIN: u32 = 1;

    /// The longest, from the data model's own cap.
    pub const MAX: u32 = MAX_STEPS as u32;

    /// Resolves a wanted length against the capacity the lane has now.
    ///
    /// `wanted` is the raw widget value, so it is whatever an
    /// [`egui::DragValue`] produced — clamped here as well as by the widget's
    /// range, NaN and infinity included (values are clamped at every
    /// boundary).
    pub fn resolve(wanted: f64, capacity: usize) -> LengthEdit {
        // `max` then `min`, as `sane` does for `f32`: `f64::clamp` would hand
        // a NaN back, and the cast of a NaN is 0 rather than the floor.
        let rounded = wanted.round().max(f64::from(Self::MIN)).min(f64::from(Self::MAX));
        let length = (rounded as u32).clamp(Self::MIN, Self::MAX);
        let grow = (length as usize > capacity).then_some(length as usize);
        LengthEdit { length, grow }
    }

    /// The length to store, inside `1..=MAX_STEPS`.
    pub fn length(&self) -> u32 {
        self.length
    }

    /// The capacity the lane must be grown to first, or `None` when it
    /// already holds the length — which is the common case, and the one that
    /// keeps a length drag inside a capacity chunk a value edit.
    pub fn grow(&self) -> Option<usize> {
        self.grow
    }
}

// ---- the pan / ratchet / probability popover ----

/// The ends of the pan slider: the packed field's own range, one slider unit
/// per packed value.
///
/// The slider works in packed units rather than in the `[-1, 1]` the host is
/// handed ([`pan_to_f32`]) so that every position on the rail is a value the
/// step can actually hold — a rail of 127 detents, with no two of them
/// rounding to the same pan and no pan unreachable. What the user reads is
/// [`pan_label`]'s left/right percentage, which is that `[-1, 1]` in
/// percent.
pub const PAN_WIDGET_MIN: f64 = Step::PAN_MIN as f64;

/// The other end of the pan slider — see [`PAN_WIDGET_MIN`].
pub const PAN_WIDGET_MAX: f64 = Step::PAN_MAX as f64;

/// A raw widget value as a packed pan: rounded, clamped, NaN-safe.
///
/// Clamped here as well as in the slider's range and in
/// [`Step::with_pan`], for [`widget_key`]'s reason: a NaN cast straight to
/// `u8` is 0, which is hard left rather than the centre the gesture was
/// nowhere near.
#[allow(clippy::manual_clamp)]
pub fn widget_pan(value: f64) -> u8 {
    // `max` then `min`, as `sane` does for `f32`: `f64::clamp` would hand a
    // NaN straight back.
    value.round().max(PAN_WIDGET_MIN).min(PAN_WIDGET_MAX) as u8
}

/// A packed pan as the popover shows it: `C` at the centre, otherwise `L`/`R`
/// and how far over it is, in percent.
///
/// The percentage *is* [`pan_to_f32`]'s `[-1, 1]`, times a hundred: the
/// number on screen is the one the host will receive, not a second scale that
/// could drift from it.
pub fn pan_label(pan: u8) -> String {
    let percent = (f64::from(pan_to_f32(pan)) * 100.0).round() as i32;
    if percent == 0 {
        "C".to_owned()
    } else if percent < 0 {
        format!("L{}", -percent)
    } else {
        format!("R{percent}")
    }
}

/// [`pan_label`] read back: `C`, `L50`, `R50`, or a bare signed percentage.
///
/// Typing into the slider is egui's own affordance, so what it emits has to
/// parse again — a round trip through [`pan_label`] lands on exactly the pan
/// it started from. A bare number is taken as the same signed percentage the
/// labels count in (`-50` is `L50`), because that is the scale the field is
/// showing; anything else is `None`, which egui leaves the value alone for.
pub fn parse_pan_label(text: &str) -> Option<u8> {
    let text = text.trim();
    let (sign, digits) = match text.chars().next()? {
        'c' | 'C' if text.len() == 1 => return Some(Step::PAN_CENTRE),
        'l' | 'L' => (-1.0, &text[1..]),
        'r' | 'R' => (1.0, &text[1..]),
        _ => (1.0, text),
    };
    let percent: f64 = digits.trim().parse().ok()?;
    percent.is_finite().then(|| pan_from_percent(sign * percent))
}

/// A pan percentage — `-100` hard left, `0` centre, `100` hard right — as a
/// packed pan.
///
/// The inverse of the scale [`pan_label`] prints, so the two agree about
/// where "L50" is.
pub fn pan_from_percent(percent: f64) -> u8 {
    let centre = f64::from(Step::PAN_CENTRE);
    widget_pan(centre + percent / 100.0 * centre)
}

/// The ends of the ratchet slider: the effective count, not the `n - 1` the
/// step packs.
pub const RATCHET_WIDGET_MIN: f64 = Step::RATCHET_MIN as f64;

/// The other end of the ratchet slider — see [`RATCHET_WIDGET_MIN`].
pub const RATCHET_WIDGET_MAX: f64 = Step::RATCHET_MAX as f64;

/// A raw widget value as a ratchet count, 1..=8.
///
/// Rounded and clamped for [`widget_pan`]'s reason, and the lower bound
/// matters twice over here: [`Step::with_ratchet`] subtracts one before
/// packing, so a 0 that slipped through would underflow.
// Not `clamp`: `max` then `min` is what folds a NaN onto the low end
// instead of handing it back to the `as u8` below.
#[allow(clippy::manual_clamp)]
pub fn widget_ratchet(value: f64) -> u8 {
    value.round().max(RATCHET_WIDGET_MIN).min(RATCHET_WIDGET_MAX) as u8
}

/// The ends of the probability slider, in percent — the packed field's own
/// scale.
pub const PROBABILITY_WIDGET_MIN: f64 = 0.0;

/// The other end of the probability slider — see [`PROBABILITY_WIDGET_MIN`].
pub const PROBABILITY_WIDGET_MAX: f64 = Step::PROBABILITY_MAX as f64;

/// A raw widget value as a step probability, 0..=100%.
///
/// Zero is a real setting (a step that never fires but keeps its values), so
/// the bottom of the rail is not clamped away.
// Not `clamp`: `max` then `min` is what folds a NaN onto the low end
// instead of handing it back to the `as u8` below.
#[allow(clippy::manual_clamp)]
pub fn widget_probability(value: f64) -> u8 {
    value.round().max(PROBABILITY_WIDGET_MIN).min(PROBABILITY_WIDGET_MAX) as u8
}

/// The ends of the velocity slider in the step popover — the packed field's
/// own scale, 1..=127.
pub const VELOCITY_WIDGET_MIN: f64 = Step::VELOCITY_MIN as f64;

/// The other end of the velocity slider — see [`VELOCITY_WIDGET_MIN`].
pub const VELOCITY_WIDGET_MAX: f64 = Step::VELOCITY_MAX as f64;

/// A raw widget value as a step velocity, 1..=127.
// Not `clamp`: `max` then `min` folds a NaN onto the low end.
#[allow(clippy::manual_clamp)]
pub fn widget_velocity(value: f64) -> u8 {
    value.round().max(VELOCITY_WIDGET_MIN).min(VELOCITY_WIDGET_MAX) as u8
}

/// The ends of the pitch slider, in semitones.
pub const PITCH_WIDGET_MIN: f64 = Step::PITCH_MIN as f64;

/// The other end of the pitch slider — see [`PITCH_WIDGET_MIN`].
pub const PITCH_WIDGET_MAX: f64 = Step::PITCH_MAX as f64;

/// A raw widget value as a pitch offset in semitones, -12..=12.
// Not `clamp`: `max` then `min` folds a NaN onto the low end.
#[allow(clippy::manual_clamp)]
pub fn widget_pitch(value: f64) -> i8 {
    value.round().max(PITCH_WIDGET_MIN).min(PITCH_WIDGET_MAX) as i8
}

/// A raw widget value as a lane volume in dB: rounded, clamped, NaN-safe.
// Not `clamp`: `max` then `min` folds a NaN onto the low end.
#[allow(clippy::manual_clamp)]
pub fn widget_volume(value: f64) -> i8 {
    value.round().max(f64::from(Lane::VOLUME_MIN)).min(f64::from(Lane::VOLUME_MAX)) as i8
}

/// How a lane volume reads in its header field: `0 dB`, `+6 dB`, `-12 dB`.
pub fn volume_label(db: i8) -> String {
    if db > 0 { format!("+{db} dB") } else { format!("{db} dB") }
}

/// [`volume_label`] read back, or a bare number: `+6 dB`, `-3`, `4db`.
pub fn parse_volume(text: &str) -> Option<i8> {
    let text = text.trim();
    let number = text
        .strip_suffix("dB")
        .or_else(|| text.strip_suffix("db"))
        .or_else(|| text.strip_suffix("DB"))
        .unwrap_or(text)
        .trim();
    let value: f64 = number.strip_prefix('+').unwrap_or(number).parse().ok()?;
    value.is_finite().then(|| widget_volume(value))
}

/// How a pitch reads in the popover and on the grid: `0`, `+5`, `-12`.
pub fn pitch_label(pitch: i8) -> String {
    if pitch > 0 { format!("+{pitch}") } else { pitch.to_string() }
}

// ---- drawing pitch (pitch mode) ----

/// Points of vertical travel per semitone when drawing pitch: the full
/// ±12 is 48 points either way of the press, about a row and a half.
pub const PITCH_DRAW_POINTS: f32 = 4.0;

/// The pitch a drag of `dy` points from the press means, from the pitch the
/// pressed step had. Up (negative `dy`) is higher, as on a keyboard roll.
pub fn drag_pitch(start: i8, dy: f32) -> i8 {
    let start = start.clamp(Step::PITCH_MIN, Step::PITCH_MAX);
    if !dy.is_finite() {
        return start;
    }
    let target = f32::from(start) - dy / PITCH_DRAW_POINTS;
    sane(target.round(), f32::from(Step::PITCH_MIN), f32::from(Step::PITCH_MAX)) as i8
}

/// The steps from `from` to `to` (both included, in that order) with their
/// pitches on the straight line between `from_pitch` and `to_pitch`, rounded
/// to whole semitones — the even slope a Shift-drag draws.
pub fn pitch_line(from: usize, from_pitch: i8, to: usize, to_pitch: i8) -> Vec<(usize, i8)> {
    level_line(from, i32::from(from_pitch), to, i32::from(to_pitch))
        .into_iter()
        // In range: every value is between two pitches.
        .map(|(step, value)| (step, value as i8))
        .collect()
}

/// [`pitch_line`] for any whole-number value: the steps from `from` to `to`
/// (both included, in that order) with their values on the straight line
/// between `from_value` and `to_value`, rounded.
pub fn level_line(from: usize, from_value: i32, to: usize, to_value: i32) -> Vec<(usize, i32)> {
    let span = from.abs_diff(to);
    if span == 0 {
        return vec![(to, to_value)];
    }
    let (a, b) = (from_value as f32, to_value as f32);
    (0..=span)
        .map(|i| {
            let step = if to >= from { from + i } else { from - i };
            let t = i as f32 / span as f32;
            (step, (a + (b - a) * t).round() as i32)
        })
        .collect()
}

/// A pitch-drawing gesture in progress, locked to one lane row.
///
/// Two ways to draw, chosen per frame by whether Shift is held:
/// - **freehand**: every step the pointer passes gets the pitch the pointer's
///   height means right now, and steps skipped between two frames are filled
///   in along a line, so a fast stroke leaves no gaps;
/// - **line** (Shift): a straight, even slope from the pressed step (at the
///   pitch it had) to the step under the pointer. Steps a longer line touched
///   earlier in the gesture go back to the pitch they had before it.
///
/// Heights are measured from the press, like [`VelocityDrag`]: the pressed
/// step's pitch is the zero point, latched so the gesture's own writes cannot
/// move it.
#[derive(Clone, Debug, PartialEq)]
pub struct PitchDraw {
    row: usize,
    origin_step: usize,
    origin_y: f32,
    start: i8,
    /// The last point freehand drawing reached, which the next is joined to.
    last: (usize, i8),
    /// Every step this gesture has written, with the pitch it had before —
    /// what a shortened line restores.
    originals: Vec<(usize, i8)>,
    moved: bool,
}

impl PitchDraw {
    /// Starts drawing on `step` of `row`, which had `pitch` when the button
    /// went down at `origin`.
    pub fn begin(row: usize, step: usize, pitch: i8, origin: Pos2) -> PitchDraw {
        let start = pitch.clamp(Step::PITCH_MIN, Step::PITCH_MAX);
        PitchDraw {
            row,
            origin_step: step,
            origin_y: origin.y,
            start,
            last: (step, start),
            originals: Vec::new(),
            moved: false,
        }
    }

    /// The lane row the gesture is locked to.
    pub fn row(&self) -> usize {
        self.row
    }

    /// The step the press landed on.
    pub fn origin_step(&self) -> usize {
        self.origin_step
    }

    /// Whether the pointer ever left the press's deadzone — `false` at the
    /// release means the gesture was a click, which resets the step's pitch.
    pub fn moved(&self) -> bool {
        self.moved
    }

    /// The pitch the pointer at screen height `y` means.
    pub fn pitch_at(&self, y: f32) -> i8 {
        drag_pitch(self.start, y - self.origin_y)
    }

    /// The pointer is over `step` at `pos` (with `line` true while Shift is
    /// held): the pitches to write, as `(step, pitch)`. `current` reads a
    /// step's pitch as the model holds it, so the first write to each step can
    /// remember what to restore.
    ///
    /// Nothing until the pointer has left the deadzone, so a click stays a
    /// click.
    pub fn draw_to(
        &mut self,
        step: usize,
        pos: Pos2,
        origin: Pos2,
        line: bool,
        current: impl Fn(usize) -> i8,
    ) -> Vec<(usize, i8)> {
        if !self.moved {
            if (pos - origin).length() < DRAG_DEADZONE {
                return Vec::new();
            }
            self.moved = true;
        }

        let pitch = self.pitch_at(pos.y);
        let mut writes = if line {
            let line = pitch_line(self.origin_step, self.start, step, pitch);
            let (lo, hi) = (self.origin_step.min(step), self.origin_step.max(step));
            // A line that got shorter hands back the steps it no longer covers.
            let mut writes: Vec<(usize, i8)> = self
                .originals
                .iter()
                .filter(|(s, _)| *s < lo || *s > hi)
                .copied()
                .collect();
            writes.extend(line);
            writes
        } else {
            let (last_step, last_pitch) = self.last;
            let mut joined = pitch_line(last_step, last_pitch, step, pitch);
            if last_step != step {
                // The last point was written by the previous frame.
                joined.remove(0);
            }
            joined
        };
        self.last = (step, pitch);

        for &(s, _) in &writes {
            if !self.originals.iter().any(|(seen, _)| *seen == s) {
                self.originals.push((s, current(s)));
            }
        }
        writes.dedup();
        writes
    }
}

// ---- lane fills ----

/// A preset rhythm the lane menu can fill a lane with, measured in beats
/// from the lane's first step, so it means the same thing at any lane rate:
/// quarter notes are every 4th step at 1/16 and every 2nd at 1/8.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fill {
    Every,
    Eighths,
    Quarters,
    Halves,
    Bars,
    OffBeats,
    Backbeat,
}

/// How close a step's start has to be to a fill's grid to count as on it.
/// Generous next to float error and far below the smallest step (1/64T).
const FILL_TOLERANCE: f64 = 1e-6;

impl Fill {
    /// Every fill, in menu order.
    pub const ALL: [Fill; 7] = [
        Fill::Every,
        Fill::Eighths,
        Fill::Quarters,
        Fill::Halves,
        Fill::Bars,
        Fill::OffBeats,
        Fill::Backbeat,
    ];

    /// The button text.
    pub const fn label(self) -> &'static str {
        match self {
            Fill::Every => "All",
            Fill::Eighths => "1/8",
            Fill::Quarters => "1/4",
            Fill::Halves => "1/2",
            Fill::Bars => "1/1",
            Fill::OffBeats => "Off-beats",
            Fill::Backbeat => "2 & 4",
        }
    }

    /// The button's hover text.
    pub const fn hint(self) -> &'static str {
        match self {
            Fill::Every => "Every step on",
            Fill::Eighths => "Eighth notes",
            Fill::Quarters => "Quarter notes (four on the floor)",
            Fill::Halves => "Half notes",
            Fill::Bars => "The first beat of every bar",
            Fill::OffBeats => "The eighth notes between the beats",
            Fill::Backbeat => "Beats 2 and 4 of every bar",
        }
    }

    /// The fill's grid as (period, offset) in beats: a step is on when its
    /// start is `offset` plus a whole number of periods. `None` for
    /// [`Fill::Every`], which needs no grid.
    const fn grid(self) -> Option<(f64, f64)> {
        match self {
            Fill::Every => None,
            Fill::Eighths => Some((0.5, 0.0)),
            Fill::Quarters => Some((1.0, 0.0)),
            Fill::Halves => Some((2.0, 0.0)),
            Fill::Bars => Some((4.0, 0.0)),
            Fill::OffBeats => Some((1.0, 0.5)),
            Fill::Backbeat => Some((2.0, 1.0)),
        }
    }

    /// Whether this fill makes sense on a lane whose steps are `step_beats`
    /// long: its grid has to fall on steps at a steady spacing. Eighths on a
    /// 1/4 lane, off-beats on a 1/4 lane, or quarters on a dotted lane would
    /// all come out as something else, so the menu greys them out.
    pub fn fits(self, step_beats: f64) -> bool {
        match self.grid() {
            None => true,
            Some((period, offset)) => {
                is_whole(period / step_beats) && period >= step_beats * (1.0 - FILL_TOLERANCE)
                    && is_whole(offset / step_beats)
            }
        }
    }

    /// Whether step `index` of a lane whose steps are `step_beats` long is
    /// on in this fill.
    pub fn hits(self, index: usize, step_beats: f64) -> bool {
        match self.grid() {
            None => true,
            Some((period, offset)) => {
                let from_offset = index as f64 * step_beats - offset;
                from_offset > -FILL_TOLERANCE * period && is_whole(from_offset / period)
            }
        }
    }
}

/// Whether `x` is a whole number, give or take [`FILL_TOLERANCE`].
fn is_whole(x: f64) -> bool {
    (x - x.round()).abs() < FILL_TOLERANCE
}

// ---- the parameter panel ----

/// What the parameter panel under the grid shows and draws, one bar per step
/// of the selected lane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParamKind {
    Velocity,
    Pan,
    Pitch,
    Chance,
    Ratchet,
}

impl ParamKind {
    /// Every parameter, in the order the panel's dropdown lists them.
    pub const ALL: [ParamKind; 5] = [
        ParamKind::Velocity,
        ParamKind::Pan,
        ParamKind::Pitch,
        ParamKind::Chance,
        ParamKind::Ratchet,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ParamKind::Velocity => "Velocity",
            ParamKind::Pan => "Pan",
            ParamKind::Pitch => "Pitch",
            ParamKind::Chance => "Chance",
            ParamKind::Ratchet => "Ratchet",
        }
    }

    /// The lowest and highest value, inclusive, in the units [`Self::read`]
    /// gives: velocity 1..=127, packed pan, semitones, percent, repeats.
    pub fn range(self) -> (i32, i32) {
        match self {
            ParamKind::Velocity => (i32::from(Step::VELOCITY_MIN), i32::from(Step::VELOCITY_MAX)),
            ParamKind::Pan => (i32::from(Step::PAN_MIN), i32::from(Step::PAN_MAX)),
            ParamKind::Pitch => (i32::from(Step::PITCH_MIN), i32::from(Step::PITCH_MAX)),
            ParamKind::Chance => (0, i32::from(Step::PROBABILITY_MAX)),
            ParamKind::Ratchet => (i32::from(Step::RATCHET_MIN), i32::from(Step::RATCHET_MAX)),
        }
    }

    /// Where a two-sided parameter's bars grow from — the middle of pan and
    /// pitch. `None` for the rest, whose bars grow from the bottom.
    pub fn centre(self) -> Option<i32> {
        match self {
            ParamKind::Pan => Some(i32::from(Step::PAN_CENTRE)),
            ParamKind::Pitch => Some(0),
            _ => None,
        }
    }

    /// The value a freshly switched-on step has, which a reset goes back to.
    pub fn default_value(self) -> i32 {
        self.read(Step::default_on())
    }

    /// The parameter's value on `step`. A never-edited step reads as the
    /// value it will have once switched on ([`Step::with_note_defaults`]).
    pub fn read(self, step: Step) -> i32 {
        let step = step.with_note_defaults();
        match self {
            ParamKind::Velocity => i32::from(step.velocity()),
            ParamKind::Pan => i32::from(step.pan()),
            ParamKind::Pitch => i32::from(step.pitch()),
            ParamKind::Chance => i32::from(step.probability()),
            ParamKind::Ratchet => i32::from(step.ratchet()),
        }
    }

    /// `step` with the parameter set to `value`, clamped into
    /// [`Self::range`] first. On or off: a never-edited step gets the default
    /// values under it first ([`Step::with_note_defaults`]), so drawing on an
    /// empty step ahead of time leaves it ready to switch on.
    pub fn write(self, step: Step, value: i32) -> Step {
        let (min, max) = self.range();
        let value = value.clamp(min, max);
        let step = step.with_note_defaults();
        // Every range above fits the field it is cast to.
        match self {
            ParamKind::Velocity => step.with_velocity(value as u8),
            ParamKind::Pan => step.with_pan(value as u8),
            ParamKind::Pitch => step.with_pitch(value as i8),
            ParamKind::Chance => step.with_probability(value as u8),
            ParamKind::Ratchet => step.with_ratchet(value as u8),
        }
    }

    /// How a value reads in the panel: `100`, `L50`, `+5`, `50%`, `3x`.
    pub fn value_label(self, value: i32) -> String {
        let (min, max) = self.range();
        let value = value.clamp(min, max);
        match self {
            ParamKind::Velocity => value.to_string(),
            ParamKind::Pan => pan_label(value as u8),
            ParamKind::Pitch => pitch_label(value as i8),
            ParamKind::Chance => format!("{value}%"),
            ParamKind::Ratchet => format!("{value}x"),
        }
    }

    /// The value a pointer at height `y` in `plot` means: the bottom edge is
    /// the lowest value and the top edge the highest. Past either edge is
    /// that end; a plot with no height means the default.
    pub fn value_at(self, plot: Rect, y: f32) -> i32 {
        let (min, max) = self.range();
        let height = plot.height();
        if height.is_nan() || height <= 0.0 {
            return self.default_value();
        }
        let t = sane((plot.max.y - y) / height, 0.0, 1.0);
        (min + (t * (max - min) as f32).round() as i32).clamp(min, max)
    }

    /// The height in `plot` that `value` sits at — [`Self::value_at`]
    /// turned around.
    pub fn y_of(self, plot: Rect, value: i32) -> f32 {
        let (min, max) = self.range();
        let t = (value.clamp(min, max) - min) as f32 / (max - min) as f32;
        plot.max.y - t * plot.height().max(0.0)
    }

    /// The bar a step's value draws in a column spanning `x`: up from the
    /// bottom of `plot`, or from its centre line for pan and pitch. A
    /// one-sided bar is never thinner than [`PARAM_MIN_BAR`], so a velocity
    /// of 1 still shows that the step is there.
    pub fn bar(self, plot: Rect, x: egui::Rangef, value: i32) -> Rect {
        let top = self.y_of(plot, value);
        let (top, bottom) = match self.centre() {
            Some(centre) => {
                let mid = self.y_of(plot, centre);
                (top.min(mid), top.max(mid))
            }
            None => (top.min(plot.max.y - PARAM_MIN_BAR), plot.max.y),
        };
        Rect::from_x_y_ranges(x, top..=bottom)
    }
}

/// The thinnest a one-sided parameter bar is drawn, in points.
pub const PARAM_MIN_BAR: f32 = 2.0;

/// A drawing gesture in the parameter panel, on one lane.
///
/// Like [`PitchDraw`], two ways to draw chosen per frame by Shift — freehand
/// (each step the pointer passes gets the value at the pointer's height, with
/// skipped steps filled in) or a straight line from the pressed step — but
/// with absolute heights: the pointer's height *is* the value, so a click
/// sets the step under it to the level clicked.
#[derive(Clone, Debug, PartialEq)]
pub struct LevelDraw {
    origin_step: usize,
    origin_value: i32,
    /// The last point drawn, which the next is joined to.
    last: (usize, i32),
    /// Every step this gesture has written, with the value it had before —
    /// what a shortened line restores.
    originals: Vec<(usize, i32)>,
}

impl LevelDraw {
    /// Starts drawing at `value` on `step`. Nothing is written until the
    /// first [`Self::draw_to`], which the press itself should make.
    pub fn begin(step: usize, value: i32) -> LevelDraw {
        LevelDraw {
            origin_step: step,
            origin_value: value,
            last: (step, value),
            originals: Vec::new(),
        }
    }

    /// The step the press landed on.
    pub fn origin_step(&self) -> usize {
        self.origin_step
    }

    /// The pointer is over `step` at the height that means `value` (with
    /// `line` true while Shift is held): the values to write, as
    /// `(step, value)`. `current` reads a step's value as the model holds
    /// it, so the first write to each step can remember what to restore.
    pub fn draw_to(
        &mut self,
        step: usize,
        value: i32,
        line: bool,
        current: impl Fn(usize) -> i32,
    ) -> Vec<(usize, i32)> {
        let mut writes = if line {
            let line = level_line(self.origin_step, self.origin_value, step, value);
            let (lo, hi) = (self.origin_step.min(step), self.origin_step.max(step));
            // A line that got shorter hands back the steps it no longer covers.
            let mut writes: Vec<(usize, i32)> = self
                .originals
                .iter()
                .filter(|(s, _)| *s < lo || *s > hi)
                .copied()
                .collect();
            writes.extend(line);
            writes
        } else {
            let (last_step, last_value) = self.last;
            let mut joined = level_line(last_step, last_value, step, value);
            if last_step != step && !self.originals.is_empty() {
                // The last point was written by the previous frame.
                joined.remove(0);
            }
            joined
        };
        self.last = (step, value);

        for &(s, _) in &writes {
            if !self.originals.iter().any(|(seen, _)| *seen == s) {
                self.originals.push((s, current(s)));
            }
        }
        writes.dedup();
        writes
    }
}

// ---- what the painter needs worked out, per cell ----

/// The part of a cell an on step's fill covers, for a fraction from
/// [`theme::fill_fraction`].
///
/// Grows from the bottom edge up, so a row of steps reads as a bar chart of
/// its velocities (fill height = velocity). The
/// fraction is clamped here as well as there: this is the function that
/// decides what reaches the screen, and a rect taller than its cell would
/// paint over the lane above.
pub fn fill_rect(cell: Rect, fraction: f32) -> Rect {
    // `max(0.0)` before the multiply, so a cell with a NaN or inverted height
    // yields an empty fill rather than one of infinite height.
    let height = cell.height().max(0.0) * sane(fraction, 0.0, 1.0);
    Rect::from_min_max(pos2(cell.min.x, cell.max.y - height), cell.max)
}


/// The bar a step's pitch draws in pitch mode: from the middle of the cell
/// up (higher) or down (lower), its length the pitch's share of half the
/// cell's height. `None` for a step with no pitch.
pub fn pitch_bar(cell: Rect, pitch: i8) -> Option<Rect> {
    if pitch == 0 || !cell.is_positive() {
        return None;
    }
    let half = cell.height() / 2.0;
    let pitch = pitch.clamp(Step::PITCH_MIN, Step::PITCH_MAX);
    let share = f32::from(pitch) / f32::from(Step::PITCH_MAX);
    // At least a hairline, so ±1 still shows.
    let length = (half * share.abs()).max(1.5);
    let mid = cell.center().y;
    let inset = sane(theme::MARKER_INSET, 0.0, cell.width() / 4.0);
    let (top, bottom) = if pitch > 0 { (mid - length, mid) } else { (mid, mid + length) };
    Some(Rect::from_min_max(pos2(cell.min.x + inset, top), pos2(cell.max.x - inset, bottom)))
}

/// The small mark a pitched step shows in steps mode: a thin bar on the cell's
/// left edge, above the middle for higher and below it for lower, sized like
/// [`pitch_bar`]. `None` for a step with no pitch.
pub fn pitch_marker(cell: Rect, pitch: i8) -> Option<Rect> {
    let bar = pitch_bar(cell, pitch)?;
    let left = cell.min.x + sane(theme::MARKER_INSET, 0.0, cell.width() / 4.0);
    let right = left + theme::PITCH_MARKER_WIDTH;
    Some(Rect::from_min_max(pos2(left, bar.min.y), pos2(right, bar.max.y)))
}

/// The ticks across the top of a ratcheted step's cell — one per sub-hit, so
/// a ratchet count of n shows n ticks.
///
/// An iterator rather than a collection: markers are worked out per cell per
/// frame for every visible step, and the grid's whole reason for culling is
/// that there can be a lot of those. Nothing is yielded for a ratchet of 1,
/// which is every step's default — a tick on all of them would say nothing
/// and only add noise.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RatchetTicks {
    /// The left edge of the next tick.
    left: f32,
    /// How far along the next one's left edge is from this one's.
    pitch: f32,
    width: f32,
    top: f32,
    height: f32,
    remaining: usize,
}

impl Iterator for RatchetTicks {
    type Item = Rect;

    fn next(&mut self) -> Option<Rect> {
        self.remaining = self.remaining.checked_sub(1)?;
        let left = self.left;
        self.left += self.pitch;
        Some(Rect::from_min_size(pos2(left, self.top), vec2(self.width, self.height)))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl ExactSizeIterator for RatchetTicks {}

/// The ticks for a step of `ratchet` sub-hits in `cell`.
///
/// The cell's width is divided into `ratchet` equal slices and a tick is
/// centred in each, so the count is readable at a glance from the spacing as
/// well as by counting. Each tick is [`theme::RATCHET_TICK_WIDTH`] wide
/// unless a slice is narrower than that, and hangs
/// [`theme::RATCHET_TICK_HEIGHT`] down from the cell's top edge — never
/// deeper than the cell, so a short row cannot be drawn through.
pub fn ratchet_ticks(cell: Rect, ratchet: u8) -> RatchetTicks {
    let count = usize::from(crate::model::clamp_u8(ratchet, Step::RATCHET_MIN, Step::RATCHET_MAX));
    let empty = RatchetTicks {
        left: cell.min.x,
        pitch: 0.0,
        width: 0.0,
        top: cell.min.y,
        height: 0.0,
        remaining: 0,
    };
    if count <= 1 {
        return empty;
    }

    let span = sane(cell.width(), 0.0, CellMetrics::MAX_SIZE);
    let pitch = span / count as f32;
    let width = sane(theme::RATCHET_TICK_WIDTH, 0.0, pitch);
    let depth = sane(cell.height(), 0.0, CellMetrics::MAX_SIZE);
    let height = sane(theme::RATCHET_TICK_HEIGHT, 0.0, depth);
    RatchetTicks {
        // Centred in the first slice; every later one is a pitch further on.
        left: cell.min.x + (pitch - width) / 2.0,
        pitch,
        width,
        top: cell.min.y,
        height,
        remaining: count,
    }
}

/// Where the dot marking a step that does not always fire goes — any
/// probability under 100% gets a corner dot.
///
/// The bottom-right corner: the ratchet ticks have the top edge, and the
/// velocity fill grows from the bottom, so a dot painted over the fill in
/// [`theme::MARKER`] reads whatever the velocity is. The inset is pulled back
/// for a cell too small to hold it, so the dot stays inside the cell it
/// belongs to rather than bleeding into the neighbour's.
pub fn probability_dot(cell: Rect) -> Pos2 {
    let short = sane(cell.width().min(cell.height()), 0.0, CellMetrics::MAX_SIZE);
    let inset = sane(theme::MARKER_DOT_RADIUS + theme::MARKER_INSET, 0.0, short / 2.0);
    pos2(cell.max.x - inset, cell.max.y - inset)
}

/// The step to draw a lane row's playhead on, or `None` for a row that has
/// none to draw.
///
/// Everything the GUI knows about playback arrives through the playhead
/// atomics, and three of the
/// things they can say mean "do not light a cell in this row":
///
/// * `playing` is `None` — the transport is stopped.
/// * `playing` is not `shown` — the grid on screen is `patsel`'s pattern,
///   which is not always the one being played: the audio thread only switches
///   at a step boundary, and the user may be editing a pattern that is not
///   playing at all. Lighting a cell of a pattern nobody is reading would be
///   a lie about where playback is.
/// * `step` is `None`, or past `length` — a lane that is not sounding, or an
///   index published before a shape edit shortened the lane. The GUI reads
///   the atomics and the sequence independently, so the two can disagree for
///   a frame; the cell simply goes unlit rather than the row drawing a
///   playhead outside itself.
pub fn playhead_cell(
    shown: usize,
    playing: Option<u32>,
    step: Option<u32>,
    length: u32,
) -> Option<usize> {
    if u64::from(playing?) != shown as u64 {
        return None;
    }
    let step = step?;
    (step < length).then_some(step as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{MAX_STEPS, Step};

    /// Metrics that make the arithmetic in the tests below easy to read:
    /// a 10×20 cell with a 2×4 gap, so the pitches are 12 and 24.
    fn test_metrics() -> CellMetrics {
        CellMetrics::new(vec2(10.0, 20.0), vec2(2.0, 4.0), 6.0)
    }

    fn view(scroll: Vec2) -> GridView {
        // A viewport 120 wide (ten columns) and 120 tall (five rows), parked
        // somewhere that is not the origin so that content and screen
        // coordinates cannot be confused.
        let viewport = Rect::from_min_size(pos2(300.0, 50.0), vec2(120.0, 120.0));
        GridView::new(test_metrics(), viewport, scroll)
    }

    /// The metrics the editor draws with are the theme's, and the derived
    /// pitches follow from them.
    #[test]
    fn metrics_come_from_the_theme() {
        let m = CellMetrics::from_theme();
        assert_eq!(m.cell_size(), vec2(theme::CELL_WIDTH, theme::CELL_HEIGHT));
        assert_eq!(m.gap(), vec2(theme::CELL_GAP, theme::ROW_GAP));
        assert_eq!(m.handle_width(), theme::END_HANDLE_WIDTH);
        assert_eq!(m.step_pitch(), theme::CELL_PITCH);
        assert_eq!(m.row_pitch(), theme::ROW_PITCH);
        assert_eq!(m, CellMetrics::default());

        // Content coordinates: step 0 and row 0 at the origin, then pitch.
        assert_eq!(m.step_x(0), 0.0);
        assert_eq!(m.row_y(0), 0.0);
        assert_eq!(m.step_x(4), 4.0 * theme::CELL_PITCH);
        assert_eq!(m.row_y(3), 3.0 * theme::ROW_PITCH);
    }

    /// A degenerate metric would divide by zero (or poison every rect with a
    /// NaN) in code that cannot report an error, so the constructor refuses
    /// to build one.
    #[test]
    fn metrics_clamp_degenerate_sizes() {
        for cell in [vec2(0.0, 0.0), vec2(-5.0, -5.0), vec2(f32::NAN, f32::NAN)] {
            let m = CellMetrics::new(cell, vec2(f32::NAN, -1.0), f32::NAN);
            assert_eq!(m.cell_size(), vec2(CellMetrics::MIN_CELL, CellMetrics::MIN_CELL));
            assert_eq!(m.gap(), Vec2::ZERO);
            assert_eq!(m.handle_width(), 0.0);
            assert!(m.step_pitch() > 0.0 && m.step_pitch().is_finite());
            assert!(m.row_pitch() > 0.0 && m.row_pitch().is_finite());
        }

        let huge = CellMetrics::new(
            Vec2::splat(f32::INFINITY),
            Vec2::splat(f32::INFINITY),
            f32::INFINITY,
        );
        assert_eq!(huge.cell_size(), Vec2::splat(CellMetrics::MAX_SIZE));
        assert_eq!(huge.gap(), Vec2::splat(CellMetrics::MAX_SIZE));
        assert_eq!(huge.handle_width(), CellMetrics::MAX_SIZE);
        assert!(huge.step_pitch().is_finite());
    }

    /// Cells tile the grid: each is the metrics' size, neighbours are a gap
    /// apart, and the pitch-wide column rects meet exactly so a column
    /// highlight has no seams.
    #[test]
    fn cells_tile_the_grid() {
        let v = view(Vec2::ZERO);
        let m = *v.metrics();

        let first = v.cell_rect(0, 0);
        assert_eq!(first.min, v.origin());
        assert_eq!(first.size(), m.cell_size());

        for step in 0..4 {
            for row in 0..3 {
                let cell = v.cell_rect(row, step);
                assert_eq!(cell.size(), m.cell_size(), "cell ({row}, {step}) size");

                let right = v.cell_rect(row, step + 1);
                assert_eq!(right.min.x - cell.max.x, m.gap().x, "gap right of {step}");
                assert_eq!(right.min.y, cell.min.y, "row {row} stays level");

                let below = v.cell_rect(row + 1, step);
                assert_eq!(below.min.y - cell.max.y, m.gap().y, "gap below {row}");
                assert_eq!(below.min.x, cell.min.x, "column {step} stays plumb");

                // The column owns its cell and the gaps after it, and meets
                // its neighbours without overlapping them.
                let column = v.column_rect(row, step);
                assert!(column.contains_rect(cell), "column ({row}, {step}) holds its cell");
                assert_eq!(column.min, cell.min);
                assert_eq!(column.max.x, v.column_rect(row, step + 1).min.x);
                assert_eq!(column.max.y, v.column_rect(row + 1, step).min.y);
            }
        }
    }

    /// Scrolling moves the rects and nothing else; the content origin is the
    /// viewport's corner minus the offset.
    #[test]
    fn cell_rects_follow_the_scroll() {
        let scroll = vec2(36.0, 48.0); // three columns, two rows
        let scrolled = view(scroll);
        let still = view(Vec2::ZERO);

        assert_eq!(scrolled.scroll(), scroll);
        assert_eq!(scrolled.origin(), still.origin() - scroll);
        assert_eq!(scrolled.viewport(), still.viewport());

        // The cell three columns and two rows along now sits where the first
        // one did.
        assert_eq!(scrolled.cell_rect(2, 3), still.cell_rect(0, 0));
        assert_eq!(scrolled.cell_rect(0, 0), still.cell_rect(0, 0).translate(-scroll));
    }

    /// Culling returns every column the viewport touches and no more, for a
    /// lane as long as the model allows.
    #[test]
    fn visible_steps_cover_the_viewport_exactly() {
        let pitch = test_metrics().step_pitch();

        for scroll_steps in [0, 1, 17, 1000, MAX_STEPS - 11] {
            // Half a column in, so both ends are partly visible — the case
            // that an off-by-one would drop a column at.
            let scroll = vec2(scroll_steps as f32 * pitch + pitch * 0.5, 0.0);
            let v = view(scroll);
            let range = v.visible_steps(MAX_STEPS);

            assert_eq!(range.start, scroll_steps, "first visible column");
            // Ten columns of viewport plus the two part-columns at the ends.
            assert_eq!(range.len(), 11, "columns drawn at offset {scroll_steps}");

            for step in range.clone() {
                let column = v.column_rect(0, step);
                assert!(
                    column.max.x > v.viewport().min.x && column.min.x < v.viewport().max.x,
                    "column {step} was drawn but is not on screen"
                );
            }
            // And the columns just outside really are off screen.
            if range.start > 0 {
                assert!(v.column_rect(0, range.start - 1).max.x <= v.viewport().min.x);
            }
            assert!(v.column_rect(0, range.end).min.x >= v.viewport().max.x);
        }
    }

    /// The bounds: a lane shorter than the viewport, an empty grid, a
    /// viewport scrolled off the end, and the infinite rect egui hands out
    /// before a scroll area has been laid out.
    #[test]
    fn visible_steps_clamp_to_the_lane() {
        let pitch = test_metrics().step_pitch();

        assert_eq!(view(Vec2::ZERO).visible_steps(0), 0..0, "nothing to draw");
        assert_eq!(view(Vec2::ZERO).visible_steps(4), 0..4, "a short lane is all visible");
        assert_eq!(view(Vec2::ZERO).visible_steps(MAX_STEPS), 0..10, "ten columns fit");

        // Scrolled past the end: a well-formed empty range, not a panic and
        // not a backwards one.
        let past = view(vec2(MAX_STEPS as f32 * pitch * 2.0, 0.0));
        let range = past.visible_steps(MAX_STEPS);
        assert!(range.is_empty(), "nothing is visible past the end: {range:?}");
        assert_eq!(range.start, MAX_STEPS);

        // A negative offset (over-scrolled, or a viewport left of the
        // content) starts at zero rather than wrapping around.
        let before = view(vec2(-50.0, -50.0));
        assert_eq!(before.visible_steps(MAX_STEPS).start, 0);
        assert_eq!(before.visible_rows(8).start, 0);

        // `Rect::EVERYTHING`: the infinities must clamp to the lane, which is
        // the only answer that draws correctly on the first frame.
        let infinite = GridView::new(test_metrics(), Rect::EVERYTHING, Vec2::ZERO);
        assert_eq!(infinite.visible_steps(MAX_STEPS), 0..MAX_STEPS);
        assert_eq!(infinite.visible_rows(9), 0..9);
        // And a NaN viewport is empty rather than enormous.
        let nan = GridView::new(test_metrics(), Rect::NAN, Vec2::ZERO);
        assert!(nan.visible_steps(MAX_STEPS).is_empty());
    }

    /// Rows cull the same way columns do, against the vertical pitch.
    #[test]
    fn visible_rows_cover_the_viewport() {
        let row_pitch = test_metrics().row_pitch();

        assert_eq!(view(Vec2::ZERO).visible_rows(100), 0..5, "five rows fit");
        assert_eq!(view(Vec2::ZERO).visible_rows(3), 0..3, "fewer lanes than fit");

        let v = view(vec2(0.0, 2.0 * row_pitch));
        assert_eq!(v.visible_rows(100), 2..7);
        for row in v.visible_rows(100) {
            let rect = v.column_rect(row, 0);
            assert!(rect.max.y > v.viewport().min.y && rect.min.y < v.viewport().max.y);
        }
    }

    /// Hit-testing, at the edges where it is easy to be one out: the corners
    /// of a cell, the gap after it, the column past a lane's length, the row
    /// past the last lane, and a position outside the viewport.
    #[test]
    fn hit_testing_finds_cells_and_their_edges() {
        let v = view(Vec2::ZERO);
        let m = *v.metrics();
        // Lane 0 is 6 steps long, lane 1 is 2, lane 2 is 1.
        let lengths = [6u32, 2, 1];
        let hit = |pos| v.hit(pos, lengths.len(), |row| lengths[row]);

        let cell = v.cell_rect(0, 3);
        assert_eq!(hit(cell.center()), Some(GridHit::Cell { row: 0, step: 3 }));
        assert_eq!(hit(cell.min), Some(GridHit::Cell { row: 0, step: 3 }), "top-left corner");
        // The gap after the cell belongs to it — no dead pixels in a row.
        let in_gap = pos2(cell.max.x + m.gap().x * 0.5, cell.center().y);
        assert_eq!(hit(in_gap), Some(GridHit::Cell { row: 0, step: 3 }), "the gap after a cell");
        // The gap below a row belongs to that row.
        let below = pos2(cell.center().x, cell.max.y + m.gap().y * 0.5);
        assert_eq!(hit(below), Some(GridHit::Cell { row: 0, step: 3 }), "the gap below a row");
        // One pitch on is the next column.
        assert_eq!(
            hit(cell.center() + vec2(m.step_pitch(), 0.0)),
            Some(GridHit::Cell { row: 0, step: 4 })
        );

        // Past the lane's length there is nothing to hit (the handle aside).
        assert_eq!(hit(v.cell_rect(0, 6).center()), None, "one past the length");
        assert_eq!(hit(v.cell_rect(1, 3).center()), None, "well past a short lane");
        assert_eq!(hit(v.cell_rect(1, 1).center()), Some(GridHit::Cell { row: 1, step: 1 }));

        // Past the last lane, and above the first row.
        assert_eq!(hit(v.cell_rect(3, 0).center()), None, "no fourth lane");
        assert_eq!(v.row_at(v.cell_rect(2, 0).center(), 3), Some(2));
        assert_eq!(v.row_at(pos2(v.origin().x, v.origin().y - 1.0), 3), None);

        // Outside the viewport: the headers are left of it and the content
        // under them is scrolled out, not clickable.
        let outside = pos2(v.viewport().min.x - 1.0, v.viewport().center().y);
        assert_eq!(hit(outside), None, "left of the viewport");
        assert_eq!(hit(pos2(v.viewport().center().x, v.viewport().max.y + 1.0)), None, "below");
    }

    /// The end handle sits on the boundary after the last step, takes
    /// precedence over the cell it overlaps, and stays grabbable at both ends
    /// of the length range.
    #[test]
    fn the_end_handle_wins_over_the_cell_beneath_it() {
        let v = view(Vec2::ZERO);
        let m = *v.metrics();
        let lengths = [4u32, 0, 1];
        let hit = |pos| v.hit(pos, lengths.len(), |row| lengths[row]);

        // Centred on the gap after step 3, a handle's width wide.
        let handle = v.end_handle_rect(0, 4);
        assert_eq!(handle.width(), m.handle_width());
        assert_eq!(handle.height(), m.cell_size().y);
        assert_eq!(handle.center().x, v.cell_rect(0, 3).max.x + m.gap().x * 0.5);
        assert_eq!(handle.min.y, v.cell_rect(0, 0).min.y);

        assert_eq!(hit(handle.center()), Some(GridHit::EndHandle { row: 0 }));
        // It overlaps the last cell, and wins there.
        let overlap = pos2(v.cell_rect(0, 3).max.x - 0.5, handle.center().y);
        assert!(handle.contains(overlap), "the handle overlaps the last cell");
        assert_eq!(hit(overlap), Some(GridHit::EndHandle { row: 0 }));
        // Just left of the handle is still the cell.
        let clear = pos2(handle.min.x - 1.0, handle.center().y);
        assert_eq!(hit(clear), Some(GridHit::Cell { row: 0, step: 3 }));

        // A lane with no steps still has a handle to drag it back open, and
        // it is inside the content rather than half a gap left of it.
        let empty = v.end_handle_rect(1, 0);
        assert!(empty.min.x >= v.origin().x, "an empty lane's handle is reachable");
        assert_eq!(hit(empty.center()), Some(GridHit::EndHandle { row: 1 }));

        // The handle moves with the length, and a maximal lane's is where the
        // content ends.
        assert_eq!(v.end_handle_rect(2, 1).center().x, v.cell_rect(2, 0).max.x + m.gap().x * 0.5);
        let last = v.end_handle_rect(0, MAX_STEPS as u32);
        let content = m.content_size(MAX_STEPS, 1);
        assert!(last.max.x <= v.origin().x + content.x, "the handle fits in the content");
        assert_eq!(content.y, m.row_pitch());
    }

    /// The top bar's slots: the follow toggle sits at the right end, `+ lane`
    /// beside it, what is left of the bar is to the left of both, and all of
    /// them stay inside the bar however small the window is.
    #[test]
    fn the_top_bar_slots_the_follow_toggle_at_its_right_edge() {
        let window = Rect::from_min_size(pos2(0.0, 0.0), vec2(800.0, 400.0));
        let layout = EditorLayout::from_theme(window);
        let bar = layout.bar_inner();
        let follow = layout.follow_button();
        let add = layout.add_lane_button();
        let rest = layout.bar_rest();

        assert!(layout.top_bar().contains_rect(bar));
        assert_eq!(follow.width(), theme::FOLLOW_WIDTH);
        assert_eq!(follow.max, bar.max, "the toggle is at the right end of the bar");
        assert_eq!(follow.y_range(), bar.y_range());

        assert_eq!(add.width(), theme::ADD_LANE_WIDTH);
        assert_eq!(add.y_range(), bar.y_range());
        assert_eq!(follow.min.x - add.max.x, theme::ITEM_SPACING, "one gap apart");

        assert!(rest.max.x <= add.min.x, "the rest of the bar stops short of them");
        assert_eq!(rest.min, bar.min);

        // A window too small for any of them still answers with rects inside
        // the bar rather than inverted ones.
        for size in [vec2(90.0, 20.0), vec2(40.0, 20.0), vec2(1.0, 1.0), vec2(0.0, 0.0)] {
            let tiny = EditorLayout::from_theme(Rect::from_min_size(pos2(0.0, 0.0), size));
            let slots = [
                tiny.bar_inner(),
                tiny.follow_button(),
                tiny.add_lane_button(),
                tiny.bar_rest(),
                tiny.pattern_bar(),
                tiny.pattern_button(0),
                tiny.pattern_button(PATTERN_BUTTONS - 1),
                tiny.pattern_button(PATTERN_BUTTONS),
                tiny.bar_status(),
            ];
            for rect in slots {
                assert!(rect.width() >= 0.0 && rect.height() >= 0.0, "{size:?} gave {rect:?}");
                assert!(tiny.top_bar().contains_rect(rect), "{size:?} gave {rect:?}");
            }
        }
    }

    /// The pattern bar: eight buttons in index order at the left end of the
    /// top bar, a gap apart, with the status line past their right edge.
    #[test]
    fn the_pattern_bar_fills_the_left_end_of_the_top_bar() {
        let window = Rect::from_min_size(pos2(0.0, 0.0), vec2(800.0, 400.0));
        let layout = EditorLayout::from_theme(window);
        let bar = layout.bar_inner();
        let rest = layout.bar_rest();
        let strip = layout.pattern_bar();

        assert_eq!(PATTERN_BUTTONS, 8, "one button per pattern");
        assert_eq!(strip.min, rest.min, "the strip starts where the bar's room does");
        assert_eq!(strip.y_range(), bar.y_range());

        let mut previous = layout.pattern_button(0);
        assert_eq!(previous.min, rest.min);
        for index in 0..PATTERN_BUTTONS {
            let slot = layout.pattern_button(index);
            assert_eq!(slot.width(), theme::PATTERN_WIDTH, "button {index}");
            assert_eq!(slot.y_range(), bar.y_range(), "button {index}");
            assert!(rest.contains_rect(slot), "button {index} is inside the bar's room");
            if index > 0 {
                assert_eq!(slot.min.x - previous.max.x, theme::ITEM_SPACING, "one gap apart");
            }
            previous = slot;
        }
        assert_eq!(previous.max.x, strip.max.x, "the strip ends with its last button");
        assert!(
            !layout.pattern_button(PATTERN_BUTTONS).is_positive(),
            "there is no ninth button to draw"
        );

        let status = layout.bar_status();
        assert_eq!(status.min.x - strip.max.x, theme::ITEM_SPACING, "one gap past the buttons");
        assert_eq!(status.max, rest.max);
        assert_eq!(status.y_range(), bar.y_range());

        // A bar with less room than the eight buttons want: all eight are
        // still there, narrower and still inside it, and the status line is
        // what gives way.
        let squeezed = Rect::from_min_size(pos2(0.0, 0.0), vec2(336.0, 40.0));
        let narrow = EditorLayout::from_theme(squeezed);
        let room = narrow.bar_rest();
        assert!(room.width() < theme::PATTERN_WIDTH * PATTERN_BUTTONS as f32, "squeezed");
        for index in 0..PATTERN_BUTTONS {
            let slot = narrow.pattern_button(index);
            assert!(slot.is_positive(), "button {index} still has room");
            assert!(slot.width() < theme::PATTERN_WIDTH, "button {index} gave way");
            assert!(room.contains_rect(slot), "button {index} is inside the bar's room");
        }
        assert!(room.contains_rect(narrow.pattern_bar()));
        assert!(!narrow.bar_status().is_positive(), "nothing is left for the status line");
    }

    /// Button index ↔ `patsel` value, both ways: the parameter is one-based
    /// and the buttons are not, and the playhead's pattern index is a third
    /// numbering again.
    #[test]
    fn pattern_buttons_map_to_the_parameters_one_based_values() {
        let min = i32::from(BlockCtx::PATTERN_SELECT_MIN);
        let max = i32::from(BlockCtx::PATTERN_SELECT_MAX);

        assert_eq!(pattern_button_value(0), min, "the first button selects the first pattern");
        assert_eq!(pattern_button_value(PATTERN_BUTTONS - 1), max);
        assert_eq!(pattern_button_value(PATTERN_BUTTONS), max, "saturates at the last button");
        assert_eq!(pattern_button_value(usize::MAX), max);

        for index in 0..PATTERN_BUTTONS {
            let value = pattern_button_value(index);
            assert!((min..=max).contains(&value), "button {index} writes {value}");
            assert_eq!(pattern_button_index(value), Some(index), "and {value} lights it again");
            // The pattern the value names, as `shown_pattern` works it out.
            assert_eq!(usize::try_from(value - min), Ok(index));
        }

        // Values the parameter cannot hold light nothing rather than the
        // nearest button.
        for value in [i32::MIN, -1, 0, max + 1, i32::MAX] {
            assert_eq!(pattern_button_index(value), None, "{value} is not a button");
        }

        // The playhead's own numbering: a pattern index, and its sentinel.
        for index in 0..PATTERN_BUTTONS {
            assert_eq!(pattern_index_button(index as u32), Some(index));
        }
        // `Playhead::NONE` is `u32::MAX`, so the sentinel is covered here
        // too — but the editor has already turned it into `None` by then.
        for index in [PATTERN_BUTTONS as u32, 99, crate::model::Playhead::NONE] {
            assert_eq!(pattern_index_button(index), None, "{index} lights no button");
        }
    }

    /// Follow mode: a step in view is left alone, one off either end is
    /// brought just inside with a margin, and the result is always a legal
    /// offset.
    #[test]
    fn follow_scrolls_the_playhead_into_view() {
        let m = test_metrics();
        let pitch = m.step_pitch();
        let margin = FOLLOW_MARGIN_STEPS * pitch;

        // Ten columns visible, parked at step 20.
        let v = view(vec2(20.0 * pitch, 0.0));
        assert_eq!(v.follow_scroll_x(24, MAX_STEPS), 20.0 * pitch, "step 24 is already in view");
        assert_eq!(v.follow_scroll_x(22, MAX_STEPS), 20.0 * pitch);

        // Past the right edge: scrolled so the step and its margin fit.
        let ahead = v.follow_scroll_x(31, MAX_STEPS);
        assert_eq!(ahead, m.step_x(31) + m.cell_size().x + margin - v.viewport().width());
        let after = GridView::new(m, v.viewport(), vec2(ahead, 0.0));
        assert!(after.visible_steps(MAX_STEPS).contains(&31), "step 31 is now drawn");

        // Behind the left edge: the step sits a margin in from the left.
        let behind = v.follow_scroll_x(19, MAX_STEPS);
        assert_eq!(behind, m.step_x(19) - margin);
        let before = GridView::new(m, v.viewport(), vec2(behind, 0.0));
        assert!(before.visible_steps(MAX_STEPS).contains(&19));

        // Step 0 cannot be scrolled left of the content, even with a margin.
        assert_eq!(v.follow_scroll_x(0, MAX_STEPS), 0.0);
        // A lane that fits on screen never scrolls at all.
        let short = view(Vec2::ZERO);
        for step in 0..8 {
            assert_eq!(short.follow_scroll_x(step, 8), 0.0, "step {step} of a short lane");
        }
        // The last step of the longest lane lands at the end of the strip.
        assert_eq!(
            short.follow_scroll_x(MAX_STEPS - 1, MAX_STEPS),
            short.max_scroll_x(MAX_STEPS)
        );
    }

    /// Follow mode holds for every step of a maximal lane, from any starting
    /// offset: the step it was asked to show is drawn, and the offset is one
    /// the scroll area would accept.
    #[test]
    fn follow_always_lands_on_a_visible_step() {
        let m = test_metrics();
        let viewport = Rect::from_min_size(pos2(300.0, 50.0), vec2(120.0, 120.0));

        for start in [0.0, 500.0, 12_345.6, 1e9, -1e9, f32::NAN] {
            let v = GridView::new(m, viewport, vec2(start, 0.0));
            let max = v.max_scroll_x(MAX_STEPS);
            for step in (0..MAX_STEPS).step_by(97) {
                let scroll = v.follow_scroll_x(step, MAX_STEPS);
                assert!(
                    (0.0..=max).contains(&scroll),
                    "step {step} from {start} gave an illegal offset {scroll}"
                );
                let moved = GridView::new(m, viewport, vec2(scroll, 0.0));
                assert!(
                    moved.visible_steps(MAX_STEPS).contains(&step),
                    "step {step} from {start} is still off screen at {scroll}"
                );
            }
        }
    }

    /// A viewport barely wider than one cell still shows the step it is
    /// following — the margin gives way rather than fighting the cell for the
    /// room.
    #[test]
    fn follow_survives_a_viewport_one_cell_wide() {
        let m = test_metrics();
        let narrow = Rect::from_min_size(pos2(0.0, 0.0), vec2(m.cell_size().x, 100.0));
        let v = GridView::new(m, narrow, Vec2::ZERO);

        for step in [0, 1, 5, MAX_STEPS - 1] {
            let scroll = v.follow_scroll_x(step, MAX_STEPS);
            let moved = GridView::new(m, narrow, vec2(scroll, 0.0));
            assert!(
                moved.visible_steps(MAX_STEPS).contains(&step),
                "step {step} is off screen in a one-cell viewport"
            );
        }
    }

    /// The scroll bounds are the content minus the viewport, on both axes,
    /// and an axis whose content fits does not scroll at all.
    #[test]
    fn scroll_bounds_follow_the_content() {
        let m = test_metrics();
        let v = view(Vec2::ZERO);
        // The viewport is ten columns and five rows of these metrics.
        assert_eq!(v.viewport().size(), vec2(10.0 * m.step_pitch(), 5.0 * m.row_pitch()));

        // Wider than the viewport: the bound leaves room for the end handle
        // of the longest lane, which sits past its last column.
        assert_eq!(v.max_scroll_x(20), m.step_x(20) + m.handle_width() - v.viewport().width());
        assert_eq!(v.max_scroll_y(10), m.row_y(10) - v.viewport().height());
        assert_eq!(v.max_scroll(20, 10), vec2(v.max_scroll_x(20), v.max_scroll_y(10)));

        // Content that fits — and an empty strip — parks at the origin.
        assert_eq!(v.max_scroll_x(9), 0.0);
        assert_eq!(v.max_scroll_y(5), 0.0);
        assert_eq!(v.max_scroll(0, 0), Vec2::ZERO);

        // Whatever the editor is holding comes back legal: inside the bounds
        // untouched, outside them at the nearest bound, and a non-finite
        // offset at the origin rather than off screen.
        let max = v.max_scroll(20, 10);
        assert_eq!(v.clamp_scroll(vec2(36.0, 48.0), 20, 10), vec2(36.0, 48.0));
        assert_eq!(v.clamp_scroll(vec2(-5.0, -5.0), 20, 10), Vec2::ZERO);
        assert_eq!(v.clamp_scroll(vec2(1e9, 1e9), 20, 10), max);
        assert_eq!(v.clamp_scroll(vec2(f32::NAN, f32::NEG_INFINITY), 20, 10), Vec2::ZERO);
        assert_eq!(v.clamp_scroll(vec2(f32::INFINITY, f32::NAN), 20, 10), vec2(max.x, 0.0));
        // A shortened lane or a deleted row drags the offset back with it.
        assert_eq!(v.clamp_scroll(max, 9, 5), Vec2::ZERO);
    }

    /// The end handle's drag: one step per column of travel, measured from
    /// the press, clamped to the lengths a lane may have.
    #[test]
    fn the_end_handle_drag_maps_dx_to_length() {
        let pitch = test_metrics().step_pitch();

        // Right lengthens, left shortens, and the step changes as the pointer
        // passes the middle of a column.
        assert_eq!(drag_length(16, 0.0, pitch), 16);
        assert_eq!(drag_length(16, pitch, pitch), 17);
        assert_eq!(drag_length(16, pitch * 0.49, pitch), 16);
        assert_eq!(drag_length(16, pitch * 0.5, pitch), 17);
        assert_eq!(drag_length(16, -pitch, pitch), 15);
        assert_eq!(drag_length(16, -pitch * 3.0, pitch), 13);

        // Both ends clamp, so a gesture that overshoots stops at a length the
        // model will take rather than wrapping.
        assert_eq!(drag_length(16, -1e6, pitch), LengthEdit::MIN);
        assert_eq!(drag_length(16, 1e9, pitch), LengthEdit::MAX);
        assert_eq!(drag_length(LengthEdit::MAX, pitch, pitch), LengthEdit::MAX);
        assert_eq!(drag_length(LengthEdit::MIN, -pitch, pitch), LengthEdit::MIN);

        // A length from outside the range (a future field widened, a hostile
        // snapshot) is clamped before the drag is added to it.
        assert_eq!(drag_length(0, 0.0, pitch), LengthEdit::MIN);
        assert_eq!(drag_length(u32::MAX, 0.0, pitch), LengthEdit::MAX);

        // Nothing egui can hand out moves the length to an end of the range:
        // a non-finite travel or pitch leaves it where it was.
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert_eq!(drag_length(16, bad, pitch), 16, "travel {bad}");
            assert_eq!(drag_length(16, pitch, bad), 16, "pitch {bad}");
        }
        for bad in [0.0, -pitch] {
            assert_eq!(drag_length(16, pitch, bad), 16, "pitch {bad}");
        }
    }

    /// The machine the editor drives: locked to its row, measured from its
    /// press, and quiet on a frame that did not move the length.
    #[test]
    fn the_end_handle_drag_reports_only_changes() {
        let v = view(Vec2::ZERO);
        let pitch = v.metrics().step_pitch();
        // Where the handle of a 16-step lane in row 2 actually is, so the
        // gesture starts exactly where a press on it would.
        let origin = v.end_handle_rect(2, 16).center();
        let mut drag = EndHandleDrag::begin(2, 16, origin.x, pitch);

        assert_eq!(drag.row(), 2);
        assert_eq!(drag.start_length(), 16);
        assert_eq!(drag.length_at(origin.x), 16);

        // The press itself owes no write, and neither does a wobble inside a
        // column or a frame that did not move at all.
        assert_eq!(drag.enter(origin.x), None, "the press writes nothing");
        assert_eq!(drag.enter(origin.x + pitch * 0.4), None, "a wobble is not a step");
        assert_eq!(drag.enter(origin.x + pitch), Some(17));
        assert_eq!(drag.enter(origin.x + pitch * 1.1), None, "still the same length");
        assert_eq!(drag.enter(origin.x + pitch * 2.0), Some(18));

        // Dragging back restores what the press found, and past either end it
        // stops at the length the model will take — once.
        assert_eq!(drag.enter(origin.x), Some(16), "the gesture measures from its press");
        assert_eq!(drag.enter(origin.x - 1e6), Some(LengthEdit::MIN));
        assert_eq!(drag.enter(origin.x - 1e9), None, "already as short as it goes");
        assert_eq!(drag.enter(origin.x + 1e9), Some(LengthEdit::MAX));
        assert_eq!(drag.enter(f32::NAN), Some(16), "a lost pointer position is no travel");

        // The row is the only one it can touch, whatever the pointer's height
        // does — the editor hands it `x` alone.
        assert_eq!(drag.row(), 2);
    }

    /// A step column is found from the pointer's `x` alone, so a gesture that
    /// drifts off its row keeps painting it.
    #[test]
    fn the_step_column_comes_from_x_alone() {
        let v = view(vec2(24.0, 0.0));
        let m = *v.metrics();
        let cell = v.cell_rect(1, 4);

        assert_eq!(v.step_at(cell.center()), Some(4));
        // Same x, every y: above the grid, in another row, below the window.
        assert_eq!(v.step_at(pos2(cell.center().x, v.origin().y - 500.0)), Some(4));
        assert_eq!(v.step_at(pos2(cell.center().x, v.cell_rect(0, 0).center().y)), Some(4));
        assert_eq!(v.step_at(pos2(cell.center().x, v.viewport().max.y + 500.0)), Some(4));
        // The gap after a cell is still that cell's column, as for `hit`.
        assert_eq!(v.step_at(pos2(cell.max.x + m.gap().x * 0.5, cell.center().y)), Some(4));

        // Unbounded on the right — the caller bounds it by the lane's length.
        assert_eq!(v.step_at(pos2(v.origin().x + m.step_x(900), 0.0)), Some(900));
        // Nothing left of step 0.
        assert_eq!(v.step_at(pos2(v.origin().x - 1.0, 0.0)), None);
    }

    /// The press toggles its cell, and what that cell became is what the rest
    /// of the gesture paints.
    #[test]
    fn a_press_latches_the_value_it_toggled_to() {
        let drawing = PaintDrag::begin(2, 5, false);
        assert_eq!(drawing.row(), 2);
        assert!(drawing.value(), "a press on an off step draws");
        assert_eq!(drawing.painted(), 5..=5, "the press's own cell is the first to write");

        let erasing = PaintDrag::begin(2, 5, true);
        assert!(!erasing.value(), "a press on an on step erases");

        // What the value means for a step, both ways round.
        let never_edited = Step::OFF;
        assert_eq!(drawing.apply(never_edited), Step::default_on(), "defaults filled in");
        let drawn = Step::default_on().with_velocity(42).with_ratchet(3).with_probability(60);
        assert!(!erasing.apply(drawn).is_on());
        assert_eq!(
            erasing.apply(drawn).with_on(true),
            drawn,
            "erasing keeps every other field, so drawing again restores the step"
        );
        assert_eq!(drawing.apply(erasing.apply(drawn)), drawn, "and it does");
    }

    /// Dragging sideways paints each newly entered cell exactly once, in
    /// either direction, and fills in cells the pointer skipped over.
    #[test]
    fn a_drag_paints_each_cell_it_enters_once() {
        let mut drag = PaintDrag::begin(0, 4, false);

        assert_eq!(drag.enter(5), 5..6, "the next column right");
        assert_eq!(drag.enter(5), 5..5, "still in the same column: nothing new");
        assert_eq!(drag.enter(4), 4..4, "back over the trail: nothing new");
        assert_eq!(drag.enter(9), 6..10, "a fast drag fills in what it flew over");
        assert_eq!(drag.painted(), 4..=9);

        // Reversing past the start extends the span the other way, and still
        // only reports what has not been written.
        assert_eq!(drag.enter(7), 7..7, "inside the span");
        assert_eq!(drag.enter(2), 2..4, "left of the start");
        assert_eq!(drag.enter(3), 3..3);
        assert_eq!(drag.painted(), 2..=9);
        assert!(drag.value(), "the value never changes mid-gesture");

        // Every step in the span was reported exactly once.
        let mut counted = PaintDrag::begin(0, 4, false);
        let mut writes: Vec<usize> = counted.painted().collect();
        for step in [5, 5, 4, 9, 7, 2, 3] {
            writes.extend(counted.enter(step));
        }
        writes.sort_unstable();
        assert_eq!(writes, (2..=9).collect::<Vec<_>>());
    }

    /// A step at the very top of the index space cannot overflow the span's
    /// arithmetic — `step_at` saturates, and the editor must not panic on a
    /// pointer position the window system handed it.
    #[test]
    fn a_drag_survives_a_saturated_step_index() {
        let mut drag = PaintDrag::begin(0, MAX_STEPS, false);
        assert_eq!(drag.enter(usize::MAX), MAX_STEPS + 1..usize::MAX);
        assert_eq!(drag.enter(usize::MAX), usize::MAX..usize::MAX);
        assert_eq!(drag.painted(), MAX_STEPS..=usize::MAX);
    }

    /// A press is a click until it has travelled [`DRAG_DEADZONE`], and then
    /// it is whichever gesture the hand committed more travel to.
    #[test]
    fn the_axis_is_decided_by_the_larger_travel() {
        // Inside the deadzone, in every direction.
        for delta in [Vec2::ZERO, vec2(2.9, 0.0), vec2(0.0, -2.9), vec2(-2.0, 2.0)] {
            assert_eq!(resolve_axis(delta), DragAxis::Undecided, "{delta:?} is still a click");
        }

        // Just past it, and well past it.
        assert_eq!(resolve_axis(vec2(3.0, 0.0)), DragAxis::Horizontal);
        assert_eq!(resolve_axis(vec2(-3.0, 0.0)), DragAxis::Horizontal);
        assert_eq!(resolve_axis(vec2(0.0, 3.0)), DragAxis::Vertical);
        assert_eq!(resolve_axis(vec2(0.0, -3.0)), DragAxis::Vertical);

        // The larger component wins however the other one got there, and a
        // tie goes to painting.
        assert_eq!(resolve_axis(vec2(20.0, -19.0)), DragAxis::Horizontal);
        assert_eq!(resolve_axis(vec2(-19.0, 20.0)), DragAxis::Vertical);
        assert_eq!(resolve_axis(vec2(20.0, 20.0)), DragAxis::Horizontal, "a tie paints");

        // A position the window system invented decides nothing.
        for delta in [vec2(f32::NAN, f32::NAN), vec2(f32::NAN, 0.0), vec2(0.0, f32::NAN)] {
            assert_eq!(resolve_axis(delta), DragAxis::Undecided, "{delta:?} decides nothing");
        }
        assert_eq!(resolve_axis(vec2(f32::INFINITY, 0.0)), DragAxis::Horizontal);
    }

    /// Dragging up raises the velocity and down lowers it, measured from the
    /// press and clamped to the MIDI range the sequencer can send.
    #[test]
    fn a_vertical_drag_maps_onto_the_velocity_range() {
        // Half the span is half the range, in both directions: 126 velocities
        // over 160 points.
        assert_eq!(drag_velocity(64, 0.0), 64, "a drag that has not moved changes nothing");
        assert_eq!(drag_velocity(64, -VELOCITY_DRAG_SPAN / 2.0), 127, "up is louder");
        assert_eq!(drag_velocity(64, VELOCITY_DRAG_SPAN / 2.0), 1, "down is quieter");
        assert_eq!(drag_velocity(64, -40.0), 96, "a quarter of the span is a quarter of it");
        // 64 - 31.5 rounds away from zero, as `f32::round` does.
        assert_eq!(drag_velocity(64, 40.0), 33);

        // Clamped at both ends, and never to the 0 that would be a note-off.
        assert_eq!(drag_velocity(64, -1_000.0), Step::VELOCITY_MAX);
        assert_eq!(drag_velocity(64, 1_000.0), Step::VELOCITY_MIN);
        assert_eq!(drag_velocity(1, 10.0), Step::VELOCITY_MIN);
        assert_eq!(drag_velocity(127, -10.0), Step::VELOCITY_MAX);

        // A start outside the range (a hostile snapshot's step reads back
        // clamped, but nothing here depends on that) is clamped first.
        assert_eq!(drag_velocity(0, 0.0), Step::VELOCITY_MIN);
        assert_eq!(drag_velocity(255, 0.0), Step::VELOCITY_MAX);

        // Monotonic over the whole screen's worth of travel.
        let mut previous = Step::VELOCITY_MAX;
        for points in -600..=600 {
            let velocity = drag_velocity(64, points as f32);
            assert!(velocity <= previous, "dy {points} went back up");
            assert!(
                (Step::VELOCITY_MIN..=Step::VELOCITY_MAX).contains(&velocity),
                "dy {points} left the range: {velocity}"
            );
            previous = velocity;
        }

        // And a non-finite drag leaves the step as the press found it.
        for dy in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert_eq!(drag_velocity(64, dy), 64, "{dy} is not a distance");
        }
    }

    /// The gesture measures from where the button went down, edits the one
    /// step it started on, and touches nothing else about it.
    #[test]
    fn a_velocity_drag_edits_one_step_from_its_origin() {
        let mut drag = VelocityDrag::begin(2, 5, 64, 300.0);
        assert_eq!(drag.row(), 2);
        assert_eq!(drag.step(), 5);
        assert_eq!(drag.start_velocity(), 64);

        // Screen heights, not deltas: the origin is 300.
        assert_eq!(drag.velocity_at(300.0), 64);
        assert_eq!(drag.velocity_at(260.0), 96);
        assert_eq!(drag.velocity_at(340.0), 33);

        // Returning to the origin restores the velocity even after the drag
        // has run into an end of the range — the gesture is absolute.
        assert_eq!(drag.velocity_at(-500.0), Step::VELOCITY_MAX);
        assert_eq!(drag.velocity_at(300.0), 64, "the start is not drifting");

        // A frame only asks for a write when the velocity actually moved —
        // including the first one, where the step already holds the start.
        assert_eq!(drag.enter(300.0), None, "the press's own velocity is already there");
        assert_eq!(drag.enter(299.5), None, "less than half a velocity of travel");
        assert_eq!(drag.enter(260.0), Some(96));
        assert_eq!(drag.enter(260.0), None, "the pointer did not move");
        assert_eq!(drag.enter(-500.0), Some(Step::VELOCITY_MAX));
        assert_eq!(drag.enter(-900.0), None, "already pinned at the top");
        assert_eq!(drag.enter(300.0), Some(64), "and back to where it started");

        // Every other field of the step survives a write.
        let drawn = Step::default_on().with_pan(10).with_ratchet(3).with_probability(60);
        let edited = drag.apply(drawn, 96);
        assert_eq!(edited.velocity(), 96);
        assert_eq!(edited.with_velocity(drawn.velocity()), drawn, "only the velocity moved");

        // A step switched off under the gesture keeps that, too.
        assert!(!drag.apply(drawn.with_on(false), 96).is_on());

        // The start is clamped on the way in, so a drag can begin on a step
        // whose bits came from somewhere else.
        drag = VelocityDrag::begin(0, 0, 0, 0.0);
        assert_eq!(drag.start_velocity(), Step::VELOCITY_MIN);
    }

    /// The three regions cover the window exactly once, and the pointer in
    /// each lands in the right one.
    #[test]
    fn the_layout_partitions_the_window() {
        let full = Rect::from_min_size(pos2(10.0, 20.0), vec2(1000.0, 500.0));
        let layout = EditorLayout::from_theme(full);

        assert_eq!(layout.full(), full);
        assert_eq!(layout.top_bar().height(), theme::TOP_BAR_HEIGHT);
        assert_eq!(layout.top_bar().width(), full.width());
        assert_eq!(layout.headers().width(), theme::HEADER_WIDTH);
        assert_eq!(layout.headers().max.y, full.max.y);
        assert_eq!(layout.steps().min.x, layout.headers().max.x);
        assert_eq!(layout.steps().max, full.max);
        assert_eq!(layout.steps().min.y, layout.top_bar().max.y);

        assert_eq!(layout.zone(full.min), Zone::TopBar);
        assert_eq!(layout.zone(layout.top_bar().center()), Zone::TopBar);
        assert_eq!(layout.zone(layout.headers().center()), Zone::LaneHeader);
        assert_eq!(layout.zone(layout.steps().center()), Zone::Steps);
        assert_eq!(layout.zone(pos2(full.min.x - 1.0, full.min.y)), Zone::Outside);
        assert_eq!(layout.zone(pos2(full.min.x, full.min.y - 1.0)), Zone::Outside);
        assert_eq!(layout.zone(full.max + vec2(1.0, 1.0)), Zone::Outside);

        // Boundaries: the first point below the bar is a header, the first
        // point right of the header is the strip.
        assert_eq!(layout.zone(pos2(full.min.x, layout.top_bar().max.y)), Zone::LaneHeader);
        assert_eq!(
            layout.zone(pos2(layout.headers().max.x, layout.steps().center().y)),
            Zone::Steps
        );

        // Every point of a lattice over the window belongs to exactly one
        // region, and no point outside it belongs to any.
        for i in 0..40 {
            for j in 0..20 {
                let pos = pos2(full.min.x + i as f32 * 25.0, full.min.y + j as f32 * 25.0);
                let zone = layout.zone(pos);
                let claims = [layout.top_bar(), layout.headers(), layout.steps()]
                    .iter()
                    .filter(|rect| {
                        pos.x >= rect.min.x
                            && pos.x < rect.max.x
                            && pos.y >= rect.min.y
                            && pos.y < rect.max.y
                    })
                    .count();
                assert_eq!(claims, 1, "{pos:?} is claimed {claims} times, as {zone:?}");
            }
        }
    }

    /// A window squeezed below the minimum, or inverted outright, still gets
    /// a split that holds together: no negative regions, no overlap.
    #[test]
    fn the_layout_survives_a_tiny_window() {
        let tiny = Rect::from_min_size(pos2(0.0, 0.0), vec2(40.0, 10.0));
        let layout = EditorLayout::from_theme(tiny);

        assert_eq!(layout.top_bar(), tiny, "the bar takes what height there is");
        assert_eq!(layout.headers().width(), tiny.width(), "the headers take the rest");
        assert_eq!(layout.headers().height(), 0.0);
        assert_eq!(layout.steps().width(), 0.0);
        assert_eq!(layout.zone(tiny.center()), Zone::TopBar);

        // An inverted rect is squared up rather than producing regions whose
        // `contains` is false for every point.
        let inverted = Rect::from_min_max(pos2(100.0, 100.0), pos2(0.0, 0.0));
        let layout = EditorLayout::from_theme(inverted);
        assert_eq!(layout.full(), Rect::from_min_size(pos2(100.0, 100.0), Vec2::ZERO));
        assert_eq!(layout.zone(pos2(100.0, 100.0)), Zone::Steps);
        assert_eq!(layout.zone(pos2(50.0, 50.0)), Zone::Outside);

        // The strip's view comes out of the layout, viewport and all.
        let view = layout.grid_view(CellMetrics::from_theme(), Vec2::ZERO);
        assert_eq!(view.viewport(), layout.steps());
        assert_eq!(view.origin(), layout.steps().min);
    }

    /// A lane header's band sits on its row's cells, so the two never drift
    /// apart whatever the scroll is.
    #[test]
    fn a_header_row_lines_up_with_its_cells() {
        let full = Rect::from_min_size(pos2(10.0, 20.0), vec2(1000.0, 500.0));
        let layout = EditorLayout::from_theme(full);
        let metrics = test_metrics();

        for scroll in [Vec2::ZERO, vec2(0.0, 36.0), vec2(250.0, 7.5)] {
            let view = layout.grid_view(metrics, scroll);
            for row in 0..6 {
                let band = layout.header_row(&view, row);
                let cell = view.cell_rect(row, 0);
                assert_eq!(band.y_range(), cell.y_range(), "row {row} at scroll {scroll:?}");
                assert_eq!(band.x_range(), layout.headers().x_range(), "row {row}");
                assert_eq!(band.height(), metrics.cell_size().y);
            }
        }
    }

    /// The nine fields run left to right in drawing order, inside the band,
    /// without overlapping — and a band too narrow for them scales them down
    /// together rather than spilling over the step strip.
    #[test]
    fn the_header_fields_tile_their_row() {
        let band = Rect::from_min_size(pos2(4.0, 60.0), vec2(theme::HEADER_WIDTH, 24.0));
        let fields = HeaderFields::split(band);

        let order = [
            fields.grip,
            fields.dot,
            fields.name,
            fields.key,
            fields.length,
            fields.rate,
            fields.volume,
            fields.mute,
            fields.solo,
        ];
        for (field, width) in order.iter().zip(HeaderFields::WIDTHS) {
            assert_eq!(field.width(), width, "a roomy band keeps the tuned widths");
            assert_eq!(field.y_range(), band.y_range(), "every field is row-height");
            assert!(band.contains_rect(*field), "{field:?} left the band");
        }
        for pair in order.windows(2) {
            assert_eq!(
                pair[1].min.x - pair[0].max.x,
                theme::HEADER_FIELD_GAP,
                "fields are one gap apart, in order"
            );
        }
        assert_eq!(fields.grip.min.x, band.min.x, "the grip starts at the left edge");
        assert_eq!(fields.mute.width(), fields.solo.width(), "the toggles are a pair");

        // The lane handle covers the three painted fields and stops before
        // the first control, so a drag on it is a reorder and a drag on the
        // key field is not (`HeaderFields::handle`).
        let handle = fields.handle();
        for field in [fields.grip, fields.dot, fields.name] {
            assert!(handle.contains_rect(field), "{field:?} is not in the handle");
        }
        assert_eq!(handle.min.x, fields.grip.min.x);
        assert_eq!(handle.max.x, fields.name.max.x);
        assert_eq!(handle.y_range(), band.y_range());
        assert!(handle.max.x < fields.key.min.x, "the handle stops before the key field");

        // The header the editor actually splits — the theme's column, inset
        // by its padding — has room for the fields at full size. Retuning one
        // width in `theme` without the others would otherwise push the mute
        // and solo toggles off the end of the header, and `split` would scale
        // them down silently rather than complain.
        let padded = band.shrink2(vec2(theme::PANEL_PADDING, 0.0));
        assert!(
            HeaderFields::needed_width() <= padded.width(),
            "the theme's fields do not fit its header: {} > {}",
            HeaderFields::needed_width(),
            padded.width()
        );
        assert!(HeaderFields::WIDTHS.iter().all(|&width| width > 0.0), "every field is drawable");

        // Half the width it wants: everything halves, nothing overlaps, and
        // the row still ends inside the band.
        let half = vec2(HeaderFields::needed_width() / 2.0, 24.0);
        let squeezed = Rect::from_min_size(band.min, half);
        let fields = HeaderFields::split(squeezed);
        assert_eq!(fields.dot.width(), theme::HEADER_DOT_WIDTH / 2.0);
        assert_eq!(fields.name.min.x - fields.dot.max.x, theme::HEADER_FIELD_GAP / 2.0);
        assert!(squeezed.contains_rect(fields.solo), "the last field left the band");

        // Nothing left at all: empty fields at the left edge rather than
        // negative rects drawn over the grid.
        for width in [0.0, -50.0, f32::NAN] {
            let none = Rect::from_min_size(band.min, vec2(width, 24.0));
            let fields = HeaderFields::split(none);
            assert_eq!(fields.grip.width(), 0.0, "width {width}");
            assert_eq!(fields.solo.min.x, band.min.x, "width {width}");
        }
    }

    /// The grip's dots: six of them, two columns of three, all inside the
    /// field — at a roomy size, at a cramped one, and at none at all.
    #[test]
    fn the_grip_dots_stay_inside_their_field() {
        let grip = Rect::from_min_size(pos2(6.0, 40.0), vec2(theme::HEADER_GRIP_WIDTH, 24.0));
        let dots = grip_dots(grip);

        // Allowed to touch the edge, never to cross it: a dot is drawn with a
        // radius around the point, so the point itself has to sit a radius in.
        let inset = grip.shrink(theme::GRIP_DOT_RADIUS);
        for dot in dots {
            assert!(inset.contains(dot), "{dot:?} is not inside {inset:?}");
        }

        let left: Vec<f32> = dots.iter().step_by(2).map(|dot| dot.x).collect();
        let right: Vec<f32> = dots.iter().skip(1).step_by(2).map(|dot| dot.y).collect();
        assert_eq!(left.len(), 3, "three dots in the left column");
        assert!(left.windows(2).all(|pair| pair[0] == pair[1]), "the column is straight");
        assert!(right.windows(2).all(|pair| pair[0] < pair[1]), "the rows run downwards");
        assert!(dots[0].x < dots[1].x, "two columns, left one first");
        assert_eq!(dots[2].y, grip.center().y, "the middle row is on the centre");

        // A field with no room collapses the dots onto its centre rather
        // than placing them outside it.
        for size in [vec2(1.0, 1.0), vec2(0.0, 0.0), vec2(f32::NAN, 10.0)] {
            let tiny = Rect::from_min_size(grip.min, size);
            for dot in grip_dots(tiny) {
                assert!(dot.x.is_finite() && dot.y.is_finite(), "{size:?} gave {dot:?}");
            }
        }
    }

    /// The channel field's two directions: the model's 0..=15 shown as
    /// 1..=16, and anything a drag value can hand back landed on a channel
    /// that exists.
    #[test]
    fn channels_are_shown_one_based_and_round_trip() {
        for channel in 0..=Lane::CHANNEL_MAX {
            let shown = channel_widget(channel);
            assert_eq!(shown, f64::from(channel) + 1.0, "channel {channel}");
            assert_eq!(widget_channel(shown), channel, "round trip of {channel}");
        }

        // The ends, and past them: the wire has 16 channels and no more.
        assert_eq!(widget_channel(1.0), 0);
        assert_eq!(widget_channel(16.0), Lane::CHANNEL_MAX);
        assert_eq!(widget_channel(0.0), 0);
        assert_eq!(widget_channel(-1e9), 0);
        assert_eq!(widget_channel(1e9), Lane::CHANNEL_MAX);
        assert_eq!(widget_channel(f64::NAN), 0, "NaN is the first channel, not the last");

        // Rounding, not truncation: a drag halfway between two channels
        // lands on the nearer one.
        assert_eq!(widget_channel(3.4), 2);
        assert_eq!(widget_channel(3.6), 3);

        // A stored channel outside the range (a snapshot from elsewhere) is
        // shown as one that exists rather than as 17.
        assert_eq!(channel_widget(200), channel_widget(Lane::CHANNEL_MAX));
    }

    /// A reorder drag: rounding to the nearest row, measuring from its own
    /// origin, clamped to the list, and quiet when nothing moved.
    #[test]
    fn a_reorder_drag_lands_on_the_nearest_row() {
        let pitch = test_metrics().row_pitch();

        // Half a row either way is still the row the lane came from.
        assert_eq!(drag_rows(2, 0.0, pitch, 6), 2);
        assert_eq!(drag_rows(2, pitch * 0.4, pitch, 6), 2);
        assert_eq!(drag_rows(2, -pitch * 0.4, pitch, 6), 2);
        // Past the halfway point it is the next one.
        assert_eq!(drag_rows(2, pitch * 0.6, pitch, 6), 3);
        assert_eq!(drag_rows(2, -pitch * 0.6, pitch, 6), 1);
        assert_eq!(drag_rows(2, pitch * 3.0, pitch, 6), 5);

        // Never off either end of the list.
        assert_eq!(drag_rows(2, pitch * 99.0, pitch, 6), 5);
        assert_eq!(drag_rows(2, -pitch * 99.0, pitch, 6), 0);
        assert_eq!(drag_rows(2, 1e30, pitch, 6), 5);
        assert_eq!(drag_rows(0, -1e30, pitch, 1), 0, "a one-lane list has one position");
        assert_eq!(drag_rows(0, pitch, pitch, 0), 0, "an empty list has no row to move to");
        assert_eq!(drag_rows(9, 0.0, pitch, 6), 5, "a stale row is held to the list");

        // Garbage from egui leaves the lane where it is.
        for (dy, pitch) in [(f32::NAN, pitch), (f32::INFINITY, pitch), (pitch, 0.0)] {
            assert_eq!(drag_rows(2, dy, pitch, 6), 2, "dy {dy} pitch {pitch}");
        }

        // The machine: absolute from the press, and only speaking up when the
        // position it wants has changed.
        let origin = 100.0;
        let mut drag = LaneReorder::begin(1, origin, pitch);
        assert_eq!(drag.from(), 1);
        assert_eq!(drag.target(), 1, "a drag that has not moved is still home");
        assert_eq!(drag.target_at(origin + pitch * 2.0, 6), 3);
        assert_eq!(drag.enter(origin + pitch * 0.1, 6), None, "no move, no edit");
        assert_eq!(drag.enter(origin + pitch * 2.0, 6), Some(3));
        assert_eq!(drag.enter(origin + pitch * 2.1, 6), None, "already there");
        assert_eq!(drag.enter(origin, 6), Some(1), "dragging back undoes the move");
        assert_eq!(drag.enter(origin - pitch * 5.0, 6), Some(0));
        assert_eq!(drag.enter(origin - pitch * 9.0, 6), None, "the top is the top");

        // The target tracks what was last handed out, which is what a cancel
        // needs in order to know whether there is anything to put back.
        assert_eq!(drag.target(), 0);
        assert_ne!(drag.target(), drag.from(), "this drag has moved the lane");
        assert_eq!(drag.enter(origin, 6), Some(1));
        assert_eq!(drag.target(), drag.from(), "and now it is home again");
    }

    /// Keys as the header shows them: the host's convention (36 is C1, 60 is
    /// C3), sharps only, every key named. 36, 38 and 42 are a kit's kick, snare
    /// and hat.
    #[test]
    fn keys_are_named_the_way_the_host_names_them() {
        assert_eq!(note_name(36), "C1", "a kit's kick");
        assert_eq!(note_name(38), "D1", "its snare");
        assert_eq!(note_name(42), "F#1", "its hat");
        assert_eq!(note_name(45), "A1", "its perc");
        assert_eq!(note_name(60), "C3", "middle C, as Bitwig names it");
        assert_eq!(note_name(0), "C-2", "the bottom of the range");
        assert_eq!(note_name(127), "G8", "and the top");
        // Out of range cannot reach a lane, and is named rather than panicking.
        assert_eq!(note_name(255), note_name(Lane::KEY_MAX));

        // Every key has a name, the names change every semitone, and the
        // octave ticks over at C.
        let mut seen = std::collections::HashSet::new();
        for key in 0..=Lane::KEY_MAX {
            let name = note_name(key);
            assert!(seen.insert(name.clone()), "key {key} is named {name} twice");
            let class = name.trim_end_matches(|c: char| c.is_ascii_digit() || c == '-');
            assert!(PITCH_CLASSES.contains(&class), "key {key} is named {name}");
            assert_eq!(class, PITCH_CLASSES[usize::from(key) % 12], "key {key}");
        }
        assert_eq!(seen.len(), 128);
    }

    /// Typed names come back as the key they name, generously spelled, and
    /// anything that is not a note is refused rather than clamped.
    #[test]
    fn note_names_round_trip_through_the_parser() {
        for key in 0..=Lane::KEY_MAX {
            assert_eq!(parse_note_name(&note_name(key)), Some(key), "key {key}");
        }

        // Case, whitespace and flats, none of which `note_name` emits.
        assert_eq!(parse_note_name("c1"), Some(36));
        assert_eq!(parse_note_name("  F#1 "), Some(42));
        assert_eq!(parse_note_name("f#1"), Some(42));
        assert_eq!(parse_note_name("Gb1"), Some(42), "a flat is the sharp below it");
        assert_eq!(parse_note_name("Bb2"), Some(58));
        assert_eq!(parse_note_name("bb2"), Some(58), "lower-case B flat");
        assert_eq!(parse_note_name("b2"), Some(59), "a lone b is the note");
        assert_eq!(parse_note_name("Cs1"), Some(37), "'s' for sharp");
        assert_eq!(parse_note_name("C-2"), Some(0), "a negative octave");
        assert_eq!(parse_note_name("C 1"), Some(36), "a space before the octave");

        // Outside the MIDI range: refused, so the field keeps the key it had.
        assert_eq!(parse_note_name("G#8"), None, "one semitone past 127");
        assert_eq!(parse_note_name("B-3"), None, "one octave below 0");
        assert_eq!(parse_note_name("Cb-2"), None, "flat off the bottom");
        assert_eq!(parse_note_name("C99999999999"), None, "no overflow, no panic");
        assert_eq!(parse_note_name("C-99999999999"), None);

        // Not notes at all.
        for text in ["", " ", "H1", "C", "C#", "1", "#1", "C1.5", "C1x", "Kick"] {
            assert_eq!(parse_note_name(text), None, "{text:?} is not a note name");
        }
    }

    /// The key field's own value reaches the lane as a key, whatever the
    /// widget's arithmetic made of it.
    #[test]
    fn a_widget_value_becomes_a_midi_key() {
        assert_eq!(widget_key(36.0), 36);
        assert_eq!(widget_key(36.4), 36, "rounds to the nearest semitone");
        assert_eq!(widget_key(36.6), 37);

        assert_eq!(widget_key(0.0), 0);
        assert_eq!(widget_key(f64::from(Lane::KEY_MAX)), Lane::KEY_MAX);
        assert_eq!(widget_key(-1.0), 0, "clamped at the bottom");
        assert_eq!(widget_key(1_000.0), Lane::KEY_MAX, "and at the top");

        // Non-finite values come from the widget, not the user. A NaN cast
        // straight to `u8` would be key 0 — a bass note nobody asked for.
        assert_eq!(widget_key(f64::NAN), 0);
        assert_eq!(widget_key(f64::INFINITY), Lane::KEY_MAX);
        assert_eq!(widget_key(f64::NEG_INFINITY), 0);
    }

    /// The key field's drag and scroll both move by semitones, and neither
    /// can run off either end of the MIDI range.
    #[test]
    fn nudging_a_key_stays_in_the_midi_range() {
        assert_eq!(nudge_key(60, 0), 60);
        assert_eq!(nudge_key(60, 12), 72);
        assert_eq!(nudge_key(60, -12), 48);

        assert_eq!(nudge_key(Lane::KEY_MAX, 1), Lane::KEY_MAX);
        assert_eq!(nudge_key(0, -1), 0);
        assert_eq!(nudge_key(60, 1_000), Lane::KEY_MAX);
        assert_eq!(nudge_key(60, -1_000), 0);
        assert_eq!(nudge_key(60, i32::MAX), Lane::KEY_MAX, "no overflow");
        assert_eq!(nudge_key(60, i32::MIN), 0);
    }

    /// Wheel scroll is banked until it is worth a semitone, and the remainder
    /// carries over so a slow wheel still moves one note per notch.
    #[test]
    fn banked_scroll_is_spent_a_semitone_at_a_time() {
        let pitch = KEY_SCROLL_PITCH;
        let mut bank = 0.0;

        // Less than a semitone's worth earns nothing and is kept.
        bank += pitch * 0.6;
        assert_eq!(take_scroll_steps(&mut bank, pitch), 0);
        assert!((bank - pitch * 0.6).abs() < 0.001, "the fraction was dropped: {bank}");

        // Another fraction tips it over; the overshoot stays banked.
        bank += pitch * 0.6;
        assert_eq!(take_scroll_steps(&mut bank, pitch), 1);
        assert!((bank - pitch * 0.2).abs() < 0.001, "the remainder is wrong: {bank}");

        // Scroll the other way, and the sign follows.
        bank -= pitch * 1.5;
        assert_eq!(take_scroll_steps(&mut bank, pitch), -1);

        // A flick worth several semitones spends them all at once.
        let mut bank = pitch * 5.5;
        assert_eq!(take_scroll_steps(&mut bank, pitch), 5);
        assert!((bank - pitch * 0.5).abs() < 0.001);

        // Nothing banked, nothing spent — the common frame.
        let mut bank = 0.0;
        assert_eq!(take_scroll_steps(&mut bank, pitch), 0);
        assert_eq!(bank, 0.0);

        // A bank or a pitch that is not a number is thrown away rather than
        // turned into a jump across the keyboard.
        for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let mut bank = value;
            assert_eq!(take_scroll_steps(&mut bank, pitch), 0, "bank {value}");
            assert_eq!(bank, 0.0, "bank {value} was kept");

            let mut bank = pitch * 3.0;
            assert_eq!(take_scroll_steps(&mut bank, value), 0, "pitch {value}");
        }
        let mut bank = pitch * 3.0;
        assert_eq!(take_scroll_steps(&mut bank, 0.0), 0, "a zero pitch divides by zero");

        // However much is banked, one frame cannot move more than the range.
        let mut bank = f32::MAX / 2.0;
        assert!(take_scroll_steps(&mut bank, pitch).abs() <= 128);
    }

    /// A wanted loop length resolves into the writes it takes: a value edit
    /// inside the capacity, a grow then a value edit past it.
    #[test]
    fn a_length_edit_grows_the_lane_only_when_it_has_to() {
        // Inside the capacity: nothing to grow.
        let edit = LengthEdit::resolve(12.0, 16);
        assert_eq!(edit.length(), 12);
        assert_eq!(edit.grow(), None);

        // Exactly the capacity is still inside it.
        let edit = LengthEdit::resolve(16.0, 16);
        assert_eq!(edit.length(), 16);
        assert_eq!(edit.grow(), None);

        // One past it: the lane is grown to the length asked for, and
        // `ensure_capacity` rounds that up to its own chunk.
        let edit = LengthEdit::resolve(17.0, 16);
        assert_eq!(edit.length(), 17);
        assert_eq!(edit.grow(), Some(17));

        // A lane with no capacity at all (a fresh `LaneData::empty`) grows
        // from nothing.
        let edit = LengthEdit::resolve(1.0, 0);
        assert_eq!(edit.length(), 1);
        assert_eq!(edit.grow(), Some(1));

        // The drag's own value is a float: it rounds to the nearest step.
        assert_eq!(LengthEdit::resolve(12.4, 64).length(), 12);
        assert_eq!(LengthEdit::resolve(12.6, 64).length(), 13);

        // Clamped at both ends, whatever the widget hands over.
        assert_eq!(LengthEdit::resolve(0.0, 64).length(), LengthEdit::MIN);
        assert_eq!(LengthEdit::resolve(-500.0, 64).length(), LengthEdit::MIN);
        assert_eq!(LengthEdit::resolve(-500.0, 64).grow(), None);
        assert_eq!(LengthEdit::resolve(1e9, 64).length(), LengthEdit::MAX);
        assert_eq!(LengthEdit::resolve(1e9, 64).grow(), Some(MAX_STEPS));
        assert_eq!(LengthEdit::MAX as usize, MAX_STEPS);

        // And at the capacity the data model itself tops out at.
        let edit = LengthEdit::resolve(f64::from(LengthEdit::MAX), MAX_STEPS);
        assert_eq!(edit.length(), LengthEdit::MAX);
        assert_eq!(edit.grow(), None, "a full-capacity lane never grows");

        // Non-finite values come from the widget's own arithmetic, not from
        // the user; they must not become a 4096-step lane or a panic.
        for wanted in [f64::NAN, f64::NEG_INFINITY] {
            assert_eq!(LengthEdit::resolve(wanted, 64).length(), LengthEdit::MIN, "{wanted}");
        }
        assert_eq!(LengthEdit::resolve(f64::INFINITY, 64).length(), LengthEdit::MAX);
    }

    /// Velocity fill: anchored to the bottom of the cell, as tall as the
    /// fraction says, and never outside the cell whatever it is handed.
    #[test]
    fn fill_rect_grows_from_the_bottom_of_the_cell() {
        let cell = Rect::from_min_size(pos2(300.0, 50.0), vec2(10.0, 20.0));

        let full = fill_rect(cell, 1.0);
        assert_eq!(full, cell, "a full fraction fills the cell exactly");

        let half = fill_rect(cell, 0.5);
        assert_eq!(half.height(), 10.0);
        assert_eq!(half.max, cell.max, "the fill sits on the cell's bottom edge");
        assert_eq!(half.min.x, cell.min.x);
        assert_eq!(half.width(), cell.width(), "the fill is as wide as the cell");

        let none = fill_rect(cell, 0.0);
        assert_eq!(none.height(), 0.0);
        assert_eq!(none.min, pos2(cell.min.x, cell.max.y));

        // Hostile fractions, including the NaN `f32::clamp` would pass on.
        for fraction in [-1.0, -0.0, f32::NAN, f32::NEG_INFINITY] {
            assert_eq!(fill_rect(cell, fraction).height(), 0.0, "fraction {fraction}");
        }
        for fraction in [1.0, 2.0, f32::INFINITY] {
            assert_eq!(fill_rect(cell, fraction), cell, "fraction {fraction}");
        }

        // The theme's own mapping stays inside the cell at both ends.
        assert!(cell.contains_rect(fill_rect(cell, theme::fill_fraction(Step::VELOCITY_MIN))));
        assert!(cell.contains_rect(fill_rect(cell, theme::fill_fraction(Step::VELOCITY_MAX))));
        assert!(
            fill_rect(cell, theme::fill_fraction(Step::VELOCITY_MIN)).height() > 0.0,
            "the quietest note must still be visible"
        );

        // A cell that is not a cell (an inverted or empty rect) yields an
        // empty fill rather than one that paints over the row above.
        let inverted = Rect::from_min_max(pos2(0.0, 20.0), pos2(10.0, 0.0));
        assert_eq!(fill_rect(inverted, 1.0).height(), 0.0);
    }

    /// The pan slider works in packed units, so every value a step can hold
    /// is reachable and nothing in between is.
    #[test]
    fn the_pan_slider_maps_one_detent_per_packed_value() {
        assert_eq!(PAN_WIDGET_MIN, f64::from(Step::PAN_MIN));
        assert_eq!(PAN_WIDGET_MAX, f64::from(Step::PAN_MAX));

        for pan in Step::PAN_MIN..=Step::PAN_MAX {
            assert_eq!(widget_pan(f64::from(pan)), pan, "pan {pan} is its own detent");
            // Through the field the slider is really editing.
            assert_eq!(Step::default_on().with_pan(pan).pan(), pan);
        }

        // Rounding to the nearer detent, both ways.
        assert_eq!(widget_pan(62.4), 62);
        assert_eq!(widget_pan(62.5), 63);
        assert_eq!(widget_pan(-0.4), 0);

        // Hostile values: the range clamps, and a NaN lands on the lower
        // bound rather than casting to 0 by accident at the other end.
        for value in [-1.0, -1000.0, f64::NEG_INFINITY, f64::NAN] {
            assert_eq!(widget_pan(value), Step::PAN_MIN, "value {value}");
        }
        for value in [127.0, 1e30, f64::INFINITY] {
            assert_eq!(widget_pan(value), Step::PAN_MAX, "value {value}");
        }
    }

    /// What the pan slider reads out is the `[-1, 1]` the host is handed,
    /// in percent — and it parses back to exactly the pan it came from.
    #[test]
    fn pan_labels_are_the_host_mapping_and_round_trip() {
        assert_eq!(pan_label(Step::PAN_CENTRE), "C");
        assert_eq!(pan_label(Step::PAN_MIN), "L100");
        assert_eq!(pan_label(Step::PAN_MAX), "R100");

        for pan in Step::PAN_MIN..=Step::PAN_MAX {
            let label = pan_label(pan);

            // The number printed is `pan_to_f32`'s, times a hundred — one
            // scale for the GUI and the wire, not two.
            let expected = (f64::from(pan_to_f32(pan)) * 100.0).round() as i32;
            let printed: i32 = label
                .trim_start_matches(['C', 'L', 'R'])
                .parse()
                .unwrap_or(0);
            assert_eq!(printed, expected.abs(), "pan {pan} printed as {label}");
            assert_eq!(label.starts_with('L'), expected < 0, "pan {pan}");
            assert_eq!(label.starts_with('R'), expected > 0, "pan {pan}");

            // And typing the label back in lands on the same pan.
            assert_eq!(parse_pan_label(&label), Some(pan), "label {label}");
        }

        // The forms a user types rather than the ones the slider prints.
        assert_eq!(parse_pan_label("  c  "), Some(Step::PAN_CENTRE));
        assert_eq!(parse_pan_label("0"), Some(Step::PAN_CENTRE));
        assert_eq!(parse_pan_label("l50"), parse_pan_label("L50"));
        assert_eq!(parse_pan_label("-50"), parse_pan_label("L50"));
        assert_eq!(parse_pan_label("50"), parse_pan_label("R50"));
        assert_eq!(parse_pan_label("R 25"), parse_pan_label("R25"));

        // A percentage past the ends is a pan at the ends, not a wrap.
        assert_eq!(parse_pan_label("L400"), Some(Step::PAN_MIN));
        assert_eq!(parse_pan_label("R400"), Some(Step::PAN_MAX));

        // Nothing a number can be read out of leaves the value alone.
        for text in ["", "   ", "centre", "C5", "L", "Lx", "pan", "inf", "NaN"] {
            assert_eq!(parse_pan_label(text), None, "text {text:?}");
        }

        // The percentage scale's own ends and centre.
        assert_eq!(pan_from_percent(0.0), Step::PAN_CENTRE);
        assert_eq!(pan_from_percent(-100.0), Step::PAN_MIN);
        assert_eq!(pan_from_percent(100.0), Step::PAN_MAX);
        assert_eq!(pan_from_percent(f64::NAN), Step::PAN_MIN);
    }

    /// The ratchet slider is in effective counts, which is the only form
    /// `Step` speaks — the `n - 1` packing never reaches the GUI.
    #[test]
    fn the_ratchet_slider_is_the_effective_count() {
        assert_eq!(RATCHET_WIDGET_MIN, f64::from(Step::RATCHET_MIN));
        assert_eq!(RATCHET_WIDGET_MAX, f64::from(Step::RATCHET_MAX));

        for ratchet in Step::RATCHET_MIN..=Step::RATCHET_MAX {
            assert_eq!(widget_ratchet(f64::from(ratchet)), ratchet);
            assert_eq!(Step::default_on().with_ratchet(ratchet).ratchet(), ratchet);
        }

        assert_eq!(widget_ratchet(2.5), 3);
        assert_eq!(widget_ratchet(2.4), 2);

        // Below the bottom is 1, never the 0 that `Step::with_ratchet` would
        // subtract one from.
        for value in [1.0, 0.0, -8.0, f64::NEG_INFINITY, f64::NAN] {
            assert_eq!(widget_ratchet(value), Step::RATCHET_MIN, "value {value}");
        }
        for value in [8.0, 99.0, f64::INFINITY] {
            assert_eq!(widget_ratchet(value), Step::RATCHET_MAX, "value {value}");
        }
    }

    /// The probability slider is the stored percentage, zero included.
    #[test]
    fn the_probability_slider_is_a_percentage_down_to_zero() {
        assert_eq!(PROBABILITY_WIDGET_MIN, 0.0);
        assert_eq!(PROBABILITY_WIDGET_MAX, f64::from(Step::PROBABILITY_MAX));

        for chance in 0..=Step::PROBABILITY_MAX {
            assert_eq!(widget_probability(f64::from(chance)), chance);
            assert_eq!(Step::default_on().with_probability(chance).probability(), chance);
        }

        assert_eq!(widget_probability(49.5), 50);
        assert_eq!(widget_probability(49.4), 49);

        for value in [0.0, -1.0, f64::NEG_INFINITY, f64::NAN] {
            assert_eq!(widget_probability(value), 0, "value {value}");
        }
        for value in [100.0, 1e9, f64::INFINITY] {
            assert_eq!(widget_probability(value), Step::PROBABILITY_MAX, "value {value}");
        }
    }

    /// The three sliders write three independent fields: moving one leaves
    /// the other two — and the velocity and on bit — exactly as they were.
    #[test]
    fn the_popover_fields_do_not_disturb_each_other() {
        let step = Step::default_on()
            .with_pan(widget_pan(10.0))
            .with_ratchet(widget_ratchet(4.0))
            .with_probability(widget_probability(70.0));

        assert!(step.is_on());
        assert_eq!(step.velocity(), Step::default_on().velocity());
        assert_eq!(step.pan(), 10);
        assert_eq!(step.ratchet(), 4);
        assert_eq!(step.probability(), 70);

        // One field at a time, as a slider moves it.
        let panned = step.with_pan(widget_pan(120.0));
        assert_eq!((panned.pan(), panned.ratchet(), panned.probability()), (120, 4, 70));
        let ratcheted = step.with_ratchet(widget_ratchet(8.0));
        assert_eq!((ratcheted.pan(), ratcheted.ratchet(), ratcheted.probability()), (10, 8, 70));
        let chanced = step.with_probability(widget_probability(0.0));
        assert_eq!((chanced.pan(), chanced.ratchet(), chanced.probability()), (10, 4, 0));
    }

    /// Ratchet ticks: one per sub-hit, evenly spread across the top of the
    /// cell, inside it, and none at all for the default count of 1.
    #[test]
    fn ratchet_ticks_mark_the_count_inside_the_cell() {
        let cell = Rect::from_min_size(pos2(300.0, 50.0), vec2(22.0, 24.0));

        // A ratchet of 1 is every step's default — nothing is drawn.
        assert_eq!(ratchet_ticks(cell, 1).count(), 0);
        // Including when a hostile value clamps to it.
        assert_eq!(ratchet_ticks(cell, 0).count(), 0);

        for ratchet in 2..=Step::RATCHET_MAX {
            let ticks: Vec<Rect> = ratchet_ticks(cell, ratchet).collect();
            assert_eq!(ticks.len(), usize::from(ratchet), "ratchet {ratchet}");

            for tick in &ticks {
                assert!(cell.contains_rect(*tick), "ratchet {ratchet}: {tick:?} escapes the cell");
                assert_eq!(tick.min.y, cell.min.y, "ticks hang from the top edge");
                assert!(tick.height() > 0.0 && tick.width() > 0.0);
            }

            // Evenly spaced: every gap is the same slice of the cell's width.
            let pitch = cell.width() / f32::from(ratchet);
            for pair in ticks.windows(2) {
                assert!((pair[1].min.x - pair[0].min.x - pitch).abs() < 1e-3);
            }
            // And the row is centred: the clear space before the first tick
            // matches the space after the last.
            let before = ticks[0].min.x - cell.min.x;
            let after = cell.max.x - ticks[ticks.len() - 1].max.x;
            assert!((before - after).abs() < 1e-3, "ratchet {ratchet}: {before} vs {after}");
        }

        // Above the field's cap, the ticks stop at the cap rather than
        // walking out of the cell.
        assert_eq!(ratchet_ticks(cell, u8::MAX).count(), usize::from(Step::RATCHET_MAX));

        // A cell too narrow for the theme's tick width gets thinner ticks
        // rather than overlapping ones.
        let narrow = Rect::from_min_size(pos2(0.0, 0.0), vec2(4.0, 2.0));
        for tick in ratchet_ticks(narrow, Step::RATCHET_MAX) {
            assert!(narrow.contains_rect(tick), "{tick:?} escapes a narrow cell");
        }

        // A cell that is not a cell yields marks of no size rather than
        // infinite ones painted across the grid.
        let inverted = Rect::from_min_max(pos2(10.0, 20.0), pos2(0.0, 0.0));
        for tick in ratchet_ticks(inverted, 4) {
            assert_eq!((tick.width(), tick.height()), (0.0, 0.0));
        }
        let nan = Rect::from_min_size(pos2(0.0, 0.0), Vec2::splat(f32::NAN));
        for tick in ratchet_ticks(nan, 4) {
            assert_eq!((tick.width(), tick.height()), (0.0, 0.0));
            assert_eq!(tick.min, nan.min, "a NaN size may not move the ticks off the origin");
        }

        // `size_hint` is the count, so a caller can size a buffer from it.
        let ticks = ratchet_ticks(cell, 5);
        assert_eq!(ticks.size_hint(), (5, Some(5)));
        assert_eq!(ticks.len(), 5);
    }

    /// The probability dot sits in the cell's bottom-right corner, clear of
    /// both edges and of the ratchet ticks along the top.
    #[test]
    fn the_probability_dot_sits_in_the_corner_of_the_cell() {
        let cell = Rect::from_min_size(pos2(300.0, 50.0), vec2(22.0, 24.0));
        let dot = probability_dot(cell);

        let radius = theme::MARKER_DOT_RADIUS;
        let bounds = Rect::from_center_size(dot, Vec2::splat(radius * 2.0));
        assert!(cell.contains_rect(bounds), "the whole dot is inside the cell");
        // Bottom-right: past the centre on both axes.
        assert!(dot.x > cell.center().x && dot.y > cell.center().y);
        // Clear of the ticks, which hang from the top edge.
        let deepest =
            ratchet_ticks(cell, Step::RATCHET_MAX).map(|tick| tick.max.y).fold(0.0, f32::max);
        assert!(dot.y - radius > deepest, "the dot and the ticks do not overlap");

        // A cell smaller than the inset keeps the dot inside it anyway.
        for size in [vec2(6.0, 6.0), vec2(1.0, 1.0), vec2(0.0, 0.0)] {
            let small = Rect::from_min_size(pos2(0.0, 0.0), size);
            let dot = probability_dot(small);
            assert!(small.contains(dot), "size {size:?} put the dot at {dot:?}");
        }
    }

    /// The playhead is drawn only when the atomics and the grid on screen
    /// agree: transport running, same pattern, step inside the lane.
    #[test]
    fn the_playhead_cell_needs_the_pattern_on_screen() {
        // Playing pattern 2 (zero-based), lane at step 5 of 16.
        assert_eq!(playhead_cell(2, Some(2), Some(5), 16), Some(5));
        assert_eq!(playhead_cell(0, Some(0), Some(0), 1), Some(0));

        // Stopped: nothing lights up.
        assert_eq!(playhead_cell(2, None, Some(5), 16), None);

        // Another pattern is playing, or being edited — the grid on screen is
        // not the one the step index belongs to.
        assert_eq!(playhead_cell(0, Some(2), Some(5), 16), None);
        assert_eq!(playhead_cell(7, Some(2), Some(5), 16), None);

        // The lane itself has no position (removed, or never scanned).
        assert_eq!(playhead_cell(2, Some(2), None, 16), None);

        // A step index from before a shape edit shortened the lane, and the
        // `Playhead::NONE` sentinel arriving as a raw value.
        assert_eq!(playhead_cell(2, Some(2), Some(16), 16), None);
        assert_eq!(playhead_cell(2, Some(2), Some(MAX_STEPS as u32), 16), None);
        assert_eq!(playhead_cell(2, Some(2), Some(u32::MAX), 16), None);
        assert_eq!(playhead_cell(2, Some(2), Some(0), 0), None, "a lane with no steps");

        // The last step of a lane is inside it; the one past it is not.
        assert_eq!(playhead_cell(0, Some(0), Some(MAX_STEPS as u32 - 1), MAX_STEPS as u32),
            Some(MAX_STEPS - 1));
    }

    // ---- pitch ----

    #[test]
    fn drag_pitch_goes_up_with_the_pointer_and_clamps() {
        assert_eq!(drag_pitch(0, -PITCH_DRAW_POINTS * 3.0), 3);
        assert_eq!(drag_pitch(0, PITCH_DRAW_POINTS * 2.0), -2);
        assert_eq!(drag_pitch(10, -1_000.0), Step::PITCH_MAX);
        assert_eq!(drag_pitch(-10, 1_000.0), Step::PITCH_MIN);
        assert_eq!(drag_pitch(7, f32::NAN), 7, "an unmeasurable pointer keeps the start");
    }

    #[test]
    fn pitch_line_is_an_even_slope_in_either_direction() {
        assert_eq!(pitch_line(2, 0, 6, 8), vec![(2, 0), (3, 2), (4, 4), (5, 6), (6, 8)]);
        assert_eq!(pitch_line(6, 8, 2, 0), vec![(6, 8), (5, 6), (4, 4), (3, 2), (2, 0)]);
        assert_eq!(pitch_line(3, -4, 3, 12), vec![(3, 12)], "one step takes the end value");
    }

    #[test]
    fn freehand_pitch_drawing_fills_the_steps_it_skips() {
        let origin = pos2(0.0, 100.0);
        let mut draw = PitchDraw::begin(0, 1, 0, origin);
        let flat = |_: usize| 0;
        // Inside the deadzone: still a click.
        assert!(draw.draw_to(1, origin + vec2(1.0, 0.0), origin, false, flat).is_empty());
        assert!(!draw.moved());
        // Up four semitones on the pressed step...
        let up = pos2(0.0, 100.0 - 4.0 * PITCH_DRAW_POINTS);
        assert_eq!(draw.draw_to(1, up, origin, false, flat), vec![(1, 4)]);
        // ...then a jump to step 4 in one frame, two semitones higher: the
        // skipped steps are filled in along the way.
        let higher = pos2(0.0, 100.0 - 6.0 * PITCH_DRAW_POINTS);
        assert_eq!(draw.draw_to(4, higher, origin, false, flat), vec![(2, 5), (3, 5), (4, 6)]);
        assert!(draw.moved());
    }

    #[test]
    fn a_shift_line_that_gets_shorter_restores_what_it_drew_over() {
        let origin = pos2(0.0, 100.0);
        let mut draw = PitchDraw::begin(0, 0, 0, origin);
        let before = |step: usize| step as i8; // pitches the lane had
        // Level with the press, but well to the right of it: out of the
        // click deadzone.
        let level = pos2(80.0, 100.0);
        let long = draw.draw_to(4, level, origin, true, before);
        assert_eq!(long, vec![(0, 0), (1, 0), (2, 0), (3, 0), (4, 0)]);
        let short = draw.draw_to(2, level, origin, true, before);
        assert_eq!(short, vec![(3, 3), (4, 4), (0, 0), (1, 0), (2, 0)], "3 and 4 go back");
    }

    #[test]
    fn the_pitch_bar_grows_up_for_higher_and_down_for_lower() {
        let cell = Rect::from_min_size(pos2(0.0, 0.0), vec2(22.0, 24.0));
        assert_eq!(pitch_bar(cell, 0), None);
        let up = pitch_bar(cell, Step::PITCH_MAX).expect("a bar");
        assert_eq!((up.min.y, up.max.y), (0.0, 12.0), "full height above the middle");
        let down = pitch_bar(cell, -6).expect("a bar");
        assert_eq!((down.min.y, down.max.y), (12.0, 18.0), "half of the lower half");
        assert!(pitch_bar(cell, 1).expect("a bar").height() >= 1.5, "±1 still shows");
        assert!(pitch_marker(cell, 3).expect("a mark").width() <= theme::PITCH_MARKER_WIDTH);
    }

    #[test]
    fn lane_volume_labels_parse_back_and_clamp() {
        assert_eq!(volume_label(0), "0 dB");
        assert_eq!(volume_label(6), "+6 dB");
        assert_eq!(volume_label(-12), "-12 dB");
        for db in Lane::VOLUME_MIN..=Lane::VOLUME_MAX {
            assert_eq!(parse_volume(&volume_label(db)), Some(db), "{db}");
        }
        assert_eq!(parse_volume("-3"), Some(-3));
        assert_eq!(parse_volume(" 4db "), Some(4));
        assert_eq!(parse_volume("+99 dB"), Some(Lane::VOLUME_MAX));
        assert_eq!(parse_volume("loud"), None);
        assert_eq!(parse_volume("nan"), None);
        assert_eq!(widget_volume(f64::NAN), Lane::VOLUME_MIN);
        assert_eq!(widget_volume(2.4), 2);
    }

    #[test]
    fn pitch_widgets_round_trip_and_label_with_a_sign() {
        assert_eq!(widget_pitch(5.4), 5);
        assert_eq!(widget_pitch(-99.0), Step::PITCH_MIN);
        assert_eq!(widget_pitch(f64::NAN), Step::PITCH_MIN);
        assert_eq!(pitch_label(5), "+5");
        assert_eq!(pitch_label(0), "0");
        assert_eq!(pitch_label(-3), "-3");
        assert_eq!(widget_velocity(0.0), Step::VELOCITY_MIN);
        assert_eq!(widget_velocity(126.6), 127);
    }

    #[test]
    fn the_mode_buttons_sit_between_the_patterns_and_the_status_line() {
        let layout = EditorLayout::from_theme(Rect::from_min_size(Pos2::ZERO, vec2(1000.0, 520.0)));
        let (steps, pitch) = (layout.mode_button(0), layout.mode_button(1));
        assert!(steps.is_positive() && pitch.is_positive());
        assert!(steps.min.x >= layout.pattern_bar().max.x, "right of the patterns");
        assert!(pitch.min.x > steps.max.x, "in order");
        assert!(layout.status_line().min.x > pitch.max.x, "the status line follows them");
        assert!(!layout.mode_button(MODE_BUTTONS).is_positive(), "no third button");
    }

    /// Each parameter's value ↔ height mapping: the bottom of the plot is the
    /// lowest value, the top the highest, and the two directions agree.
    #[test]
    fn param_heights_map_to_values_and_back() {
        let plot = Rect::from_min_max(pos2(0.0, 100.0), pos2(30.0, 227.0));
        for kind in ParamKind::ALL {
            let (min, max) = kind.range();
            assert_eq!(kind.value_at(plot, plot.max.y), min, "{kind:?} bottom");
            assert_eq!(kind.value_at(plot, plot.min.y), max, "{kind:?} top");
            assert_eq!(kind.value_at(plot, plot.max.y + 50.0), min, "{kind:?} below");
            assert_eq!(kind.value_at(plot, plot.min.y - 50.0), max, "{kind:?} above");
            assert_eq!(kind.value_at(plot, f32::NAN), min, "{kind:?} NaN");
            for value in min..=max {
                let y = kind.y_of(plot, value);
                assert_eq!(kind.value_at(plot, y), value, "{kind:?} {value}");
            }
            let flat = Rect::from_min_max(pos2(0.0, 5.0), pos2(30.0, 5.0));
            assert_eq!(kind.value_at(flat, 5.0), kind.default_value(), "{kind:?} no height");
        }
        // Pan and pitch are centred in the panel.
        let mid = plot.center().y;
        assert_eq!(ParamKind::Pitch.value_at(plot, mid), 0);
        assert_eq!(ParamKind::Pan.value_at(plot, mid), i32::from(Step::PAN_CENTRE));
    }

    /// Reading and writing a parameter touches only its own field, and
    /// writes are clamped into range.
    #[test]
    fn params_read_and_write_their_own_field() {
        let base = Step::default_on().with_pitch(3);
        for kind in ParamKind::ALL {
            let (min, max) = kind.range();
            for value in [min, max, (min + max) / 2] {
                let written = kind.write(base, value);
                assert_eq!(kind.read(written), value, "{kind:?} {value}");
                for other in ParamKind::ALL.into_iter().filter(|k| *k != kind) {
                    assert_eq!(other.read(written), other.read(base), "{kind:?} moved {other:?}");
                }
                assert!(written.is_on());
            }
            assert_eq!(kind.read(kind.write(base, max + 100)), max, "{kind:?} clamps high");
            assert_eq!(kind.read(kind.write(base, min - 100)), min, "{kind:?} clamps low");
        }
        assert_eq!(ParamKind::Velocity.default_value(), 100);
        assert_eq!(ParamKind::Chance.default_value(), 100);
        assert_eq!(ParamKind::Ratchet.default_value(), 1);
        assert_eq!(ParamKind::Pitch.default_value(), 0);
        // An empty step reads as its defaults, and a value written on it
        // leaves it off with the defaults under it.
        assert_eq!(ParamKind::Velocity.read(Step::OFF), 100);
        let drawn = ParamKind::Velocity.write(Step::OFF, 40);
        assert!(!drawn.is_on());
        assert_eq!(drawn.velocity(), 40);
        assert_eq!(drawn.probability(), Step::PROBABILITY_MAX);
        assert_eq!(drawn.turned_on().velocity(), 40, "kept when switched on");
    }

    #[test]
    fn param_values_read_as_the_panel_shows_them() {
        assert_eq!(ParamKind::Velocity.value_label(100), "100");
        assert_eq!(ParamKind::Pan.value_label(i32::from(Step::PAN_CENTRE)), "C");
        assert_eq!(ParamKind::Pitch.value_label(5), "+5");
        assert_eq!(ParamKind::Pitch.value_label(-3), "-3");
        assert_eq!(ParamKind::Chance.value_label(50), "50%");
        assert_eq!(ParamKind::Ratchet.value_label(3), "3x");
    }

    /// One-sided bars grow from the bottom and never vanish; pan and pitch
    /// grow up or down from the middle.
    #[test]
    fn param_bars_grow_from_the_bottom_or_the_centre() {
        let plot = Rect::from_min_max(pos2(0.0, 0.0), pos2(30.0, 126.0));
        let x = egui::Rangef::new(2.0, 28.0);

        let full = ParamKind::Velocity.bar(plot, x, 127);
        assert_eq!((full.min.y, full.max.y), (0.0, 126.0));
        let quiet = ParamKind::Velocity.bar(plot, x, 1);
        assert_eq!(quiet.max.y, 126.0);
        assert!(quiet.height() >= PARAM_MIN_BAR, "velocity 1 is still drawn");
        assert_eq!(quiet.x_range(), x);

        let up = ParamKind::Pitch.bar(plot, x, 12);
        assert_eq!((up.min.y, up.max.y), (0.0, 63.0));
        let down = ParamKind::Pitch.bar(plot, x, -6);
        assert_eq!((down.min.y, down.max.y), (63.0, 94.5));
        assert_eq!(ParamKind::Pitch.bar(plot, x, 0).height(), 0.0);
    }

    #[test]
    fn level_line_matches_pitch_line() {
        assert_eq!(level_line(2, 0, 6, 8), [(2, 0), (3, 2), (4, 4), (5, 6), (6, 8)]);
        assert_eq!(level_line(4, 10, 2, 0), [(4, 10), (3, 5), (2, 0)]);
        assert_eq!(level_line(3, 7, 3, 9), [(3, 9)]);
        for (a, b) in [(-24i8, 24i8), (5, -7), (0, 1)] {
            let pitch: Vec<(usize, i32)> =
                pitch_line(0, a, 9, b).into_iter().map(|(s, p)| (s, i32::from(p))).collect();
            assert_eq!(pitch, level_line(0, i32::from(a), 9, i32::from(b)));
        }
    }

    /// Freehand drawing fills skipped steps; a Shift line runs from the press
    /// and gives back steps a longer line had covered.
    #[test]
    fn level_draw_fills_gaps_and_draws_straight_lines() {
        let values = [50, 50, 50, 50, 50, 50, 50, 50];
        let current = |s: usize| values[s];

        let mut draw = LevelDraw::begin(1, 100);
        assert_eq!(draw.origin_step(), 1);
        assert_eq!(draw.draw_to(1, 100, false, current), [(1, 100)], "the press");
        assert_eq!(draw.draw_to(4, 40, false, current), [(2, 80), (3, 60), (4, 40)]);

        let mut line = LevelDraw::begin(0, 0);
        line.draw_to(0, 0, true, current);
        assert_eq!(line.draw_to(6, 60, true, current).len(), 7);
        let shorter = line.draw_to(3, 30, true, current);
        for s in 4..=6 {
            assert!(shorter.contains(&(s, 50)), "step {s} goes back to what it had");
        }
        assert!(shorter.ends_with(&[(0, 0), (1, 10), (2, 20), (3, 30)]));
    }

    /// The parameter panel takes its height off the bottom of the headers and
    /// the strip, lines up with them, and is its own zone.
    #[test]
    fn the_parameter_panel_sits_under_the_grid() {
        let window = Rect::from_min_size(pos2(0.0, 0.0), vec2(1000.0, 600.0));
        let plain = EditorLayout::from_theme(window);
        assert!(!plain.panel().is_positive(), "no panel unless asked for");
        assert_eq!(plain.steps().max.y, 600.0);

        let layout = plain.with_panel(140.0);
        assert_eq!(layout.panel_strip().height(), 140.0);
        assert_eq!(layout.steps().max.y, 460.0);
        assert_eq!(layout.headers().max.y, 460.0);
        assert_eq!(layout.panel_strip().x_range(), layout.steps().x_range());
        assert_eq!(layout.panel_header().x_range(), layout.headers().x_range());
        assert_eq!(layout.zone(layout.panel_strip().center()), Zone::Panel);
        assert_eq!(layout.zone(layout.panel_header().center()), Zone::Panel);
        assert_eq!(layout.zone(layout.steps().center()), Zone::Steps);

        // Columns line up with the step strip's at any scroll.
        let metrics = CellMetrics::from_theme();
        let grid = layout.grid_view(metrics, vec2(37.0, 50.0));
        let panel = layout.panel_view(metrics, 37.0);
        for step in [0, 3, 10] {
            assert_eq!(panel.cell_rect(0, step).x_range(), grid.cell_rect(0, step).x_range());
        }

        // A short window keeps half of itself for the grid.
        let short = EditorLayout::from_theme(Rect::from_min_size(Pos2::ZERO, vec2(800.0, 200.0)))
            .with_panel(140.0);
        let below_bar = 200.0 - theme::TOP_BAR_HEIGHT;
        assert_eq!(short.panel_strip().height(), below_bar * PANEL_MAX_SHARE);
    }

    /// The panel toggle sits left of `+ Lane`, one gap apart, and the
    /// pattern buttons' room ends before it.
    #[test]
    fn the_panel_toggle_sits_left_of_add_lane() {
        let layout = EditorLayout::from_theme(Rect::from_min_size(Pos2::ZERO, vec2(1000.0, 500.0)));
        let toggle = layout.panel_button();
        let add = layout.add_lane_button();
        assert_eq!(toggle.width(), theme::PANEL_TOGGLE_WIDTH);
        assert_eq!(add.min.x - toggle.max.x, theme::ITEM_SPACING);
        assert_eq!(toggle.y_range(), add.y_range());
        assert_eq!(toggle.min.x - layout.bar_rest().max.x, theme::ITEM_SPACING);
    }

    /// The steps a fill turns on in the first `count` steps at `rate`.
    fn fill_steps(fill: Fill, rate: crate::sequencer::Rate, count: usize) -> Vec<usize> {
        (0..count).filter(|&index| fill.hits(index, rate.rate_beats())).collect()
    }

    #[test]
    fn fills_land_on_the_beats_at_sixteenths() {
        use crate::sequencer::Rate;
        let r = Rate::Sixteenth;
        assert_eq!(fill_steps(Fill::Every, r, 4), vec![0, 1, 2, 3]);
        assert_eq!(fill_steps(Fill::Eighths, r, 8), vec![0, 2, 4, 6]);
        assert_eq!(fill_steps(Fill::Quarters, r, 16), vec![0, 4, 8, 12]);
        assert_eq!(fill_steps(Fill::Halves, r, 16), vec![0, 8]);
        assert_eq!(fill_steps(Fill::Bars, r, 32), vec![0, 16]);
        assert_eq!(fill_steps(Fill::OffBeats, r, 16), vec![2, 6, 10, 14]);
        assert_eq!(fill_steps(Fill::Backbeat, r, 32), vec![4, 12, 20, 28]);
        assert!(Fill::ALL.iter().all(|fill| fill.fits(r.rate_beats())));
    }

    #[test]
    fn fills_follow_the_lane_rate() {
        use crate::sequencer::Rate;
        // Quarter notes are every 2nd step at 1/8 and every 3rd at 1/8T.
        assert_eq!(fill_steps(Fill::Quarters, Rate::Eighth, 8), vec![0, 2, 4, 6]);
        assert_eq!(fill_steps(Fill::Quarters, Rate::EighthTriplet, 12), vec![0, 3, 6, 9]);
        assert_eq!(fill_steps(Fill::OffBeats, Rate::Eighth, 8), vec![1, 3, 5, 7]);
        assert_eq!(fill_steps(Fill::Eighths, Rate::Eighth, 4), vec![0, 1, 2, 3]);
        // A long lane far from step 0 still lands exactly.
        let far = fill_steps(Fill::Quarters, Rate::SixtyFourthTriplet, 4096);
        assert_eq!(far.len(), 4095 / 24 + 1);
        assert!(far.iter().all(|index| index % 24 == 0));
    }

    #[test]
    fn fills_that_miss_the_lane_steps_do_not_fit() {
        use crate::sequencer::Rate;
        let quarter = Rate::Quarter.rate_beats();
        assert!(!Fill::Eighths.fits(quarter), "finer than the steps");
        assert!(!Fill::OffBeats.fits(quarter), "between the steps");
        assert!(Fill::Quarters.fits(quarter));
        assert!(Fill::Backbeat.fits(quarter));
        // Triplet eighths cannot spell straight eighths or off-beats.
        let triplet = Rate::EighthTriplet.rate_beats();
        assert!(!Fill::Eighths.fits(triplet));
        assert!(!Fill::OffBeats.fits(triplet));
        assert!(Fill::Quarters.fits(triplet));
        // A dotted lane fits only every step.
        let dotted = Rate::SixteenthDotted.rate_beats();
        assert!(Fill::Every.fits(dotted));
        assert!(!Fill::Quarters.fits(dotted));
        // Whole-note steps: only a bar per step fits.
        let whole = Rate::Whole.rate_beats();
        assert!(Fill::Bars.fits(whole));
        assert!(!Fill::Halves.fits(whole));
    }
}
