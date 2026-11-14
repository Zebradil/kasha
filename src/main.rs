mod gc;
mod manifest;
mod mirror;
mod narinfo;
mod pull;
mod remote;
mod retention;
mod server;
mod store;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use std::io::Read;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use manifest::Manifest;
use narinfo::PubKey;
use remote::{Remote, S3Remote};
use retention::Policy;

#[derive(Parser)]
#[command(name = "kasha", version, about = "net-local nix binary cache")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the box: cache HTTP server + mirror workers + GC timer.
    Serve {
        /// Store root (flat binary-cache layout).
        #[arg(long, env = "KASHA_DATA", default_value = "/kasha")]
        data: String,
        /// Listen address for the cache HTTP server.
        #[arg(long, env = "KASHA_LISTEN", default_value = "0.0.0.0:5000")]
        listen: String,
        /// Write token; writes are refused when unset.
        #[arg(long, env = "KASHA_TOKEN", hide_env_values = true)]
        token: Option<String>,
        /// Trusted narinfo signing keys (`name:base64`, comma/space separated).
        #[arg(long, env = "KASHA_TRUSTED_KEYS", required = true)]
        trusted_keys: String,
        /// Remote cache as s3://bucket?endpoint=…&region=… (enables mirroring).
        #[arg(long, env = "KASHA_REMOTE")]
        remote: Option<String>,
        /// Upstream caches tried after the remote cache, by mirror-down and
        /// by pull-through, comma separated.
        #[arg(
            long,
            env = "KASHA_UPSTREAMS",
            default_value = "https://cache.nixos.org"
        )]
        upstreams: String,
        /// Seconds to sleep between sync cycles (mirror-down, then mirror-up).
        #[arg(long, env = "KASHA_SYNC_INTERVAL", default_value = "300")]
        sync_interval_secs: u64,
        /// Minimum seconds between box GC sweeps, checked after each sync cycle.
        #[arg(long, env = "KASHA_GC_INTERVAL", default_value = "86400")]
        gc_interval_secs: u64,
        /// Cap on concurrent requests. Each in-flight response holds a slot
        /// for its whole transfer, so this bounds slow readers, not CPU.
        #[arg(
            long,
            env = "KASHA_MAX_INFLIGHT",
            default_value = "256",
            value_parser = clap::value_parser!(u64).range(1..)
        )]
        max_inflight: u64,
        /// `Priority` advertised in /nix-cache-info; nix asks substituters
        /// with a lower number first.
        #[arg(long, env = "KASHA_PRIORITY", default_value = "10")]
        priority: u32,
        /// Pull-through: on a narinfo miss, fetch the path from the remote
        /// cache or an upstream cache, verify and store it (ADR-0011).
        #[arg(
            long,
            env = "KASHA_PULL_THROUGH",
            value_parser = clap::builder::BoolishValueParser::new()
        )]
        pull_through: bool,
        /// Seconds a path no source has is answered 404 without asking again.
        #[arg(long, env = "KASHA_PULL_THROUGH_NEGATIVE_TTL", default_value = "60")]
        pull_through_negative_ttl_secs: u64,
        /// Concurrent pull-through NAR downloads.
        #[arg(
            long,
            env = "KASHA_PULL_THROUGH_WORKERS",
            default_value = "8",
            value_parser = clap::value_parser!(u64).range(1..)
        )]
        pull_through_workers: u64,
        /// Cap on paths answered but not yet stored, queued downloads
        /// included; further narinfo misses get 404.
        #[arg(
            long,
            env = "KASHA_PULL_THROUGH_MAX_PENDING",
            default_value = "4096",
            value_parser = clap::value_parser!(u64).range(1..)
        )]
        pull_through_max_pending: u64,
        /// Seconds a source gets to accept a connection and to answer a
        /// narinfo lookup.
        #[arg(
            long,
            env = "KASHA_PULL_THROUGH_LOOKUP_TIMEOUT",
            default_value = "10",
            value_parser = clap::value_parser!(u64).range(1..)
        )]
        pull_through_lookup_timeout_secs: u64,
        /// Seconds a NAR download may receive nothing before it is dropped;
        /// under 3600, the age at which a temp file counts as abandoned.
        #[arg(
            long,
            env = "KASHA_PULL_THROUGH_STALL_TIMEOUT",
            default_value = "60",
            value_parser = clap::value_parser!(u64).range(1..store::TEMP_MAX_AGE.as_secs())
        )]
        pull_through_stall_timeout_secs: u64,
    },
    /// Emit a v3 generation manifest (closure store paths on stdin).
    Emit {
        /// Flake id the manifest is filed under (roots/<flake>/).
        #[arg(long)]
        flake: String,
        /// Generation id; names the manifest object (roots/<flake>/<gen>.json).
        #[arg(long = "gen")]
        gen_id: String,
        /// Source branch; `main` selects the main retention tier, anything else the other tier.
        #[arg(long)]
        branch: String,
        /// Built attribute; generations are retained per (flake, branch, attr) group.
        #[arg(long)]
        attr: String,
        /// ISO-8601 UTC; defaults to now.
        #[arg(long)]
        timestamp: Option<String>,
        /// Publish target: s3://bucket?endpoint=… (AWS_* creds) or an
        /// http(s):// box URL (KASHA_TOKEN). Prints to stdout either way.
        #[arg(long)]
        to: Option<String>,
    },
    /// Sweep the remote cache (run from CI with delete-capable creds).
    Gc {
        /// Remote cache as s3://bucket?endpoint=…&region=… (delete-capable creds).
        #[arg(long, env = "KASHA_REMOTE")]
        remote: String,
        /// Print what would be deleted without deleting anything.
        #[arg(long)]
        dry_run: bool,
        /// Objects younger than this are never deleted.
        #[arg(long, default_value = "24")]
        grace_hours: u64,
        #[command(flatten)]
        policy: PolicyArgs,
    },
    /// List the remote cache's generations with the verdict `gc` would apply.
    Ls {
        /// Remote cache as s3://bucket?endpoint=…&region=… (read-only creds suffice).
        #[arg(long, env = "KASHA_REMOTE")]
        remote: String,
        #[command(flatten)]
        policy: PolicyArgs,
    },
    /// Run one box GC sweep now and exit; safe beside a running `serve`.
    Sweep {
        /// Store root (flat binary-cache layout).
        #[arg(long, env = "KASHA_DATA", default_value = "/kasha")]
        data: String,
    },
}

// Shared by `gc` and `ls`, so a listing predicts what a sweep with the same
// flags keeps.
#[derive(clap::Args)]
struct PolicyArgs {
    /// Keep the N newest `main` generations per group [default: 5].
    #[arg(long)]
    main_keep: Option<usize>,
    /// Also keep `main` generations younger than this many weeks [default: 4].
    #[arg(long)]
    main_age_weeks: Option<u32>,
    /// Keep the N newest non-`main` generations per group [default: 1].
    #[arg(long)]
    other_keep: Option<usize>,
    /// Also keep non-`main` generations younger than this many weeks [default: 1].
    #[arg(long)]
    other_age_weeks: Option<u32>,
    /// Drop a group's count-retained generations once its newest one trails
    /// the newest of its flake's tier (main or other) by this many weeks;
    /// 0 disables [default: 4].
    #[arg(long)]
    stale_weeks: Option<u32>,
}

impl PolicyArgs {
    fn policy(&self) -> Policy {
        let mut p = Policy::remote();
        if let Some(n) = self.main_keep {
            p.main.keep_newest = n;
        }
        if let Some(w) = self.main_age_weeks {
            p.main.max_age = retention::WEEK * w;
        }
        if let Some(n) = self.other_keep {
            p.other.keep_newest = n;
        }
        if let Some(w) = self.other_age_weeks {
            p.other.max_age = retention::WEEK * w;
        }
        if let Some(w) = self.stale_weeks {
            p.stale_after = retention::WEEK * w;
        }
        p
    }
}

fn parse_keys(s: &str) -> Result<Vec<PubKey>> {
    let keys: Vec<PubKey> = s
        .split([',', ' '])
        .filter(|k| !k.trim().is_empty())
        .map(|k| PubKey::parse(k.trim()))
        .collect::<Result<_>>()?;
    if keys.is_empty() {
        bail!("no trusted keys configured");
    }
    Ok(keys)
}

fn main() -> Result<()> {
    use std::io::IsTerminal;
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    if std::io::stderr().is_terminal() {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .init();
    } else {
        tracing_subscriber::fmt()
            .json()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .init();
    }

    match Cli::parse().cmd {
        Cmd::Serve {
            data,
            listen,
            token,
            trusted_keys,
            remote,
            upstreams,
            sync_interval_secs,
            gc_interval_secs,
            max_inflight,
            priority,
            pull_through,
            pull_through_negative_ttl_secs,
            pull_through_workers,
            pull_through_max_pending,
            pull_through_lookup_timeout_secs,
            pull_through_stall_timeout_secs,
        } => {
            let store = Arc::new(store::Store::open(&data)?);
            let stale = store.clean_temps();
            if stale > 0 {
                tracing::info!(stale, "removed temp files of an interrupted run");
            }
            let keys = parse_keys(&trusted_keys)?;
            let s3 = remote.as_deref().map(S3Remote::from_url).transpose()?;
            let ups: Vec<String> = upstreams
                .split(',')
                .map(|u| u.trim().trim_end_matches('/').to_string())
                .filter(|u| !u.is_empty())
                .collect();

            let pull = if pull_through {
                let remote = s3
                    .as_ref()
                    .map(|s3| Box::new(s3.presigner()) as pull::UrlFn);
                let cfg = pull::Config {
                    negative_ttl: Duration::from_secs(pull_through_negative_ttl_secs),
                    workers: pull_through_workers as usize,
                    max_pending: pull_through_max_pending as usize,
                    // A quarter of the request slots for each kind of
                    // request that waits on a source, so half stay free for
                    // the ones the store can answer.
                    max_lookups: (max_inflight as usize / 4).max(1),
                    max_nar_waiters: (max_inflight as usize / 4).max(1),
                    lookup_timeout: Duration::from_secs(pull_through_lookup_timeout_secs),
                    stall_timeout: Duration::from_secs(pull_through_stall_timeout_secs),
                };
                let sources = pull::sources(remote, &ups);
                tracing::info!(sources = sources.len(), "pull-through enabled");
                Some(pull::Pull::start(
                    store.clone(),
                    keys.clone(),
                    sources,
                    cfg,
                )?)
            } else {
                None
            };

            let app = Arc::new(server::App {
                store,
                keys,
                token,
                status: Mutex::new(server::Status::default()),
                counters: Default::default(),
                ages: Default::default(),
                priority,
                pull,
            });
            if app.token.is_none() {
                tracing::warn!("KASHA_TOKEN unset: all writes disabled");
            }

            let maintain_app = app.clone();
            std::thread::spawn(move || gc::maintain(&maintain_app, gc::CLEAN_EVERY));

            if let Some(s3) = s3 {
                let sync_app = app.clone();
                std::thread::spawn(move || {
                    sync_loop(sync_app, s3, ups, sync_interval_secs, gc_interval_secs)
                });
            } else {
                tracing::warn!("KASHA_REMOTE unset: mirroring and GC disabled");
            }

            if std::env::var_os("KASHA_HTTP_THREADS").is_some() {
                tracing::warn!("KASHA_HTTP_THREADS is retired, use KASHA_MAX_INFLIGHT");
            }

            server::serve(app, &listen, max_inflight as usize)
        }

        Cmd::Emit {
            flake,
            gen_id,
            branch,
            attr,
            timestamp,
            to,
        } => {
            let mut input = String::new();
            std::io::stdin().read_to_string(&mut input)?;
            let mut closure: Vec<String> = input
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect();
            closure.sort();
            closure.dedup();
            let m = Manifest {
                version: 3,
                flake,
                gen_id,
                branch,
                attr,
                timestamp: timestamp.unwrap_or_else(|| {
                    humantime::format_rfc3339_seconds(SystemTime::now()).to_string()
                }),
                closure,
            };
            m.validate()?;
            let bytes = serde_json::to_vec(&m)?;
            println!("{}", serde_json::to_string(&m)?);
            let key = format!("roots/{}/{}.json", m.flake, m.gen_id);
            match to.as_deref() {
                None => {}
                Some(url) if url.starts_with("s3://") => {
                    S3Remote::from_url(url)?.put(&key, &bytes)?;
                    tracing::info!(key, "manifest published to remote cache");
                }
                Some(url) if url.starts_with("http") => {
                    let token = std::env::var("KASHA_TOKEN")
                        .context("KASHA_TOKEN required for a box target")?;
                    ureq::Agent::new_with_defaults()
                        .put(format!("{}/{}", url.trim_end_matches('/'), key))
                        .header("Authorization", format!("Bearer {token}"))
                        .send(&bytes[..])
                        .context("manifest push to box")?;
                    tracing::info!(key, "manifest pushed to box");
                }
                Some(url) => bail!("unsupported --to target: {url}"),
            }
            Ok(())
        }

        Cmd::Gc {
            remote,
            dry_run,
            grace_hours,
            policy,
        } => {
            let s3 = S3Remote::from_url(&remote)?;
            let report = gc::remote_sweep(
                &s3,
                &policy.policy(),
                SystemTime::now(),
                Duration::from_secs(grace_hours * 3600),
                dry_run,
            )?;
            for key in &report.deleted {
                println!(
                    "{}{}",
                    if dry_run { "would delete " } else { "deleted " },
                    key
                );
            }
            Ok(())
        }

        Cmd::Ls { remote, policy } => {
            let s3 = S3Remote::from_url(&remote)?;
            let now = SystemTime::now();
            let mut listed = gc::remote_ls(&s3, &policy.policy(), now)?;
            listed.sort_by(|a, b| match (&a.generation, &b.generation) {
                (Some((x, _)), Some((y, _))) => (&x.flake, &x.branch, &x.attr, y.time)
                    .cmp(&(&y.flake, &y.branch, &y.attr, x.time)),
                _ => b
                    .generation
                    .is_some()
                    .cmp(&a.generation.is_some())
                    .then(a.key.cmp(&b.key)),
            });
            let rows: Vec<[String; 6]> = listed
                .iter()
                .map(|l| match &l.generation {
                    Some((g, v)) => [
                        match v {
                            retention::Verdict::KeepAge => "keep:age",
                            retention::Verdict::KeepCount => "keep:count",
                            retention::Verdict::DropStale => "drop:stale",
                            retention::Verdict::Drop => "drop",
                        }
                        .to_string(),
                        format!(
                            "{}d",
                            now.duration_since(g.time).unwrap_or_default().as_secs() / 86400
                        ),
                        g.flake.clone(),
                        g.branch.clone(),
                        g.attr.clone(),
                        l.key.clone(),
                    ],
                    None => ["garbage", "-", "-", "-", "-", &l.key].map(str::to_string),
                })
                .collect();
            let header = ["VERDICT", "AGE", "FLAKE", "BRANCH", "ATTR", "KEY"].map(str::to_string);
            let mut width = [0; 6];
            for r in std::iter::once(&header).chain(&rows) {
                for (w, c) in width.iter_mut().zip(r) {
                    *w = (*w).max(c.len());
                }
            }
            for r in std::iter::once(&header).chain(&rows) {
                let line: Vec<String> = r
                    .iter()
                    .zip(width)
                    .map(|(c, w)| format!("{c:w$}"))
                    .collect();
                println!("{}", line.join("  ").trim_end());
            }
            Ok(())
        }

        Cmd::Sweep { data } => {
            let store = store::Store::open(&data)?;
            let report = gc::box_sweep(&store, SystemTime::now(), gc::GRACE);
            // Stamped after the sweep, unlike the timer: a running `serve`
            // prunes its index when the stamp moves, so the stamp must land
            // once every deletion (even of a failed sweep) is on disk. It also
            // defers the timer's next sweep, which this one makes redundant.
            store.record_sweep().context("sweep stamp")?;
            for key in &report?.deleted {
                println!("deleted {key}");
            }
            Ok(())
        }
    }
}

fn sync_loop(
    app: Arc<server::App>,
    s3: S3Remote,
    upstreams: Vec<String>,
    sync_interval: u64,
    gc_interval: u64,
) {
    let gc_interval = Duration::from_secs(gc_interval);
    let m = mirror::Mirror {
        store: &app.store,
        remote: &s3,
        upstreams,
        keys: &app.keys,
        agent: ureq::Agent::new_with_defaults(),
        misses: Default::default(),
        counters: &app.counters,
    };
    let mut seen_sweep = app.store.last_sweep();
    loop {
        // A stamp this loop did not write is a `kasha sweep` from another
        // process, whose deletions this index has not seen.
        let stamp = app.store.last_sweep();
        if stamp != seen_sweep {
            let pruned = app.store.prune_index();
            tracing::info!(pruned, "index pruned after external sweep");
            seen_sweep = stamp;
        }
        match m.down() {
            Ok(report) => {
                let total: usize = report.gaps.values().sum();
                tracing::info!(fetched = report.fetched_paths, gaps = total, "synced");
                let now = SystemTime::now();
                let mut st = app.status.lock().unwrap();
                for (flake, gaps) in report.gaps {
                    st.flakes.insert(flake, (now, gaps));
                }
                // Replaced whole: branches dropped by retention drop their series.
                st.generations = report.newest;
            }
            Err(e) => tracing::warn!(error = format!("{e:#}"), "mirror-down failed"),
        }
        match m.up() {
            Ok(pending) => app.status.lock().unwrap().pending_mirror_up = pending,
            Err(e) => tracing::warn!(error = format!("{e:#}"), "mirror-up failed"),
        }
        // Sweep in this thread so it never reads manifests concurrently with
        // mirror-down. Due-ness comes from the stamp, which survives restarts:
        // timing from boot means a box restarting more often than the interval
        // never sweeps. Stamping first means a sweep that dies mid-way waits a
        // full interval instead of rerunning on every restart of a crash loop.
        let due = app
            .store
            .last_sweep()
            .and_then(|t| t.elapsed().ok())
            .is_none_or(|elapsed| elapsed >= gc_interval);
        if due {
            if let Err(e) = app.store.record_sweep() {
                tracing::warn!(error = format!("{e:#}"), "sweep stamp failed");
            }
            seen_sweep = app.store.last_sweep();
            match gc::box_sweep(&app.store, SystemTime::now(), gc::GRACE) {
                Ok(r) => {
                    app.counters
                        .sweep_deleted
                        .fetch_add(r.deleted.len() as u64, Ordering::Relaxed);
                }
                Err(e) => tracing::warn!(error = format!("{e:#}"), "box sweep failed"),
            }
        }
        std::thread::sleep(Duration::from_secs(sync_interval));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stall_timeout_stays_under_the_temp_age_limit() {
        let serve = |stall: &str| {
            Cli::try_parse_from([
                "kasha",
                "serve",
                "--trusted-keys",
                "k:x",
                "--pull-through-stall-timeout-secs",
                stall,
            ])
        };
        let limit = store::TEMP_MAX_AGE.as_secs();
        assert!(serve(&(limit - 1).to_string()).is_ok());
        assert!(serve(&limit.to_string()).is_err());
    }
}
