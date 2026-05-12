//! xtask: headless soak / replay harness for the Rust viewer pipeline.
//!
//! Connects to a btelem TCP server, runs ingest into a MockStore for a
//! configurable duration, and emits a JSON metrics report on stdout:
//!
//! ```json
//! {
//!   "duration_s": 10.0,
//!   "channels": 8,
//!   "samples_per_channel": [12345, ...],
//!   "total_samples": 98760,
//!   "samples_per_sec": 9876.0,
//!   "store_revision": 98768,
//!   "rss_mb_start": 12.3,
//!   "rss_mb_end":   13.1,
//!   "rss_slope_mb_per_hour": 288.0,
//!   "query_p50_us": 14,
//!   "query_p99_us": 240
//! }
//! ```
//!
//! CI runs this with `--duration 5` as a smoke gate; nightly runs longer
//! with `--duration 600` for memory-leak detection.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use btelem_ingest::TcpSource;
use btelem_store::{ChannelKind, MockStore, Store};
use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "xtask", about)]
enum Cmd {
    /// Replay/soak: drive ingest from a btelem TCP server for N seconds and
    /// emit a JSON metrics report.
    Replay {
        /// Address of the btelem TCP server.
        #[arg(long, default_value = "127.0.0.1:4040")]
        addr: String,
        /// Total duration in seconds.
        #[arg(long, default_value_t = 5.0)]
        duration: f64,
        /// How many viewport queries per second to perform (simulates the GUI).
        #[arg(long, default_value_t = 60.0)]
        query_hz: f64,
        /// If set, spawn this binary as a subprocess instead of connecting to
        /// an already-running server. Path is treated as the counter-server
        /// binary; the address argument supplies the port.
        #[arg(long)]
        spawn: Option<String>,
        /// Number of entries the spawned counter server emits.
        #[arg(long, default_value_t = 5_000_000)]
        spawn_entries: u64,
    },
}

fn rss_mb() -> f64 {
    // Linux: read RSS from /proc/self/status.
    if let Ok(s) = std::fs::read_to_string("/proc/self/status") {
        for line in s.lines() {
            if let Some(rest) = line.strip_prefix("VmRSS:") {
                let kb: f64 = rest
                    .split_whitespace()
                    .next()
                    .and_then(|w| w.parse().ok())
                    .unwrap_or(0.0);
                return kb / 1024.0;
            }
        }
    }
    0.0
}

fn main() {
    let cmd = Cmd::parse();
    match cmd {
        Cmd::Replay {
            addr,
            duration,
            query_hz,
            spawn,
            spawn_entries,
        } => replay(addr, duration, query_hz, spawn, spawn_entries),
    }
}

fn replay(
    addr: String,
    duration_s: f64,
    query_hz: f64,
    spawn_path: Option<String>,
    spawn_entries: u64,
) {
    let _server_proc = spawn_path.map(|path| {
        let port = addr.rsplit(':').next().unwrap_or("4040");
        let mut cmd = Command::new(path);
        cmd.arg(port)
            .arg(spawn_entries.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().expect("spawn server");
        if let Some(stderr) = child.stderr.take() {
            thread::spawn(move || {
                let r = BufReader::new(stderr);
                for _ in r.lines().map_while(Result::ok) {}
            });
        }
        thread::sleep(Duration::from_millis(150));
        ChildGuard(child)
    });

    let store = MockStore::new();
    let rss_start = rss_mb();

    // Connect with retries.
    let handle = (0..40)
        .find_map(|_| {
            thread::sleep(Duration::from_millis(50));
            TcpSource::connect(&addr, store.clone(), None).ok()
        })
        .expect("connect failed after 2s of retries");

    // Run a query loop in lock-step with the soak window.
    let start = Instant::now();
    let dur = Duration::from_secs_f64(duration_s);
    let qperiod = Duration::from_secs_f64(1.0 / query_hz.max(1.0));
    let mut q_us: Vec<u128> = Vec::new();

    while start.elapsed() < dur {
        let qstart = Instant::now();
        if let Some((t0, t1)) = store.time_bounds() {
            let chs = store.channels();
            for c in &chs {
                match c.kind {
                    ChannelKind::Scalar => {
                        let _ = store.query_scalar(c.id, t0, t1, 1024);
                    }
                    ChannelKind::State { .. } => {
                        let _ = store.query_state(c.id, t0, t1);
                    }
                }
            }
        }
        q_us.push(qstart.elapsed().as_micros());
        let elapsed = qstart.elapsed();
        if elapsed < qperiod {
            thread::sleep(qperiod - elapsed);
        }
    }
    let elapsed = start.elapsed().as_secs_f64();

    // Stop ingest before measuring final state.
    let _ = handle.join();

    let chs = store.channels();
    let mut spc: Vec<u64> = Vec::new();
    let mut total: u64 = 0;
    for c in &chs {
        let n = match c.kind {
            ChannelKind::Scalar => store.query_scalar(c.id, 0, u64::MAX, usize::MAX).len() as u64,
            ChannelKind::State { .. } => store.query_state(c.id, 0, u64::MAX).len() as u64,
        };
        spc.push(n);
        total += n;
    }

    let rss_end = rss_mb();
    let slope = if elapsed > 0.0 {
        (rss_end - rss_start) * 3600.0 / elapsed
    } else {
        0.0
    };

    q_us.sort_unstable();
    let p = |frac: f64| -> u128 {
        if q_us.is_empty() {
            0
        } else {
            let idx = ((q_us.len() as f64) * frac).min(q_us.len() as f64 - 1.0) as usize;
            q_us[idx]
        }
    };

    println!(
        "{{\"duration_s\":{:.3},\"channels\":{},\"samples_per_channel\":{:?},\"total_samples\":{},\"samples_per_sec\":{:.1},\"store_revision\":{},\"rss_mb_start\":{:.2},\"rss_mb_end\":{:.2},\"rss_slope_mb_per_hour\":{:.2},\"query_p50_us\":{},\"query_p99_us\":{}}}",
        elapsed,
        chs.len(),
        spc,
        total,
        total as f64 / elapsed.max(1e-9),
        store.revision(),
        rss_start,
        rss_end,
        slope,
        p(0.50),
        p(0.99),
    );
}

struct ChildGuard(std::process::Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
