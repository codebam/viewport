// SPDX-License-Identifier: GPL-3.0-or-later
//
// Viewport, in Rust. Ports src/main.c.
//
// This is the scaffold: the pieces below it are real and tested, but the
// Smithay backend is not wired up yet. See docs/RUST-REWRITE.md for the order
// of work and what is deliberately absent.

mod framing;

use anyhow::Result;
use tracing_subscriber::EnvFilter;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("VIEWPORT_LOG").unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    tracing::info!("viewport {} (smithay rewrite)", env!("CARGO_PKG_VERSION"));
    tracing::warn!("the compositor backend is not implemented yet");

    Ok(())
}
