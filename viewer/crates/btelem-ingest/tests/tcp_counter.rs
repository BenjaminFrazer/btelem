//! Integration test: drive ingest against the C `btelem_test_counter_server`.
//!
//! Skipped if the binary isn't built (CI must `make build` first).

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::Duration;

use btelem_ingest::TcpSource;
use btelem_store::{ChannelKind, MockStore, Store};

fn server_path() -> Option<PathBuf> {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for _ in 0..5 {
        let candidate = p.join("build/btelem_test_counter_server");
        if candidate.is_file() {
            return Some(candidate);
        }
        if !p.pop() {
            break;
        }
    }
    None
}

fn pick_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn counter_server_round_trip() {
    let Some(server) = server_path() else {
        eprintln!("skipped: btelem_test_counter_server not built (run `make build`)");
        return;
    };

    let port = pick_port();
    let mut cmd = Command::new(&server);
    cmd.arg(port.to_string())
        .arg("100000")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let child = ChildGuard(cmd.spawn().expect("spawn server"));

    let addr = format!("127.0.0.1:{port}");
    let store = MockStore::new();
    let mut handle = None;
    for _ in 0..40 {
        thread::sleep(Duration::from_millis(50));
        if let Ok(h) = TcpSource::connect(&addr, store.clone()) {
            handle = Some(h);
            break;
        }
    }
    let handle = handle.expect("connect to counter server");

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut bounds = None;
    while std::time::Instant::now() < deadline {
        if let Some(b) = store.time_bounds() {
            if b.1 > b.0 {
                bounds = Some(b);
                break;
            }
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(bounds.is_some(), "no samples ingested");

    let chs = store.channels();
    assert_eq!(
        chs.len(),
        8,
        "expected 8 scalar channels, got {}",
        chs.len()
    );
    for c in &chs {
        assert!(c.path.starts_with("counters.c["), "path: {}", c.path);
        assert_eq!(c.kind, ChannelKind::Scalar);
    }

    for c in &chs {
        let (t0, t1) = bounds.unwrap();
        let bs = store.query_scalar(c.id, t0, t1 + 1, chs.len() * 1000);
        let mut prev = f64::NEG_INFINITY;
        for b in &bs {
            assert!(b.min >= prev, "channel {} non-monotonic", c.path);
            prev = b.max;
        }
    }

    drop(handle);
    drop(child);
}
