//! Cera: stylus-first handwritten notes for Linux tablets.

mod application;
mod canvas;
mod dialogs;
mod note;
mod palm;
mod sidebar;
mod swatch;
mod vault;
mod window;

use std::io::IsTerminal;
use std::sync::OnceLock;

use gtk::{gio, glib, prelude::*};
use tracing_subscriber::filter::{LevelFilter, Targets};
use tracing_subscriber::prelude::*;

use crate::application::CeraApplication;

pub const APP_ID: &str = "io.github.troikoss.Cera";

fn main() -> glib::ExitCode {
    init_logging();
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "starting");

    gio::resources_register_include!("cera.gresource").expect("embedded GResource is valid");

    CeraApplication::new().run()
}

/// `CERA_DEBUG=1` turns on the tile/memory/frame-time overlay.
pub fn debug_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("CERA_DEBUG").is_some_and(|v| v == "1"))
}

/// Logs to stderr, filtered by `RUST_LOG` (`target=level` pairs, e.g. `cera=debug`).
/// Without it: our crates at `info`, everything else at `warn`.
fn init_logging() {
    let default = || {
        Targets::new()
            .with_default(LevelFilter::WARN)
            .with_target("cera", LevelFilter::INFO)
    };
    let filter = match std::env::var("RUST_LOG") {
        Ok(spec) => spec.parse().unwrap_or_else(|err| {
            eprintln!("ignoring invalid RUST_LOG={spec:?}: {err}");
            default()
        }),
        Err(_) => default(),
    };
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_ansi(std::io::stderr().is_terminal()),
        )
        .with(filter)
        .init();
}
