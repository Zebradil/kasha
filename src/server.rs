//! Binary-cache HTTP endpoints + authenticated ingest + /status + /metrics.
//!
//! GET/HEAD (unauthenticated, LAN): /nix-cache-info, /<hash>.narinfo,
//! /nar/<file>, /roots/<flake>/<gen>.json, /status, /metrics (Prometheus
//! text format). 404 on miss — the consumer's substituter list falls back to
//! remote/upstream (no pull-through).
//!
//! PUT (bearer or basic auth): same object paths. Every ingested narinfo must
//! carry a signature from a trusted key or a content address that reproduces
//! its store path (ADR: box holds no signing key). PUT of a manifest marks the
//! gen local-origin for mirror-up.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::io::Read;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tiny_http::{Header, Method, Request, Response, Server};

use crate::manifest::Manifest;
use crate::narinfo::{NarInfo, PubKey};
use crate::store::Store;

/// Worker-reported sync state surfaced at /status.
#[derive(Default)]
pub struct Status {
    /// flake -> (last sync, gap count).
    pub flakes: HashMap<String, (SystemTime, usize)>,
    pub pending_mirror_up: usize,
}

/// Monotonic counters surfaced at /metrics; reset on restart, which
/// Prometheus `rate()` handles.
#[derive(Default)]
pub struct Counters {
    pub narinfo_hits: AtomicU64,
    pub narinfo_misses: AtomicU64,
    pub ingest_accepted: AtomicU64,
    pub ingest_rejected: AtomicU64,
    pub ingest_unauthorized: AtomicU64,
    pub sweep_deleted: AtomicU64,
}

fn bump(c: &AtomicU64) {
    c.fetch_add(1, Ordering::Relaxed);
}

pub struct App {
    pub store: Store,
    pub keys: Vec<PubKey>,
    /// Write token; None disables all writes.
    pub token: Option<String>,
    pub status: Mutex<Status>,
    pub counters: Counters,
}

impl App {
    fn authorized(&self, req: &Request) -> bool {
        let Some(token) = &self.token else {
            return false;
        };
        let Some(h) = req
            .headers()
            .iter()
            .find(|h| h.field.equiv("Authorization"))
        else {
            return false;
        };
        let v = h.value.as_str();
        if let Some(t) = v.strip_prefix("Bearer ") {
            return t == token;
        }
        // netrc-driven `nix copy` sends Basic <base64(user:token)>.
        if let Some(b64) = v.strip_prefix("Basic ")
            && let Ok(creds) = data_encoding::BASE64.decode(b64.trim().as_bytes())
            && let Ok(s) = std::str::from_utf8(&creds)
        {
            return s.split_once(':').map(|(_, p)| p == token).unwrap_or(false);
        }
        false
    }
}

/// Serve until the listener dies, with at most `max_inflight` requests in
/// flight.
///
/// A response is written synchronously to its client, so an in-flight request
/// occupies its slot for the whole transfer: one NAR to one slow client can
/// hold a slot for minutes. Requests beyond the cap wait, health probes
/// included — so the cap has to exceed the concurrency a real client fleet
/// produces (nix opens `http-connections`, 25 by default, per builder), not
/// the box's core count.
///
/// The cap covers responses, not sockets: tiny_http keeps accepting and
/// parsing behind a blocked `acquire`, so a connection flood still queues in
/// its reader. Shedding those needs a depth probe tiny_http does not expose.
///
/// ponytail: a hard cap; slots are only reclaimed when the client finishes or
/// its TCP connection dies (tiny_http sets no write timeout, so a suspended
/// laptop holds a slot until keepalive reaps it). Non-blocking I/O is the
/// upgrade path if the cap is ever reached in anger.
pub fn serve(app: Arc<App>, listen: &str, max_inflight: usize) -> Result<()> {
    let server = Server::http(listen).map_err(|e| anyhow::anyhow!("bind {listen}: {e}"))?;
    tracing::info!(
        listen,
        max_inflight,
        objects = app.store.len(),
        "kasha serving"
    );
    let slots = Arc::new(Slots::new(max_inflight));
    let mut failures = 0u32;
    loop {
        let req = match server.recv() {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "accept failed");
                failures += 1;
                if failures >= MAX_ACCEPT_FAILURES {
                    anyhow::bail!("listener failed {failures} times in a row: {e}");
                }
                // A wedged listener errors instantly; without this the loop
                // spins a core and floods the log.
                std::thread::sleep(std::time::Duration::from_millis(100));
                continue;
            }
        };
        failures = 0;
        let slot = slots.acquire();
        let app = app.clone();
        // Handlers block on I/O rather than recurse, so they need far less
        // than the 8 MiB default a full cap would reserve.
        let spawned = std::thread::Builder::new()
            .stack_size(512 * 1024)
            .spawn(move || {
                let _slot = slot;
                handle(&app, req);
            });
        if let Err(e) = spawned {
            // Slot released with the closure; shed the request rather than
            // unwinding the accept loop, which would take the server down.
            tracing::error!(error = %e, "spawn failed, shedding request");
        }
    }
}

/// Consecutive accept failures tolerated before `serve` gives up and lets the
/// supervisor restart the process.
const MAX_ACCEPT_FAILURES: u32 = 100;

/// A counting semaphore over the in-flight request slots.
struct Slots {
    used: Mutex<usize>,
    freed: Condvar,
    max: usize,
}

impl Slots {
    fn new(max: usize) -> Self {
        Slots {
            used: Mutex::new(0),
            freed: Condvar::new(),
            max: max.max(1),
        }
    }

    fn acquire(self: &Arc<Self>) -> Slot {
        let mut used = self
            .freed
            .wait_while(self.used.lock().unwrap(), |used| *used >= self.max)
            .unwrap();
        *used += 1;
        Slot(Arc::clone(self))
    }
}

/// Releases its slot on drop, so a panicking handler cannot leak one.
struct Slot(Arc<Slots>);

impl Drop for Slot {
    fn drop(&mut self) {
        *self.0.used.lock().unwrap() -= 1;
        self.0.freed.notify_one();
    }
}

fn respond<R: Read>(req: Request, resp: Response<R>) {
    let method = req.method().clone();
    let url = req.url().to_string();
    let code = resp.status_code().0;
    if let Err(e) = req.respond(resp) {
        tracing::debug!(error = %e, "client went away");
    }
    tracing::debug!(%method, url, code, "request");
}

fn text(code: u32, body: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_string(body).with_status_code(code as u16)
}

pub fn handle(app: &App, mut req: Request) {
    let url = req.url().to_string();
    let path = url.split('?').next().unwrap_or("").trim_start_matches('/');
    match req.method() {
        Method::Get | Method::Head => respond_get(app, req, path),
        Method::Put => {
            if !app.authorized(&req) {
                tracing::warn!(path, "unauthorized write");
                bump(&app.counters.ingest_unauthorized);
                // Challenge lets libcurl (nix copy + netrc) retry with Basic
                // when it did not send credentials preemptively.
                let resp = text(401, "unauthorized").with_header(
                    Header::from_bytes("WWW-Authenticate", "Basic realm=\"kasha\"").unwrap(),
                );
                return respond(req, resp);
            }
            match ingest(app, &mut req, path) {
                Ok(msg) => {
                    bump(&app.counters.ingest_accepted);
                    respond(req, text(201, &msg))
                }
                Err(e) => {
                    tracing::warn!(path, error = %e, "rejected ingest");
                    bump(&app.counters.ingest_rejected);
                    respond(req, text(400, &format!("{e:#}")))
                }
            }
        }
        _ => respond(req, text(405, "method not allowed")),
    }
}

fn respond_get(app: &App, req: Request, path: &str) {
    match path {
        "nix-cache-info" => {
            return respond(
                req,
                text(
                    200,
                    "StoreDir: /nix/store\nWantMassQuery: 1\nPriority: 10\n",
                ),
            );
        }
        "status" => {
            let st = app.status.lock().unwrap();
            let body = serde_json::json!({
                "objects": app.store.len(),
                "store_bytes": app.store.disk_usage(),
                "pending_mirror_up": st.pending_mirror_up,
                "flakes": st.flakes.iter().map(|(f, (t, gaps))| {
                    let t = humantime::format_rfc3339_seconds(*t).to_string();
                    (f.clone(), serde_json::json!({"last_sync": t, "gaps": gaps}))
                }).collect::<serde_json::Map<_, _>>(),
            });
            let resp = Response::from_string(body.to_string())
                .with_header(Header::from_bytes("Content-Type", "application/json").unwrap());
            return respond(req, resp);
        }
        "metrics" => {
            let mut body = String::new();
            metrics(app, &mut body).expect("writing to a String cannot fail");
            let resp = Response::from_string(body).with_header(
                Header::from_bytes("Content-Type", "text/plain; version=0.0.4").unwrap(),
            );
            return respond(req, resp);
        }
        _ => {}
    }
    let file = object_path(&app.store, path).filter(|p| p.is_file());
    if path.ends_with(".narinfo") {
        bump(if file.is_some() {
            &app.counters.narinfo_hits
        } else {
            &app.counters.narinfo_misses
        });
    }
    match file {
        Some(p) => match std::fs::File::open(&p) {
            Ok(f) => respond(req, Response::from_file(f)),
            Err(e) => {
                tracing::error!(path, error = %e, "open failed");
                respond(req, text(500, "io error"))
            }
        },
        None => respond(req, text(404, "not found")),
    }
}

/// Prometheus text exposition (format 0.0.4). Leaves out `store_bytes`: it
/// walks the whole store, which a scrape every few seconds should not do.
fn metrics(app: &App, o: &mut String) -> std::fmt::Result {
    use std::fmt::Write;
    fn head(o: &mut String, name: &str, kind: &str, help: &str) -> std::fmt::Result {
        writeln!(o, "# HELP {name} {help}\n# TYPE {name} {kind}")
    }
    let secs = |t: SystemTime| t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let n = |c: &AtomicU64| c.load(Ordering::Relaxed);
    let c = &app.counters;

    head(o, "kasha_objects", "gauge", "Narinfos in the box's index.")?;
    writeln!(o, "kasha_objects {}", app.store.len())?;

    {
        let st = app.status.lock().unwrap();
        head(
            o,
            "kasha_pending_mirror_up",
            "gauge",
            "Local-origin generations not yet mirrored up.",
        )?;
        writeln!(o, "kasha_pending_mirror_up {}", st.pending_mirror_up)?;
        head(
            o,
            "kasha_flake_last_sync_timestamp_seconds",
            "gauge",
            "End of the last mirror-down cycle that covered the flake.",
        )?;
        for (f, (t, _)) in &st.flakes {
            let f = label(f);
            writeln!(
                o,
                "kasha_flake_last_sync_timestamp_seconds{{flake=\"{f}\"}} {}",
                secs(*t)
            )?;
        }
        head(
            o,
            "kasha_flake_gaps",
            "gauge",
            "Closure paths in the flake's manifests that no source could supply.",
        )?;
        for (f, (_, gaps)) in &st.flakes {
            writeln!(o, "kasha_flake_gaps{{flake=\"{}\"}} {gaps}", label(f))?;
        }
    }

    head(
        o,
        "kasha_narinfo_requests_total",
        "counter",
        "Narinfo GET/HEAD requests by result.",
    )?;
    writeln!(
        o,
        "kasha_narinfo_requests_total{{result=\"hit\"}} {}",
        n(&c.narinfo_hits)
    )?;
    writeln!(
        o,
        "kasha_narinfo_requests_total{{result=\"miss\"}} {}",
        n(&c.narinfo_misses)
    )?;
    head(
        o,
        "kasha_ingest_requests_total",
        "counter",
        "PUT requests by result.",
    )?;
    for (result, v) in [
        ("accepted", &c.ingest_accepted),
        ("rejected", &c.ingest_rejected),
        ("unauthorized", &c.ingest_unauthorized),
    ] {
        writeln!(
            o,
            "kasha_ingest_requests_total{{result=\"{result}\"}} {}",
            n(v)
        )?;
    }

    // Absent until the store's first sweep: 0 would read as "stale since 1970".
    if let Some(t) = app.store.last_sweep() {
        head(
            o,
            "kasha_last_sweep_timestamp_seconds",
            "gauge",
            "Start of the last box GC sweep attempt.",
        )?;
        writeln!(o, "kasha_last_sweep_timestamp_seconds {}", secs(t))?;
    }
    head(
        o,
        "kasha_sweep_deleted_objects_total",
        "counter",
        "Narinfos and NARs deleted by box GC sweeps since start.",
    )?;
    writeln!(
        o,
        "kasha_sweep_deleted_objects_total {}",
        n(&c.sweep_deleted)
    )
}

/// Escape a Prometheus label value; flake names come from remote manifests.
fn label(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

/// Map a URL path onto a store file, refusing traversal.
fn object_path(store: &Store, path: &str) -> Option<std::path::PathBuf> {
    if let Some(hash) = path.strip_suffix(".narinfo") {
        return store.narinfo_path(hash).ok();
    }
    if let Some(file) = path.strip_prefix("nar/") {
        return store.nar_path(file).ok();
    }
    if let Some(rest) = path.strip_prefix("roots/") {
        let (flake, file) = rest.split_once('/')?;
        let gen_id = file.strip_suffix(".json")?;
        return store.manifest_path(flake, gen_id).ok();
    }
    None
}

fn ingest(app: &App, req: &mut Request, path: &str) -> Result<String> {
    if let Some(hash) = path.strip_suffix(".narinfo") {
        let mut raw = Vec::new();
        req.as_reader().read_to_end(&mut raw)?;
        let text = std::str::from_utf8(&raw).context("narinfo not utf-8")?;
        let info = NarInfo::parse(text)?;
        anyhow::ensure!(
            info.store_hash() == hash,
            "narinfo StorePath hash {} does not match URL {}",
            info.store_hash(),
            hash
        );
        anyhow::ensure!(
            info.verify(&app.keys),
            "neither a trusted signature nor a valid content address on {} (sig keys: {:?}, CA: {:?})",
            info.store_path,
            info.sig_key_names(),
            info.ca
        );
        app.store.put_narinfo(&info, &raw)?;
        tracing::info!(hash, "ingested narinfo");
        return Ok("narinfo stored".into());
    }
    if let Some(file) = path.strip_prefix("nar/") {
        let n = app.store.put_nar(file, req.as_reader())?;
        tracing::info!(file, bytes = n, "ingested nar");
        return Ok("nar stored".into());
    }
    if let Some(rest) = path.strip_prefix("roots/") {
        let (flake, file) = rest
            .split_once('/')
            .context("manifest path must be roots/<flake>/<gen>.json")?;
        let gen_id = file
            .strip_suffix(".json")
            .context("manifest must be .json")?;
        let mut raw = Vec::new();
        req.as_reader().read_to_end(&mut raw)?;
        let m = Manifest::parse(&raw)?;
        anyhow::ensure!(
            m.flake == flake && m.gen_id == gen_id,
            "manifest fields ({}, {}) do not match URL ({flake}, {gen_id})",
            m.flake,
            m.gen_id
        );
        app.store.put_manifest(&m, &raw)?;
        app.store.mark_local_origin(flake, gen_id)?;
        tracing::info!(flake, gen = gen_id, "ingested manifest (local-origin)");
        return Ok("manifest stored".into());
    }
    anyhow::bail!("no such write endpoint: /{path}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use data_encoding::BASE64;
    use ed25519_dalek::{Signer, SigningKey};

    /// Spin a real server on an ephemeral port; return its base URL.
    fn spawn(app: Arc<App>) -> String {
        let server = Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_ip().unwrap();
        std::thread::spawn(move || {
            while let Ok(req) = server.recv() {
                handle(&app, req)
            }
        });
        format!("http://{addr}")
    }

    fn signed_narinfo(sk: &SigningKey) -> (String, String) {
        let body = "StorePath: /nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-pkg-1.0\n\
URL: nar/deadbeef.nar.xz\n\
Compression: xz\n\
NarHash: sha256:00g966jlz9h37xkb9pmr3rc700i4k19mkyqm3gmwvlaik16qam5x\n\
NarSize: 42\n\
References: \n";
        let n = NarInfo::parse(body).unwrap();
        let sig = sk.sign(n.fingerprint().as_bytes());
        (
            format!("{body}Sig: test-1:{}\n", BASE64.encode(&sig.to_bytes())),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
        )
    }

    #[test]
    fn slots_bound_concurrency_and_release_on_drop() {
        use std::time::Duration;
        let slots = Arc::new(Slots::new(1));
        let held = slots.acquire();
        let (tx, rx) = std::sync::mpsc::channel();
        let waiting = Arc::clone(&slots);
        std::thread::spawn(move || {
            let _slot = waiting.acquire();
            let _ = tx.send(());
        });
        assert!(
            rx.recv_timeout(Duration::from_millis(200)).is_err(),
            "acquire handed out more slots than the cap"
        );
        drop(held);
        rx.recv_timeout(Duration::from_secs(5))
            .expect("dropping a slot did not release it");
    }

    #[test]
    fn label_escapes_quote_backslash_newline() {
        assert_eq!(label("a\"b\\c\nd"), r#"a\"b\\c\nd"#);
    }

    #[test]
    fn push_then_substitute_roundtrip() {
        let dir = std::env::temp_dir().join(format!("kasha-srv-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sk = SigningKey::from_bytes(&[3u8; 32]);
        let pk = format!("test-1:{}", BASE64.encode(sk.verifying_key().as_bytes()));
        let app = Arc::new(App {
            store: Store::open(&dir).unwrap(),
            keys: vec![PubKey::parse(&pk).unwrap()],
            token: Some("s3cret".into()),
            status: Mutex::new(Status::default()),
            counters: Counters::default(),
        });
        let base = spawn(app.clone());
        let agent = ureq::Agent::new_with_defaults();

        // Cache info + 404 miss.
        let info = agent
            .get(format!("{base}/nix-cache-info"))
            .call()
            .unwrap()
            .body_mut()
            .read_to_string()
            .unwrap();
        assert!(info.contains("StoreDir: /nix/store"));
        assert_eq!(
            agent
                .get(format!("{base}/nope.narinfo"))
                .call()
                .unwrap_err_status(),
            404
        );

        let (narinfo, hash) = signed_narinfo(&sk);

        // Unauthenticated PUT refused.
        assert_eq!(
            agent
                .put(format!("{base}/{hash}.narinfo"))
                .send(narinfo.as_bytes())
                .unwrap_err_status(),
            401
        );

        // Authenticated PUTs: nar, narinfo, manifest.
        let auth = ("Authorization", "Bearer s3cret");
        agent
            .put(format!("{base}/nar/deadbeef.nar.xz"))
            .header(auth.0, auth.1)
            .send(&b"NARBYTES"[..])
            .unwrap();
        agent
            .put(format!("{base}/{hash}.narinfo"))
            .header(auth.0, auth.1)
            .send(narinfo.as_bytes())
            .unwrap();
        let manifest = serde_json::json!({
            "version": 3, "flake": "znix", "gen": "main-abc-x", "branch": "main",
            "attr": "x", "timestamp": "2026-08-20T10:00:00Z",
            "closure": ["/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-pkg-1.0"],
        });
        agent
            .put(format!("{base}/roots/znix/main-abc-x.json"))
            .header(auth.0, auth.1)
            .send(manifest.to_string().as_bytes())
            .unwrap();
        assert!(app.store.is_local_origin("znix", "main-abc-x"));

        // Basic auth (netrc-style: any user, password = token).
        let basic = data_encoding::BASE64.encode(b"nix:s3cret");
        agent
            .put(format!("{base}/nar/other.nar.xz"))
            .header("Authorization", format!("Basic {basic}"))
            .send(&b"X"[..])
            .unwrap();

        // Tampered narinfo (bad sig) refused.
        let bad = narinfo.replace("NarSize: 42", "NarSize: 43");
        assert_eq!(
            agent
                .put(format!("{base}/{hash}.narinfo"))
                .header(auth.0, auth.1)
                .send(bad.as_bytes())
                .unwrap_err_status(),
            400
        );

        // Substitute path: GET narinfo + nar back byte-identical.
        let got = agent
            .get(format!("{base}/{hash}.narinfo"))
            .call()
            .unwrap()
            .body_mut()
            .read_to_string()
            .unwrap();
        assert_eq!(got, narinfo);
        let mut nar = Vec::new();
        agent
            .get(format!("{base}/nar/deadbeef.nar.xz"))
            .call()
            .unwrap()
            .body_mut()
            .as_reader()
            .read_to_end(&mut nar)
            .unwrap();
        assert_eq!(nar, b"NARBYTES");

        // Traversal refused.
        assert_eq!(
            agent
                .get(format!("{base}/nar/..%2f..%2fetc"))
                .call()
                .unwrap_err_status(),
            404
        );

        // Status JSON.
        let st = agent
            .get(format!("{base}/status"))
            .call()
            .unwrap()
            .body_mut()
            .read_to_string()
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&st).unwrap();
        assert_eq!(v["objects"], 1);

        // Metrics: counters reflect the requests above.
        app.status.lock().unwrap().flakes.insert(
            "znix".into(),
            (
                UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000),
                2,
            ),
        );
        let resp = agent.get(format!("{base}/metrics")).call().unwrap();
        assert_eq!(resp.status(), 200);
        let m = resp.into_body().read_to_string().unwrap();
        for line in [
            "# TYPE kasha_objects gauge",
            "kasha_objects 1",
            "kasha_pending_mirror_up 0",
            "kasha_flake_last_sync_timestamp_seconds{flake=\"znix\"} 1700000000",
            "kasha_flake_gaps{flake=\"znix\"} 2",
            "# TYPE kasha_narinfo_requests_total counter",
            "kasha_narinfo_requests_total{result=\"hit\"} 1",
            "kasha_narinfo_requests_total{result=\"miss\"} 1",
            "kasha_ingest_requests_total{result=\"accepted\"} 4",
            "kasha_ingest_requests_total{result=\"rejected\"} 1",
            "kasha_ingest_requests_total{result=\"unauthorized\"} 1",
            "kasha_sweep_deleted_objects_total 0",
        ] {
            assert!(m.lines().any(|l| l == line), "missing {line:?} in:\n{m}");
        }
        assert!(!m.contains("kasha_last_sweep_timestamp_seconds"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    trait ErrStatus {
        fn unwrap_err_status(self) -> u16;
    }
    impl<E> ErrStatus for std::result::Result<ureq::http::Response<ureq::Body>, E>
    where
        E: std::fmt::Debug,
    {
        fn unwrap_err_status(self) -> u16 {
            match self {
                Ok(r) => panic!("expected error status, got {}", r.status()),
                Err(e) => {
                    let s = format!("{e:?}");
                    // ureq 3 returns Error::StatusCode(u16) for non-2xx.
                    s.split(|c: char| !c.is_ascii_digit())
                        .find(|t| t.len() == 3)
                        .and_then(|t| t.parse().ok())
                        .unwrap_or_else(|| panic!("no status in {s}"))
                }
            }
        }
    }
}
