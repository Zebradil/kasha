//! Optional pull-through on a narinfo miss (ADR-0011).
//!
//! `GET <hash>.narinfo` for a path the box lacks asks the sources in order:
//! the remote cache, then the upstream caches. The first narinfo that passes
//! the trust check and names the requested path is answered at once, rendered
//! with a `URL` the box derives from `NarHash`. Its NAR is downloaded on a
//! worker pool, checked against `NarHash` and `NarSize`, and only then stored,
//! followed by the narinfo; until then the narinfo lives in memory.
//!
//! Nothing else starts a fetch. `HEAD` reports what the store holds, and
//! `GET nar/<file>` waits for a fetch of that file already in flight.
//!
//! A push goes through the same door: `nix copy --to` asks for the narinfo
//! of every path with a GET before it uploads, so the box pulls the paths a
//! source holds and the client skips them. Only paths no source has are
//! uploaded.
//!
//! A push or mirror-down that stores the path first wins: the fetch is
//! dropped and its download discarded. A client still holding the pending
//! narinfo then gets `404` for its NAR and moves on to its next substituter.

use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::narinfo::{NarInfo, PubKey, nix32_encode};
use crate::store::{Published, Store};

pub struct Config {
    /// How long a miss on every source is remembered.
    pub negative_ttl: Duration,
    /// Concurrent NAR downloads.
    pub workers: usize,
    /// Cap on pending narinfos, each with its queued or running download.
    pub max_pending: usize,
    /// Cap on narinfo requests held up by a lookup, their own or a shared
    /// one. Each holds a request slot of the server for as long as the
    /// sources take, and requests the store can answer need slots too.
    pub max_lookups: usize,
    /// Cap on NAR requests waiting for a download. Separate from
    /// `max_lookups`: a NAR wait lasts minutes, a lookup milliseconds, and
    /// under one cap either kind could crowd out the other.
    pub max_nar_waiters: usize,
    /// Bound on connecting to a source and on its narinfo answer.
    pub lookup_timeout: Duration,
    /// Bound on a NAR download that receives nothing.
    pub stall_timeout: Duration,
}

/// Maps an object key to the URL a source serves it at.
pub type UrlFn = Box<dyn Fn(&str) -> String + Send + Sync>;

/// One place a narinfo and its NAR can come from.
pub struct Source {
    /// For logs; never the URL, which may be presigned.
    name: String,
    /// The remote cache rather than an upstream cache, for byte accounting.
    remote: bool,
    url: UrlFn,
}

/// The sources in the order they are asked: the remote cache when the box has
/// one, then the upstream caches as configured. Mirror-down's order.
pub fn sources(remote: Option<UrlFn>, upstreams: &[String]) -> Vec<Source> {
    let remote = remote.map(|url| Source {
        name: "remote cache".into(),
        remote: true,
        url,
    });
    let upstreams = upstreams.iter().map(|base| {
        let base = base.clone();
        Source {
            name: base.clone(),
            remote: false,
            url: Box::new(move |key| format!("{base}/{key}")),
        }
    });
    remote.into_iter().chain(upstreams).collect()
}

/// Monotonic counters surfaced at /metrics.
#[derive(Default)]
pub struct Stats {
    pub found: AtomicU64,
    pub missed: AtomicU64,
    /// Misses answered from the negative cache.
    pub negative: AtomicU64,
    pub failed: AtomicU64,
    pub capped: AtomicU64,
    /// Narinfo requests turned away because `max_lookups` were held up.
    pub busy: AtomicU64,
    /// NAR requests that waited for a download.
    pub nar_waited: AtomicU64,
    /// NAR requests turned away because `max_nar_waiters` were waiting.
    pub nar_busy: AtomicU64,
    pub stored: AtomicU64,
    pub fetch_failed: AtomicU64,
    /// Fetches dropped because the path was stored by another route first.
    pub abandoned: AtomicU64,
    pub bytes_remote: AtomicU64,
    pub bytes_upstream: AtomicU64,
}

fn bump(c: &AtomicU64) {
    c.fetch_add(1, Ordering::Relaxed);
}

/// A narinfo that is being answered while its NAR is not stored yet.
struct Pending {
    hash: String,
    /// The winning source's narinfo as it served it.
    raw: String,
    info: NarInfo,
    source: usize,
    nar_hash: [u8; 32],
    /// NAR file name in the store, derived from `NarHash`.
    file: String,
    /// What narinfo requests get until the NAR is stored.
    text: String,
}

#[derive(Default)]
struct State {
    /// Store hash to its pending narinfo, queued or downloading.
    pending: HashMap<String, Arc<Pending>>,
    /// NAR file name to the number of pending narinfos that produce it.
    files: HashMap<String, usize>,
    /// Store hashes waiting for a worker.
    queue: VecDeque<String>,
    /// NAR files a client is waiting for; their downloads go first.
    requested: HashSet<String>,
    /// Store hashes being looked up, so concurrent requests share one lookup.
    lookups: HashSet<String>,
    /// Store hashes no source had, with when that was learned.
    negative: HashMap<String, Instant>,
    negative_purged: Option<Instant>,
    /// Narinfo requests held up by a lookup.
    lookup_waiters: usize,
    /// NAR requests waiting for a download.
    nar_waiters: usize,
}

pub struct Pull {
    store: Arc<Store>,
    keys: Vec<PubKey>,
    sources: Vec<Source>,
    agent: ureq::Agent,
    cfg: Config,
    state: Mutex<State>,
    /// Signals workers that the queue grew.
    jobs: Condvar,
    /// Signals lookup and NAR waiters that a lookup or a fetch ended.
    done: Condvar,
    pub stats: Stats,
}

enum Probe {
    Found(Box<Pending>),
    Miss,
    Failed,
}

enum Outcome {
    Stored,
    Abandoned,
}

/// Ends a lookup: frees the request's waiter slot and wakes the requests
/// that wait for its outcome. Also when the lookup unwinds, so they do not
/// hang on its marker.
struct LookupGuard<'a>(&'a Pull, &'a str);

impl Drop for LookupGuard<'_> {
    fn drop(&mut self) {
        let mut st = self.0.state.lock().unwrap();
        st.lookups.remove(self.1);
        st.lookup_waiters -= 1;
        drop(st);
        self.0.done.notify_all();
    }
}

/// Removes a download that did not make it into the store.
struct TempFile(PathBuf);

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// No narinfo comes close; a larger answer is not one.
const NARINFO_MAX: u64 = 1024 * 1024;

fn is_store_hash(s: &str) -> bool {
    s.len() == 32
        && s.bytes()
            .all(|b| b"0123456789abcdfghijklmnpqrsvwxyz".contains(&b))
}

impl Pull {
    /// Build the mode and start its download workers.
    pub fn start(
        store: Arc<Store>,
        keys: Vec<PubKey>,
        sources: Vec<Source>,
        cfg: Config,
    ) -> Result<Arc<Self>> {
        ensure!(!sources.is_empty(), "pull-through needs a source");
        let agent = ureq::Agent::config_builder()
            .timeout_resolve(Some(cfg.lookup_timeout))
            .timeout_connect(Some(cfg.lookup_timeout))
            .timeout_per_read(Some(cfg.stall_timeout))
            .build()
            .into();
        let pull = Arc::new(Pull {
            store,
            keys,
            sources,
            agent,
            cfg,
            state: Mutex::default(),
            jobs: Condvar::new(),
            done: Condvar::new(),
            stats: Stats::default(),
        });
        for i in 0..pull.cfg.workers.max(1) {
            let pull = pull.clone();
            std::thread::Builder::new()
                .name(format!("pull-{i}"))
                .spawn(move || pull.work())
                .context("spawn pull-through worker")?;
        }
        Ok(pull)
    }

    /// (pending narinfos, of which still queued).
    pub fn gauges(&self) -> (usize, usize) {
        let st = self.state.lock().unwrap();
        (st.pending.len(), st.queue.len())
    }

    /// The narinfo to answer a `GET <hash>.narinfo` the store could not, or
    /// None for a `404`. Starts the lookup and the background fetch.
    pub fn narinfo(&self, hash: &str) -> Option<String> {
        if !is_store_hash(hash) {
            return None;
        }
        let mut st = self.state.lock().unwrap();
        // Set once this request holds a waiter slot.
        let mut held_up = false;
        // Set once it has waited for another request's lookup.
        let mut shared = false;
        let answer = loop {
            if let Some(answer) = self.answer_now(&mut st, hash) {
                break Some(answer);
            }
            // From here the request waits on a source, for its own lookup
            // or for another request's.
            if !held_up {
                if st.lookup_waiters >= self.cfg.max_lookups {
                    bump(&self.stats.busy);
                    return None;
                }
                st.lookup_waiters += 1;
                held_up = true;
            }
            if !st.lookups.contains(hash) {
                // The lookup it shared left no answer: a source failed. It
                // shares that outcome too, rather than asking the sources
                // again, one waiter after another.
                break shared.then_some(None);
            }
            shared = true;
            st = self.done.wait(st).unwrap();
        };
        // Lookups count: each may turn into a pending narinfo.
        let capped =
            answer.is_none() && st.pending.len() + st.lookups.len() >= self.cfg.max_pending;
        if answer.is_some() || capped {
            if held_up {
                st.lookup_waiters -= 1;
            }
            if capped {
                bump(&self.stats.capped);
            }
            return answer.flatten();
        }
        st.lookups.insert(hash.to_string());
        drop(st);

        let guard = LookupGuard(self, hash);
        let found = self.lookup(hash);
        let mut st = self.state.lock().unwrap();
        let text = match found {
            Ok(p) => {
                bump(&self.stats.found);
                let text = p.text.clone();
                *st.files.entry(p.file.clone()).or_default() += 1;
                st.queue.push_back(p.hash.clone());
                st.pending.insert(p.hash.clone(), Arc::new(p));
                self.jobs.notify_one();
                Some(text)
            }
            Err(failed) => {
                if failed {
                    bump(&self.stats.failed);
                } else {
                    bump(&self.stats.missed);
                    self.remember_miss(&mut st, hash);
                }
                None
            }
        };
        drop(st);
        drop(guard);
        text
    }

    /// The answer when no source has to be asked: `Some(narinfo)` for a
    /// pending or stored path, `None` for a remembered miss.
    fn answer_now(&self, st: &mut State, hash: &str) -> Option<Option<String>> {
        if let Some(p) = st.pending.get(hash) {
            return Some(Some(p.text.clone()));
        }
        // Stored since the caller looked, by a fetch that just ended. Disk,
        // not the index: the index can still list a narinfo that `kasha
        // sweep` deleted.
        if let Ok(Some(stored)) = self.store.read_narinfo(hash) {
            return Some(Some(stored));
        }
        match st.negative.get(hash) {
            Some(t) if t.elapsed() < self.cfg.negative_ttl => {
                bump(&self.stats.negative);
                Some(None)
            }
            Some(_) => {
                st.negative.remove(hash);
                None
            }
            None => None,
        }
    }

    fn remember_miss(&self, st: &mut State, hash: &str) {
        let ttl = self.cfg.negative_ttl;
        if ttl.is_zero() {
            return;
        }
        // Expired entries are only dropped here, once per TTL, so the map
        // holds at most two TTLs' worth of misses.
        if st.negative_purged.is_none_or(|t| t.elapsed() >= ttl) {
            st.negative.retain(|_, t| t.elapsed() < ttl);
            st.negative_purged = Some(Instant::now());
        }
        st.negative.insert(hash.to_string(), Instant::now());
    }

    /// Ask the sources in order. `Err(failed)` when none supplied the path;
    /// with `failed` set some source could not answer, so it is not a miss.
    fn lookup(&self, hash: &str) -> std::result::Result<Pending, bool> {
        let mut failed = false;
        for i in 0..self.sources.len() {
            match self.probe(i, hash) {
                Probe::Found(p) => return Ok(*p),
                Probe::Miss => {}
                Probe::Failed => failed = true,
            }
        }
        Err(failed)
    }

    fn probe(&self, i: usize, hash: &str) -> Probe {
        let source = self.sources[i].name.as_str();
        let raw = match self.get_narinfo(i, hash) {
            Ok(Some(raw)) => raw,
            Ok(None) => return Probe::Miss,
            Err(e) => {
                tracing::warn!(
                    hash,
                    source,
                    error = format!("{e:#}"),
                    "narinfo lookup failed"
                );
                return Probe::Failed;
            }
        };
        let info = match NarInfo::parse(&raw) {
            Ok(info) => info,
            Err(e) => {
                tracing::warn!(hash, source, error = %e, "bad narinfo from source");
                return Probe::Miss;
            }
        };
        if info.store_hash() != hash {
            tracing::warn!(
                hash,
                source,
                path = info.store_path,
                "narinfo names another path, skipping source"
            );
            return Probe::Miss;
        }
        if !info.verify(&self.keys) {
            tracing::warn!(
                hash,
                source,
                sig_keys = ?info.sig_key_names(),
                ca = info.ca.as_deref(),
                "narinfo neither trusted-signed nor content-addressed, skipping source"
            );
            return Probe::Miss;
        }
        // Answering a narinfo whose NAR cannot be verified would promise a
        // path the box can never store.
        let (Some(nar_hash), Some(ext)) = (info.nar_sha256(), extension(&info.compression)) else {
            tracing::warn!(
                hash,
                source,
                compression = info.compression,
                nar_hash = info.nar_hash,
                "cannot verify this NAR encoding or hash, skipping source"
            );
            return Probe::Failed;
        };
        let file = format!("{}.nar{ext}", nix32_encode(&nar_hash));
        let text = render(&raw, &file, &info.compression, None);
        Probe::Found(Box::new(Pending {
            hash: hash.to_string(),
            raw,
            info,
            source: i,
            nar_hash,
            file,
            text,
        }))
    }

    fn get_narinfo(&self, i: usize, hash: &str) -> Result<Option<String>> {
        let url = (self.sources[i].url)(&format!("{hash}.narinfo"));
        let resp = self
            .agent
            .get(url)
            .config()
            .timeout_global(Some(self.cfg.lookup_timeout))
            .build()
            .call();
        match resp {
            Ok(resp) => {
                let bytes = resp
                    .into_body()
                    .into_with_config()
                    .limit(NARINFO_MAX)
                    .read_to_vec()?;
                Ok(Some(String::from_utf8(bytes).context("narinfo not utf-8")?))
            }
            Err(ureq::Error::StatusCode(404)) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Wait while a fetch of the NAR `file` is in flight. The caller looks in
    /// the store afterwards: the file is there, or the fetch failed.
    pub fn join(&self, file: &str) {
        let Ok(path) = self.store.nar_path(file) else {
            return;
        };
        let mut st = self.state.lock().unwrap();
        if !st.files.contains_key(file) {
            return;
        }
        if st.nar_waiters >= self.cfg.max_nar_waiters {
            bump(&self.stats.nar_busy);
            return;
        }
        bump(&self.stats.nar_waited);
        st.nar_waiters += 1;
        st.requested.insert(file.to_string());
        // Two paths can share a NAR; the file is there after the first.
        while st.files.contains_key(file) && !path.is_file() {
            st = self.done.wait(st).unwrap();
        }
        st.nar_waiters -= 1;
    }

    fn work(&self) {
        loop {
            let p = self.next_job();
            let source = self.sources[p.source].name.as_str();
            // A panic must not leave the narinfo pending forever.
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.fetch(&p)))
                .unwrap_or_else(|_| Err(anyhow::anyhow!("fetch panicked")));
            match outcome {
                Ok(Outcome::Stored) => {
                    bump(&self.stats.stored);
                    tracing::info!(hash = p.hash, source, "pulled through");
                }
                Ok(Outcome::Abandoned) => bump(&self.stats.abandoned),
                Err(e) => {
                    bump(&self.stats.fetch_failed);
                    tracing::warn!(
                        hash = p.hash,
                        source,
                        error = format!("{e:#}"),
                        "pull-through fetch failed"
                    );
                }
            }
            let mut st = self.state.lock().unwrap();
            st.pending.remove(&p.hash);
            if let Some(n) = st.files.get_mut(&p.file) {
                *n -= 1;
                if *n == 0 {
                    st.files.remove(&p.file);
                    st.requested.remove(&p.file);
                }
            }
            drop(st);
            self.done.notify_all();
        }
    }

    /// The next download: one a client waits for if there is any, else the
    /// oldest.
    fn next_job(&self) -> Arc<Pending> {
        let mut st = self.state.lock().unwrap();
        loop {
            let wanted = st
                .queue
                .iter()
                .position(|h| st.requested.contains(&st.pending[h].file));
            if let Some(hash) = st.queue.remove(wanted.unwrap_or(0)) {
                return st.pending[&hash].clone();
            }
            st = self.jobs.wait(st).unwrap();
        }
    }

    fn fetch(&self, p: &Pending) -> Result<Outcome> {
        if self.store.narinfo_path(&p.hash)?.exists() {
            return Ok(Outcome::Abandoned);
        }
        let dest = self.store.nar_path(&p.file)?;
        // A NAR of this name can already be there: another path with the
        // same contents, or a pushed uncompressed NAR, which nix also names
        // by its hash. It is kept, and has to be this NAR.
        let (mut tmp, mut meta) = if dest.is_file() {
            (None, self.verify_stored(&dest, p)?)
        } else {
            let (tmp, meta) = self.download(p)?;
            (Some(tmp), meta)
        };
        loop {
            let raw = render(&p.raw, &p.file, &p.info.compression, Some(&meta));
            let info = NarInfo::parse(&raw)?;
            let nar_tmp = tmp.as_ref().map(|t| t.0.as_path());
            match self.store.publish_pulled(&info, raw.as_bytes(), nar_tmp)? {
                Published::Stored => return Ok(Outcome::Stored),
                Published::Lost => return Ok(Outcome::Abandoned),
                Published::NarExists => {
                    tmp = None;
                    meta = self.verify_stored(&dest, p)?;
                }
                Published::NarMissing => bail!("stored NAR {} vanished", p.file),
            }
        }
    }

    fn verify_stored(&self, path: &Path, p: &Pending) -> Result<FileMeta> {
        let file = File::open(path)?;
        verify(
            file,
            None,
            &p.info.compression,
            &p.nar_hash,
            p.info.nar_size,
        )
        .with_context(|| format!("stored NAR {} is not this path's NAR", p.file))
    }

    fn download(&self, p: &Pending) -> Result<(TempFile, FileMeta)> {
        let src = &self.sources[p.source];
        let resp = self
            .agent
            .get((src.url)(&p.info.url))
            .call()
            .with_context(|| format!("GET {}", p.info.url))?;
        let (tmp, mut file) = self.store.nar_temp(&p.file)?;
        let tmp = TempFile(tmp);
        let meta = verify(
            resp.into_body().into_reader(),
            Some(&mut file),
            &p.info.compression,
            &p.nar_hash,
            p.info.nar_size,
        )?;
        file.sync_all()?;
        let bytes = if src.remote {
            &self.stats.bytes_remote
        } else {
            &self.stats.bytes_upstream
        };
        bytes.fetch_add(meta.size, Ordering::Relaxed);
        Ok((tmp, meta))
    }
}

/// Hash and size of a NAR file as stored, compressed.
struct FileMeta {
    hash: [u8; 32],
    size: u64,
}

/// NAR file extension for a `Compression` the box can decode, hence verify.
fn extension(compression: &str) -> Option<&'static str> {
    match compression {
        "none" => Some(""),
        "xz" => Some(".xz"),
        "zstd" => Some(".zst"),
        "bzip2" => Some(".bz2"),
        _ => None,
    }
}

/// Memory one xz decoder may use. Covers every xz preset (the largest
/// needs 65 MiB); a source cannot make a worker allocate more with a huge
/// dictionary.
const XZ_MEMORY: u64 = 128 * 1024 * 1024;

fn decoder<'a>(compression: &str, r: impl Read + 'a) -> Result<Box<dyn Read + 'a>> {
    Ok(match compression {
        "none" => Box::new(r),
        "xz" => {
            use liblzma::stream::{CONCATENATED, Stream};
            let stream = Stream::new_stream_decoder(XZ_MEMORY, CONCATENATED)?;
            Box::new(liblzma::read::XzDecoder::new_stream(r, stream))
        }
        "zstd" => Box::new(zstd::stream::read::Decoder::new(r)?),
        "bzip2" => Box::new(bzip2::read::MultiBzDecoder::new(r)),
        other => bail!("unsupported compression {other}"),
    })
}

/// The box's narinfo for a pulled-through path: the source's lines, with
/// `URL` and `Compression` written by the box and `FileHash`/`FileSize`
/// dropped, or set to the stored file's once there is one. Signatures stay
/// valid: they cover none of these fields.
fn render(raw: &str, file: &str, compression: &str, stored: Option<&FileMeta>) -> String {
    let mut out = String::with_capacity(raw.len() + 128);
    for line in raw.lines() {
        match line.split_once(": ").map(|(key, _)| key) {
            Some("URL") => {
                out += &format!("URL: nar/{file}\nCompression: {compression}\n");
                if let Some(m) = stored {
                    out += &format!(
                        "FileHash: sha256:{}\nFileSize: {}\n",
                        nix32_encode(&m.hash),
                        m.size
                    );
                }
            }
            Some("Compression" | "FileHash" | "FileSize") => {}
            _ if line.trim().is_empty() => {}
            _ => {
                out += line;
                out.push('\n');
            }
        }
    }
    out
}

/// Slack over `NarSize` allowed to a compressed file before the download is
/// cut: no encoding grows a NAR by more than a sliver, and an endless body
/// that decodes to nothing would otherwise fill the disk.
const ENCODING_SLACK: u64 = 1024 * 1024;

/// Read a NAR file from `body`, copying it to `sink` if there is one, and
/// check that it decompresses to `nar_size` bytes hashing to `nar_hash`.
fn verify(
    body: impl Read,
    sink: Option<&mut File>,
    compression: &str,
    nar_hash: &[u8; 32],
    nar_size: u64,
) -> Result<FileMeta> {
    let mut tee = Tee {
        inner: body,
        sink,
        hasher: Sha256::new(),
        len: 0,
        max: nar_size + nar_size / 2 + ENCODING_SLACK,
    };
    let mut nar = NarSink {
        hasher: Sha256::new(),
        len: 0,
        max: nar_size,
    };
    std::io::copy(&mut decoder(compression, &mut tee)?, &mut nar)?;
    // The decoder may stop before the end of the body; the rest still
    // belongs to the file.
    std::io::copy(&mut tee, &mut std::io::sink())?;
    ensure!(
        nar.len == nar_size,
        "NAR is {} bytes, narinfo says {nar_size}",
        nar.len
    );
    ensure!(
        <[u8; 32]>::from(nar.hasher.finalize()) == *nar_hash,
        "NAR does not hash to NarHash"
    );
    Ok(FileMeta {
        hash: tee.hasher.finalize().into(),
        size: tee.len,
    })
}

/// Hashes and counts what passes through, optionally copying it to a file.
struct Tee<'a, R> {
    inner: R,
    sink: Option<&'a mut File>,
    hasher: Sha256,
    len: u64,
    max: u64,
}

impl<R: Read> Read for Tee<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.len += n as u64;
        if self.len > self.max {
            return Err(std::io::Error::other(
                "body larger than any encoding of the NAR",
            ));
        }
        self.hasher.update(&buf[..n]);
        if let Some(f) = &mut self.sink {
            f.write_all(&buf[..n])?;
        }
        Ok(n)
    }
}

/// Hashes and counts the decompressed NAR, refusing more than `NarSize`.
struct NarSink {
    hasher: Sha256,
    len: u64,
    max: u64,
}

impl Write for NarSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.len += buf.len() as u64;
        if self.len > self.max {
            return Err(std::io::Error::other("NAR larger than NarSize"));
        }
        self.hasher.update(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::{App, handle};
    use data_encoding::BASE64;
    use ed25519_dalek::{Signer, SigningKey};
    use std::sync::mpsc;

    /// A binary cache on localhost that records what it is asked and can
    /// hold or break answers.
    #[derive(Default)]
    struct Upstream {
        objects: Mutex<HashMap<String, Vec<u8>>>,
        /// `METHOD key[?query]` per request, in arrival order.
        log: Mutex<Vec<String>>,
        /// Keys answered with this status instead of their object.
        broken: Mutex<HashMap<String, u16>>,
        /// Requests for keys with this prefix wait until it is cleared.
        hold: Mutex<Option<String>>,
        released: Condvar,
    }

    impl Upstream {
        fn start() -> (Arc<Self>, String) {
            let up = Arc::new(Upstream::default());
            let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
            let base = format!("http://{}", server.server_addr().to_ip().unwrap());
            let serving = up.clone();
            std::thread::spawn(move || {
                while let Ok(req) = server.recv() {
                    let up = serving.clone();
                    std::thread::spawn(move || up.answer(req));
                }
            });
            (up, base)
        }

        fn answer(&self, req: tiny_http::Request) {
            let url = req.url().trim_start_matches('/').to_string();
            self.log
                .lock()
                .unwrap()
                .push(format!("{} {url}", req.method()));
            let key = url.split('?').next().unwrap_or_default().to_string();
            let hold = self.hold.lock().unwrap();
            drop(
                self.released
                    .wait_while(hold, |h| h.as_deref().is_some_and(|p| key.starts_with(p)))
                    .unwrap(),
            );
            let status = self.broken.lock().unwrap().get(&key).copied();
            let object = self.objects.lock().unwrap().get(&key).cloned();
            let _ = match (status, object) {
                (Some(code), _) => req.respond(tiny_http::Response::empty(code)),
                (None, Some(body)) => req.respond(tiny_http::Response::from_data(body)),
                (None, None) => req.respond(tiny_http::Response::empty(404)),
            };
        }

        fn put(&self, key: &str, body: &[u8]) {
            self.objects
                .lock()
                .unwrap()
                .insert(key.into(), body.to_vec());
        }

        fn add(&self, o: &Obj) {
            self.put(&format!("{}.narinfo", o.hash), o.narinfo.as_bytes());
            self.put(&o.key, &o.file);
        }

        fn hold(&self, prefix: &str) {
            *self.hold.lock().unwrap() = Some(prefix.into());
        }

        fn release(&self) {
            *self.hold.lock().unwrap() = None;
            self.released.notify_all();
        }

        fn asked(&self, entry: &str) -> usize {
            self.log
                .lock()
                .unwrap()
                .iter()
                .filter(|e| *e == entry)
                .count()
        }

        fn log(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }
    }

    fn key() -> SigningKey {
        SigningKey::from_bytes(&[11u8; 32])
    }

    fn trusted(sk: &SigningKey) -> Vec<PubKey> {
        let pk = BASE64.encode(sk.verifying_key().as_bytes());
        vec![PubKey::parse(&format!("test-1:{pk}")).unwrap()]
    }

    fn encode(compression: &str, nar: &[u8]) -> Vec<u8> {
        match compression {
            "none" => nar.to_vec(),
            "xz" => {
                let mut e = liblzma::write::XzEncoder::new(Vec::new(), 1);
                e.write_all(nar).unwrap();
                e.finish().unwrap()
            }
            "zstd" => zstd::encode_all(nar, 1).unwrap(),
            "bzip2" => {
                let mut e = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::fast());
                e.write_all(nar).unwrap();
                e.finish().unwrap()
            }
            other => panic!("no encoder for {other}"),
        }
    }

    /// A store path as an upstream serves it.
    struct Obj {
        hash: String,
        narinfo: String,
        /// The upstream's key for the NAR file.
        key: String,
        /// The NAR file, compressed.
        file: Vec<u8>,
        /// The name the box stores the NAR file under.
        box_file: String,
    }

    fn sign(sk: &SigningKey, body: &str) -> String {
        let sig = sk.sign(NarInfo::parse(body).unwrap().fingerprint().as_bytes());
        format!("{body}Sig: test-1:{}\n", BASE64.encode(&sig.to_bytes()))
    }

    /// The path `<c*32>-pkg-<c>`, signed by `sk`. `c` is a nix32 character.
    fn object(sk: &SigningKey, c: char, compression: &str) -> Obj {
        let nar = format!("nar of {c} ").repeat(500).into_bytes();
        let file = encode(compression, &nar);
        let hash: String = std::iter::repeat_n(c, 32).collect();
        let nar_hash = nix32_encode(&Sha256::digest(&nar));
        let ext = extension(compression).unwrap();
        let key = format!("nar/upstream-{c}.nar{ext}");
        // FileHash and FileSize are the upstream's, deliberately not those of
        // the file: the box must write its own.
        let body = format!(
            "StorePath: /nix/store/{hash}-pkg-{c}\n\
URL: {key}\n\
Compression: {compression}\n\
FileHash: sha256:{}\n\
FileSize: 1\n\
NarHash: sha256:{nar_hash}\n\
NarSize: {}\n\
References: \n\
Deriver: {hash}-pkg-{c}.drv\n",
            nix32_encode(&[0u8; 32]),
            nar.len(),
        );
        Obj {
            hash,
            narinfo: sign(sk, &body),
            key,
            file,
            box_file: format!("{nar_hash}.nar{ext}"),
        }
    }

    fn config() -> Config {
        Config {
            negative_ttl: Duration::from_secs(60),
            workers: 2,
            max_pending: 16,
            max_lookups: 16,
            max_nar_waiters: 16,
            lookup_timeout: Duration::from_secs(5),
            stall_timeout: Duration::from_secs(5),
        }
    }

    /// A box serving on localhost.
    struct Kasha {
        app: Arc<App>,
        base: String,
    }

    impl Kasha {
        fn start(sources: Option<Vec<Source>>, cfg: Config, priority: u32) -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "kasha-pull-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&dir);
            let store = Arc::new(Store::open(&dir).unwrap());
            let keys = trusted(&key());
            let pull = sources.map(|s| Pull::start(store.clone(), keys.clone(), s, cfg).unwrap());
            let app = Arc::new(App {
                store,
                keys,
                token: None,
                status: Default::default(),
                counters: Default::default(),
                ages: Default::default(),
                priority,
                pull,
            });
            let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
            let base = format!("http://{}", server.server_addr().to_ip().unwrap());
            let serving = app.clone();
            std::thread::spawn(move || {
                while let Ok(req) = server.recv() {
                    let app = serving.clone();
                    std::thread::spawn(move || handle(&app, req));
                }
            });
            Kasha { app, base }
        }

        /// A pull-through box in front of `upstreams`, no remote cache.
        fn pulling(upstreams: &[&str], cfg: Config) -> Self {
            let ups: Vec<String> = upstreams.iter().map(|u| u.to_string()).collect();
            Self::start(Some(sources(None, &ups)), cfg, 10)
        }

        fn get(&self, key: &str) -> (u16, Vec<u8>) {
            let mut resp = client().get(format!("{}/{key}", self.base)).call().unwrap();
            let body = resp.body_mut().read_to_vec().unwrap();
            (resp.status().as_u16(), body)
        }

        fn narinfo(&self, o: &Obj) -> (u16, String) {
            let (code, body) = self.get(&format!("{}.narinfo", o.hash));
            (code, String::from_utf8(body).unwrap())
        }

        fn head(&self, key: &str) -> u16 {
            let resp = client()
                .head(format!("{}/{key}", self.base))
                .call()
                .unwrap();
            resp.status().as_u16()
        }

        fn nar_dir(&self) -> Vec<String> {
            let mut names: Vec<String> = std::fs::read_dir(self.app.store.root.join("nar"))
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        }

        /// Wait until no narinfo is pending, i.e. every fetch has ended.
        fn settle(&self) {
            let pull = self.app.pull.as_ref().unwrap();
            eventually("fetches to end", || pull.gauges().0 == 0);
        }
    }

    fn client() -> ureq::Agent {
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .into()
    }

    fn eventually(what: &str, done: impl Fn() -> bool) {
        for _ in 0..1000 {
            if done() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out waiting for {what}");
    }

    fn field<'a>(narinfo: &'a str, key: &str) -> Option<&'a str> {
        narinfo
            .lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix(": "))
    }

    #[test]
    fn miss_is_answered_at_once_then_stored_and_served() {
        let (up, base) = Upstream::start();
        let o = object(&key(), 'a', "xz");
        up.add(&o);
        up.hold("nar/");
        let k = Kasha::pulling(&[&base], config());

        // The narinfo comes back while the upstream still holds the NAR.
        let (code, pending) = k.narinfo(&o);
        assert_eq!(code, 200);
        assert_eq!(
            field(&pending, "URL"),
            Some(&*format!("nar/{}", o.box_file))
        );
        assert_eq!(field(&pending, "Compression"), Some("xz"));
        assert_eq!(field(&pending, "FileHash"), None);
        assert_eq!(field(&pending, "FileSize"), None);
        for kept in [
            "StorePath",
            "NarHash",
            "NarSize",
            "References",
            "Deriver",
            "Sig",
        ] {
            assert_eq!(field(&pending, kept), field(&o.narinfo, kept), "{kept}");
        }
        assert!(NarInfo::parse(&pending).unwrap().verify(&trusted(&key())));
        assert!(!k.app.store.has(&o.hash));
        assert!(!k.app.store.narinfo_path(&o.hash).unwrap().exists());

        // A second narinfo request and two NAR requests join the one fetch.
        assert_eq!(k.narinfo(&o), (200, pending.clone()));
        let (tx, rx) = mpsc::channel();
        for _ in 0..2 {
            let (tx, url) = (tx.clone(), format!("{}/nar/{}", k.base, o.box_file));
            std::thread::spawn(move || {
                let mut resp = client().get(url).call().unwrap();
                let body = resp.body_mut().read_to_vec().unwrap();
                tx.send((resp.status().as_u16(), body)).unwrap();
            });
        }
        assert!(
            rx.recv_timeout(Duration::from_millis(200)).is_err(),
            "NAR served before it was fetched"
        );
        up.release();
        for _ in 0..2 {
            let (code, body) = rx.recv_timeout(Duration::from_secs(10)).unwrap();
            assert_eq!((code, &body), (200, &o.file));
        }
        assert_eq!(up.asked(&format!("GET {}.narinfo", o.hash)), 1);
        assert_eq!(up.asked(&format!("GET {}", o.key)), 1);

        // On disk: the NAR as the upstream served it, and a narinfo that
        // describes that file.
        k.settle();
        let stored = k.app.store.read_narinfo(&o.hash).unwrap().unwrap();
        assert_eq!(
            field(&stored, "FileHash"),
            Some(&*format!(
                "sha256:{}",
                nix32_encode(&Sha256::digest(&o.file))
            ))
        );
        assert_eq!(field(&stored, "FileSize"), Some(&*o.file.len().to_string()));
        assert_eq!(field(&stored, "URL"), field(&pending, "URL"));
        assert_eq!(field(&stored, "Sig"), field(&o.narinfo, "Sig"));
        assert_eq!(k.nar_dir(), [o.box_file.as_str()]);

        // A hit now: served from the store, the upstream is not asked again.
        assert_eq!(k.narinfo(&o), (200, stored));
        assert_eq!(k.get(&format!("nar/{}", o.box_file)), (200, o.file.clone()));
        assert_eq!(up.log().len(), 2);
    }

    #[test]
    fn nar_is_stored_even_if_no_client_asks_for_it() {
        let (up, base) = Upstream::start();
        let o = object(&key(), 'a', "none");
        up.add(&o);
        let k = Kasha::pulling(&[&base], config());
        assert_eq!(k.narinfo(&o).0, 200);
        k.settle();
        assert!(k.app.store.has(&o.hash));
    }

    #[test]
    fn every_supported_encoding_verifies() {
        let (up, base) = Upstream::start();
        let k = Kasha::pulling(&[&base], config());
        for (c, compression) in [('a', "none"), ('b', "xz"), ('c', "zstd"), ('d', "bzip2")] {
            let o = object(&key(), c, compression);
            up.add(&o);
            assert_eq!(k.narinfo(&o).0, 200, "{compression}");
            k.settle();
            let stored = k.app.store.read_narinfo(&o.hash).unwrap().unwrap();
            assert_eq!(field(&stored, "Compression"), Some(compression));
            assert_eq!(k.get(&format!("nar/{}", o.box_file)), (200, o.file.clone()));
        }
    }

    #[test]
    fn unsupported_encoding_is_a_failure_not_a_miss() {
        let (up, base) = Upstream::start();
        let mut o = object(&key(), 'a', "none");
        o.narinfo = o.narinfo.replace("Compression: none", "Compression: br");
        up.add(&o);
        let k = Kasha::pulling(&[&base], config());
        assert_eq!(k.narinfo(&o).0, 404);
        assert_eq!(k.narinfo(&o).0, 404);
        assert_eq!(up.asked(&format!("GET {}.narinfo", o.hash)), 2);
        assert_eq!(up.asked(&format!("GET {}", o.key)), 0);
    }

    /// The narinfo is refused, so nothing is fetched or stored, and the
    /// refusal is remembered like any miss.
    fn assert_refused(up: &Upstream, k: &Kasha, o: &Obj) {
        assert_eq!(k.narinfo(o).0, 404);
        assert_eq!(k.get(&format!("nar/{}", o.box_file)).0, 404);
        assert_eq!(k.narinfo(o).0, 404);
        assert_eq!(up.asked(&format!("GET {}.narinfo", o.hash)), 1);
        assert_eq!(up.asked(&format!("GET {}", o.key)), 0);
        assert!(!k.app.store.has(&o.hash));
        assert!(k.nar_dir().is_empty());
    }

    #[test]
    fn untrusted_signature_is_refused() {
        let (up, base) = Upstream::start();
        let o = object(&SigningKey::from_bytes(&[12u8; 32]), 'a', "xz");
        up.add(&o);
        let k = Kasha::pulling(&[&base], config());
        assert_refused(&up, &k, &o);
    }

    #[test]
    fn narinfo_naming_another_path_is_refused() {
        let (up, base) = Upstream::start();
        let asked = object(&key(), 'a', "xz");
        let other = object(&key(), 'b', "xz");
        // Validly signed, but for the path `b`.
        up.put(&format!("{}.narinfo", asked.hash), other.narinfo.as_bytes());
        up.put(&other.key, &other.file);
        let k = Kasha::pulling(&[&base], config());
        assert_eq!(k.narinfo(&asked).0, 404);
        assert_eq!(k.narinfo(&asked).0, 404);
        assert_eq!(up.log(), [format!("GET {}.narinfo", asked.hash)]);
        assert!(!k.app.store.has(&asked.hash) && !k.app.store.has(&other.hash));
        assert!(k.nar_dir().is_empty());
    }

    /// The narinfo is good, so it is answered; the NAR then fails its check.
    /// Nothing is stored, the NAR is a 404, and nothing is remembered.
    fn assert_fetch_fails(up: &Upstream, k: &Kasha, o: &Obj) {
        assert_eq!(k.narinfo(o).0, 200);
        assert_eq!(k.get(&format!("nar/{}", o.box_file)).0, 404);
        k.settle();
        assert!(!k.app.store.has(&o.hash));
        assert!(!k.app.store.narinfo_path(&o.hash).unwrap().exists());
        assert!(k.nar_dir().is_empty(), "left behind: {:?}", k.nar_dir());
        assert_eq!(k.narinfo(o).0, 200);
        assert_eq!(up.asked(&format!("GET {}.narinfo", o.hash)), 2);
        k.settle();
    }

    #[test]
    fn nar_hash_mismatch_is_not_stored() {
        let (up, base) = Upstream::start();
        let o = object(&key(), 'a', "xz");
        up.add(&o);
        // Same length, other bytes.
        let other = "NAR OF a ".repeat(500).into_bytes();
        up.put(&o.key, &encode("xz", &other));
        let k = Kasha::pulling(&[&base], config());
        assert_fetch_fails(&up, &k, &o);
    }

    #[test]
    fn nar_size_mismatch_is_not_stored() {
        let (up, base) = Upstream::start();
        for (c, off_by) in [('a', -1i64), ('b', 1)] {
            let mut o = object(&key(), c, "zstd");
            let size: i64 = field(&o.narinfo, "NarSize").unwrap().parse().unwrap();
            // Signed over the wrong size, so only the size check can object.
            let body = o
                .narinfo
                .lines()
                .filter(|l| !l.starts_with("Sig: "))
                .map(|l| format!("{l}\n"))
                .collect::<String>()
                .replace(
                    &format!("NarSize: {size}"),
                    &format!("NarSize: {}", size + off_by),
                );
            o.narinfo = sign(&key(), &body);
            up.add(&o);
            let k = Kasha::pulling(&[&base], config());
            assert_fetch_fails(&up, &k, &o);
        }
    }

    #[test]
    fn interrupted_download_leaves_nothing_behind() {
        let (up, base) = Upstream::start();
        let o = object(&key(), 'a', "xz");
        up.add(&o);
        up.put(&o.key, &o.file[..o.file.len() / 2]);
        let k = Kasha::pulling(&[&base], config());
        assert_fetch_fails(&up, &k, &o);
    }

    #[test]
    fn narinfo_without_nar_on_the_source_is_not_masked() {
        let (up, base) = Upstream::start();
        let (other, other_base) = Upstream::start();
        let o = object(&key(), 'a', "xz");
        up.put(&format!("{}.narinfo", o.hash), o.narinfo.as_bytes());
        other.add(&o);
        let k = Kasha::pulling(&[&base, &other_base], config());
        assert_eq!(k.narinfo(&o).0, 200);
        assert_eq!(k.get(&format!("nar/{}", o.box_file)).0, 404);
        assert!(
            other.log().is_empty(),
            "second source asked: {:?}",
            other.log()
        );
    }

    #[test]
    fn miss_on_every_source_is_remembered_for_the_ttl() {
        let (up, base) = Upstream::start();
        let o = object(&key(), 'a', "xz");
        let cfg = Config {
            negative_ttl: Duration::from_secs(1),
            ..config()
        };
        let k = Kasha::pulling(&[&base], cfg);
        let asked = || up.asked(&format!("GET {}.narinfo", o.hash));
        assert_eq!(k.narinfo(&o).0, 404);
        assert_eq!(k.narinfo(&o).0, 404);
        assert_eq!(asked(), 1);
        std::thread::sleep(Duration::from_millis(1100));
        assert_eq!(k.narinfo(&o).0, 404);
        assert_eq!(asked(), 2);
        // Published upstream meanwhile: found once the TTL has passed.
        up.add(&o);
        assert_eq!(k.narinfo(&o).0, 404);
        std::thread::sleep(Duration::from_millis(1100));
        assert_eq!(k.narinfo(&o).0, 200);
    }

    #[test]
    fn source_error_is_not_remembered() {
        let (bad, bad_base) = Upstream::start();
        let (good, good_base) = Upstream::start();
        let o = object(&key(), 'a', "xz");
        let narinfo = format!("{}.narinfo", o.hash);
        bad.broken.lock().unwrap().insert(narinfo.clone(), 500);
        let k = Kasha::pulling(&[&bad_base, &good_base], config());

        // One source fails, the other has nothing: not a definitive miss.
        assert_eq!(k.narinfo(&o).0, 404);
        assert_eq!(k.narinfo(&o).0, 404);
        assert_eq!(bad.asked(&format!("GET {narinfo}")), 2);
        assert_eq!(good.asked(&format!("GET {narinfo}")), 2);

        // A failing source does not stop the next one from supplying it.
        good.add(&o);
        assert_eq!(k.narinfo(&o).0, 200);
        k.settle();
        assert!(k.app.store.has(&o.hash));
    }

    #[test]
    fn slow_source_is_a_failure() {
        let (up, base) = Upstream::start();
        let (a, b) = (object(&key(), 'a', "xz"), object(&key(), 'b', "xz"));
        up.add(&a);
        up.add(&b);
        let cfg = Config {
            lookup_timeout: Duration::from_secs(1),
            stall_timeout: Duration::from_secs(1),
            ..config()
        };
        let k = Kasha::pulling(&[&base], cfg);

        // A narinfo lookup that outlasts its bound: 404, asked again later.
        up.hold(&a.hash);
        assert_eq!(k.narinfo(&a).0, 404);
        up.release();
        assert_eq!(k.narinfo(&a).0, 200);

        // A NAR download that receives nothing: dropped, NAR is a 404.
        up.hold(&b.key);
        assert_eq!(k.narinfo(&b).0, 200);
        assert_eq!(k.get(&format!("nar/{}", b.box_file)).0, 404);
        up.release();
        k.settle();
        assert!(k.app.store.has(&a.hash) && !k.app.store.has(&b.hash));
    }

    #[test]
    fn sources_are_asked_in_order_remote_first() {
        let (remote, remote_base) = Upstream::start();
        let (up1, base1) = Upstream::start();
        let (up2, base2) = Upstream::start();
        let (x, y, z) = (
            object(&key(), 'a', "xz"),
            object(&key(), 'b', "xz"),
            object(&key(), 'c', "xz"),
        );
        for o in [&x, &y, &z] {
            up2.add(o);
        }
        up1.add(&x);
        up1.add(&y);
        remote.add(&x);
        let url: UrlFn = Box::new(move |key| format!("{remote_base}/{key}?signed"));
        let k = Kasha::start(Some(sources(Some(url), &[base1, base2])), config(), 10);
        // One at a time: a download in the background would interleave with
        // the next lookup in the logs.
        for o in [&x, &y, &z] {
            assert_eq!(k.narinfo(o).0, 200);
            k.settle();
        }
        let narinfo = |o: &Obj| format!("GET {}.narinfo", o.hash);
        let signed = |o: &Obj| format!("GET {}.narinfo?signed", o.hash);
        // x: the remote cache has it, nobody else is asked.
        assert_eq!(
            remote.log(),
            [
                signed(&x),
                format!("GET {}?signed", x.key),
                signed(&y),
                signed(&z)
            ]
        );
        // y: the first upstream has it; z: only the second.
        assert_eq!(
            up1.log(),
            [narinfo(&y), format!("GET {}", y.key), narinfo(&z)]
        );
        assert_eq!(up2.log(), [narinfo(&z), format!("GET {}", z.key)]);
    }

    #[test]
    fn only_a_narinfo_get_starts_a_fetch() {
        let (up, base) = Upstream::start();
        let o = object(&key(), 'a', "xz");
        up.add(&o);
        up.hold("nar/");
        let k = Kasha::pulling(&[&base], config());
        let (narinfo, nar) = (format!("{}.narinfo", o.hash), format!("nar/{}", o.box_file));
        assert_eq!(k.head(&narinfo), 404);
        assert_eq!(k.head(&nar), 404);
        assert_eq!(k.get(&nar).0, 404);
        assert_eq!(k.get("not-a-store-hash.narinfo").0, 404);
        assert!(up.log().is_empty(), "upstream asked: {:?}", up.log());

        // A pending path is not in the store yet, which is what HEAD reports.
        assert_eq!(k.narinfo(&o).0, 200);
        assert_eq!(k.head(&narinfo), 404);
        assert_eq!(k.head(&nar), 404);
        up.release();
        k.settle();
        assert_eq!(k.head(&narinfo), 200);
        assert_eq!(k.head(&nar), 200);
    }

    #[test]
    fn past_the_pending_cap_a_miss_is_404_and_not_remembered() {
        let (up, base) = Upstream::start();
        let (a, b) = (object(&key(), 'a', "xz"), object(&key(), 'b', "xz"));
        up.add(&a);
        up.add(&b);
        up.hold("nar/");
        let cfg = Config {
            max_pending: 1,
            ..config()
        };
        let k = Kasha::pulling(&[&base], cfg);
        assert_eq!(k.narinfo(&a).0, 200);
        assert_eq!(k.narinfo(&b).0, 404);
        assert_eq!(up.asked(&format!("GET {}.narinfo", b.hash)), 0);
        up.release();
        k.settle();
        assert_eq!(k.narinfo(&b).0, 200);
    }

    #[test]
    fn past_the_waiter_cap_a_nar_request_is_404() {
        let (up, base) = Upstream::start();
        let o = object(&key(), 'a', "xz");
        up.add(&o);
        up.hold("nar/");
        let cfg = Config {
            max_nar_waiters: 1,
            ..config()
        };
        let k = Kasha::pulling(&[&base], cfg);
        assert_eq!(k.narinfo(&o).0, 200);
        let (tx, rx) = mpsc::channel();
        let url = format!("{}/nar/{}", k.base, o.box_file);
        std::thread::spawn(move || {
            let resp = client().get(url).call().unwrap();
            tx.send(resp.status().as_u16()).unwrap();
        });
        let pull = k.app.pull.as_ref().unwrap();
        eventually("the first request to wait", || {
            pull.state.lock().unwrap().nar_waiters == 1
        });
        assert_eq!(k.get(&format!("nar/{}", o.box_file)).0, 404);

        // Waiting NAR requests have a cap of their own: lookups still run.
        let other = object(&key(), 'b', "xz");
        up.add(&other);
        assert_eq!(k.narinfo(&other).0, 200);
        let (_, metrics) = k.get("metrics");
        let metrics = String::from_utf8(metrics).unwrap();
        for line in [
            "kasha_pull_through_nar_waits_total{result=\"waited\"} 1",
            "kasha_pull_through_nar_waits_total{result=\"busy\"} 1",
            "kasha_pull_through_lookups_total{result=\"busy\"} 0",
        ] {
            assert!(metrics.lines().any(|l| l == line), "missing {line:?}");
        }
        up.release();
        assert_eq!(rx.recv_timeout(Duration::from_secs(10)).unwrap(), 200);
    }

    #[test]
    fn a_requested_nar_is_fetched_before_unrequested_ones() {
        let (up, base) = Upstream::start();
        let objs = ['a', 'b', 'c'].map(|c| object(&key(), c, "xz"));
        for o in &objs {
            up.add(o);
        }
        up.hold("nar/");
        let cfg = Config {
            workers: 1,
            ..config()
        };
        let k = Kasha::pulling(&[&base], cfg);
        let [a, b, c] = &objs;
        assert_eq!(k.narinfo(a).0, 200);
        // The one worker is now stuck on `a`; `b` and `c` queue behind it.
        eventually("the download of a to start", || {
            up.asked(&format!("GET {}", a.key)) == 1
        });
        assert_eq!(k.narinfo(b).0, 200);
        assert_eq!(k.narinfo(c).0, 200);
        let (tx, rx) = mpsc::channel();
        let url = format!("{}/nar/{}", k.base, c.box_file);
        std::thread::spawn(move || {
            let resp = client().get(url).call().unwrap();
            tx.send(resp.status().as_u16()).unwrap();
        });
        let pull = k.app.pull.as_ref().unwrap();
        eventually("the request for c to wait", || {
            pull.state.lock().unwrap().nar_waiters == 1
        });
        up.release();
        assert_eq!(rx.recv_timeout(Duration::from_secs(10)).unwrap(), 200);
        k.settle();
        let nars: Vec<String> = up
            .log()
            .into_iter()
            .filter(|e| e.contains("nar/"))
            .collect();
        assert_eq!(
            nars,
            [&a.key, &c.key, &b.key].map(|key| format!("GET {key}"))
        );
    }

    #[test]
    fn mode_off_answers_a_miss_as_before() {
        let k = Kasha::start(None, config(), 10);
        let o = object(&key(), 'a', "xz");
        assert_eq!(k.narinfo(&o), (404, "not found".into()));
        assert_eq!(
            k.get(&format!("nar/{}", o.box_file)),
            (404, b"not found".to_vec())
        );
        let (code, info) = k.get("nix-cache-info");
        assert_eq!(code, 200);
        assert_eq!(
            info,
            b"StoreDir: /nix/store\nWantMassQuery: 1\nPriority: 10\n"
        );
        let (_, metrics) = k.get("metrics");
        let metrics = String::from_utf8(metrics).unwrap();
        assert!(!metrics.contains("kasha_pull_through") && !metrics.contains("\"pull\""));
    }

    #[test]
    fn a_path_stored_meanwhile_is_not_replaced() {
        let (up, base) = Upstream::start();
        let o = object(&key(), 'a', "xz");
        up.add(&o);
        up.hold("nar/");
        let k = Kasha::pulling(&[&base], config());
        assert_eq!(k.narinfo(&o).0, 200);

        // A push of the same path lands while the fetch is held.
        let pushed = o.narinfo.replace(&o.key, "nar/pushed.nar.xz");
        k.app.store.put_nar("pushed.nar.xz", &o.file[..]).unwrap();
        k.app
            .store
            .put_narinfo(&NarInfo::parse(&pushed).unwrap(), pushed.as_bytes())
            .unwrap();
        up.release();
        k.settle();

        // The push won: its objects are untouched and the fetch left nothing.
        assert_eq!(k.app.store.read_narinfo(&o.hash).unwrap().unwrap(), pushed);
        assert_eq!(k.nar_dir(), ["pushed.nar.xz"]);
        assert_eq!(
            k.app
                .pull
                .as_ref()
                .unwrap()
                .stats
                .abandoned
                .load(Ordering::Relaxed),
            1
        );
    }

    #[test]
    fn an_existing_nar_of_the_same_name_is_kept() {
        let (up, base) = Upstream::start();
        let k = Kasha::pulling(&[&base], config());

        // A pushed uncompressed NAR is named by its hash too. It is reused,
        // not downloaded again and not replaced.
        let good = object(&key(), 'a', "none");
        up.add(&good);
        k.app.store.put_nar(&good.box_file, &good.file[..]).unwrap();
        // The same file before and after, not a copy moved over it.
        let inode = || {
            use std::os::unix::fs::MetadataExt;
            std::fs::metadata(k.app.store.nar_path(&good.box_file).unwrap())
                .unwrap()
                .ino()
        };
        let pushed = inode();
        assert_eq!(k.narinfo(&good).0, 200);
        k.settle();
        assert!(k.app.store.has(&good.hash));
        assert_eq!(up.asked(&format!("GET {}", good.key)), 0);
        assert_eq!(pushed, inode());

        // A file of that name with other contents is not this NAR: it stays,
        // and the path is not stored.
        let bad = object(&key(), 'b', "none");
        up.add(&bad);
        k.app
            .store
            .put_nar(&bad.box_file, &b"something else"[..])
            .unwrap();
        assert_eq!(k.narinfo(&bad).0, 200);
        k.settle();
        assert!(!k.app.store.has(&bad.hash));
        assert_eq!(
            std::fs::read(k.app.store.nar_path(&bad.box_file).unwrap()).unwrap(),
            b"something else"
        );
        assert_eq!(up.asked(&format!("GET {}", bad.key)), 0);
    }

    #[test]
    fn configured_priority_and_metrics_are_served() {
        let (up, base) = Upstream::start();
        let o = object(&key(), 'a', "xz");
        up.add(&o);
        let k = Kasha::start(Some(sources(None, &[base])), config(), 30);
        assert_eq!(
            k.get("nix-cache-info").1,
            b"StoreDir: /nix/store\nWantMassQuery: 1\nPriority: 30\n"
        );
        assert_eq!(k.narinfo(&o).0, 200);
        k.settle();
        assert_eq!(k.narinfo(&object(&key(), 'b', "xz")).0, 404);
        let (_, metrics) = k.get("metrics");
        let metrics = String::from_utf8(metrics).unwrap();
        for line in [
            "kasha_pull_through_lookups_total{result=\"found\"} 1".to_string(),
            "kasha_pull_through_lookups_total{result=\"miss\"} 1".to_string(),
            "kasha_pull_through_fetches_total{result=\"stored\"} 1".to_string(),
            "kasha_pull_through_pending 0".to_string(),
            "kasha_pull_through_queued 0".to_string(),
            format!(
                "kasha_nar_bytes_total{{direction=\"pull\",source=\"upstream\"}} {}",
                o.file.len()
            ),
        ] {
            assert!(
                metrics.lines().any(|l| l == line),
                "missing {line:?} in:\n{metrics}"
            );
        }
    }

    #[test]
    fn content_addressed_path_needs_no_signature() {
        let (up, base) = Upstream::start();
        // An unsigned source tree, the way a flake input reaches a cache.
        let nar = b"nar of a source tree ".repeat(200);
        let nar_hash = Sha256::digest(&nar);
        let path = crate::narinfo::source_path(&nar_hash, "source");
        let hash = crate::narinfo::store_hash_of(&path).to_string();
        let nar_hash = nix32_encode(&nar_hash);
        let narinfo = format!(
            "StorePath: {path}\n\
URL: nar/upstream-source.nar.zst\n\
Compression: zstd\n\
NarHash: sha256:{nar_hash}\n\
NarSize: {}\n\
References: \n\
CA: fixed:r:sha256:{nar_hash}\n",
            nar.len()
        );
        let o = Obj {
            hash,
            narinfo,
            key: "nar/upstream-source.nar.zst".into(),
            file: encode("zstd", &nar),
            box_file: format!("{nar_hash}.nar.zst"),
        };
        up.add(&o);
        let k = Kasha::pulling(&[&base], config());
        assert_eq!(k.narinfo(&o).0, 200);
        assert_eq!(k.get(&format!("nar/{}", o.box_file)), (200, o.file.clone()));
        k.settle();
        let stored = k.app.store.read_narinfo(&o.hash).unwrap().unwrap();
        assert_eq!(field(&stored, "CA"), field(&o.narinfo, "CA"));
        assert_eq!(field(&stored, "Sig"), None);
        assert!(NarInfo::parse(&stored).unwrap().verify(&[]));

        // The same narinfo over other contents is not that path.
        let forged = object(&key(), 'a', "zstd");
        let mut bad = Obj {
            hash: forged.hash.clone(),
            narinfo: o
                .narinfo
                .replace(&path, &format!("/nix/store/{}-source", forged.hash)),
            ..forged
        };
        bad.narinfo = bad.narinfo.replace("upstream-source", "upstream-a");
        up.add(&bad);
        assert_refused(&up, &Kasha::pulling(&[&base], config()), &bad);
    }

    #[test]
    fn concurrent_requests_for_one_path_share_one_lookup() {
        let (up, base) = Upstream::start();
        let (found, failing) = (object(&key(), 'a', "xz"), object(&key(), 'b', "xz"));
        up.add(&found);
        up.add(&failing);
        let broken = format!("{}.narinfo", failing.hash);
        up.broken.lock().unwrap().insert(broken.clone(), 500);
        let k = Kasha::pulling(&[&base], config());
        let pull = k.app.pull.as_ref().unwrap();

        // Three requests for `o` while the source holds its answer.
        let ask_thrice = |o: &Obj| {
            up.hold(&o.hash);
            let (tx, rx) = mpsc::channel();
            for _ in 0..3 {
                let (tx, url) = (tx.clone(), format!("{}/{}.narinfo", k.base, o.hash));
                std::thread::spawn(move || {
                    let mut resp = client().get(url).call().unwrap();
                    let body = resp.body_mut().read_to_string().unwrap();
                    tx.send((resp.status().as_u16(), body)).unwrap();
                });
            }
            eventually("all three requests to wait", || {
                pull.state.lock().unwrap().lookup_waiters == 3
            });
            up.release();
            let answers: Vec<_> = (0..3)
                .map(|_| rx.recv_timeout(Duration::from_secs(10)).unwrap())
                .collect();
            assert_eq!(pull.state.lock().unwrap().lookup_waiters, 0);
            answers
        };

        let answers = ask_thrice(&found);
        assert!(answers.iter().all(|a| a.0 == 200 && a.1 == answers[0].1));
        assert_eq!(up.asked(&format!("GET {}.narinfo", found.hash)), 1);

        // A lookup that fails is shared as well: the two that waited for it
        // do not ask the source again. Nothing is remembered, so a later
        // request does.
        let answers = ask_thrice(&failing);
        assert!(answers.iter().all(|a| a.0 == 404));
        assert_eq!(up.asked(&format!("GET {broken}")), 1);
        assert_eq!(k.narinfo(&failing).0, 404);
        assert_eq!(up.asked(&format!("GET {broken}")), 2);
        k.settle();
    }

    #[test]
    fn stuck_lookups_cannot_take_every_request_slot() {
        let (up, base) = Upstream::start();
        let objs = ['a', 'b', 'c'].map(|c| object(&key(), c, "xz"));
        for o in &objs {
            up.add(o);
        }
        // A source that accepts connections and never answers.
        up.hold("");
        let cfg = Config {
            max_lookups: 2,
            ..config()
        };
        let k = Kasha::pulling(&[&base], cfg);
        let [a, b, c] = &objs;
        let (tx, rx) = mpsc::channel();
        for o in [a, b] {
            let (tx, url) = (tx.clone(), format!("{}/{}.narinfo", k.base, o.hash));
            std::thread::spawn(move || {
                let resp = client().get(url).call().unwrap();
                tx.send(resp.status().as_u16()).unwrap();
            });
        }
        let pull = k.app.pull.as_ref().unwrap();
        eventually("two lookups to be stuck", || {
            pull.state.lock().unwrap().lookup_waiters == 2
        });

        // A third lookup is turned away at once, and the box keeps answering
        // what it can answer itself.
        assert_eq!(k.narinfo(c).0, 404);
        assert_eq!(up.asked(&format!("GET {}.narinfo", c.hash)), 0);
        assert_eq!(k.get("nix-cache-info").0, 200);
        let (_, metrics) = k.get("metrics");
        let busy = "kasha_pull_through_lookups_total{result=\"busy\"} 1";
        assert!(
            String::from_utf8(metrics)
                .unwrap()
                .lines()
                .any(|l| l == busy)
        );

        // Nothing was remembered: once the source answers, so does the box.
        up.release();
        for _ in 0..2 {
            assert_eq!(rx.recv_timeout(Duration::from_secs(10)).unwrap(), 200);
        }
        assert_eq!(k.narinfo(c).0, 200);
        k.settle();
    }

    #[test]
    fn legacy_lzma_is_not_xz() {
        let (up, base) = Upstream::start();
        let o = object(&key(), 'a', "xz");
        up.add(&o);
        let nar = "nar of a ".repeat(500).into_bytes();
        let options = liblzma::stream::LzmaOptions::new_preset(1).unwrap();
        let stream = liblzma::stream::Stream::new_lzma_encoder(&options).unwrap();
        let mut e = liblzma::write::XzEncoder::new_stream(Vec::new(), stream);
        e.write_all(&nar).unwrap();
        up.put(&o.key, &e.finish().unwrap());
        let k = Kasha::pulling(&[&base], config());
        assert_fetch_fails(&up, &k, &o);
    }
}
