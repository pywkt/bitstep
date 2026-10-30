# bitstep user guide

bitstep is a step sequencer that sends notes, not sound. Put it in front of an
instrument (usually Bitwig's **Drum Machine**) and it plays that instrument
from a grid of steps. It works like Bitwig's Stepwise, but with as many lanes
as you want, and each lane can have its own length and speed.

If it isn't installed yet, start with [Installing bitstep](install.md).

## Quick start

1. Make a new instrument track with a **Drum Machine** and load drum samples
   on its first pads (C1 upwards).
2. Add **bitstep** to the same track, *before* the Drum Machine.
3. Open bitstep's window. You'll see eight lanes, **Lane 1** to **Lane 8**,
   on notes C1 to G1, one per Drum Machine pad in order. Each is 32 steps
   long: two bars of 1/16 steps.
4. Press play in Bitwig and click some steps. They play on the next pass, so
   you don't need to stop the transport to edit.

bitstep only plays while Bitwig's transport is running. Its steps line up with
the song position, so step 1 of a 16-step 1/16 lane always falls on the first
beat of the bar.

## The window

```
┌──────────────────────────────────────────────────────────────────────┐
│ ‹[1]…[8]› [Steps][Pitch][Song][Grid][Undo][Redo] … [Params][+ Lane]   │  top bar
├──────────────────────────────┬───────────────────────────────────────┤
│ ⠿● Lane 1  C1  16  1/16 ▾ M S │ ■ □ □ □ ■ □ □ □ ■ □ □ □ ■ □ □ □ ┃      │
│ ⠿● Lane 2  C#1 16  1/16 ▾ M S │ □ □ □ □ ■ □ □ □ □ □ □ □ ■ □ □ □ ┃      │
├──────────────────────────────┼───────────────────────────────────────┤
│ Lane 1                        │ ▇ ▁ ▁ ▁ ▅ ▁ ▁ ▁ ▇ ▁ ▁ ▁ ▃ ▁ ▁ ▁        │  parameter
│ [Velocity][Pan][Pitch]…       │                                       │  panel
└──────────────────────────────┴───────────────────────────────────────┘
  lane headers                    step grid (┃ = end handle)
```

- **Top bar:** the pattern buttons with page arrows either side, the
  **Steps / Pitch** mode switch, **Song** and, in song mode, **Grid** (see
  [Song mode](#song-mode)), **Undo / Redo** (see [Undo](#undo)), a status
  line, **Params** (shows or hides the parameter panel), **+ Lane**, and
  **Follow**.
- **Lane headers** (left): one row per lane, with its name, note, length,
  rate, and mute/solo.
- **Step grid** (right): one row of steps per lane. Every group of four steps
  is shaded alternately so you can count beats, and a brighter line marks
  every 16 steps (one bar of 1/16 steps; at other rates it is still every 16
  steps). The cell under the mouse is outlined, so you can see where a click
  will land.
- **Parameter panel** (bottom): the velocity, pan, pitch, chance or ratchet
  of every step in the selected lane, as bars you can draw. See
  [Parameter panel](#parameter-panel).

The window can be resized. It reopens at the size you left it until you close
the project.

## Steps

| To do this | Do this |
|---|---|
| Turn a step on | Click it. |
| Turn a step off | Click an on step and release without moving. |
| Turn a run of steps on | Click an off step and drag sideways along the row. |
| Turn a run of steps off | Click an on step and drag sideways along the row. |
| Change a step's velocity | Click an on step and drag **up** (louder) or **down** (softer). |
| Edit velocity, pan, pitch, ratchet and chance | **Right-click** an on step. |
| Edit them across a whole lane | Use the [parameter panel](#parameter-panel). |
| Draw pitch | Switch to [pitch mode](#pitch), or use the parameter panel. |

Velocity is shown by how full the cell is: a quiet step is a thin sliver and a
full-velocity step fills the cell. New steps start at velocity 100.

Turning a step off keeps its settings. When you turn it back on, it returns
with the velocity, pan, ratchet and chance it had before.

### Step settings

Right-click an on step to open its settings:

- **Velocity:** 1–127. The same value a vertical drag on the cell sets, for
  when you want an exact number.
- **Pan:** from L100 through C (centre) to R100. You can also type `C`, `L50`
  or `R50`. Pan is sent to the instrument as a per-note pan, so it only does
  something if the instrument listens to per-note pan.
- **Pitch:** −24 to +24 semitones. See [Pitch](#pitch).
- **Ratchet:** 1–8. Plays the step as that many quick repeats inside its
  length, for rolls and flams.
- **Chance:** 0–100%. How likely the step is to play each time it comes
  around. At 100% it always plays.

Changes apply as you move the sliders. Close the box by clicking outside it or
pressing Escape.

Small markers show these settings on the grid: a step with a ratchet has small
ticks along its top edge (one per repeat), a step with chance below 100% has a
dot in its corner, and a step with a pitch has a thin bar on its left edge,
above the middle for higher and below it for lower.

Each note sounds for half of its step, or half of each repeat when ratcheted.

## Pitch

Each step can be pitched up or down by up to 24 semitones (two octaves). The
step still plays its lane's note, so it still triggers the same Drum Machine
pad; the pitch is sent alongside it as a per-note pitch change, and the pad's
instrument retunes that one hit.

To draw pitch, click **Pitch** in the top bar. The grid then shows each step's
pitch as a bar up or down from a centre line, with its value written on it.

| To do this | Do this |
|---|---|
| Set a step's pitch | Click the step and drag **up** (higher) or **down** (lower). Every 4 points of movement is one semitone. |
| Draw across several steps | Keep the button down and move sideways: each step you pass gets the pitch at the pointer's height. |
| Draw an even slope | Hold **Shift** while dragging: the steps from where you pressed to where the pointer is get a straight ramp, from the pressed step's pitch to the pointer's height. |
| Reset a step to 0 | Click it without dragging. |

Drawing only changes pitch: it never turns steps on or off, so you can draw a
pitch curve first and add steps later. Click **Steps** to go back to editing
steps. You can also set an exact pitch from a step's settings (right-click),
or draw it in the [parameter panel](#parameter-panel).

Pitch only changes the sound if the instrument responds to per-note pitch. If
a pitched step sounds the same as an unpitched one, the instrument on that
pad is ignoring it.

### Pitching samples

A sample dropped onto a Drum Machine pad plays in a Sampler, which only
follows pitch when its **keytrack** is on (the small keyboard icon with two
arrows above the waveform). To pitch a sample:

1. Click the pad to show its Sampler and turn on **keytrack**. Leave its
   **root key** at C3.
2. Make sure the lane's **Sample pad** setting is ticked (right-click the
   lane's name). It's on for every new lane.

With keytrack on, the Sampler plays the pad's low note (C1, say) well below
the sample's own pitch. **Sample pad** makes up the difference, so unpitched
steps sound like the original sample and pitched steps move from there.
Dragging a new sample onto the Sampler's waveform keeps keytrack on.

Pads with keytrack off ignore pitch entirely, and **Sample pad** makes no
difference to them. Turn **Sample pad** off for lanes playing synth drums
(such as Bitwig's E-drum kits) or other instruments, or they'll play two
octaves too high. Projects saved before this setting existed load with it off,
so they sound as they did.

## Parameter panel

The panel under the grid shows one setting for every step of one lane, as a
row of bars, so you can see and draw a whole lane's velocities (or pan, pitch,
chance, ratchet) at once.

1. **Pick the lane:** click its name in the lane headers, or click any of its
   steps. The selected lane is marked with a blue edge, and its name is shown
   at the left of the panel.
2. **Pick the setting:** click **Velocity**, **Pan**, **Pitch**, **Chance**
   or **Ratchet** at the left of the panel.
3. **Draw:**

| To do this | Do this |
|---|---|
| Set a step | Click its bar at the height you want. The top of the panel is the highest value, the bottom the lowest. |
| Draw across steps | Keep the button down and move sideways: each step you pass gets the value at the pointer's height. |
| Draw a straight line | Hold **Shift** while dragging: the steps from where you pressed to the pointer get an even ramp. |
| Reset a step | **Right-click** its bar (velocity 100, pan centre, pitch 0, chance 100%, ratchet 1). |

Pan and pitch bars grow up or down from a centre line; the others grow up from
the bottom. Pointing at a bar shows its exact value under the buttons.

You can draw on any step, including ones that are off: their bars are drawn
dimmer, and drawing never turns steps on or off. Whatever you draw on an off
step is kept, so when you turn it on it plays with those settings. An empty
step you haven't drawn on starts with the usual defaults (velocity 100, pan
centre, chance 100%, no ratchet).

Click **Params** in the top bar to hide the panel and give the grid more room.

## Lanes

Each lane plays one note (for a Drum Machine, one pad). New instances start
with eight lanes on consecutive notes from C1, matching the Drum Machine's
pads.

### The lane header

| Control | What it does |
|---|---|
| **Name and colour dot** | Drag to move the lane up or down; drag to the top or bottom edge of the list to scroll it. Press Escape while dragging to put it back. Double-click the name to rename the lane: the whole name is selected, so just type. Press Enter or click anywhere else to keep the new name, or Escape to keep the old one. Right-click for the [lane menu](#lane-menu). |
| **Note** (e.g. `C1`) | The note the lane plays. Drag up/right to raise it, or use the mouse wheel over it. Hold Shift while dragging for finer control. Click to type a note name such as `F#1` or `Bb2`; while typing, Up/Down step it by a semitone. |
| **Length** | How many steps the lane has, from 1 to 4096. Drag or click to type; while typing, Up/Down add or remove a step. A new length takes effect at once, so a playing lane may jump to a different step. |
| **Rate** | How long each step is: 1/1 down to 1/64, each also in triplet (T) and dotted (D) versions. |
| **Volume** (e.g. `0 dB`) | Turns the whole lane up or down, from −36 dB to +12 dB. Drag up or down, or click to type (Up/Down step 1 dB). Right-click to reset to 0 dB. See [Lane volume](#lane-volume). |
| **M** | Mute the lane. |
| **S** | Solo the lane. When any lane is soloed, only soloed lanes play. Mute wins over solo. |

You can also change a lane's length by dragging the **end handle**, the grey
bar just after its last step.

Making a lane shorter hides the steps past the end rather than deleting them.
Make it longer again and they come back.

### Lane volume

The **Volume** field turns a lane up or down, for example to bring up quiet
hi-hats. It works on top of the steps' velocities: bitstep sends it with every
note as a per-note volume, which Bitwig treats like the **Gain** expression in
its note editor. It is saved with the project.

It goes up to +12 dB, the most a per-note volume can add. If a sound needs
more than that, raise the volume of its pad inside the Drum Machine. As with
pan and pitch, the instrument has to respond to per-note volume.

### Cutting a sound off (choke)

bitstep only sends notes; it can't stop a sound that is already ringing. To
make a sound cut itself off when it plays again, or have a closed hi-hat cut
an open one, set it up in the Drum Machine: right-click a pad and set its
**Choke** options.

### Lanes that play the same note

Several lanes can play the same note, for example a kick lane and a copy of it
with a different pattern. If two of them hit at exactly the same moment, the
note plays once, at the velocity of the upper lane. If one lane hits while
the note from another is still sounding, the old note stops and the new one
starts.

### Polymeter and polyrhythm

Each lane loops on its own length at its own rate, so lanes don't have to
match. For example:

- A 16-step kick against a 15-step snare drifts by one step each bar and lines
  up again every 15 bars.
- A hi-hat lane at 1/16T plays triplets against straight 1/16 lanes.

Because lanes follow the song position, changing a lane's length while playing
can make it jump to a different step straight away. That's expected: the
lane is now counting a different cycle.

### Lane menu

Right-click a lane's name to open its menu:

- **Name:** type a new name (up to 64 characters).
- **Colour:** pick one of ten colours.
- **channel:** the MIDI channel the lane sends on, 1–16.
- **Sample pad:** retunes the lane for a keytracked sample with root key C3.
  On for new lanes; turn it off for synth drums. See
  [Pitching samples](#pitching-samples).
- **duplicate:** adds a copy of the lane directly below it, in every
  pattern.
- **delete:** removes the lane from every pattern. Click it twice to
  confirm. [Undo](#undo) brings it back.
- **clear:** turns off every step in the current pattern and resets their
  settings. Click it twice to confirm.
- **invert:** flips every step on↔off in the current pattern.
- **randomise:** turns each step on or off at random (50/50) in the current
  pattern. This replaces the lane's current on/off pattern.
- **nudge −1 / +1:** rotates the lane's steps one place left or right.
- **Fill:** replaces the lane's on/off pattern in the current pattern with a
  preset rhythm, keeping each step's settings:
  - **All:** every step.
  - **1/8, 1/4, 1/2:** eighth, quarter or half notes. **1/4** is four on the
    floor.
  - **1/1:** the first beat of every bar.
  - **Off-beats:** the eighth notes between the beats, the classic open
    hi-hat.
  - **2 & 4:** beats 2 and 4 of every bar, the usual snare or clap.

  Fills are counted in beats from the lane's first step, so they work at any
  rate: at 1/16, **1/4** turns on every 4th step, and at 1/8 every 2nd. A fill
  that doesn't line up with the lane's rate, such as **1/8** on a 1/4 lane or
  anything on a dotted rate except **All**, is greyed out. Bars are counted
  as 4/4.

### Adding lanes

Click **+ Lane** in the top bar. The new lane appears at the bottom, playing
the note above the previous last lane, with 32 steps at 1/16. You can have up
to 256 lanes.

## Patterns

bitstep holds **64 patterns**. The top bar shows 8 pattern buttons at a time:
click **‹** or **›** either side of them to see the previous or next 8
(1–8, 9–16, and so on, wrapping round at the ends). Click a pattern button to
switch.

- The grid switches to the new pattern straight away, so you can edit it.
- While playing, each lane switches when it reaches the end of its current
  step, so the change stays in time. Until every lane has switched, the
  button you clicked shows a blue number, and the old pattern's button stays
  lit.
- While stopped, the switch is immediate.

What belongs to a pattern and what is shared between patterns:

| Different in each pattern | Shared by all patterns |
|---|---|
| Steps and their settings, length, rate | Name, colour, note, channel, mute, solo |

New patterns start empty.

### Copying a pattern

Right-click a pattern button to copy that pattern into another slot. Under
**Copy pattern N to**, click the number of the slot to copy into (all 64 are
listed, 8 to a row). Every lane's
steps, step settings, length and rate are copied; lane settings like name,
note and volume are shared by all patterns anyway.

- Copying into an empty pattern happens straight away.
- If the target pattern has notes, the number turns red with a **?**. Click
  it again to replace that pattern. [Undo](#undo) puts it back.
- The copy is independent: editing it doesn't change the original. You stay
  on the pattern you copied from; click the new one to switch to it.
- A pattern's [name](#song-mode) stays with its slot and isn't copied.

### Switching patterns from Bitwig

The pattern is a Bitwig parameter called **Pattern Select** (1–64), so you
can automate it in the arranger, or map a modulator or controller to it. It's
bitstep's only automatable parameter. In song mode the song picks what plays,
and Pattern Select only picks which pattern's steps you see.

## Song mode

Song mode plays your patterns from a song grid, following Bitwig's timeline.
The grid has one row for each of the 64 patterns and one column per bar. You
place a pattern by clicking in its row, and anything placed in the same bars
on different rows plays together. Nothing is recorded, so when you change a
pattern, every part of the song that uses it changes too.

1. Click **Song** in the top bar. Song mode turns on, the song grid takes the
   place of the steps, and a **Grid** button appears next to **Song**.
2. Click in a pattern's row at the bar where it should start. The pattern is
   placed there as a block, as long as the pattern itself (see below).
3. Press play in Bitwig. The song starts at bar 1 of Bitwig's arranger.

```
 [Loop] 12 bars │ 1       5       9       13
 ───────────────┼─────────────────────────────
 Pattern 1      │ [1 ───][1 ───]
 Pattern 2      │                [2 ───]
 Pattern 3      │ [3 ────────────────────]     ← plays along with 1 and 2
 …
 Pattern 64     │
```

**Working with blocks:**

- **Place:** click an empty bar in a row. The block is that row's pattern.
  Press and drag to the right to draw it longer in one go.
- **Move:** drag a block left or right. Drag it up or down to another row to
  play a different pattern there instead.
- **Resize:** drag a block's right edge. A block longer than its pattern plays
  the pattern again from its first step each time it comes round (thin lines
  inside the block mark where). A block shorter than its pattern cuts it off.
- **Delete:** right-click a block.
- **Escape** while dragging puts the block back where it was.
- **Select a pattern:** click its name on the left. The row is highlighted,
  and it's the pattern you see if you click **Grid** to switch to the steps.
- **Name a pattern:** double-click its name on the left and type, for example
  `Verse` or `Chorus`. Press Enter or click anywhere else to keep it, or
  Escape to keep the old name. Delete the name (or leave it as
  `Pattern N`) to go back to the number. Names are up to 64 characters,
  saved with the project, and shown when you hover over the pattern's
  button in the top bar.
- **Edit a pattern:** double-click one of its blocks, or select it and click
  **Grid**. The steps view opens on that pattern while the song keeps
  playing. Click **Grid** to go back to the grid.

**How long a block is when placed:** as long as its pattern's longest lane,
rounded up to whole bars. A pattern of 16 sixteenth notes is one bar of 4/4,
32 sixteenths (or 32 eighths) is two (or four) bars, and a pattern whose
longest lane is 12 sixteenths still takes a whole bar. Lanes start at 32
sixteenths, so a pattern you haven't shortened is placed two bars long. The
length of each pattern is shown next to its name. If the block would run into
the next block in its row, it's cut short to fit. Once placed, a block keeps
its length even if you change the pattern later; drag its edge to change it.

**Patterns playing together:** every lane of every pattern that has a block
in a bar plays. If two of them hit the same note at the same moment, it
sounds once. For example, put a kick-and-snare pattern on one row and a
hi-hat pattern on another, over the same bars.

What else to know:

- **The song ends after its last block.** It then starts again from bar 1.
  To stop there instead, click **Loop** in the top-left corner of the grid:
  it changes to **End**, and after the last block bitstep stays silent until
  you start again from earlier in the song. Click it again to go back to
  looping. The corner also shows how many bars the song is.
- **Each block starts its pattern from the first step**, even when the same
  pattern plays in the block before it.
- **Bars follow Bitwig's time signature** (4 beats in 4/4, 3 in 3/4 or 6/8).
  Keep one time signature for the whole song.
- **While it plays**, a blue line runs down the grid at the current bar, and
  the blocks sounding are lit and outlined. The names of the patterns that
  are sounding turn blue. With **Follow** on, the grid scrolls to keep the
  line in view. Scroll the grid with the mouse wheel (rows) and Shift + wheel
  (bars).
- **Grid switches between the song grid and the steps** while song mode is
  on, without changing what plays: click it to edit the selected pattern's
  steps, and again to come back. It's only there in song mode. Turning song
  mode off goes back to the steps, and the pattern buttons play one pattern
  at a time again.
- **Editing while it plays:** in the steps view, the pattern buttons only
  choose which pattern you edit. The playhead shows in the steps only while
  the pattern on screen is sounding in the song. A pattern button's number is
  blue while that pattern is sounding and you're looking at another one.
- **Looping and jumping** in Bitwig's arranger work: bitstep plays whichever
  bar the arranger is on.
- **Projects saved with a typed song order** (from older versions of bitstep)
  open with that order laid out on the grid, one block after another from
  bar 1. A silent bar (`0`) at the very end of an old order is dropped, since
  the song now ends with its last block.
- Click **Song** again to go back to playing one pattern at a time.

## Scrolling and follow

- **Mouse wheel:** scrolls lanes (or, in the song grid, patterns) up and
  down, when there are more than fit.
- **Shift + wheel** (or a horizontal scroll): scrolls steps (or bars) left
  and right.
- **Follow** (on by default): keeps the playhead in view while playing.
  Turn it off if you want to scroll freely while the transport runs.

The current step is highlighted as it plays.

## Undo

**Undo** and **Redo** in the top bar, or **Ctrl+Z** and **Ctrl+Shift+Z**
(or **Ctrl+Y**) while bitstep's window is in front, step back and forth
through your edits in bitstep:

- One drag is one step: a whole paint stroke, a length drag, a drawn curve
  in the parameter panel or moving or resizing a song block is undone in one
  go, and so is typing a lane or pattern name.
- Everything you edit in bitstep's window can be undone: steps and their
  settings, lane settings, adding, deleting, duplicating and moving lanes,
  clear/invert/randomise/nudge/fills, pattern copies and the song grid.
- **Mute and solo are not undo steps.** They're for playing, so undo leaves
  them as they are.
- **Pattern Select** belongs to Bitwig: use Bitwig's own undo for it.
- bitstep remembers the last 100 steps. The history starts fresh when the
  project is opened and isn't saved with it.

Bitwig's own undo doesn't reach changes made inside bitstep, and bitstep's
undo doesn't reach changes made in Bitwig.

## Keyboard shortcuts

While bitstep's window is in front, **Ctrl+Z** and **Ctrl+Shift+Z** / **Ctrl+Y**
are bitstep's [undo and redo](#undo). **Space** (play/stop), **F1–F12** and
other shortcuts held with **Ctrl** (save and so on) are passed on to Bitwig,
so you don't have to click back into Bitwig to start the transport. To use
Bitwig's undo, click into Bitwig first. Other keys stay with bitstep, and
nothing is passed on while you're typing a lane name (there, Ctrl+Z undoes
your typing).

## Playing along

Notes you play into bitstep's track (from a keyboard or a clip) pass straight
through to the instrument along with bitstep's own notes. So do sustain pedal,
mod wheel, pitch bend and other controller messages.

## Recording into a clip

Bitwig records the notes coming *into* a track, not the notes made by devices
on it. With bitstep on the same track as the Drum Machine, pressing record
captures nothing. To record bitstep's pattern as a clip, give it its own track:

1. Make a new instrument track (call it "bitstep") and put only **bitstep** on
   it. Move bitstep there from the drum track if it's already there.
2. On the drum track, open the track's input chooser (in the Inspector, or the
   I/O section of the mixer) and pick the **bitstep** track as its note input.
3. **Arm** the drum track. You should now hear the pattern through the kit
   when you press play.
4. Record as usual, into a clip launcher slot or the arranger.

After recording, **disarm** the drum track. Otherwise the clip and bitstep
play together and every hit sounds twice. To keep jamming live instead, leave
it armed and don't play the clip.

## Saving

Everything is saved with your Bitwig project: all lanes and all 64 patterns,
including steps hidden past a lane's end, the song grid, whether song mode
is on and whether the song loops, plus the selected pattern. The
window size, view, follow setting and scroll position are not saved.

## Limits

| | Maximum |
|---|---|
| Lanes | 256 |
| Steps per lane | 4096 |
| Patterns | 64 |
| Song length | 4096 bars |
| Blocks in a song | 4096 |
| Ratchet | 8 repeats |
| Pitch | ±24 semitones |

## Things to know

- **Note names use C3 = middle C (MIDI note 60).** If Bitwig is set to show
  middle C as C4, bitstep's names will read one octave lower than Bitwig's.
- **Pan, pitch and lane volume depend on the instrument.** If an instrument
  ignores per-note pan, pitch or volume, those settings have no effect.
- **The quietest velocity is 1, and it still plays.** A note can't have a
  velocity of 0 (in MIDI that means "note off"). How quiet velocity 1 sounds
  is up to the instrument: if quiet steps are still too loud, raise the
  instrument's velocity sensitivity, lower the lane's volume, or turn the step
  off or its chance down instead.
- **Linux only** for now.
