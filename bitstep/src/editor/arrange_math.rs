//! Everything about the arranger (the song grid) that can be worked out
//! without drawing it.
//!
//! The arranger is one hand-painted widget like the step strip: one row per
//! pattern, one column per bar, and the song's blocks drawn over them. Every
//! rect, every culling bound, every pointer lookup and both block gestures
//! (moving and resizing) are arithmetic this module does, so they are tested
//! without a display. Like `grid_math`, it imports no nice-plug types.

use std::ops::Range;

use egui::{Pos2, Rect, Vec2, pos2, vec2};

use super::grid_math::sane;
use super::theme;
use crate::model::{MAX_SONG_BARS, NUM_PATTERNS, SongBlock};

/// How much clear space follow mode leaves between the playhead and the
/// edge it is scrolling towards, in bars.
pub const FOLLOW_MARGIN_BARS: f32 = 2.0;

/// What a pointer position on the arranger lands on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArrangeHit {
    /// A bar of a pattern's row with no block of that pattern in it.
    Empty { row: usize, bar: u32 },
    /// A block's body, at bar `bar`.
    Block { block: SongBlock, bar: u32 },
    /// A block's right edge: its resize handle.
    Edge { block: SongBlock },
}

/// One frame's arranger: where its rows sit on screen, how far they are
/// scrolled, and how many bar columns there are.
///
/// Content coordinates put bar 0 of row 0 at the origin; the viewport is the
/// rows' area (under the ruler), and `scroll` is how far the content has
/// moved left and up under it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ArrangeView {
    viewport: Rect,
    scroll: Vec2,
    bars: u32,
}

impl ArrangeView {
    /// The arranger with `bars` columns in `viewport`, scrolled by `scroll`
    /// (clamped to what the content allows).
    pub fn new(viewport: Rect, scroll: Vec2, bars: u32) -> ArrangeView {
        let bars = bars.clamp(1, MAX_SONG_BARS);
        let probe = ArrangeView { viewport, scroll: Vec2::ZERO, bars };
        ArrangeView { scroll: probe.clamp_scroll(scroll), ..probe }
    }

    /// How many columns the arranger has for a song `song_bars` long in a
    /// viewport `width` wide: the song and [`theme::ARRANGE_SPARE_BARS`]
    /// more, and at least a screenful, never past the grid's end.
    pub fn columns(song_bars: u32, width: f32) -> u32 {
        let screen = (sane(width, 0.0, 1e6) / theme::ARRANGE_BAR_PITCH).ceil() as u32;
        song_bars.saturating_add(theme::ARRANGE_SPARE_BARS).max(screen).clamp(1, MAX_SONG_BARS)
    }

    pub fn viewport(&self) -> Rect {
        self.viewport
    }

    pub fn scroll(&self) -> Vec2 {
        self.scroll
    }

    pub fn bars(&self) -> u32 {
        self.bars
    }

    /// The content's size: every column and all [`NUM_PATTERNS`] rows,
    /// without the trailing gaps.
    pub fn content_size(&self) -> Vec2 {
        vec2(
            self.bars as f32 * theme::ARRANGE_BAR_PITCH - theme::ARRANGE_BAR_GAP,
            NUM_PATTERNS as f32 * theme::ARRANGE_ROW_PITCH - theme::ARRANGE_ROW_GAP,
        )
    }

    /// The largest scroll offsets that still show content.
    pub fn max_scroll(&self) -> Vec2 {
        let content = self.content_size();
        vec2(
            sane(content.x - self.viewport.width(), 0.0, f32::MAX),
            sane(content.y - self.viewport.height(), 0.0, f32::MAX),
        )
    }

    /// `scroll` pulled into `0..=max_scroll` on both axes; a NaN comes back
    /// as 0.
    pub fn clamp_scroll(&self, scroll: Vec2) -> Vec2 {
        let max = self.max_scroll();
        vec2(sane(scroll.x, 0.0, max.x), sane(scroll.y, 0.0, max.y))
    }

    /// The screen `x` of bar `bar`'s left edge; fractional bars land inside
    /// it, which is what the playhead uses.
    pub fn bar_x(&self, bar: f32) -> f32 {
        self.viewport.min.x + bar * theme::ARRANGE_BAR_PITCH - self.scroll.x
    }

    /// The screen `y` of row `row`'s top edge.
    pub fn row_y(&self, row: usize) -> f32 {
        self.viewport.min.y + row as f32 * theme::ARRANGE_ROW_PITCH - self.scroll.y
    }

    /// One bar of one row, gaps excluded.
    pub fn cell_rect(&self, row: usize, bar: u32) -> Rect {
        let min = pos2(self.bar_x(bar as f32), self.row_y(row));
        Rect::from_min_size(min, vec2(theme::ARRANGE_BAR_WIDTH, theme::ARRANGE_ROW_HEIGHT))
    }

    /// A block, from the left edge of its first bar to the right edge of its
    /// last: one piece, with no gaps inside it.
    pub fn block_rect(&self, block: SongBlock) -> Rect {
        let first = self.cell_rect(block.pattern(), block.start);
        let last = self.cell_rect(block.pattern(), block.end().saturating_sub(1).max(block.start));
        first.union(last)
    }

    /// A block's resize handle: its right end, never more than half of it,
    /// so a one-bar block can still be grabbed to move.
    pub fn edge_rect(&self, block: SongBlock) -> Rect {
        let rect = self.block_rect(block);
        let width = theme::ARRANGE_EDGE_WIDTH.min(rect.width() * 0.5);
        Rect::from_min_max(pos2(rect.max.x - width, rect.min.y), rect.max)
    }

    /// The rows at least partly in view.
    pub fn visible_rows(&self) -> Range<usize> {
        let pitch = theme::ARRANGE_ROW_PITCH;
        let first = (self.scroll.y / pitch).floor().max(0.0) as usize;
        let last = ((self.scroll.y + self.viewport.height()) / pitch).ceil().max(0.0) as usize;
        first.min(NUM_PATTERNS)..last.min(NUM_PATTERNS)
    }

    /// The bar columns at least partly in view.
    pub fn visible_bars(&self) -> Range<u32> {
        let pitch = theme::ARRANGE_BAR_PITCH;
        let first = (self.scroll.x / pitch).floor().max(0.0) as u32;
        let last = ((self.scroll.x + self.viewport.width()) / pitch).ceil().max(0.0) as u32;
        first.min(self.bars)..last.min(self.bars)
    }

    /// The bar column under screen `x`, whether or not it is in view —
    /// clamped onto the grid, so a drag past either edge stops at it.
    pub fn bar_at(&self, x: f32) -> u32 {
        let content = x - self.viewport.min.x + self.scroll.x;
        let bar = (content / theme::ARRANGE_BAR_PITCH).floor();
        sane(bar, 0.0, (MAX_SONG_BARS - 1) as f32) as u32
    }

    /// The row under screen `y`, clamped onto the grid like
    /// [`Self::bar_at`]. A row owns the gap below it.
    pub fn row_at(&self, y: f32) -> usize {
        let content = y - self.viewport.min.y + self.scroll.y;
        let row = (content / theme::ARRANGE_ROW_PITCH).floor();
        sane(row, 0.0, (NUM_PATTERNS - 1) as f32) as usize
    }

    /// The row and bar under `pos`, or `None` outside the viewport or past
    /// the content.
    pub fn spot(&self, pos: Pos2) -> Option<(usize, u32)> {
        if !self.viewport.contains(pos) {
            return None;
        }
        let content = pos - self.viewport.min + self.scroll;
        let row = (content.y / theme::ARRANGE_ROW_PITCH).floor();
        let bar = (content.x / theme::ARRANGE_BAR_PITCH).floor();
        let fits = row >= 0.0 && bar >= 0.0;
        let (row, bar) = (row as usize, bar as u32);
        (fits && row < NUM_PATTERNS && bar < self.bars).then_some((row, bar))
    }

    /// What `pos` lands on, given the song's blocks.
    pub fn hit(&self, pos: Pos2, blocks: &[SongBlock]) -> Option<ArrangeHit> {
        let (row, bar) = self.spot(pos)?;
        let mut row_blocks = blocks.iter().copied().filter(|block| block.pattern() == row);
        let block = row_blocks.find(|block| block.contains(bar));
        Some(match block {
            Some(block) if self.edge_rect(block).contains(pos) => ArrangeHit::Edge { block },
            Some(block) => ArrangeHit::Block { block, bar },
            None => ArrangeHit::Empty { row, bar },
        })
    }

    /// The horizontal scroll that keeps song position `bar` (fractional, in
    /// bars) in view: unchanged while it is inside the margins, otherwise
    /// the page turned so it sits [`FOLLOW_MARGIN_BARS`] from the left.
    pub fn follow_scroll_x(&self, bar: f32) -> f32 {
        if !bar.is_finite() {
            return self.scroll.x;
        }
        let margin = FOLLOW_MARGIN_BARS * theme::ARRANGE_BAR_PITCH;
        let x = bar * theme::ARRANGE_BAR_PITCH;
        let margin = margin.min(self.viewport.width() * 0.25);
        let left = self.scroll.x + margin;
        let right = self.scroll.x + self.viewport.width() - margin;
        let wanted = match x < left || x > right {
            true => x - margin,
            false => self.scroll.x,
        };
        sane(wanted, 0.0, self.max_scroll().x)
    }
}

/// Moving a block: it follows the pointer by whole bars, holding on to the
/// bar of it that was grabbed, and changes row (and so pattern) with the
/// pointer's row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockMove {
    /// The block as it was when the press landed.
    pub from: SongBlock,
    /// Which of its bars was grabbed, counted from its first.
    grab: u32,
}

impl BlockMove {
    /// A move of `block`, grabbed at bar `bar` (one of its own).
    pub fn begin(block: SongBlock, bar: u32) -> BlockMove {
        BlockMove { from: block, grab: bar.saturating_sub(block.start).min(block.bars - 1) }
    }

    /// Where the block goes with the pointer at `pos`: the row under it and
    /// the start bar that keeps the grabbed bar under it, clamped so the
    /// block stays on the grid.
    pub fn target(&self, view: &ArrangeView, pos: Pos2) -> (usize, u32) {
        let row = view.row_at(pos.y);
        let latest = MAX_SONG_BARS.saturating_sub(self.from.bars);
        let start = view.bar_at(pos.x).saturating_sub(self.grab).min(latest);
        (row, start)
    }
}

/// Resizing a block by its right edge: the edge follows the pointer to the
/// nearest bar line, never shorter than one bar.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BlockResize {
    /// The block as it was when the press landed.
    pub from: SongBlock,
    /// How far right of the block's end the press landed, in content points,
    /// so the edge does not jump to the pointer on the first move.
    offset: f32,
}

impl BlockResize {
    /// A resize of `block`, pressed at `pos`.
    pub fn begin(view: &ArrangeView, block: SongBlock, pos: Pos2) -> BlockResize {
        let end = view.bar_x(block.end() as f32);
        let offset = if pos.x.is_finite() { pos.x - end } else { 0.0 };
        BlockResize { from: block, offset }
    }

    /// How many bars long the block is with the pointer at `pos`: at least
    /// one, and never past the end of the grid. (The model clamps it to the
    /// room before the next block.)
    pub fn bars(&self, view: &ArrangeView, pos: Pos2) -> u32 {
        let end = pos.x - self.offset - view.bar_x(0.0);
        let end = (end / theme::ARRANGE_BAR_PITCH).round();
        let most = MAX_SONG_BARS - self.from.start;
        let bars = sane(end - self.from.start as f32, 1.0, most as f32);
        bars as u32
    }
}

/// Where each pattern's repeats start inside a block, for the lines drawn
/// across a stretched block: every `every` bars after its start, before its
/// end.
pub fn repeat_bars(block: SongBlock, every: u32) -> impl Iterator<Item = u32> {
    let every = every.max(1);
    (1..).map(move |n| block.start + n * every).take_while(move |&bar| bar < block.end())
}

/// The bar numbers the ruler labels: the first bar of every
/// [`theme::ARRANGE_BAR_GROUP`] (one-based: 1, 5, 9, …).
pub fn ruler_label(bar: u32) -> Option<String> {
    bar.is_multiple_of(theme::ARRANGE_BAR_GROUP).then(|| (bar + 1).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(scroll: Vec2) -> ArrangeView {
        let viewport = Rect::from_min_size(pos2(100.0, 50.0), vec2(320.0, 240.0));
        ArrangeView::new(viewport, scroll, 64)
    }

    fn block(pattern: u8, start: u32, bars: u32) -> SongBlock {
        SongBlock { pattern, start, bars }
    }

    #[test]
    fn columns_cover_the_song_spare_bars_and_the_screen() {
        assert_eq!(ArrangeView::columns(0, 0.0), theme::ARRANGE_SPARE_BARS);
        assert_eq!(ArrangeView::columns(40, 0.0), 40 + theme::ARRANGE_SPARE_BARS);
        let wide = 100.0 * theme::ARRANGE_BAR_PITCH;
        assert_eq!(ArrangeView::columns(0, wide), 100);
        assert_eq!(ArrangeView::columns(MAX_SONG_BARS, wide), MAX_SONG_BARS);
        assert_eq!(ArrangeView::columns(0, f32::NAN), theme::ARRANGE_SPARE_BARS);
    }

    #[test]
    fn cells_and_blocks_are_placed_from_the_scroll() {
        let still = view(Vec2::ZERO);
        let cell = still.cell_rect(2, 3);
        assert_eq!(cell.min, pos2(100.0 + 3.0 * theme::ARRANGE_BAR_PITCH, 50.0 + 2.0 * 24.0));
        assert_eq!(cell.size(), vec2(theme::ARRANGE_BAR_WIDTH, theme::ARRANGE_ROW_HEIGHT));

        let rect = still.block_rect(block(2, 3, 4));
        assert_eq!(rect.min, cell.min);
        assert_eq!(rect.width(), 4.0 * theme::ARRANGE_BAR_PITCH - theme::ARRANGE_BAR_GAP);
        let edge = still.edge_rect(block(2, 3, 4));
        assert_eq!((edge.max, edge.width()), (rect.max, theme::ARRANGE_EDGE_WIDTH));
        let one = still.edge_rect(block(0, 0, 1));
        assert!(one.width() <= theme::ARRANGE_BAR_WIDTH * 0.5, "a short block keeps a body");

        let scrolled = view(vec2(theme::ARRANGE_BAR_PITCH, theme::ARRANGE_ROW_PITCH));
        assert_eq!(scrolled.cell_rect(3, 4).min, cell.min, "one bar and one row along");
    }

    #[test]
    fn scroll_is_clamped_to_the_content() {
        let far = view(vec2(1e9, 1e9));
        assert_eq!(far.scroll(), far.max_scroll());
        let content = far.content_size();
        assert_eq!(far.max_scroll(), vec2(content.x - 320.0, content.y - 240.0));
        assert_eq!(view(vec2(-5.0, f32::NAN)).scroll(), Vec2::ZERO);
        assert_eq!(content.y, 64.0 * theme::ARRANGE_ROW_PITCH - theme::ARRANGE_ROW_GAP);
    }

    #[test]
    fn only_what_is_in_view_is_visited() {
        let still = view(Vec2::ZERO);
        assert_eq!(still.visible_rows(), 0..10);
        assert_eq!(still.visible_bars(), 0..10);
        let scrolled = view(vec2(theme::ARRANGE_BAR_PITCH * 2.5, theme::ARRANGE_ROW_PITCH * 60.0));
        assert_eq!(scrolled.visible_bars(), 2..13);
        assert_eq!(scrolled.visible_rows().end, NUM_PATTERNS);
    }

    #[test]
    fn a_pointer_lands_on_empty_bars_blocks_and_edges() {
        let still = view(Vec2::ZERO);
        let blocks = [block(1, 2, 3), block(0, 0, 1)];
        let at = |row: usize, bar: u32| still.cell_rect(row, bar).center();

        assert_eq!(still.hit(at(1, 0), &blocks), Some(ArrangeHit::Empty { row: 1, bar: 0 }));
        assert_eq!(
            still.hit(at(1, 3), &blocks),
            Some(ArrangeHit::Block { block: blocks[0], bar: 3 })
        );
        let edge = still.edge_rect(blocks[0]).center();
        assert_eq!(still.hit(edge, &blocks), Some(ArrangeHit::Edge { block: blocks[0] }));
        // Row 2 at bar 3 is another pattern's row: empty there.
        assert_eq!(still.hit(at(2, 3), &blocks), Some(ArrangeHit::Empty { row: 2, bar: 3 }));
        assert_eq!(still.hit(pos2(0.0, 0.0), &blocks), None, "outside the viewport");
    }

    #[test]
    fn bars_and_rows_under_a_drag_are_clamped_onto_the_grid() {
        let still = view(Vec2::ZERO);
        assert_eq!(still.bar_at(-1e6), 0);
        assert_eq!(still.bar_at(1e9), MAX_SONG_BARS - 1);
        assert_eq!(still.row_at(-50.0), 0);
        assert_eq!(still.row_at(1e9), NUM_PATTERNS - 1);
        assert_eq!(still.bar_at(f32::NAN), 0);
    }

    #[test]
    fn a_moved_block_keeps_its_grabbed_bar_under_the_pointer() {
        let still = view(Vec2::ZERO);
        let moving = BlockMove::begin(block(1, 2, 3), 3);
        assert_eq!(moving.target(&still, still.cell_rect(1, 3).center()), (1, 2), "unmoved");
        assert_eq!(moving.target(&still, still.cell_rect(4, 7).center()), (4, 6));
        assert_eq!(moving.target(&still, still.cell_rect(0, 0).center()), (0, 0), "stops at bar 1");
        assert_eq!(moving.target(&still, pos2(1e9, 1e9)), (NUM_PATTERNS - 1, MAX_SONG_BARS - 3));
    }

    #[test]
    fn a_resized_block_follows_the_pointer_to_the_nearest_bar_line() {
        let still = view(Vec2::ZERO);
        let from = block(1, 2, 3);
        let grab = still.edge_rect(from).center();
        let resize = BlockResize::begin(&still, from, grab);
        assert_eq!(resize.bars(&still, grab), 3, "the press alone changes nothing");
        let pitch = theme::ARRANGE_BAR_PITCH;
        assert_eq!(resize.bars(&still, grab + vec2(pitch * 2.0, 40.0)), 5);
        assert_eq!(resize.bars(&still, grab + vec2(pitch * 0.4, 0.0)), 3, "rounds to the line");
        assert_eq!(resize.bars(&still, grab + vec2(pitch * 0.6, 0.0)), 4);
        assert_eq!(resize.bars(&still, grab - vec2(1e6, 0.0)), 1, "never shorter than a bar");
        assert_eq!(resize.bars(&still, grab + vec2(1e9, 0.0)), MAX_SONG_BARS - 2);
    }

    #[test]
    fn follow_turns_the_page_when_the_playhead_leaves_the_margins() {
        let still = view(Vec2::ZERO);
        assert_eq!(still.follow_scroll_x(3.0), 0.0, "in view: left alone");
        let pitch = theme::ARRANGE_BAR_PITCH;
        assert_eq!(still.follow_scroll_x(20.0), 18.0 * pitch);
        assert_eq!(still.follow_scroll_x(f32::NAN), 0.0);
        assert_eq!(still.follow_scroll_x(1e9), still.max_scroll().x);
    }

    #[test]
    fn repeats_and_ruler_labels() {
        let stretched = block(0, 4, 7);
        assert_eq!(repeat_bars(stretched, 2).collect::<Vec<_>>(), [6, 8, 10]);
        assert_eq!(repeat_bars(stretched, 7).count(), 0, "one whole pass: no lines");
        assert_eq!(repeat_bars(stretched, 0).count(), 6, "a zero length reads as one bar");
        assert_eq!(ruler_label(0).as_deref(), Some("1"));
        assert_eq!(ruler_label(4).as_deref(), Some("5"));
        assert_eq!(ruler_label(5), None);
    }
}
