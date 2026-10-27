# Installing bitstep

bitstep is a CLAP plugin. Right now it is **Linux only** (x86_64) and there
are no prebuilt downloads, so you build it from source. It takes a few
minutes the first time.

## What you need

- **Bitwig Studio** with CLAP support (Bitwig added CLAP in version 4.3; any
  current version works).
- **Rust**, installed through [rustup](https://rustup.rs). You don't need to
  pick a version: the repository's `rust-toolchain.toml` pins one, and rustup
  downloads it automatically the first time you build.
- **A C toolchain and the X11/OpenGL development libraries** for the
  plugin window:

  ```sh
  # Arch / Manjaro
  sudo pacman -S --needed base-devel libx11 libxcb xcb-util-wm libxcursor libxkbcommon libglvnd

  # Debian / Ubuntu
  sudo apt install build-essential pkg-config libgl-dev libx11-xcb-dev libxcb1-dev \
    libxcb-dri2-0-dev libxcb-icccm4-dev libxcursor-dev libxkbcommon-dev \
    libxcb-shape0-dev libxcb-xfixes0-dev
  ```

## 1. Build

Get the source code (on GitHub, **Code → Download ZIP**, or `git clone` the
repository), then from its root folder run:

```sh
cargo xtask bundle bitstep --release
```

The finished plugin is `target/bundled/bitstep.clap` (a single file on Linux).

> **Using the Flatpak version of Bitwig, or a rolling-release distro like
> Arch?** A plugin only loads if it was built against a C library (glibc) no
> newer than the one Bitwig runs with. The Flatpak brings its own, older
> glibc, so a plugin built on an up-to-date system can fail to load. Build it
> in an Ubuntu 24.04 container instead (needs Docker; you don't need Rust
> installed for this). The result works with the Flatpak and with most Linux
> systems:
>
> ```sh
> ./scripts/build-in-docker.sh
> ```
>
> (Run it with `sudo` if your user isn't in the `docker` group.) The first run
> takes a while to set up the container. The plugin lands in the same place,
> `target/bundled/bitstep.clap`.

## 2. Install

Copy it into your user CLAP folder:

```sh
mkdir -p ~/.clap
cp target/bundled/bitstep.clap ~/.clap/
```

To install it for every user on the machine instead, copy it to
`/usr/lib/clap/` (needs `sudo`).

> **Developing bitstep?** Symlink instead of copying, so every rebuild is
> picked up without reinstalling:
> `ln -s "$PWD/target/bundled/bitstep.clap" ~/.clap/`

## 3. Load it in Bitwig

1. Open **Settings → Locations → Plug-in Locations** and check that `~/.clap`
   is scanned (it is by default).
2. Click **Rescan**, or restart Bitwig.
3. In the device browser, search for **bitstep**. It is a **note effect**, so
   it belongs *before* an instrument on a track, for example:
   **bitstep → Drum Machine**.
4. Click the device's plugin-window button to open the step editor.

If it installed correctly:

- bitstep is listed with the note effects, not with the instruments.
- On the track it shows a note input and a note output, and no audio.
- Its window opens with eight lanes and readable text.

Continue with the [user guide](user-guide.md).

## Updating

Pull the new source, rebuild (step 1), copy the file again (step 2), then
rescan in Bitwig or restart it. Bitwig caches plugin information, so a rescan
is the safest way to make sure it loads the new build.

Patterns saved in your projects carry over between versions.

## Uninstalling

```sh
rm ~/.clap/bitstep.clap
```

Projects that used bitstep will open with the device marked as missing.

## Troubleshooting

| Problem | What to try |
|---|---|
| bitstep doesn't appear in the browser | Check the file is at `~/.clap/bitstep.clap`, then rescan. Then look for `bitstep` in Bitwig's log, `~/.BitwigStudio/log/BitwigStudio.log`: `grep -i bitstep ~/.BitwigStudio/log/*.log` |
| The log says ``version `GLIBC_2.xx' not found`` | The plugin was built on a system with a newer C library than Bitwig uses. Build it in the container (see the note under [step 1](#1-build)), copy it again and rescan. |
| The build fails with a missing `xcb`, `X11` or `GL` library | Install the system packages listed under [What you need](#what-you-need). |
| The editor window opens but shows no text | The plugin was built without egui's bundled fonts. Build with the command in step 1 from an unmodified checkout. |
| Bitwig still runs the old version after an update | Rescan plug-ins or restart Bitwig. |
| No sound | bitstep makes no sound itself. Put an instrument (e.g. Drum Machine) *after* it on the same track and make sure the lanes' keys match pads that have samples. |

## Checking a build (developers)

To see the newest glibc a build needs (it must be no newer than the one
Bitwig runs with; the Flatpak's runtime 25.08 has 2.42):

```sh
objdump -T target/bundled/bitstep.clap | grep -o 'GLIBC_[0-9.]*' | sort -V | tail -1
```

To run the CLAP validator against a build:

```sh
clap-validator validate -x '^param-set-(events|no-cookies)$' target/bundled/bitstep.clap
```

It should exit 0. The two excluded tests are a false positive in
clap-validator 0.4.1: it only ever tries one random value for Pattern Select,
which happens to equal the default, so it reports that the value didn't
change. Use `validate <path>`
rather than `list plugins`, because 0.4.1's scanner doesn't see flat
`.clap` files on Linux.
