//! nice-plug's bundler, as this workspace's `cargo xtask` entry point.
//!
//! `cargo xtask bundle bitstep --release` → `target/bundled/bitstep.clap`.

fn main() -> nice_plug_xtask::Result<()> {
    nice_plug_xtask::main()
}
