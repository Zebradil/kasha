mod gc;
mod manifest;
mod mirror;
mod narinfo;
mod remote;
mod retention;
mod server;
mod store;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use std::io::Read;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use manifest::Manifest;
use narinfo::PubKey;
use remote::{Remote, S3Remote};
use retention::Policy;

#[derive(Parser)]
#[command(name = "kasha", about = "net-local nix binary cache")]
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
        #[arg(long, env = "KASHA_LISTEN", default_value = "0.0.0.0:5000")]
        listen: String,
        /// Write token; writes are refused when unset.
        #[arg(long, env = "KASHA_TOKEN")]
        token: Option<String>,
        /// Trusted narinfo signing keys (`name:base64`, comma/space separated).
        #[arg(long, env = "KASHA_TRUSTED_KEYS", required = true)]
        trusted_keys: String,
        /// Remote cache as s3://bucket?endpoint=…&region=… (enables mirroring).
        #[arg(long, env = "KASHA_REMOTE")]
        remote: Option<String>,
        /// Upstream substituters tried after the remote cache, comma separated.
        #[arg(
            long,
            env = "KASHA_UPSTREAMS",
            default_value = "https://cache.nixos.org"
        )]
        upstreams: String,
        #[arg(long, env = "KASHA_SYNC_INTERVAL", default_value = "300")]
        sync_interval_secs: u64,
        #[arg(long, env = "KASHA_GC_INTERVAL", default_value = "86400")]
        gc_interval_secs: u64,
        #[arg(long, env = "KASHA_HTTP_THREADS", default_value = "8")]
        threads: usize,
    },
    /// Emit a v3 generation manifest (closure store paths on stdin).
    Emit {
        #[arg(long)]
        flake: String,
        #[arg(long = "gen")]
        gen_id: String,
        #[arg(long)]
        branch: String,
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
        #[arg(long, env = "KASHA_REMOTE")]
        remote: String,
        #[arg(long)]
        dry_run: bool,
        #[arg(long, default_value = "24")]
        grace_hours: u64,
        /// Override retention (defaults: main N=5 M=4wk, non-main N=1 M=1wk).
        #[arg(long)]
        main_keep: Option<usize>,
        #[arg(long)]
        main_age_weeks: Option<u32>,
        #[arg(long)]
        other_keep: Option<usize>,
        #[arg(long)]
        other_age_weeks: Option<u32>,
    },
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
            threads,
        } => {
            let app = Arc::new(server::App {
                store: store::Store::open(&data)?,
                keys: parse_keys(&trusted_keys)?,
                token,
                status: Mutex::new(server::Status::default()),
            });
            if app.token.is_none() {
                tracing::warn!("KASHA_TOKEN unset: all writes disabled");
            }

            if let Some(url) = remote {
                let s3 = S3Remote::from_url(&url)?;
                let ups: Vec<String> = upstreams
                    .split(',')
                    .map(|u| u.trim().trim_end_matches('/').to_string())
                    .filter(|u| !u.is_empty())
                    .collect();
                let sync_app = app.clone();
                std::thread::spawn(move || sync_loop(sync_app, s3, ups, sync_interval_secs));
                let gc_app = app.clone();
                std::thread::spawn(move || gc_loop(gc_app, gc_interval_secs));
            } else {
                tracing::warn!("KASHA_REMOTE unset: mirroring and GC disabled");
            }

            server::serve(app, &listen, threads)
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
            main_keep,
            main_age_weeks,
            other_keep,
            other_age_weeks,
        } => {
            let mut policy = Policy::remote();
            if let Some(n) = main_keep {
                policy.main.keep_newest = n;
            }
            if let Some(w) = main_age_weeks {
                policy.main.max_age = retention::WEEK * w;
            }
            if let Some(n) = other_keep {
                policy.other.keep_newest = n;
            }
            if let Some(w) = other_age_weeks {
                policy.other.max_age = retention::WEEK * w;
            }
            let s3 = S3Remote::from_url(&remote)?;
            let report = gc::remote_sweep(
                &s3,
                &policy,
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
    }
}

fn sync_loop(app: Arc<server::App>, s3: S3Remote, upstreams: Vec<String>, interval: u64) {
    loop {
        let m = mirror::Mirror {
            store: &app.store,
            remote: &s3,
            upstreams: upstreams.clone(),
            keys: &app.keys,
            agent: ureq::Agent::new_with_defaults(),
        };
        match m.down() {
            Ok(report) => {
                let total: usize = report.gaps.values().sum();
                tracing::info!(fetched = report.fetched_paths, gaps = total, "synced");
                let now = humantime::format_rfc3339_seconds(SystemTime::now()).to_string();
                let mut st = app.status.lock().unwrap();
                for (flake, gaps) in report.gaps {
                    st.flakes.insert(flake, (now.clone(), gaps));
                }
            }
            Err(e) => tracing::warn!(error = format!("{e:#}"), "mirror-down failed"),
        }
        match m.up() {
            Ok(pending) => app.status.lock().unwrap().pending_mirror_up = pending,
            Err(e) => tracing::warn!(error = format!("{e:#}"), "mirror-up failed"),
        }
        std::thread::sleep(Duration::from_secs(interval));
    }
}

fn gc_loop(app: Arc<server::App>, interval: u64) {
    let interval = Duration::from_secs(interval);
    loop {
        // Sleep only the remainder of the interval since the last sweep, which
        // survives restarts: waiting a full interval from boot means a box that
        // restarts more often than that never sweeps at all, while sweeping
        // unconditionally on boot would sweep every restart of a crash loop.
        let elapsed = app
            .store
            .last_sweep()
            .and_then(|t| t.elapsed().ok())
            .unwrap_or(interval);
        std::thread::sleep(interval.saturating_sub(elapsed));
        if let Err(e) = gc::box_sweep(&app.store, SystemTime::now(), gc::GRACE) {
            tracing::warn!(error = format!("{e:#}"), "box sweep failed");
        }
        if let Err(e) = app.store.record_sweep() {
            tracing::warn!(error = format!("{e:#}"), "sweep stamp failed");
        }
    }
}
