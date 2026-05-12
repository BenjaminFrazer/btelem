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
}
