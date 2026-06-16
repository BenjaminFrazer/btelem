//! Library entry point so integration tests can construct `ViewerApp`.

#![forbid(unsafe_code)]

pub mod app;
pub mod layout;
pub mod plot_renderers;
pub mod view_state;

use clap::Parser;

#[derive(Parser, Debug, Clone)]
#[command(name = "btelem-viewer", about)]
pub struct Args {
    /// btelem TCP endpoint, e.g. 127.0.0.1:4040
    #[arg(long, default_value = "127.0.0.1:4040")]
    pub addr: String,

    /// Seconds to retry the initial connection. 0 = single attempt.
    #[arg(long, default_value_t = 5.0)]
    pub connect_timeout: f64,

    /// Open a .btlm capture file on startup instead of connecting.
    #[arg(long, value_name = "PATH")]
    pub file: Option<std::path::PathBuf>,

    /// Load a layout JSON file on startup (works with both live and file mode).
    #[arg(long, value_name = "PATH")]
    pub layout: Option<std::path::PathBuf>,

    /// Prefix for auto-generated capture filenames (default: "btelem").
    #[arg(long, value_name = "STR", default_value = "btelem")]
    pub capture_prefix: String,

    /// Suffix appended before the .btlm extension in auto-generated filenames.
    #[arg(long, value_name = "STR")]
    pub capture_suffix: Option<String>,

    /// Default directory for the capture save dialog.
    #[arg(long, value_name = "DIR")]
    pub save_dir: Option<std::path::PathBuf>,
}
