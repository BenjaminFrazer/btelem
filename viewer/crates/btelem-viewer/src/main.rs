//! btelem-viewer entry point.
//!
//! See the library crate for the bulk of the implementation.

#![forbid(unsafe_code)]

use std::sync::Arc;

use btelem_viewer::{app::ViewerApp, Args};
use clap::Parser;

fn main() -> eframe::Result<()> {
    let args = Args::parse();
    let opts = eframe::NativeOptions::default();
    eframe::run_native(
        "btelem-viewer",
        opts,
        Box::new(move |_cc| Ok(Box::new(ViewerApp::new(Arc::new(args))))),
    )
}
