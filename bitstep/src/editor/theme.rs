//! The only place the look of the editor lives.
//!
//! Every colour, every size and every value-to-pixels mapping the grid draws
//! with is named here, so a later restyling is an edit to
//! this file rather than a hunt through the drawing code. Nothing here reads
//! or writes the sequence: the helpers take the value they colour (a velocity,
//! a step index, a lane's `0xRRGGBB`) and hand back an `egui` colour, stroke
//! or fraction.
//!
//! The lane palette is [`crate::state::LANE_PALETTE`] converted to
//! [`Color32`], not a second list of its own: the colours a fresh instance
//! hands its lanes are saved into the host's project, so a palette that drifts
//! from the one in `state.rs` would repaint every reloaded patch.

use egui::{Color32, CornerRadius, Shadow, Stroke, Vec2, Visuals};

use crate::model::Step;
use crate::state::LANE_PALETTE;

// ---- greys (Bitwig-like dark greys) ----

/// Behind everything; the darkest tone, so panels read as raised.
pub const WINDOW: Color32 = Color32::from_gray(24);

/// Top bar and lane headers — the chrome around the grid.
pub const PANEL: Color32 = Color32::from_gray(33);

/// Controls sitting on [`PANEL`] (buttons, drag values, dropdowns).
pub const CONTROL: Color32 = Color32::from_gray(45);

/// A control under the pointer.
pub const CONTROL_HOVER: Color32 = Color32::from_gray(56);

/// A control being clicked or held.
pub const CONTROL_ACTIVE: Color32 = Color32::from_gray(66);

/// Separators and outlines: the top bar's lower edge, the header↔grid
/// split, and the line egui draws around every widget.
pub const GRID_LINE: Color32 = Color32::from_gray(54);

/// An off step's cell, on an unshaded column.
pub const CELL: Color32 = Color32::from_gray(38);

/// An off step's cell on a shaded column — see [`is_shaded_column`].
pub const CELL_SHADED: Color32 = Color32::from_gray(47);

/// Normal label text.
pub const TEXT: Color32 = Color32::from_gray(205);

/// Secondary text: step numbers, units, inactive pattern buttons.
pub const TEXT_DIM: Color32 = Color32::from_gray(138);

/// The one non-grey of the chrome: active pattern button, follow toggle,
/// focused control. Lane colours are the palette's job, not this one's.
pub const ACCENT: Color32 = Color32::from_rgb(0x55, 0x9D, 0xD6);

/// The playhead column's wash, drawn over the cells rather than instead of
/// them so a lit step keeps its lane colour.
pub const PLAYHEAD: Color32 = Color32::from_rgba_premultiplied(26, 26, 26, 26);

/// The end handle (`┃`) after a lane's last active step.
pub const END_HANDLE: Color32 = Color32::from_gray(120);

/// An armed destructive menu item — a `delete` or a `clear` waiting for its
/// second click (`editor::lane_menu`).
///
/// A red of the same family as the palette's, so it sits in the window
/// without matching any one lane: the point is that the item has changed
/// meaning, not that it belongs to anything.
pub const WARN: Color32 = Color32::from_rgb(0xE8, 0x6A, 0x6A);

/// The lane header's drag grip — the dots a row is picked up by to reorder
/// it.
///
/// Dimmer than [`TEXT`] and brighter than [`GRID_LINE`]: a grip is an
/// affordance rather than content, so it has to be findable without being the
/// first thing in the row the eye lands on.
pub const GRIP: Color32 = Color32::from_gray(104);

/// The wash drawn over the lane header being dragged to a new position, so
/// the row the hand is holding reads as picked up.
pub const ROW_DRAG: Color32 = Color32::from_rgba_premultiplied(40, 40, 40, 40);

/// The per-step markers — ratchet ticks and the probability dot.
///
/// One colour for both, and a near-white one: a marker has to read over a
/// cell's own shade *and* over whatever share of it a lane-coloured velocity
/// fill has taken, so it cannot be a tint of either.
pub const MARKER: Color32 = Color32::from_gray(232);

/// What is thrown under a menu, a dropdown or the step popover.
///
/// Tighter and more squarely under its surface than egui's stock shadow,
/// which is offset ten points to the right and twenty down: these open over
/// a grid whose columns are [`CELL_PITCH`] apart, and a shadow cast that far
/// sideways darkens the very cells the pointer is aiming at next. Straight
/// down and a little stronger instead, so the surface still lifts off the
/// grid.
pub const SHADOW: Shadow =
    Shadow { offset: [0, 4], blur: 12, spread: 0, color: Color32::from_black_alpha(140) };

/// A muted or solo-shadowed lane's steps are drawn at this much of their
/// colour, so the grid still reads but is plainly not sounding.
pub const MUTED_FACTOR: f32 = 0.45;

/// How far towards white the playhead's step is pushed.
pub const PLAYHEAD_BRIGHTEN: f32 = 0.35;

/// How much of its lane colour an off step's outline keeps. Dim enough to
/// stay a hint, bright enough to say which lane the empty row belongs to —
/// and below [`MUTED_FACTOR`], so even a silenced lane's steps read as on.
///
/// The floor under it is the cells themselves. At 0.28 the brightest channel
/// of the dimmest palette colour cleared [`CELL_SHADED`] by nine levels out
/// of 255, so on the shaded half of the grid's columns the hairline read as
/// a shadow around the cell rather than as a tint of the lane. Here every
/// palette colour clears both shades plainly — the test at the foot of this
/// file holds it there.
pub const OFF_OUTLINE_FACTOR: f32 = 0.38;

// ---- sizes and spacing, in logical points ----

/// One step cell: square, and big enough to hit comfortably with a mouse —
/// a ratchet marker, the probability dot and a pitch label all fit in it.
pub const CELL_WIDTH: f32 = 30.0;
pub const CELL_HEIGHT: f32 = 30.0;

/// Gap between neighbouring cells in a row.
pub const CELL_GAP: f32 = 2.0;

/// Distance from one step column to the next — what `grid_math.rs` steps by.
pub const CELL_PITCH: f32 = CELL_WIDTH + CELL_GAP;

/// Gap between lane rows, and the row-to-row distance that follows from it.
pub const ROW_GAP: f32 = 4.0;
pub const ROW_PITCH: f32 = CELL_HEIGHT + ROW_GAP;

/// Every control of the chrome — a button in the top bar, a drag value in a
/// lane header, an item in a menu — is drawn exactly as tall as a step cell,
/// so the window's three bands read as one grid instead of as a toolbar
/// stacked on a list stacked on a grid.
pub const CONTROL_HEIGHT: f32 = CELL_HEIGHT;

/// Inside a control, between its frame and its text.
///
/// Half of egui's stock horizontal padding, because the lane header spends
/// its width on eight fields: at 8 points a side the key field had 20 left
/// for a note name and clipped the lowest of them, and the rate dropdown
/// truncated every triplet label.
pub const CONTROL_PADDING: Vec2 = Vec2::new(4.0, 2.0);

/// The narrowest egui may lay a control out at, and the one size here that
/// is egui's own rather than this grid's: only the controls that size
/// themselves see it (a menu item, the box on a slider), since everything in
/// the top bar and the lane header is placed into a rect of its own.
pub const CONTROL_MIN_WIDTH: f32 = 40.0;

/// The lane header's fields, in the order the header draws them.
///
/// Widths rather than weights: every lane's header has to line its columns up
/// with every other's, and `grid_math::HeaderFields` lays them out from these
/// alone. Each holds the longest thing it has to show, once
/// [`CONTROL_PADDING`] and a dropdown's arrow have taken their share — the
/// lowest note name ("C-2"), a four-digit length, the widest rate label
/// ("1/16T") — because every one of these fields truncates rather than
/// wraps.
pub const HEADER_GRIP_WIDTH: f32 = 10.0;
pub const HEADER_DOT_WIDTH: f32 = 12.0;
pub const HEADER_NAME_WIDTH: f32 = 64.0;
pub const HEADER_KEY_WIDTH: f32 = 38.0;
pub const HEADER_LENGTH_WIDTH: f32 = 40.0;
pub const HEADER_RATE_WIDTH: f32 = 72.0;
/// The lane volume field, wide enough for "-36 dB".
pub const HEADER_VOLUME_WIDTH: f32 = 48.0;
/// One of the two square-ish Mute / Solo buttons.
pub const HEADER_TOGGLE_WIDTH: f32 = 19.0;

/// Space between the lane header's fields. Tighter than [`ITEM_SPACING`]:
/// eight controls share one row, and they belong to one lane rather than
/// reading as eight separate things.
pub const HEADER_FIELD_GAP: f32 = 3.0;

/// What the nine fields above take side by side, their eight gaps included.
pub const HEADER_FIELDS_WIDTH: f32 = HEADER_GRIP_WIDTH
    + HEADER_DOT_WIDTH
    + HEADER_NAME_WIDTH
    + HEADER_KEY_WIDTH
    + HEADER_LENGTH_WIDTH
    + HEADER_RATE_WIDTH
    + HEADER_VOLUME_WIDTH
    + 2.0 * HEADER_TOGGLE_WIDTH
    + 8.0 * HEADER_FIELD_GAP;

/// The pinned lane header: drag grip, colour dot, name, key, length, rate,
/// volume, mute/solo.
///
/// Derived rather than typed out, so that widening one field cannot quietly
/// push the mute and solo toggles off the end of the column — which is what
/// `grid_math::HeaderFields::split` answers a column too narrow with: every
/// field scaled down a little. The spare point absorbs the one the two sums
/// can differ by, float addition not being associative.
pub const HEADER_WIDTH: f32 = HEADER_FIELDS_WIDTH + 2.0 * PANEL_PADDING + 1.0;

/// The pattern buttons / `+ lane` / follow strip across the top: one
/// [`CONTROL_HEIGHT`] control with [`PANEL_PADDING`] above and below it, so
/// the bar is as tall as a lane row and no taller.
pub const TOP_BAR_HEIGHT: f32 = CONTROL_HEIGHT + 2.0 * PANEL_PADDING;

/// Padding inside the top bar and the lane header.
pub const PANEL_PADDING: f32 = 6.0;

/// The width of the top bar's follow toggle, which is placed from its own
/// rect rather than laid out so that the pattern buttons cannot push it off
/// the right edge.
pub const FOLLOW_WIDTH: f32 = 76.0;

/// The width of one of the top bar's eight pattern buttons. Wide enough for
/// a single digit and a hair more, so the row of them reads as one strip
/// rather than eight separate controls.
pub const PATTERN_WIDTH: f32 = 24.0;

/// The width of the top bar's `+ lane` button, which sits beside the follow
/// toggle and is placed from its own rect for the same reason.
pub const ADD_LANE_WIDTH: f32 = 66.0;

/// Width of each edit-mode button ("Steps", "Pitch") in the top bar.
pub const MODE_WIDTH: f32 = 52.0;

/// The width of the top bar's toggle for the parameter panel, placed from
/// the right like the follow toggle and the `+ Lane` button.
pub const PANEL_TOGGLE_WIDTH: f32 = 70.0;

/// How tall the parameter panel under the grid is when it is shown: tall
/// enough that one point of travel is about one velocity step.
pub const PARAM_PANEL_HEIGHT: f32 = 140.0;

/// Clear space above and below the bars inside the parameter panel, so the
/// top and bottom of the range can still be reached without leaving it.
pub const PARAM_PLOT_INSET: f32 = 8.0;

/// The wash behind the selected lane's header — the lane the parameter panel
/// edits.
pub const SELECTED_ROW: Color32 = Color32::from_rgba_premultiplied(30, 30, 30, 30);

/// Width of the accent bar at the left edge of the selected lane's header.
pub const SELECTED_MARK_WIDTH: f32 = 3.0;

/// Width of the pitch mark on the left edge of a pitched step in steps mode.
pub const PITCH_MARKER_WIDTH: f32 = 2.0;

/// Font size of the pitch value written on a cell in pitch mode.
pub const PITCH_LABEL_SIZE: f32 = 9.0;

/// How far an on step's lane colour is dimmed behind its pitch bar in pitch
/// mode — enough to see which steps play, faint enough for the bar to read.
pub const PITCH_MODE_ON_FACTOR: f32 = 0.35;

/// Space between controls inside a panel.
pub const ITEM_SPACING: f32 = 4.0;

/// Grab width of the end handle — the drag target, not just the line drawn.
pub const END_HANDLE_WIDTH: f32 = 6.0;

/// The lane's colour dot in its header.
pub const LANE_DOT_RADIUS: f32 = 5.0;

/// One dot of a lane header's drag grip ([`GRIP`]).
pub const GRIP_DOT_RADIUS: f32 = 1.4;

/// The corner dot marking a step whose probability is below 100%.
pub const MARKER_DOT_RADIUS: f32 = 2.0;

/// The ticks marking a ratcheted step.
pub const RATCHET_TICK_WIDTH: f32 = 1.5;

/// How far down a cell a ratchet tick hangs from its top edge.
pub const RATCHET_TICK_HEIGHT: f32 = 5.0;

/// Clear space between a marker and the cell edges it sits against.
pub const MARKER_INSET: f32 = 2.0;

/// Cells, and anything else that wants the same softness.
pub const CELL_CORNER: CornerRadius = CornerRadius::same(3);

/// Buttons and other panel controls.
pub const CONTROL_CORNER: CornerRadius = CornerRadius::same(4);

/// Hairlines: cell outlines, separators.
pub const HAIRLINE: f32 = 1.0;

/// The caret in the one field the editor types into — the rename box in a
/// lane's context menu. Two points, as egui's own, because a hairline caret
/// is lost in the text it sits between.
pub const TEXT_CURSOR_WIDTH: f32 = 2.0;

/// Steps per shading group — a beat at 1/16, which is what makes the
/// alternating blocks line up with the bar.
pub const BEAT_STEPS: usize = 4;

/// The size the empty-state hint is drawn at — the one text in the editor
/// that is not a widget's, shown when there are no lanes. A little above the
/// body text, because it is all there is to read.
pub const HINT_SIZE: f32 = 15.0;

/// The shortest an on step's fill is drawn. Velocity 1 is still a note, so it
/// may not vanish into an off cell.
pub const MIN_FILL_FRACTION: f32 = 0.12;

// ---- the lane palette ----

/// [`LANE_PALETTE`] as `egui` colours, in the same order — index *n* here is
/// the colour lane *n* of a fresh instance was given.
pub const LANE_SWATCHES: [Color32; LANE_PALETTE.len()] = {
    let mut swatches = [Color32::BLACK; LANE_PALETTE.len()];
    let mut index = 0;
    while index < LANE_PALETTE.len() {
        swatches[index] = rgb(LANE_PALETTE[index]);
        index += 1;
    }
    swatches
};

/// The bits of a stored lane colour that are the colour — see [`rgb`].
///
/// Named here because the context menu compares a lane's colour against the
/// palette to light the swatch it came from, and it has to mask exactly what
/// the painter reads.
pub const RGB_MASK: u32 = 0x00FF_FFFF;

/// One colour swatch in a lane's context menu. Square, and large enough to
/// hit without the ten of them outgrowing the menu's width.
pub const SWATCH_SIZE: f32 = 18.0;

/// A stored `0xRRGGBB` lane colour as an opaque `egui` colour.
///
/// Only the low 24 bits are read: a snapshot from a future version (or a
/// hostile one) that sets the top byte must not come out as a different
/// colour, let alone a transparent one.
pub const fn rgb(colour: u32) -> Color32 {
    let [_, r, g, b] = colour.to_be_bytes();
    Color32::from_rgb(r, g, b)
}

/// The palette colour for a lane position, cycling once past the end of the
/// palette — there may be more lanes than colours.
pub fn lane_swatch(index: usize) -> Color32 {
    LANE_SWATCHES[index % LANE_SWATCHES.len()]
}

/// The same colour as the `0xRRGGBB` the model stores, for handing to
/// `SeqModel::add_lane` / `set_colour`.
pub fn lane_swatch_rgb(index: usize) -> u32 {
    LANE_PALETTE[index % LANE_PALETTE.len()]
}

// ---- step colours ----

/// Pushes a colour towards white — the playhead's step, a hovered cell.
pub fn brighten(colour: Color32, amount: f32) -> Color32 {
    colour.lerp_to_gamma(Color32::WHITE, amount.clamp(0.0, 1.0))
}

/// Takes a colour down without tinting it — a muted lane's steps.
pub fn dim(colour: Color32, factor: f32) -> Color32 {
    colour.gamma_multiply(factor.clamp(0.0, 1.0))
}

/// The cell behind a step: alternating blocks of [`BEAT_STEPS`] columns, so
/// the eye can count beats without a ruler.
pub fn is_shaded_column(step: usize) -> bool {
    (step / BEAT_STEPS) % 2 == 1
}

/// The background of a step's cell, before anything is drawn in it.
pub fn cell_background(step: usize) -> Color32 {
    if is_shaded_column(step) { CELL_SHADED } else { CELL }
}

/// The dim outline an off step is drawn with, tinted by its lane so an empty
/// row still says whose it is.
pub fn off_step_stroke(lane: Color32) -> Stroke {
    Stroke::new(HAIRLINE, dim(lane, OFF_OUTLINE_FACTOR))
}

/// The fill of an on step. `audible` is false for a lane silenced by mute or
/// by another lane's solo; the velocity goes into the fill *height*
/// ([`fill_fraction`]), never into this colour.
pub fn on_step_fill(lane: Color32, audible: bool) -> Color32 {
    if audible { lane } else { dim(lane, MUTED_FACTOR) }
}

/// The same fill, for the step the playhead is on.
pub fn playhead_step_fill(lane: Color32, audible: bool) -> Color32 {
    brighten(on_step_fill(lane, audible), PLAYHEAD_BRIGHTEN)
}

/// How much of a cell's height an on step's fill occupies, from its velocity.
///
/// Monotonic over the whole MIDI range and never zero: velocity
/// [`Step::VELOCITY_MIN`] draws [`MIN_FILL_FRACTION`] of the cell and
/// [`Step::VELOCITY_MAX`] fills it. Out-of-range input is clamped rather than
/// trusted — a step can carry anything a snapshot put in it.
pub fn fill_fraction(velocity: u8) -> f32 {
    let velocity = velocity.clamp(Step::VELOCITY_MIN, Step::VELOCITY_MAX);
    let span = (Step::VELOCITY_MAX - Step::VELOCITY_MIN) as f32;
    let position = (velocity - Step::VELOCITY_MIN) as f32 / span;
    MIN_FILL_FRACTION + (1.0 - MIN_FILL_FRACTION) * position
}

// ---- egui's own styling ----

/// The [`Visuals`] the editor's widgets inherit: egui's dark theme with this
/// module's greys and accent substituted, so stock widgets (buttons, combo
/// boxes, menus) match the hand-drawn grid without being styled one by one.
pub fn visuals() -> Visuals {
    let mut visuals = Visuals::dark();

    visuals.panel_fill = PANEL;
    visuals.window_fill = PANEL;
    visuals.extreme_bg_color = WINDOW;
    visuals.faint_bg_color = CELL_SHADED;
    visuals.window_stroke = Stroke::new(HAIRLINE, GRID_LINE);
    visuals.window_corner_radius = CONTROL_CORNER;
    visuals.menu_corner_radius = CONTROL_CORNER;
    visuals.window_shadow = SHADOW;
    visuals.popup_shadow = SHADOW;
    visuals.hyperlink_color = ACCENT;
    visuals.warn_fg_color = WARN;
    visuals.text_cursor.stroke = Stroke::new(TEXT_CURSOR_WIDTH, ACCENT);
    // A slider then says its value twice: in its box, and in how far along
    // the rail the fill has reached. The three in the step popover are read
    // at a glance and set by dragging, which is what the rail is for.
    visuals.slider_trailing_fill = true;
    visuals.selection.bg_fill = dim(ACCENT, 0.55);
    // A selected control — a lit Mute, the playing pattern's button — keeps
    // its text at full brightness over that fill.
    visuals.selection.stroke = Stroke::new(HAIRLINE, TEXT);

    for (widget, fill) in [
        (&mut visuals.widgets.noninteractive, PANEL),
        (&mut visuals.widgets.inactive, CONTROL),
        (&mut visuals.widgets.hovered, CONTROL_HOVER),
        (&mut visuals.widgets.active, CONTROL_ACTIVE),
        (&mut visuals.widgets.open, CONTROL_HOVER),
    ] {
        widget.bg_fill = fill;
        widget.weak_bg_fill = fill;
        widget.bg_stroke = Stroke::new(HAIRLINE, GRID_LINE);
        widget.corner_radius = CONTROL_CORNER;
        widget.fg_stroke = Stroke::new(HAIRLINE, TEXT);
        // Widgets growing under the pointer would pull the grid's columns out
        // of line with the lane headers.
        widget.expansion = 0.0;
    }
    // Everything above is a control's own text. This is the rest: a label
    // beside a drag value, a heading inside a menu, the status line at the
    // end of the top bar. egui takes a *non-interactive* widget's foreground
    // as the colour for all of it, so this one line is the editor's whole
    // text hierarchy — and the reason `override_text_color` is left `None`,
    // since a value there would flatten the two back into one tone.
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(HAIRLINE, TEXT_DIM);

    visuals
}

/// Installs [`visuals`] and this module's spacing on a context. Called once
/// per window, from the editor's `build`.
///
/// The plugin is dark whatever the desktop is doing, so the theme is pinned
/// and *both* of egui's styles are written: were the host or the OS to flip
/// the preference mid-session, the light style it switched to would otherwise
/// be egui's stock one.
pub fn apply(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Dark);
    ctx.all_styles_mut(|style| {
        style.visuals = visuals();

        // The spacing egui lays out *with*, as against the geometry this
        // module hands `grid_math`: the grid, the lane headers and the top
        // bar are placed rect by rect, but a menu, a dropdown and the step
        // popover lay themselves out, and they have to come out at the same
        // rhythm as the rest of the window rather than at egui's stock one.
        let spacing = &mut style.spacing;
        spacing.item_spacing = egui::vec2(ITEM_SPACING, ITEM_SPACING);
        spacing.window_margin = egui::Margin::same(PANEL_PADDING as i8);
        spacing.menu_margin = egui::Margin::same(PANEL_PADDING as i8);
        spacing.menu_spacing = ITEM_SPACING;
        spacing.button_padding = CONTROL_PADDING;
        spacing.icon_spacing = ITEM_SPACING;
        spacing.interact_size = egui::vec2(CONTROL_MIN_WIDTH, CONTROL_HEIGHT);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The palette is `state`'s, converted — not a second list — and it keeps
    /// handing out colours past its end so a tenth lane is not black.
    #[test]
    fn the_palette_mirrors_state_and_cycles() {
        assert_eq!(LANE_SWATCHES.len(), LANE_PALETTE.len());
        for (index, &colour) in LANE_PALETTE.iter().enumerate() {
            let expected =
                Color32::from_rgb((colour >> 16) as u8, (colour >> 8) as u8, colour as u8);
            assert_eq!(lane_swatch(index), expected, "swatch {index}");
            assert_eq!(lane_swatch_rgb(index), colour, "rgb {index}");
        }

        let len = LANE_SWATCHES.len();
        for index in 0..len {
            assert_eq!(lane_swatch(index + len), lane_swatch(index), "wrap at {index}");
            assert_eq!(lane_swatch(index + 3 * len), lane_swatch(index), "wrap thrice at {index}");
            assert_eq!(lane_swatch_rgb(index + len), lane_swatch_rgb(index));
        }

        assert!(LANE_SWATCHES.iter().all(Color32::is_opaque), "lane colours are opaque");
    }

    /// A stored colour reaches the screen as itself, and a stray top byte
    /// (a future version's flag, a corrupt snapshot) changes nothing.
    #[test]
    fn rgb_reads_only_the_low_twenty_four_bits() {
        assert_eq!(rgb(0x12_34_56), Color32::from_rgb(0x12, 0x34, 0x56));
        assert_eq!(rgb(0xFF_12_34_56), rgb(0x12_34_56));
        assert_eq!(rgb(0), Color32::BLACK);
        assert_eq!(rgb(0xFF_FF_FF), Color32::WHITE);
    }

    /// Velocity → fill height: the ends are pinned, the middle rises, and
    /// nothing outside 1..=127 escapes the range.
    #[test]
    fn fill_fraction_spans_the_cell_monotonically() {
        assert_eq!(fill_fraction(Step::VELOCITY_MIN), MIN_FILL_FRACTION);
        assert_eq!(fill_fraction(Step::VELOCITY_MAX), 1.0);

        // Out of range below and above, including the 0 a packed step can
        // never hold but a raw `Step(0).velocity()` would have clamped.
        assert_eq!(fill_fraction(0), fill_fraction(Step::VELOCITY_MIN));
        assert_eq!(fill_fraction(255), fill_fraction(Step::VELOCITY_MAX));

        let mut previous = 0.0;
        for velocity in 0..=255u8 {
            let fraction = fill_fraction(velocity);
            assert!(fraction >= previous, "velocity {velocity} went backwards");
            assert!(
                (MIN_FILL_FRACTION..=1.0).contains(&fraction),
                "velocity {velocity} left the cell: {fraction}"
            );
            previous = fraction;
        }

        // Strictly rising where it matters, or the grid could not show an
        // edit of one velocity step.
        for velocity in Step::VELOCITY_MIN..Step::VELOCITY_MAX {
            assert!(
                fill_fraction(velocity + 1) > fill_fraction(velocity),
                "velocity {velocity} and {} draw the same",
                velocity + 1
            );
        }

        // Half velocity is about half a cell, above the floor.
        let middle = fill_fraction(64);
        assert!((0.45..=0.6).contains(&middle), "mid velocity fills {middle}");
    }

    /// Column shading alternates in blocks of a beat, starting unshaded, so
    /// step 0 of every bar is on the same side of the pattern.
    #[test]
    fn column_shading_alternates_every_beat() {
        for step in 0..BEAT_STEPS {
            assert!(!is_shaded_column(step), "step {step}");
            assert_eq!(cell_background(step), CELL);
        }
        for step in BEAT_STEPS..2 * BEAT_STEPS {
            assert!(is_shaded_column(step), "step {step}");
            assert_eq!(cell_background(step), CELL_SHADED);
        }
        // A bar of 16 and a 4096-step lane both land where they started.
        for step in [16, 64, 4080] {
            assert_eq!(is_shaded_column(step), is_shaded_column(0), "step {step}");
        }
        assert_ne!(CELL, CELL_SHADED, "the two shades must be distinguishable");
    }

    /// The step colours differ in the ways the grid relies on: a lit step is
    /// its lane colour, the playhead's is brighter, a silenced one dimmer,
    /// and an off step's outline is dimmer still than all of them.
    #[test]
    fn step_colours_are_ordered_by_brightness() {
        let lane = lane_swatch(0);

        let lit = on_step_fill(lane, true);
        let silenced = on_step_fill(lane, false);
        let playing = playhead_step_fill(lane, true);
        let outline = off_step_stroke(lane).color;

        assert_eq!(lit, lane, "an audible step is drawn in its lane's colour");
        assert!(playing.intensity() > lit.intensity(), "the playhead's step must stand out");
        assert!(silenced.intensity() < lit.intensity(), "a muted lane must read as muted");
        assert!(outline.intensity() < silenced.intensity(), "off steps are the dimmest");
        assert_eq!(off_step_stroke(lane).width, HAIRLINE);

        // The ordering is the palette's, not one colour's: every lane in the
        // grid has to read the same way.
        for (index, swatch) in LANE_SWATCHES.iter().enumerate() {
            let lit = on_step_fill(*swatch, true);
            let silenced = on_step_fill(*swatch, false);
            let outline = off_step_stroke(*swatch).color;
            assert!(playhead_step_fill(*swatch, true).intensity() > lit.intensity(), "{index}");
            assert!(silenced.intensity() < lit.intensity(), "lane {index} mutes to nothing");
            assert!(outline.intensity() < silenced.intensity(), "lane {index} outline");
        }
    }

    /// An off step's outline is a *tint* of its lane rather than a shadow
    /// around the cell: on every palette colour its strongest channel clears
    /// both of the cell shades it is drawn against, the shaded columns
    /// included ([`OFF_OUTLINE_FACTOR`]).
    #[test]
    fn off_step_outlines_read_against_both_cell_shades() {
        // The levels a tint has to beat, and by enough to be seen at one
        // point wide — the cell shades are greys, so one channel says it.
        let background = CELL.r().max(CELL_SHADED.r());
        const MARGIN: u8 = 16;

        for (index, swatch) in LANE_SWATCHES.iter().enumerate() {
            let outline = off_step_stroke(*swatch).color;
            let peak = outline.r().max(outline.g()).max(outline.b());
            assert!(
                peak >= background + MARGIN,
                "lane {index}'s off outline is lost in the cell: {peak} against {background}"
            );
        }
    }

    /// The chrome is built out of the step cell: its controls are a cell
    /// tall, and the top bar is one of them inside its padding. That is what
    /// lines the three bands of the window up, so it is a sum rather than a
    /// number anyone has to keep in step by hand.
    // The constants are the subject: these asserts are here so a change to
    // one of them fails the suite with its numbers, which a `const` block
    // (clippy's suggestion) could neither format nor report per test.
    #[allow(clippy::assertions_on_constants)]
    #[test]
    fn the_chrome_is_built_from_the_cell_height() {
        assert_eq!(CONTROL_HEIGHT, CELL_HEIGHT);
        assert_eq!(TOP_BAR_HEIGHT, CONTROL_HEIGHT + 2.0 * PANEL_PADDING);
        assert!(TOP_BAR_HEIGHT > CONTROL_HEIGHT, "the bar's padding has to fit around it");
        assert!(2.0 * CONTROL_PADDING.y < CONTROL_HEIGHT, "a control is all padding");
        assert_eq!(CELL_PITCH, CELL_WIDTH + CELL_GAP);
        assert_eq!(ROW_PITCH, CELL_HEIGHT + ROW_GAP);
    }

    /// The lane header's column holds the fields it pins, by construction:
    /// [`HEADER_WIDTH`] is their sum plus the panel's padding, and each of
    /// them has room for its text once the control's own padding is out of
    /// it.
    // The constants are the subject: these asserts are here so a change to
    // one of them fails the suite with its numbers, which a `const` block
    // (clippy's suggestion) could neither format nor report per test.
    #[allow(clippy::assertions_on_constants)]
    #[test]
    fn the_header_column_holds_its_fields() {
        let fields = [
            HEADER_GRIP_WIDTH,
            HEADER_DOT_WIDTH,
            HEADER_NAME_WIDTH,
            HEADER_KEY_WIDTH,
            HEADER_LENGTH_WIDTH,
            HEADER_RATE_WIDTH,
            HEADER_VOLUME_WIDTH,
            HEADER_TOGGLE_WIDTH,
            HEADER_TOGGLE_WIDTH,
        ];
        let summed: f32 = fields.iter().sum::<f32>() + HEADER_FIELD_GAP * (fields.len() - 1) as f32;
        assert!((HEADER_FIELDS_WIDTH - summed).abs() < 0.01, "{HEADER_FIELDS_WIDTH} != {summed}");
        assert!(
            HEADER_WIDTH - 2.0 * PANEL_PADDING >= HEADER_FIELDS_WIDTH,
            "the fields do not fit the column: {HEADER_FIELDS_WIDTH} in {HEADER_WIDTH}"
        );

        // Every field that draws text through a control: a field narrower
        // than its own padding would show none of it.
        for (name, width) in [
            ("key", HEADER_KEY_WIDTH),
            ("length", HEADER_LENGTH_WIDTH),
            ("rate", HEADER_RATE_WIDTH),
            ("mute/solo", HEADER_TOGGLE_WIDTH),
        ] {
            assert!(width > 2.0 * CONTROL_PADDING.x, "the {name} field is all padding");
        }
        assert!(fields.iter().all(|&width| width > 0.0), "every field is drawable");
    }

    /// Clamping, so a caller cannot produce a colour outside the range by
    /// over- or under-shooting.
    #[test]
    fn brighten_and_dim_clamp_their_amounts() {
        let lane = lane_swatch(4);

        assert_eq!(brighten(lane, 5.0), Color32::WHITE);
        assert_eq!(brighten(lane, -1.0), lane);
        assert_eq!(dim(lane, 5.0), lane);
        assert_eq!(dim(lane, -1.0), lane.gamma_multiply(0.0));
    }

    /// The style a window opens with is this module's, not egui's default
    /// dark theme — the one check that needs a context, and it needs no
    /// display to make one.
    #[test]
    fn the_context_takes_this_palette() {
        let ctx = egui::Context::default();
        apply(&ctx);

        let style = ctx.global_style();
        assert!(style.visuals.dark_mode);
        assert_eq!(style.visuals.panel_fill, PANEL);
        assert_eq!(style.visuals.widgets.inactive.bg_fill, CONTROL);
        assert_eq!(style.visuals.window_shadow, SHADOW);
        assert_eq!(style.visuals.popup_shadow, SHADOW);

        // The text hierarchy: a control says its own text at full
        // brightness, everything else is secondary, and no override is left
        // in place to collapse the two.
        assert_eq!(style.visuals.override_text_color, None);
        assert_eq!(style.visuals.widgets.inactive.fg_stroke.color, TEXT);
        assert_eq!(style.visuals.widgets.hovered.fg_stroke.color, TEXT);
        assert_eq!(style.visuals.selection.stroke.color, TEXT);
        assert_eq!(style.visuals.widgets.noninteractive.fg_stroke.color, TEXT_DIM);
        assert_eq!(style.visuals.text_color(), TEXT_DIM, "a plain label is secondary text");

        // The spacing a menu or the step popover lays itself out with.
        assert_eq!(style.spacing.item_spacing, egui::vec2(ITEM_SPACING, ITEM_SPACING));
        assert_eq!(style.spacing.button_padding, CONTROL_PADDING);
        assert_eq!(style.spacing.interact_size.y, CONTROL_HEIGHT);
        assert_eq!(style.spacing.menu_margin, egui::Margin::same(PANEL_PADDING as i8));

        // Light stays dark: a theme flip must not hand the editor egui's
        // stock palette.
        ctx.set_theme(egui::Theme::Light);
        assert_eq!(ctx.global_style().visuals.panel_fill, PANEL);
    }
}
