//! Headless construction smoke: viewer must build cleanly with an offline
//! address (no display, no real connection). This catches missing fields,
//! borrow-checker regressions and obvious panics in startup.

use btelem_viewer::app::ViewerApp;
use std::sync::Arc;

#[test]
fn constructs_offline() {
    let args = Arc::new(btelem_viewer::Args {
        addr: "127.0.0.1:1".to_string(),
        connect_timeout: 0.0,
        file: None,
    });
    let _app = ViewerApp::new(args);
}
