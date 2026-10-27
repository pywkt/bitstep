# bitstep

A step sequencer plugin for [Bitwig Studio](https://www.bitwig.com/) that sends
notes, not sound. Put it in front of a **Drum Machine** (or any instrument)
and program beats on a grid, like Bitwig's Stepwise, but with:

- **As many lanes as you want**, each playing one note (one Drum Machine pad).
  New instances start with eight lanes on consecutive pads from C1.
- **Its own length and rate per lane** (1/1 to 1/64, triplet and dotted), for
  polymeters and polyrhythms.
- **Per-step settings:** velocity, pan, pitch (±12 semitones), ratchets
  (up to 8 repeats) and chance.
- **A parameter panel** to draw a whole lane's velocity, pan, pitch, chance or
  ratchet with the mouse (Shift for straight lines).
- **A pitch mode** for drawing pitch curves right on the grid.
- **A volume per lane** (−36 to +12 dB).
- **Pitched samples:** a *Sample pad* lane setting, so Drum Machine samples
  follow the pitch without setting a root key on every pad.
- **8 patterns**, switchable from an automatable *Pattern Select* parameter.
- Edit while the transport runs; everything is saved with your project.

bitstep makes no sound itself, and only plays while Bitwig's transport is
running. Its output can be recorded into clips like any other notes.

## Status

- **CLAP** plugin, **Linux** (x86_64) only for now.
- Version 0.1.0. No prebuilt downloads yet: you build it from source, which
  [takes a few minutes](docs/install.md).

## Getting started

1. [Install bitstep](docs/install.md): build it, copy it into `~/.clap`, and
   load it in Bitwig.
2. Read the [user guide](docs/user-guide.md): the grid, lanes, patterns, the
   parameter panel, pitch, recording into clips and keyboard shortcuts.

Quick version, if you already have [Rust](https://rustup.rs) and the build
dependencies listed in the install guide:

```sh
cargo xtask bundle bitstep --release
mkdir -p ~/.clap && cp target/bundled/bitstep.clap ~/.clap/
```

Using the **Flatpak** version of Bitwig? Build with
`./scripts/build-in-docker.sh` instead; [here's why](docs/install.md#1-build).

## Building and testing

```sh
cargo build --workspace            # build everything
cargo test --workspace             # run the test suite
cargo xtask bundle bitstep --release   # make target/bundled/bitstep.clap
```

The Rust version is pinned in `rust-toolchain.toml`; rustup installs it
automatically. The plugin is built with
[nice-plug](https://crates.io/crates/nice-plug) (a fork of NIH-plug) and
[egui](https://github.com/emilk/egui).

## License

[ISC](LICENSE)
