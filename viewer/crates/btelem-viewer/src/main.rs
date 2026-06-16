//! btelem-viewer entry point.
//!
//! See the library crate for the bulk of the implementation.

#![forbid(unsafe_code)]

use std::sync::Arc;

use btelem_viewer::{app::ViewerApp, Args};
use clap::Parser;

fn main() -> eframe::Result<()> {
    let args = Args::parse();

    // Fail fast for paths that don't exist.
    if let Some(ref path) = args.file {
        if !path.exists() {
            eprintln!("error: file not found: {}", path.display());
            std::process::exit(1);
        }
    }
    if let Some(ref path) = args.layout {
        if !path.exists() {
            eprintln!("error: layout file not found: {}", path.display());
            std::process::exit(1);
        }
    }

    let opts = eframe::NativeOptions::default();
    eframe::run_native(
        "btelem-viewer",
        opts,
        Box::new(move |_cc| Ok(Box::new(ViewerApp::new(Arc::new(args))))),
    )
}
