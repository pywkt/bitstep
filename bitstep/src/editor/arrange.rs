//! The arranger: the song grid, as a view of the editor.
//!
//! One row per pattern and one column per bar. A block in a row plays that
//! row's pattern for its bars; blocks of different patterns in the same bars
//! play together. The grid starts blank: a click in a row places that
//! pattern there at its own length, a drag moves a block (sideways, or to
//! another pattern's row), a drag of its right edge resizes it, a right-click
//! deletes it, and a double-click opens its pattern in the steps view.
//!
//! Like the step strip it is one hand-painted widget; everything computable
//! about it is in [`super::arrange_math`]. Every edit is a shape edit through
//! [`crate::model::SeqModel`], so it is in the next block the audio thread
//! plays and in the undo history (a whole drag as one step).

use egui::{
    Align2, Button, CornerRadius, CursorIcon, FontId, Painter, PointerButton, Pos2, Rect, Sense,
    StrokeKind, pos2, vec2,
};
use egui::text::{LayoutJob, TextWrapping};

use super::arrange_math::{
    ArrangeHit, ArrangeView, BlockMove, BlockResize, repeat_bars, ruler_label,
};
use super::grid_math::EditorLayout;
use super::{
    BitstepEditor, NameEdit, NameEditEnd, NameTarget, name_field, pattern_is_empty, save_name,
    step_press, theme,
};
use crate::model::{NUM_PATTERNS, PatternBars, Playhead, SeqData, SongBlock};

/// What the arranger's interaction id is hashed from (`egui::Id::unique`,
/// deterministic), as the step strip's is.
const ARRANGE_ID: &str = "bitstep:arranger";

/// What each row header's click target salts its id with, alongside its row.
const ROW_HEADER_ID: &str = "bitstep:arrange-row";

/// How wide the loop/end switch in the ruler's corner is.
const LOOP_WIDTH: f32 = 52.0;

/// A block gesture in the arranger, while the button is down. Each step of
/// it is written through the model as it happens; `at` is where the block
/// is now, so the next step knows which block to move on.
pub(super) enum ArrangeGesture {
    /// Moving a block.
    Move { drag: BlockMove, at: SongBlock },
    /// Resizing a block by its right edge. `placed` is set when this press
    /// placed the block, so Escape takes it away again.
    Resize { drag: BlockResize, at: SongBlock, placed: bool },
}

impl BitstepEditor {
    /// The arranger view under the top bar: the ruler, the pattern rows'
    /// headers and the grid of bars. `shown` is the pattern the steps view
    /// shows (`patsel`), whose row is marked.
    pub(super) fn ui_arrange(
        &mut self,
        ui: &mut egui::Ui,
        layout: &EditorLayout,
        data: &SeqData,
        shown: usize,
    ) {
        let bar_beats = self.playhead.bar_beats();
        let lengths = PatternBars::all(data, bar_beats);
        let viewport = layout.steps();
        let columns = ArrangeView::columns(data.song.total_bars(), viewport.width());

        // Where the song is, in bars from its start; only while song mode
        // is playing.
        let playing = self.playhead.song_beat().map(|beat| (beat / bar_beats) as f32);
        let probe = ArrangeView::new(viewport, self.arrange_scroll, columns);
        if self.follow
            && self.arrange_gesture.is_none()
            && let Some(bar) = playing
        {
            self.arrange_scroll.x = probe.follow_scroll_x(bar);
        }
        let view = ArrangeView::new(viewport, self.arrange_scroll, columns);
        self.arrange_scroll = view.scroll();

        // Before the painting, so a block placed by this frame's press is
        // drawn the frame after — the same one frame a shape edit always
        // takes — and the hover outline shows where it will go meanwhile.
        let hover = self.interact_arrange(ui, &view, data, &lengths);
        let painter = ui.painter_at(viewport);
        let editing = self.arrange_gesture.as_ref().map(|gesture| match gesture {
            ArrangeGesture::Move { at, .. } | ArrangeGesture::Resize { at, .. } => *at,
        });
        let marks = Marks { shown, hover, editing, playing };
        paint_arrange(&painter, &view, data, &lengths, &self.playhead, marks);
        self.ui_ruler(ui, layout, &view, data, playing);
        self.ui_row_headers(ui, layout, &view, data, &lengths, shown);
    }

    /// Pointer handling for the grid: a press places, moves or resizes, a
    /// right-click deletes, a double-click opens the pattern, and Escape
    /// puts a running drag back. Returns what a click would land on, for
    /// the hover outline.
    ///
    /// [`Sense::drag`] and the raw press events, as the step strip does and
    /// for its reasons: a quick tap must place a block, and a gesture must
    /// know the moment the button goes down where it went down.
    fn interact_arrange(
        &mut self,
        ui: &mut egui::Ui,
        view: &ArrangeView,
        data: &SeqData,
        lengths: &PatternBars,
    ) -> Option<ArrangeHit> {
        let id = egui::Id::unique(ARRANGE_ID);
        let response = ui.interact(view.viewport(), id, Sense::drag());
        let blocks = data.song.blocks();

        // egui reports a double-click on the second release, after the
        // second press has already picked the block up (without moving it).
        let double = ui.input(|input| input.pointer.button_double_clicked(PointerButton::Primary));
        // A double-click on an empty bar places a block with its first
        // click; the second must not then open it.
        if double
            && response.hovered()
            && let Some(pos) = response.hover_pos()
            && let Some(ArrangeHit::Block { block, .. } | ArrangeHit::Edge { block }) =
                view.hit(pos, blocks)
            && self.arrange_placed != Some(block)
        {
            self.open_pattern(block.pattern());
            return None;
        }
        if response.hovered()
            && let Some((pos, button, _)) = ui.input(step_press)
        {
            match (button, view.hit(pos, blocks)) {
                (
                    PointerButton::Secondary,
                    Some(ArrangeHit::Block { block, .. } | ArrangeHit::Edge { block }),
                ) => {
                    self.model.remove_block(block.pattern(), block.start);
                    self.arrange_gesture = None;
                }
                (PointerButton::Primary, Some(hit)) => self.begin_arrange(view, hit, pos, lengths),
                _ => {}
            }
        }

        if self.arrange_gesture.is_some() && ui.input(|input| input.key_pressed(egui::Key::Escape))
        {
            self.cancel_arrange();
        }
        if response.dragged_by(PointerButton::Primary) {
            if let Some(pos) = response.interact_pointer_pos() {
                self.continue_arrange(view, pos);
            }
        } else {
            // Every way a drag ends leaves `dragged` false: the block is
            // already where it was let go.
            self.arrange_gesture = None;
        }

        let hit = response.hover_pos().and_then(|pos| view.hit(pos, blocks));
        let cursor = match (&self.arrange_gesture, hit) {
            (Some(ArrangeGesture::Move { .. }), _) => Some(CursorIcon::Grabbing),
            (Some(ArrangeGesture::Resize { .. }), _) => Some(CursorIcon::ResizeHorizontal),
            (None, Some(ArrangeHit::Edge { .. })) => Some(CursorIcon::ResizeHorizontal),
            (None, Some(ArrangeHit::Block { .. })) => Some(CursorIcon::Grab),
            _ => None,
        };
        if let Some(cursor) = cursor
            && (response.hovered() || self.arrange_gesture.is_some())
        {
            ui.ctx().set_cursor_icon(cursor);
        }
        let hint = match hit {
            Some(ArrangeHit::Empty { row, .. }) => {
                format!("Click to place pattern {} here", row + 1)
            }
            Some(ArrangeHit::Block { block, .. } | ArrangeHit::Edge { block }) => format!(
                "Pattern {}, bars {}-{}. Drag to move, drag the right edge to resize, \
                 right-click to delete, double-click to edit the pattern.",
                block.pattern + 1,
                block.start + 1,
                block.end()
            ),
            None => String::new(),
        };
        if self.arrange_gesture.is_none() && !hint.is_empty() {
            response.on_hover_text(hint);
        }

        if self.arrange_gesture.is_some() { None } else { hit }
    }

    /// A press landed on the grid: place a block on an empty bar (and go on
    /// to resize it, so a press-and-drag draws a block of any length), or
    /// pick up a block's body or its edge.
    fn begin_arrange(
        &mut self,
        view: &ArrangeView,
        hit: ArrangeHit,
        pos: Pos2,
        lengths: &PatternBars,
    ) {
        // The block the last press placed, for as long as presses keep
        // landing on it (see the double-click in `interact_arrange`).
        let placed_hit = match hit {
            ArrangeHit::Block { block, .. } | ArrangeHit::Edge { block } => Some(block),
            ArrangeHit::Empty { .. } => None,
        };
        if placed_hit != self.arrange_placed {
            self.arrange_placed = None;
        }
        self.arrange_gesture = match hit {
            ArrangeHit::Empty { row, bar } => {
                let Some(placed) = self.model.add_block(row, bar, lengths.of(row)) else {
                    return;
                };
                self.arrange_placed = Some(placed);
                let drag = BlockResize::begin(view, placed, pos);
                Some(ArrangeGesture::Resize { drag, at: placed, placed: true })
            }
            ArrangeHit::Block { block, bar } => {
                Some(ArrangeGesture::Move { drag: BlockMove::begin(block, bar), at: block })
            }
            ArrangeHit::Edge { block } => {
                let drag = BlockResize::begin(view, block, pos);
                Some(ArrangeGesture::Resize { drag, at: block, placed: false })
            }
        };
    }

    /// The pointer moved with the button down: move or resize the block. A
    /// move to where the block does not fit (over another block of that
    /// pattern) is refused, and the block stays where it last fitted.
    fn continue_arrange(&mut self, view: &ArrangeView, pos: Pos2) {
        match &mut self.arrange_gesture {
            Some(ArrangeGesture::Move { drag, at }) => {
                let (row, start) = drag.target(view, pos);
                if (row, start) != (at.pattern(), at.start)
                    && self.model.move_block(at.pattern(), at.start, row, start)
                {
                    *at = SongBlock { pattern: row as u8, start, ..*at };
                }
            }
            Some(ArrangeGesture::Resize { drag, at, .. }) => {
                let bars = drag.bars(view, pos);
                if bars != at.bars
                    && let Some(got) = self.model.resize_block(at.pattern(), at.start, bars)
                {
                    at.bars = got;
                }
            }
            None => {}
        }
    }

    /// Escape during a drag: the block goes back to where the press found
    /// it, and a block the press placed is taken away again.
    fn cancel_arrange(&mut self) {
        match self.arrange_gesture.take() {
            Some(ArrangeGesture::Move { drag, at }) => {
                let from = drag.from;
                self.model.move_block(at.pattern(), at.start, from.pattern(), from.start);
            }
            Some(ArrangeGesture::Resize { at, placed: true, .. }) => {
                self.model.remove_block(at.pattern(), at.start);
            }
            Some(ArrangeGesture::Resize { drag, at, placed: false }) => {
                self.model.resize_block(at.pattern(), at.start, drag.from.bars);
            }
            None => {}
        }
    }

    /// The ruler along the top: bar numbers over the columns, the song's
    /// end, the playhead's marker, and in the corner the loop/end switch and
    /// the song's length.
    fn ui_ruler(
        &mut self,
        ui: &mut egui::Ui,
        layout: &EditorLayout,
        view: &ArrangeView,
        data: &SeqData,
        playing: Option<f32>,
    ) {
        let strip = layout.ruler_strip();
        let song = &data.song;
        if strip.is_positive() {
            let painter = ui.painter_at(strip);
            let font = FontId::proportional(theme::PITCH_LABEL_SIZE + 1.0);
            let tick = egui::Stroke::new(theme::HAIRLINE, theme::GRID_LINE);
            let group = egui::Stroke::new(theme::HAIRLINE, theme::BAR_LINE);
            for bar in view.visible_bars() {
                let x = view.bar_x(bar as f32);
                match ruler_label(bar) {
                    Some(label) => {
                        painter.vline(x, strip.y_range(), group);
                        let at = pos2(x + 3.0, strip.center().y);
                        painter.text(at, Align2::LEFT_CENTER, label, font.clone(), theme::TEXT);
                    }
                    None => {
                        let top = strip.max.y - strip.height() * 0.35;
                        painter.vline(x, top..=strip.max.y, tick);
                    }
                }
            }
            if song.total_bars() > 0 {
                let x = view.bar_x(song.total_bars() as f32) - theme::ARRANGE_BAR_GAP * 0.5;
                let end = egui::Stroke::new(theme::ARRANGE_PLAYHEAD_WIDTH, theme::TEXT_DIM);
                painter.vline(x, strip.y_range(), end);
            }
            if let Some(bar) = playing {
                let x = view.bar_x(bar);
                let marker = Rect::from_min_max(
                    pos2(x - 3.0, strip.max.y - 6.0),
                    pos2(x + 3.0, strip.max.y),
                );
                painter.rect_filled(marker, CornerRadius::ZERO, theme::ARRANGE_PLAYHEAD);
            }
        }

        let corner = layout.ruler_corner();
        if !corner.is_positive() {
            return;
        }
        let looped = song.is_looped();
        let size = vec2(LOOP_WIDTH.min(corner.width()), corner.height());
        let switch = Rect::from_min_size(corner.min, size);
        let label = if looped { "Loop" } else { "End" };
        let button = put_short(ui, switch, Button::new(label).selected(looped));
        if button.clicked() {
            self.model.set_song_looped(!looped);
        }
        button.on_hover_text(match looped {
            true => "After the last block the song starts again from bar 1. \
                     Click to stop at the end instead.",
            false => "After the last block the song goes silent. \
                      Click to loop back to bar 1 instead.",
        });
        let length = match song.total_bars() {
            0 => "Empty".to_owned(),
            1 => "1 bar".to_owned(),
            bars => format!("{bars} bars"),
        };
        let at = pos2(switch.max.x + theme::ITEM_SPACING * 2.0, corner.center().y);
        let font = FontId::proportional(theme::PITCH_LABEL_SIZE + 2.0);
        ui.painter_at(corner).text(at, Align2::LEFT_CENTER, length, font, theme::TEXT_DIM);
    }

    /// The pattern rows' headers down the left: each pattern's name and
    /// length, marked when it is the selected one (the one the steps view
    /// shows) and lit while it sounds. A click selects the pattern; a
    /// double-click types a new name in place ([`super::NameEdit`]).
    fn ui_row_headers(
        &mut self,
        ui: &mut egui::Ui,
        layout: &EditorLayout,
        view: &ArrangeView,
        data: &SeqData,
        lengths: &PatternBars,
        shown: usize,
    ) {
        let headers = layout.headers();
        if !headers.is_positive() {
            return;
        }
        if let Some(edit) = self.name_edit.as_mut() {
            edit.shown = !matches!(edit.target, NameTarget::Pattern(_));
        }
        let mut select = None;
        let painter = ui.painter_at(headers);
        let font = FontId::proportional(theme::PITCH_LABEL_SIZE + 3.0);
        let small = FontId::proportional(theme::PITCH_LABEL_SIZE + 1.0);
        for row in view.visible_rows() {
            let top = view.row_y(row);
            let band = Rect::from_min_max(
                pos2(headers.min.x, top),
                pos2(headers.max.x, top + theme::ARRANGE_ROW_HEIGHT),
            );
            let visible = band.intersect(headers);
            if !visible.is_positive() {
                continue;
            }
            let id = egui::Id::unique(ROW_HEADER_ID).with(row);
            let response = ui.interact(visible, id, Sense::click());
            if row == shown {
                painter.rect_filled(band, CornerRadius::ZERO, theme::SELECTED_ROW);
                let mark = Rect::from_min_size(
                    band.min,
                    vec2(theme::SELECTED_MARK_WIDTH, band.height()),
                );
                painter.rect_filled(mark, CornerRadius::ZERO, theme::ACCENT);
            } else if response.hovered() {
                painter.rect_filled(band, CornerRadius::ZERO, theme::ROW_DRAG);
            }

            let target = NameTarget::Pattern(row);
            let inner = band.shrink2(vec2(theme::PANEL_PADDING, 0.0));
            let editing = self.name_edit.as_ref().is_some_and(|edit| edit.target == target);
            if response.double_clicked() && !editing {
                let text = pattern_label(data, row);
                self.name_edit = Some(NameEdit { target, text, opening: true, shown: true });
            }
            match self.name_edit.as_mut().filter(|edit| edit.target == target) {
                Some(edit) => match name_field(ui, inner.intersect(headers), edit) {
                    Some(NameEditEnd::Save) => {
                        let edit = self.name_edit.take().expect("the edit just drawn");
                        save_name(&self.model, edit);
                    }
                    Some(NameEditEnd::Cancel) => self.name_edit = None,
                    None => {}
                },
                None => {
                    let colour = match (self.playhead.song_plays(row), pattern_is_empty(data, row))
                    {
                        (true, _) => theme::ACCENT,
                        (false, true) => theme::TEXT_DIM,
                        (false, false) => theme::TEXT,
                    };
                    let bars = match lengths.of(row) {
                        1 => "1 bar".to_owned(),
                        bars => format!("{bars} bars"),
                    };
                    let right = pos2(inner.max.x - theme::PANEL_PADDING, band.center().y);
                    let bars = painter.text(
                        right,
                        Align2::RIGHT_CENTER,
                        bars,
                        small.clone(),
                        theme::TEXT_DIM,
                    );
                    // Cut short with an ellipsis before it reaches the length.
                    let left = inner.min.x + theme::PANEL_PADDING;
                    let room = (bars.min.x - theme::ITEM_SPACING - left).max(0.0);
                    let mut job = LayoutJob::simple_singleline(
                        pattern_label(data, row),
                        font.clone(),
                        colour,
                    );
                    job.wrap = TextWrapping::truncate_at_width(room);
                    let galley = painter.layout_job(job);
                    let at = pos2(left, band.center().y - galley.size().y * 0.5);
                    painter.galley(at, galley, colour);
                }
            }
            if response.clicked() {
                select = Some(row);
            }
            let number = row + 1;
            response.on_hover_text(match data.pattern_name(row) {
                "" => format!("Pattern {number}: click to select, double-click to name it"),
                name => {
                    format!("Pattern {number} ({name}): click to select, double-click to rename")
                }
            });
        }
        // A name field whose row scrolled away has lost the keyboard: that
        // is leaving it, so the name is saved.
        if let Some(edit) = self.name_edit.take_if(|edit| !edit.shown) {
            save_name(&self.model, edit);
        }
        if let Some(pattern) = select {
            self.set_pattern(pattern);
        }
    }
}

/// What a pattern's row is called: its own name, or "Pattern N".
pub(super) fn pattern_label(data: &SeqData, pattern: usize) -> String {
    match data.pattern_name(pattern) {
        "" => default_pattern_label(pattern),
        name => name.to_owned(),
    }
}

/// The name a pattern without one goes by.
pub(super) fn default_pattern_label(pattern: usize) -> String {
    format!("Pattern {}", pattern + 1)
}

/// What the grid marks, besides the song itself: the pattern the steps view
/// shows, what a click would land on, the block being dragged, and where
/// the song is playing (in bars).
#[derive(Clone, Copy)]
struct Marks {
    shown: usize,
    hover: Option<ArrangeHit>,
    editing: Option<SongBlock>,
    playing: Option<f32>,
}

/// Paints the grid: the bar cells, the song's end, the blocks with their
/// repeat lines, the hover outline and the playhead. `painter` is clipped to
/// the viewport.
fn paint_arrange(
    painter: &Painter,
    view: &ArrangeView,
    data: &SeqData,
    lengths: &PatternBars,
    playhead: &Playhead,
    marks: Marks,
) {
    let rows = view.visible_rows();
    let bars = view.visible_bars();
    for row in rows.clone() {
        for bar in bars.clone() {
            let shaded = (bar / theme::ARRANGE_BAR_GROUP) % 2 == 1;
            let fill = if shaded { theme::CELL_SHADED } else { theme::CELL };
            painter.rect_filled(view.cell_rect(row, bar), theme::CELL_CORNER, fill);
        }
        if row == marks.shown {
            let last = view.cell_rect(row, bars.end.max(1) - 1);
            let band = view.cell_rect(row, bars.start).union(last);
            painter.rect_filled(band, CornerRadius::ZERO, theme::SELECTED_ROW);
        }
    }

    let song = &data.song;
    if song.total_bars() > 0 {
        let x = view.bar_x(song.total_bars() as f32) - theme::ARRANGE_BAR_GAP * 0.5;
        let end = egui::Stroke::new(theme::ARRANGE_PLAYHEAD_WIDTH, theme::BAR_LINE);
        painter.vline(x, view.viewport().y_range(), end);
    }

    let label_font = FontId::proportional(theme::PITCH_LABEL_SIZE + 2.0);
    let playing_bar = playhead.song_bar();
    for block in song.blocks().iter().copied() {
        let visible = rows.contains(&block.pattern())
            && block.start < bars.end
            && block.end() > bars.start;
        if !visible {
            continue;
        }
        let rect = view.block_rect(block);
        let colour = theme::pattern_colour(block.pattern());
        let sounding = playhead.song_plays(block.pattern())
            && playing_bar.is_some_and(|bar| block.contains(bar));
        let fill = if sounding { colour } else { theme::dim(colour, theme::ARRANGE_IDLE_FACTOR) };
        painter.rect_filled(rect, theme::CELL_CORNER, fill);
        for bar in repeat_bars(block, lengths.of(block.pattern())) {
            let x = view.bar_x(bar as f32) - theme::ARRANGE_BAR_GAP * 0.5;
            let line = egui::Stroke::new(theme::ARRANGE_BAR_GAP, theme::ARRANGE_REPEAT_LINE);
            painter.vline(x, rect.y_range(), line);
        }
        let at = pos2(rect.min.x + 4.0, rect.center().y);
        let label = (block.pattern + 1).to_string();
        painter.text(at, Align2::LEFT_CENTER, label, label_font.clone(), theme::WINDOW);

        let edge_lit = marks.editing == Some(block)
            || matches!(marks.hover, Some(ArrangeHit::Edge { block: hovered }) if hovered == block);
        if edge_lit {
            painter.rect_filled(view.edge_rect(block), theme::CELL_CORNER, theme::END_HANDLE_HOVER);
        }
        if sounding || marks.editing == Some(block) {
            let stroke = egui::Stroke::new(theme::HAIRLINE, theme::MARKER);
            painter.rect_stroke(rect, theme::CELL_CORNER, stroke, StrokeKind::Inside);
        }
    }

    let outline = egui::Stroke::new(theme::HAIRLINE, theme::HOVER_OUTLINE);
    let outlined = match marks.hover {
        Some(ArrangeHit::Empty { row, bar }) => {
            // Where a click would put the pattern: its own length, cut short
            // by the next block of its row.
            let bars = lengths.of(row).min(song.room(row, bar, None));
            (bars > 0).then_some(SongBlock { pattern: row as u8, start: bar, bars })
        }
        Some(ArrangeHit::Block { block, .. }) => Some(block),
        _ => None,
    };
    if let Some(block) = outlined {
        let rect = view.block_rect(block);
        painter.rect_stroke(rect, theme::CELL_CORNER, outline, StrokeKind::Inside);
    }

    if let Some(bar) = marks.playing {
        let x = view.bar_x(bar);
        let line = egui::Stroke::new(theme::ARRANGE_PLAYHEAD_WIDTH, theme::ARRANGE_PLAYHEAD);
        painter.vline(x, view.viewport().y_range(), line);
    }

    if song.is_empty() {
        painter.text(
            view.viewport().center(),
            Align2::CENTER_CENTER,
            "Click in a pattern's row to place it on the song",
            FontId::proportional(theme::HINT_SIZE),
            theme::TEXT_DIM,
        );
    }
}

/// Puts `button` in `rect`, which may be shorter than a control: a button is
/// never shorter than the style's control height, which is taller than the
/// ruler, so it is sized down to stay in its row.
pub(super) fn put_short(ui: &mut egui::Ui, rect: Rect, button: Button) -> egui::Response {
    let mut row = ui.new_child(egui::UiBuilder::new().max_rect(rect));
    row.spacing_mut().interact_size.y = rect.height();
    row.put(rect, button)
}

// Every row is a pattern; the model's 64 is what the grid's height is.
const _: () = assert!(NUM_PATTERNS == 64);
