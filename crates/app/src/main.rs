//! Cera: stylus-first handwritten notes for Linux tablets.

mod application;
mod window;

use std::io::IsTerminal;

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
