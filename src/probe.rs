//! Health probes on a listener of their own (`KASHA_PROBE_LISTEN`).
//!
//! The cache listener cannot answer probes reliably: it binds only after the
//! boot scan, and its accept loop stops reading requests while every slot is
//! held, so a probe there times out exactly when the box is busiest. This
//! listener binds before the scan and answers from its own thread, outside
//! the slot cap.
//!
//! - `GET /livez`: 200 unless the cache listener has waited [`STALL`] for a
//!   free slot, which means no response finished in that time.
//! - `GET /readyz`: 503 until the cache listener is bound, 200 after. It
//!   ignores a full slot cap on purpose: with one replica, NotReady turns a
//!   queue into an outage.

use anyhow::Result;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tiny_http::{Response, Server};

use crate::store::ScanProgress;

/// How long the accept loop may wait for a slot before /livez fails.
/// The kubelet's `failureThreshold` adds its own tolerance on top.
pub const STALL: Duration = Duration::from_secs(60);

pub struct Health {
    pub scan: ScanProgress,
    epoch: Instant,
    ready: AtomicBool,
    /// Milliseconds after `epoch` the accept loop started waiting for a slot,
    /// plus one; 0 while it is not waiting.
    waiting_since: AtomicU64,
}

impl Default for Health {
    fn default() -> Self {
        Health {
            scan: ScanProgress::default(),
            epoch: Instant::now(),
            ready: AtomicBool::new(false),
            waiting_since: AtomicU64::new(0),
        }
    }
}

impl Health {
    pub fn set_ready(&self) {
        self.ready.store(true, Ordering::Relaxed);
    }

    /// Brackets the accept loop's wait for a slot.
    pub fn waiting(&self, waiting: bool) {
        let v = if waiting {
            self.epoch.elapsed().as_millis() as u64 + 1
        } else {
            0
        };
        self.waiting_since.store(v, Ordering::Relaxed);
    }

    fn stalled_for(&self) -> Option<Duration> {
        match self.waiting_since.load(Ordering::Relaxed) {
            0 => None,
            since => Some(
                self.epoch
                    .elapsed()
                    .saturating_sub(Duration::from_millis(since - 1)),
            ),
        }
    }

    /// Status code and body for a probe path; None for an unknown path.
    pub fn answer(&self, path: &str) -> Option<(u16, String)> {
        match path {
            "/livez" => Some(match self.stalled_for() {
                Some(d) if d >= STALL => (
                    503,
                    format!("stalled: no request slot freed for {}s\n", d.as_secs()),
                ),
                _ => (200, "ok\n".into()),
            }),
            "/readyz" => Some(if self.ready.load(Ordering::Relaxed) {
                (200, "ok\n".into())
            } else {
                let (done, total) = self.scan.get();
                (503, format!("starting: indexed {done}/{total} narinfos\n"))
            }),
            _ => None,
        }
    }
}

/// Bind `listen` and answer probes from a thread of its own until the process
/// exits. Requests are answered one at a time: each is a few bytes from
/// atomics, and a second thread would only matter to a client that never
/// reads its response.
pub fn spawn(listen: &str, health: std::sync::Arc<Health>) -> Result<String> {
    let server =
        Server::http(listen).map_err(|e| anyhow::anyhow!("bind probe listener {listen}: {e}"))?;
    let addr = server.server_addr().to_string();
    tracing::info!(listen = addr, "probes listening");
    std::thread::spawn(move || {
        for req in server.incoming_requests() {
            let path = req.url().split('?').next().unwrap_or("").to_string();
            let (code, body) = health.answer(&path).unwrap_or((404, "not found\n".into()));
            let _ = req.respond(Response::from_string(body).with_status_code(code));
        }
    });
    Ok(addr)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readyz_reports_scan_until_ready() {
        let h = Health::default();
        h.scan.start(3);
        h.scan.advance();
        assert_eq!(
            h.answer("/readyz"),
            Some((503, "starting: indexed 1/3 narinfos\n".into()))
        );
        h.set_ready();
        assert_eq!(h.answer("/readyz"), Some((200, "ok\n".into())));
        assert_eq!(h.answer("/nope"), None);
    }

    #[test]
    fn livez_fails_only_after_a_long_wait() {
        let h = Health::default();
        assert_eq!(h.answer("/livez").unwrap().0, 200);
        h.waiting(true);
        assert_eq!(h.answer("/livez").unwrap().0, 200);
        // Pretend the wait began before STALL elapsed.
        let ago = STALL + Duration::from_secs(5);
        let h = Health {
            epoch: Instant::now() - ago,
            ..Health::default()
        };
        h.waiting_since.store(1, Ordering::Relaxed);
        let (code, body) = h.answer("/livez").unwrap();
        assert_eq!(code, 503);
        assert!(body.starts_with("stalled:"), "{body}");
        h.waiting(false);
        assert_eq!(h.answer("/livez").unwrap().0, 200);
    }

    #[test]
    fn serves_over_http() {
        let health = std::sync::Arc::new(Health::default());
        let addr = spawn("127.0.0.1:0", health.clone()).unwrap();
        let agent = ureq::Agent::new_with_defaults();
        let status = |p: &str| match agent.get(format!("http://{addr}{p}")).call() {
            Ok(r) => r.status().as_u16(),
            Err(ureq::Error::StatusCode(c)) => c,
            Err(e) => panic!("{e}"),
        };
        assert_eq!(status("/readyz"), 503);
        health.set_ready();
        assert_eq!(status("/readyz"), 200);
        assert_eq!(status("/livez"), 200);
        assert_eq!(status("/nope"), 404);
    }
}
