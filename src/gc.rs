//! Garbage collection: box sweep (in-process, between sync cycles) and remote
//! sweep (CI).
//!
//! Both mark-sweep from surviving manifests via the shared retention
//! selector. Mark set = union of retained manifests' closure lists (no
//! transitive narinfo walk); live nar keys come from retained narinfos'
//! URL fields. A 24h grace window skips young objects, which — together
//! with manifest-published-last ordering — makes the sweep race-safe and
//! idempotent without a lock across processes.
//!
//! Under a cap (ADR-0013) the box keeps unlisted paths, those no retained
//! manifest lists, by use instead: `evict` removes the least recently used
//! while they take more than the cap, from `serve`'s maintenance thread.

use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use crate::manifest::{Head, Manifest};
use crate::narinfo::store_hash_of;
use crate::remote::{Remote, fan_out};
use crate::retention::{Gen, Policy, Verdict, retain, verdicts};
use crate::server::App;
use crate::store::Store;

pub const GRACE: Duration = Duration::from_secs(24 * 3600);

/// A remote sweep reads one object per retained narinfo, so a large bucket runs
/// for minutes even fanned out. Report progress this often to keep a slow sweep
/// distinguishable from a wedged one.
const PROGRESS_EVERY: usize = 1000;

/// Bytes `ls` reads per manifest; `kasha emit` headers fit with room to
/// spare, and a longer one costs a full GET, not a wrong answer.
const HEAD_BYTES: usize = 1024;

#[derive(Debug, Default)]
pub struct SweepReport {
    pub retained_manifests: usize,
    pub deleted: Vec<String>,
    pub skipped_young: usize,
}

fn to_gen(id: String, m: &Manifest) -> Gen {
    Gen {
        id,
        flake: m.flake.clone(),
        branch: m.branch.clone(),
        attr: m.attr.clone(),
        time: m.time().unwrap_or(SystemTime::UNIX_EPOCH),
    }
}

/// Box GC: deletes no manifests (mirror-down reflects remote retention);
/// marks from the newest-N manifests per group plus every unmirrored
/// local-origin gen (box may hold the only copy), then sweeps objects.
/// With `lru`, unlisted narinfos are left to `evict` instead of swept by age.
pub fn box_sweep(
    store: &Store,
    now: SystemTime,
    grace: Duration,
    lru: bool,
) -> Result<SweepReport> {
    sweep(store, now, grace, lru, || ())
}

/// Store hashes the retained manifests list, with the number of those
/// manifests; `None` when the store has no manifest at all.
fn marked(store: &Store, now: SystemTime) -> Result<Option<(usize, HashSet<String>)>> {
    // Two passes so only one closure is in memory at a time: retention needs
    // every manifest's header, marking only the retained closures.
    let files = store.manifest_files()?;
    let mut gens = Vec::new();
    for (flake, gen_id, path) in &files {
        if let Some(m) = store.load_manifest(path)? {
            gens.push(to_gen(format!("{flake}/{gen_id}"), &m));
        }
    }
    if gens.is_empty() {
        return Ok(None);
    }
    let mut keep = retain(&gens, &Policy::boxed(), now);
    drop(gens);
    for (flake, gen_id, _) in &files {
        if store.is_local_origin(flake, gen_id) && !store.is_mirrored(flake, gen_id) {
            keep.insert(format!("{flake}/{gen_id}"));
        }
    }

    let mut mark: HashSet<String> = HashSet::new();
    for (flake, gen_id, path) in &files {
        if keep.contains(&format!("{flake}/{gen_id}"))
            && let Some(m) = store.load_manifest(path)?
        {
            mark.extend(m.closure.iter().map(|p| store_hash_of(p).to_string()));
        }
    }
    Ok(Some((keep.len(), mark)))
}

/// `box_sweep`, with `nars_listed` called once the NARs are listed and
/// before any is removed: the point where a test lets a writer in.
fn sweep(
    store: &Store,
    now: SystemTime,
    grace: Duration,
    lru: bool,
    nars_listed: impl FnOnce(),
) -> Result<SweepReport> {
    // No manifests means no mark set, which would sweep the whole store. That
    // is never a retention decision — it is a store that has not synced yet
    // (fresh volume, restored backup), so leave it alone.
    let Some((retained, mark)) = marked(store, now)? else {
        tracing::info!("box sweep skipped: no manifests to mark from");
        return Ok(SweepReport::default());
    };
    let mut live_nars: HashSet<String> = mark.iter().filter_map(|h| nar_of(store, h)).collect();

    let mut report = SweepReport {
        retained_manifests: retained,
        ..Default::default()
    };
    for (hash, mtime, _) in store.narinfo_files()? {
        if mark.contains(&hash) {
            continue;
        }
        let young = now.duration_since(mtime).unwrap_or(Duration::ZERO) < grace;
        if young || lru {
            report.skipped_young += usize::from(young);
            // Its NAR can be older than the narinfo, past grace already: a
            // pulled-through path that reuses a stored NAR. A narinfo the
            // sweep keeps must keep its NAR.
            live_nars.extend(nar_of(store, &hash));
            continue;
        }
        store.remove_narinfo(&hash)?;
        report.deleted.push(format!("{hash}.narinfo"));
    }
    let nars = store.nar_files()?;
    nars_listed();
    let young = |mtime: SystemTime| now.duration_since(mtime).unwrap_or(Duration::ZERO) < grace;
    for (file, mtime, _) in nars {
        if live_nars.contains(&file) {
            continue;
        }
        // Read again right before removing: a narinfo that names this NAR
        // resets its age, and may have done so since the listing, from this
        // process or from `serve` beside `kasha sweep`.
        if young(mtime) || !store.remove_nar_unless(&file, young)? {
            report.skipped_young += 1;
            continue;
        }
        report.deleted.push(format!("nar/{file}"));
    }
    tracing::info!(
        retained = report.retained_manifests,
        deleted = report.deleted.len(),
        skipped_young = report.skipped_young,
        "box sweep done"
    );
    Ok(report)
}

/// The NAR file a stored narinfo names, from the index.
fn nar_of(store: &Store, hash: &str) -> Option<String> {
    store
        .url_of(hash)
        .and_then(|u| u.strip_prefix("nar/").map(str::to_string))
}

/// A path used this recently is never evicted: a client that fetched its
/// narinfo may not have asked for the NAR yet.
const RECENT: Duration = Duration::from_secs(600);

/// How often `maintain` runs an eviction pass, whatever was stored since.
const PASS_EVERY: Duration = Duration::from_secs(300);

/// How often `serve`'s maintenance thread wakes.
pub const POLL: Duration = Duration::from_secs(5);

/// The cap on unlisted paths (ADR-0013) and what eviction did under it.
#[derive(Default)]
pub struct Lru {
    pub max_bytes: u64,
    /// Since start: narinfos evicted, and bytes of every file eviction
    /// removed.
    pub evicted: AtomicU64,
    pub evicted_bytes: AtomicU64,
    /// Bytes and number of unlisted paths after the last pass.
    pub bytes: AtomicU64,
    pub paths: AtomicU64,
}

impl Lru {
    pub fn new(max_bytes: u64) -> Self {
        Lru {
            max_bytes,
            ..Default::default()
        }
    }
}

#[derive(Debug, Default)]
pub struct EvictReport {
    /// Files eviction removed, as `<hash>.narinfo` and `nar/<file>`.
    pub evicted: Vec<String>,
    /// NARs no narinfo named, removed past the grace window.
    pub orphans: Vec<String>,
}

/// One eviction pass: while unlisted paths take more than the cap, remove
/// the least recently used, down to 90% of it. `busy` names NAR files a
/// fetch in flight produces or reuses; they stay.
pub fn evict(
    store: &Store,
    now: SystemTime,
    lru: &Lru,
    busy: &dyn Fn(&str) -> bool,
) -> Result<EvictReport> {
    evict_with(store, now, lru, busy, || ())
}

/// `evict`, with `listed` called once the store is listed and before
/// anything is removed: the point where a test lets a writer in.
fn evict_with(
    store: &Store,
    now: SystemTime,
    lru: &Lru,
    busy: &dyn Fn(&str) -> bool,
    listed: impl FnOnce(),
) -> Result<EvictReport> {
    let mark = marked(store, now)?
        .map(|(_, mark)| mark)
        .unwrap_or_default();
    let nars: HashMap<String, (SystemTime, u64)> = store
        .nar_files()?
        .into_iter()
        .map(|(file, mtime, size)| (file, (mtime, size)))
        .collect();
    let nar_size = |file: &str| nars.get(file).map_or(0, |&(_, size)| size);
    // Narinfos naming each NAR, listed or not: a NAR goes with the last.
    let mut refs: HashMap<String, usize> = HashMap::new();
    let mut listed_nars: HashSet<String> = HashSet::new();
    // (last use, store hash, narinfo size, NAR file) per unlisted path.
    let mut unlisted = Vec::new();
    for (hash, mtime, size) in store.narinfo_files()? {
        let nar = nar_of(store, &hash);
        if let Some(file) = &nar {
            *refs.entry(file.clone()).or_default() += 1;
        }
        if mark.contains(&hash) {
            listed_nars.extend(nar);
            continue;
        }
        let used = nar
            .as_ref()
            .and_then(|file| nars.get(file))
            .map_or(mtime, |&(nar_mtime, _)| mtime.max(nar_mtime));
        unlisted.push((used, hash, size, nar));
    }
    // A NAR counts once, and not at all while a listed path keeps it.
    let counted: HashSet<&str> = unlisted
        .iter()
        .filter_map(|(_, _, _, nar)| nar.as_deref())
        .filter(|file| !listed_nars.contains(*file))
        .collect();
    let mut bytes = unlisted.iter().map(|(_, _, size, _)| size).sum::<u64>()
        + counted.iter().map(|file| nar_size(file)).sum::<u64>();
    let mut paths = unlisted.len() as u64;
    listed();

    let recent = |t: SystemTime| now.duration_since(t).unwrap_or(Duration::ZERO) < RECENT;
    let mut report = EvictReport::default();
    let mut freed = 0;
    if bytes > lru.max_bytes {
        // Below the cap, so that the next pass is not due at the next path
        // stored.
        let target = lru.max_bytes / 10 * 9;
        unlisted.sort();
        for (used, hash, size, nar) in unlisted {
            if bytes <= target || recent(used) {
                break;
            }
            // Read again under the lock: a request since the listing made
            // the narinfo or its NAR recent.
            let nar_recent = || {
                nar.as_deref()
                    .and_then(|file| store.nar_modified(file))
                    .is_some_and(recent)
            };
            match store.remove_narinfo_unless(&hash, |t| recent(t) || nar_recent()) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(e) => {
                    tracing::warn!(hash, error = format!("{e:#}"), "eviction failed");
                    continue;
                }
            }
            bytes = bytes.saturating_sub(size);
            paths -= 1;
            freed += size;
            report.evicted.push(format!("{hash}.narinfo"));
            let Some(file) = nar else { continue };
            let left = refs.get_mut(&file).expect("counted while listing");
            *left -= 1;
            if *left > 0 {
                continue;
            }
            match store.remove_nar_unless(&file, |t| recent(t) || busy(&file)) {
                Ok(true) => {
                    bytes = bytes.saturating_sub(nar_size(&file));
                    freed += nar_size(&file);
                    report.evicted.push(format!("nar/{file}"));
                }
                Ok(false) => {}
                Err(e) => tracing::warn!(file, error = format!("{e:#}"), "eviction failed"),
            }
        }
    }
    // A push stores its NAR before the narinfo, so a NAR no narinfo names
    // goes only past the grace window, as in the sweep.
    let young = |t: SystemTime| now.duration_since(t).unwrap_or(Duration::ZERO) < GRACE;
    for (file, &(mtime, _)) in &nars {
        if refs.contains_key(file) || young(mtime) {
            continue;
        }
        match store.remove_nar_unless(file, |t| young(t) || busy(file)) {
            Ok(true) => report.orphans.push(format!("nar/{file}")),
            Ok(false) => {}
            Err(e) => tracing::warn!(file, error = format!("{e:#}"), "orphan removal failed"),
        }
    }

    let evicted = report
        .evicted
        .iter()
        .filter(|k| k.ends_with(".narinfo"))
        .count() as u64;
    lru.evicted.fetch_add(evicted, Ordering::Relaxed);
    lru.evicted_bytes.fetch_add(freed, Ordering::Relaxed);
    lru.bytes.store(bytes, Ordering::Relaxed);
    lru.paths.store(paths, Ordering::Relaxed);
    tracing::info!(
        evicted,
        freed,
        orphans = report.orphans.len(),
        bytes,
        paths,
        max_bytes = lru.max_bytes,
        "eviction pass done"
    );
    Ok(report)
}

/// How often `maintain` removes temp files a killed writer left behind.
pub const CLEAN_EVERY: Duration = Duration::from_secs(3600);

/// The box's housekeeping, on every box with or without a remote cache,
/// waking every `poll`. Never returns.
pub fn maintain(app: &App, poll: Duration) {
    let mut cleaned = Instant::now();
    // When the last eviction pass ended, and the NAR bytes received by then.
    let mut passed: Option<(Instant, u64)> = None;
    let received = || {
        let pulled = app.pull.as_ref().map_or(0, |p| {
            p.stats.bytes_remote.load(Ordering::Relaxed)
                + p.stats.bytes_upstream.load(Ordering::Relaxed)
        });
        pulled + app.counters.bytes_ingested.load(Ordering::Relaxed)
    };
    let busy = |file: &str| app.pull.as_ref().is_some_and(|p| p.busy(file));
    loop {
        std::thread::sleep(poll);
        // What a crash shortly before the last start left behind was too
        // young to remove then.
        if cleaned.elapsed() >= CLEAN_EVERY {
            let stale = app.store.clean_temps();
            if stale > 0 {
                tracing::info!(stale, "removed abandoned temp files");
            }
            cleaned = Instant::now();
        }
        let Some(lru) = &app.lru else { continue };
        // Bytes received approximate what was stored: a pass is due early
        // once they could have taken the unlisted paths over the cap. At
        // least a tenth of the cap, though: a pass that ended over it, with
        // only recent paths left, would otherwise rerun at every poll.
        let now_received = received();
        let headroom = (lru
            .max_bytes
            .saturating_sub(lru.bytes.load(Ordering::Relaxed)))
        .max(lru.max_bytes / 10);
        let due = passed.is_none_or(|(at, seen)| {
            at.elapsed() >= PASS_EVERY || now_received.saturating_sub(seen) > headroom
        });
        if !due {
            continue;
        }
        if let Err(e) = evict(&app.store, SystemTime::now(), lru, &busy) {
            tracing::warn!(error = format!("{e:#}"), "eviction pass failed");
        }
        passed = Some((Instant::now(), now_received));
    }
}

/// Remote sweep: the only place manifests are deleted. Marks only from v3
/// manifests, so v2 manifests, orphaned `.drv` objects, and broken v2
/// generations are swept as ordinary garbage.
pub fn remote_sweep(
    remote: &dyn Remote,
    policy: &Policy,
    now: SystemTime,
    grace: Duration,
    dry_run: bool,
) -> Result<SweepReport> {
    // LIST the whole bucket first; roots/ decisions come after (race note in
    // module docs).
    let listing = remote.list("")?;
    let mut narinfos: Vec<(&str, SystemTime)> = Vec::new(); // (hash, t)
    let mut nars: Vec<(&str, SystemTime)> = Vec::new(); // (key, t)
    let mut roots: Vec<(&str, SystemTime)> = Vec::new();
    for (key, t) in &listing {
        if let Some(h) = key.strip_suffix(".narinfo") {
            if !key.contains('/') {
                narinfos.push((h, *t));
            }
        } else if key.starts_with("nar/") {
            nars.push((key, *t));
        } else if key.starts_with("roots/") {
            roots.push((key, *t));
        }
        // Anything else (nix-cache-info, logs) is not ours to delete.
    }
    tracing::info!(
        objects = listing.len(),
        narinfos = narinfos.len(),
        nars = nars.len(),
        roots = roots.len(),
        "remote sweep: bucket listed"
    );

    // Retention over parseable v3 manifests; the rest of roots/ is garbage.
    let read = fan_out(
        &roots,
        PROGRESS_EVERY,
        "remote sweep: reading manifests",
        |(key, t)| {
            let m = remote.get(key)?.as_deref().map(Manifest::parse);
            Ok((*key, *t, m))
        },
    )?;
    let mut gens = Vec::new();
    let mut manifests = Vec::new();
    let mut garbage_roots: Vec<(&str, SystemTime)> = Vec::new();
    for (key, t, m) in read {
        match m {
            Some(Ok(m)) => {
                gens.push(to_gen(key.to_string(), &m));
                manifests.push((key.to_string(), t, m));
            }
            _ => garbage_roots.push((key, t)),
        }
    }
    let keep = retain(&gens, policy, now);
    tracing::info!(
        manifests = manifests.len(),
        garbage_roots = garbage_roots.len(),
        retained = keep.len(),
        "remote sweep: manifests read"
    );

    let mut mark: HashSet<&str> = HashSet::new();
    for (key, _, m) in &manifests {
        if keep.contains(key) {
            mark.extend(m.closure.iter().map(|p| store_hash_of(p)));
        }
    }

    // Live nar keys: URL fields of retained narinfos (the only per-object
    // reads). Each is an independent round trip, so fan them out.
    let marked: Vec<&str> = narinfos
        .iter()
        .map(|(h, _)| *h)
        .filter(|h| mark.contains(h))
        .collect();
    let live_nars: HashSet<String> = fan_out(
        &marked,
        PROGRESS_EVERY,
        "remote sweep: reading narinfos",
        |h| {
            let Some(raw) = remote.get(&format!("{h}.narinfo"))? else {
                return Ok(None);
            };
            Ok(crate::narinfo::NarInfo::parse(std::str::from_utf8(&raw)?)
                .ok()
                .map(|info| info.url))
        },
    )?
    .into_iter()
    .flatten()
    .collect();
    tracing::info!(live_nars = live_nars.len(), "remote sweep: narinfos read");

    let mut report = SweepReport {
        retained_manifests: keep.len(),
        ..Default::default()
    };
    // Collect first, delete after: batched deletes need the whole set, and a
    // sweep that dies mid-delete is picked up by the next run either way.
    let kill = |key: String, t: SystemTime, report: &mut SweepReport| {
        if now.duration_since(t).unwrap_or(Duration::ZERO) < grace {
            report.skipped_young += 1;
            return;
        }
        report.deleted.push(key);
    };

    for (key, t, _) in &manifests {
        if !keep.contains(key) {
            kill(key.clone(), *t, &mut report);
        }
    }
    for (key, t) in garbage_roots {
        kill(key.to_string(), t, &mut report);
    }
    for (h, t) in &narinfos {
        if !mark.contains(h) {
            kill(format!("{h}.narinfo"), *t, &mut report);
        }
    }
    for (key, t) in &nars {
        if !live_nars.contains(*key) {
            kill(key.to_string(), *t, &mut report);
        }
    }
    if !dry_run {
        tracing::info!(deleted = report.deleted.len(), "remote sweep: deleting");
        remote.delete_many(&report.deleted)?;
    }
    tracing::info!(
        retained = report.retained_manifests,
        deleted = report.deleted.len(),
        skipped_young = report.skipped_young,
        dry_run,
        "remote sweep done"
    );
    Ok(report)
}

/// One manifest under `roots/`, with the verdict `remote_sweep` would apply
/// under the same policy; `None` for an object it would delete as garbage.
pub struct Listed {
    pub key: String,
    pub generation: Option<(Gen, Verdict)>,
}

/// Retention preview of the remote cache from manifest headers alone: one
/// ranged GET per manifest instead of a full read plus a narinfo read per
/// retained path. Trusts the header: a manifest whose closure fails
/// validation shows a verdict here but is garbage to the sweep.
pub fn remote_ls(remote: &dyn Remote, policy: &Policy, now: SystemTime) -> Result<Vec<Listed>> {
    let roots = remote.list("roots/")?;
    let heads = fan_out(
        &roots,
        PROGRESS_EVERY,
        "ls: reading manifest headers",
        |(key, _)| {
            let head = match remote.get_prefix(key, HEAD_BYTES)? {
                Some(b) if b.len() == HEAD_BYTES => match Head::from_prefix(&b) {
                    Some(h) => Some(h),
                    None => remote.get(key)?.as_deref().and_then(Head::from_prefix),
                },
                b => b.as_deref().and_then(Head::from_prefix),
            };
            Ok((key.clone(), head))
        },
    )?;
    let mut gens = Vec::new();
    let mut garbage = Vec::new();
    for (key, head) in heads {
        match head {
            Some(h) => gens.push(Gen {
                time: h.time().unwrap_or(SystemTime::UNIX_EPOCH),
                id: key,
                flake: h.flake,
                branch: h.branch,
                attr: h.attr,
            }),
            None => garbage.push(key),
        }
    }
    let v = verdicts(&gens, policy, now);
    Ok(gens
        .into_iter()
        .zip(v)
        .map(|(g, v)| Listed {
            key: g.id.clone(),
            generation: Some((g, v)),
        })
        .chain(garbage.into_iter().map(|key| Listed {
            key,
            generation: None,
        }))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::narinfo::{NarInfo, PubKey};
    use crate::remote::fake::FakeRemote;
    use data_encoding::BASE64;
    use ed25519_dalek::{Signer, SigningKey};

    fn object(c: char, name: &str) -> (String, String, Vec<u8>) {
        let sk = SigningKey::from_bytes(&[5u8; 32]);
        let hash: String = std::iter::repeat_n(c, 32).collect();
        let body = format!(
            "StorePath: /nix/store/{hash}-{name}\n\
URL: nar/{hash}.nar.xz\n\
NarHash: sha256:00g966jlz9h37xkb9pmr3rc700i4k19mkyqm3gmwvlaik16qam5x\n\
NarSize: 8\n\
References: \n"
        );
        let info = NarInfo::parse(&body).unwrap();
        let sig = sk.sign(info.fingerprint().as_bytes());
        (
            hash,
            format!("{body}Sig: test-1:{}\n", BASE64.encode(&sig.to_bytes())),
            b"NAR".to_vec(),
        )
    }

    #[allow(dead_code)]
    fn test_key() -> PubKey {
        let sk = SigningKey::from_bytes(&[5u8; 32]);
        PubKey::parse(&format!(
            "test-1:{}",
            BASE64.encode(sk.verifying_key().as_bytes())
        ))
        .unwrap()
    }

    fn manifest_at(gen_id: &str, branch: &str, paths: &[String], ts: &str) -> Vec<u8> {
        serde_json::json!({
            "version": 3, "flake": "znix", "gen": gen_id, "branch": branch,
            "attr": "x", "timestamp": ts, "closure": paths,
        })
        .to_string()
        .into_bytes()
    }

    /// An empty store root of its own.
    fn tmp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kasha-gc-{name}-{}",
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn age(path: &std::path::Path, t: SystemTime) {
        std::fs::File::open(path).unwrap().set_modified(t).unwrap();
    }

    /// Store a path, NAR first, both files last modified at `t`.
    fn put_path(store: &Store, info: &str, nar: &[u8], t: SystemTime) {
        let parsed = NarInfo::parse(info).unwrap();
        let file = parsed.url.strip_prefix("nar/").unwrap();
        store.put_nar(file, nar).unwrap();
        store.put_narinfo(&parsed, info.as_bytes()).unwrap();
        age(&store.nar_path(file).unwrap(), t);
        age(&store.narinfo_path(parsed.store_hash()).unwrap(), t);
    }

    fn ts(now: SystemTime, days_ago: u64) -> String {
        humantime::format_rfc3339_seconds(now - Duration::from_secs(days_ago * 86400)).to_string()
    }

    #[test]
    fn remote_sweep_full_scenario() {
        let now = SystemTime::now();
        let old = now - Duration::from_secs(100 * 86400);
        let remote = FakeRemote::default();

        // Gen A: newest of its group, old — retained by N=... it's the only main gen.
        let (ha, ia, na) = object('a', "pkg-a");
        remote.insert_at(&format!("{ha}.narinfo"), ia.as_bytes(), old);
        remote.insert_at(&format!("nar/{ha}.nar.xz"), &na, old);
        remote.insert_at(
            "roots/znix/main-1-x.json",
            &manifest_at(
                "main-1-x",
                "main",
                &[format!("/nix/store/{ha}-pkg-a")],
                &ts(now, 100),
            ),
            old,
        );
        // Gen B: old feature gen, second-newest in its group of two -> swept.
        let (hb, ib, nb) = object('b', "pkg-b");
        remote.insert_at(&format!("{hb}.narinfo"), ib.as_bytes(), old);
        remote.insert_at(&format!("nar/{hb}.nar.xz"), &nb, old);
        remote.insert_at(
            "roots/znix/feat-1-x.json",
            &manifest_at(
                "feat-1-x",
                "feature",
                &[format!("/nix/store/{hb}-pkg-b")],
                &ts(now, 100),
            ),
            old,
        );
        let (hc, ic, nc) = object('c', "pkg-c");
        remote.insert_at(&format!("{hc}.narinfo"), ic.as_bytes(), old);
        remote.insert_at(&format!("nar/{hc}.nar.xz"), &nc, old);
        remote.insert_at(
            "roots/znix/feat-2-x.json",
            &manifest_at(
                "feat-2-x",
                "feature",
                &[format!("/nix/store/{hc}-pkg-c")],
                &ts(now, 50),
            ),
            old,
        );
        // v2 manifest: ordinary garbage.
        remote.insert_at(
            "roots/znix/old-v2.json",
            br#"{"version":2,"flake":"znix","gen":"old-v2","timestamp":"2025-01-01T00:00:00Z","roots":[{"outPath":"/nix/store/x","drvPath":"/nix/store/y"}]}"#,
            old,
        );
        // Orphan nar (e.g. a .drv leftover) and a young orphan under grace.
        remote.insert_at("nar/orphan.nar.xz", b"X", old);
        remote.insert_at("nar/young.nar.xz", b"Y", now);
        // Foreign key: untouched.
        remote.insert_at("nix-cache-info", b"StoreDir: /nix/store\n", old);

        let report = remote_sweep(&remote, &Policy::remote(), now, GRACE, false).unwrap();
        assert_eq!(report.retained_manifests, 2); // main-1 + feat-2 (newest per group)
        let mut deleted = report.deleted.clone();
        deleted.sort();
        assert_eq!(
            deleted,
            vec![
                format!("{hb}.narinfo"),
                "nar/".to_string() + &hb + ".nar.xz",
                "nar/orphan.nar.xz".to_string(),
                "roots/znix/feat-1-x.json".to_string(),
                "roots/znix/old-v2.json".to_string(),
            ]
        );
        assert_eq!(report.skipped_young, 1);
        let keys = remote.keys();
        assert!(keys.contains(&format!("{ha}.narinfo")));
        assert!(keys.contains(&format!("{hc}.narinfo")));
        assert!(keys.contains(&"nix-cache-info".to_string()));
        assert!(keys.contains(&"nar/young.nar.xz".to_string()));

        // Idempotent second run deletes nothing.
        let again = remote_sweep(&remote, &Policy::remote(), now, GRACE, false).unwrap();
        assert!(again.deleted.is_empty());
    }

    #[test]
    fn ls_agrees_with_the_sweep() {
        let now = SystemTime::now();
        let old = now - Duration::from_secs(100 * 86400);
        let remote = FakeRemote::default();
        let paths = [format!("/nix/store/{}-x", "a".repeat(32))];
        for (gen_id, branch, days) in [
            ("main-1", "main", 100),
            ("feat-1", "feature", 100),
            ("feat-2", "feature", 50),
        ] {
            remote.insert_at(
                &format!("roots/znix/{gen_id}.json"),
                &manifest_at(gen_id, branch, &paths, &ts(now, days)),
                old,
            );
        }
        remote.insert_at("roots/znix/bad.json", b"{}", old);

        let listed = remote_ls(&remote, &Policy::remote(), now).unwrap();
        let verdict = |key: &str| {
            let l = listed.iter().find(|l| l.key == key).unwrap();
            l.generation.as_ref().map(|(_, v)| *v)
        };
        assert_eq!(verdict("roots/znix/main-1.json"), Some(Verdict::KeepCount));
        assert_eq!(verdict("roots/znix/feat-2.json"), Some(Verdict::KeepCount));
        assert_eq!(verdict("roots/znix/feat-1.json"), Some(Verdict::Drop));
        assert_eq!(verdict("roots/znix/bad.json"), None);

        let report = remote_sweep(&remote, &Policy::remote(), now, GRACE, true).unwrap();
        let mut dropped: Vec<&str> = listed
            .iter()
            .filter(|l| !l.generation.as_ref().is_some_and(|(_, v)| v.keeps()))
            .map(|l| l.key.as_str())
            .collect();
        dropped.sort();
        let mut swept: Vec<&str> = report
            .deleted
            .iter()
            .map(String::as_str)
            .filter(|k| k.starts_with("roots/"))
            .collect();
        swept.sort();
        assert_eq!(dropped, swept);
    }

    #[test]
    fn remote_sweep_dry_run_deletes_nothing() {
        let now = SystemTime::now();
        let remote = FakeRemote::default();
        remote.insert_at("nar/orphan.nar.xz", b"X", now - Duration::from_secs(90000));
        let report = remote_sweep(&remote, &Policy::remote(), now, GRACE, true).unwrap();
        assert_eq!(report.deleted, vec!["nar/orphan.nar.xz".to_string()]);
        assert_eq!(remote.keys(), vec!["nar/orphan.nar.xz".to_string()]);
    }

    #[test]
    fn box_sweep_marks_newest_and_guards_local_origin() {
        let now = SystemTime::now();
        let dir = std::env::temp_dir().join(format!(
            "kasha-gc-{}",
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::open(&dir).unwrap();

        // 4 main gens (a..d, newest first a) -> box keeps 3 newest.
        let mut hashes = Vec::new();
        for (i, c) in ['a', 'b', 'c', 'd'].iter().enumerate() {
            let (h, info, nar) = object(*c, &format!("pkg-{c}"));
            store.put_nar(&format!("{h}.nar.xz"), &nar[..]).unwrap();
            store
                .put_narinfo(&NarInfo::parse(&info).unwrap(), info.as_bytes())
                .unwrap();
            let mb = manifest_at(
                &format!("main-{i}-x"),
                "main",
                &[format!("/nix/store/{h}-pkg-{c}")],
                &ts(now, (i as u64) * 10),
            );
            store
                .put_manifest(&Manifest::parse(&mb).unwrap(), &mb)
                .unwrap();
            hashes.push(h);
        }
        // Unmirrored local-origin feature gen, older than everything: guarded.
        let (he, ie, ne) = object('e', "pkg-e");
        store.put_nar(&format!("{he}.nar.xz"), &ne[..]).unwrap();
        store
            .put_narinfo(&NarInfo::parse(&ie).unwrap(), ie.as_bytes())
            .unwrap();
        let mb = manifest_at(
            "local-1-x",
            "feature",
            &[format!("/nix/store/{he}-pkg-e")],
            &ts(now, 300),
        );
        store
            .put_manifest(&Manifest::parse(&mb).unwrap(), &mb)
            .unwrap();
        store.mark_local_origin("znix", "local-1-x").unwrap();

        // Grace covers everything -> nothing deleted yet.
        let r = box_sweep(&store, now, GRACE, false).unwrap();
        assert!(r.deleted.is_empty());
        assert!(r.skipped_young > 0);

        // Zero grace: gen 'd' (4th newest main) swept; local-origin 'e' kept.
        let r = box_sweep(&store, now, Duration::ZERO, false).unwrap();
        let mut deleted = r.deleted.clone();
        deleted.sort();
        let hd = &hashes[3];
        assert_eq!(
            deleted,
            vec![format!("{hd}.narinfo"), format!("nar/{hd}.nar.xz")]
        );
        assert!(store.has(&hashes[0]));
        assert!(store.has(he.as_str()));
        // Manifests are never deleted by the box.
        assert_eq!(store.manifest_files().unwrap().len(), 5);

        // Once mirrored, the guard lifts; 'e' is still newest in its group
        // so it stays retained by count.
        store.mark_mirrored("znix", "local-1-x").unwrap();
        let r = box_sweep(&store, now, Duration::ZERO, false).unwrap();
        assert!(r.deleted.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn box_sweep_keeps_the_nar_of_a_narinfo_it_keeps() {
        let now = SystemTime::now();
        let dir = std::env::temp_dir().join(format!(
            "kasha-gc-young-{}",
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::open(&dir).unwrap();
        // A retained generation, so the sweep has a mark set.
        let (ha, ia, na) = object('a', "pkg-a");
        store.put_nar(&format!("{ha}.nar.xz"), &na[..]).unwrap();
        store
            .put_narinfo(&NarInfo::parse(&ia).unwrap(), ia.as_bytes())
            .unwrap();
        let mb = manifest_at(
            "main-1-x",
            "main",
            &[format!("/nix/store/{ha}-pkg-a")],
            &ts(now, 0),
        );
        store
            .put_manifest(&Manifest::parse(&mb).unwrap(), &mb)
            .unwrap();

        // In no manifest: a NAR past grace under a narinfo still within it.
        let (hb, ib, nb) = object('b', "pkg-b");
        let nar = format!("{hb}.nar.xz");
        store.put_nar(&nar, &nb[..]).unwrap();
        std::fs::File::options()
            .write(true)
            .open(store.nar_path(&nar).unwrap())
            .unwrap()
            .set_modified(now - 2 * GRACE)
            .unwrap();
        store
            .put_narinfo(&NarInfo::parse(&ib).unwrap(), ib.as_bytes())
            .unwrap();

        let r = box_sweep(&store, now, GRACE, false).unwrap();
        assert!(r.deleted.is_empty(), "deleted {:?}", r.deleted);
        assert!(store.nar_path(&nar).unwrap().exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn box_sweep_spares_a_nar_reused_while_it_runs() {
        let now = SystemTime::now();
        let dir = std::env::temp_dir().join(format!(
            "kasha-gc-reuse-{}",
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::open(&dir).unwrap();
        // A retained generation, so the sweep has a mark set.
        let (ha, ia, na) = object('a', "pkg-a");
        store.put_nar(&format!("{ha}.nar.xz"), &na[..]).unwrap();
        store
            .put_narinfo(&NarInfo::parse(&ia).unwrap(), ia.as_bytes())
            .unwrap();
        let mb = manifest_at(
            "main-1-x",
            "main",
            &[format!("/nix/store/{ha}-pkg-a")],
            &ts(now, 0),
        );
        store
            .put_manifest(&Manifest::parse(&mb).unwrap(), &mb)
            .unwrap();

        // A NAR no narinfo names, past grace: the sweep is about to take it.
        let (_, ib, nb) = object('b', "pkg-b");
        let info = NarInfo::parse(&ib).unwrap();
        let nar = info.url.strip_prefix("nar/").unwrap();
        store.put_nar(nar, &nb[..]).unwrap();
        std::fs::File::options()
            .write(true)
            .open(store.nar_path(nar).unwrap())
            .unwrap()
            .set_modified(now - 2 * GRACE)
            .unwrap();

        // Pull-through publishes a path that reuses it after the sweep has
        // listed narinfos and NARs.
        let r = sweep(&store, now, GRACE, false, || {
            let published = store.publish_pulled(&info, ib.as_bytes(), None).unwrap();
            assert_eq!(published, crate::store::Published::Stored);
        })
        .unwrap();
        assert!(r.deleted.is_empty(), "deleted {:?}", r.deleted);
        assert!(store.nar_path(nar).unwrap().exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Narinfos on disk whose NAR is not.
    fn dangling(store: &Store) -> Vec<String> {
        store
            .narinfo_files()
            .unwrap()
            .into_iter()
            .map(|(hash, _, _)| hash)
            .filter(|hash| {
                let text = store.read_narinfo(hash).unwrap().unwrap();
                let url = NarInfo::parse(&text).unwrap().url;
                !store
                    .nar_path(url.strip_prefix("nar/").unwrap())
                    .unwrap()
                    .exists()
            })
            .collect()
    }

    #[test]
    fn box_sweep_spares_a_nar_a_push_names_while_it_runs() {
        let now = SystemTime::now();
        let store = Store::open(tmp("push")).unwrap();
        let (ha, ia, na) = object('a', "pkg-a");
        put_path(&store, &ia, &na, now);
        let mb = manifest_at(
            "main-1-x",
            "main",
            &[format!("/nix/store/{ha}-pkg-a")],
            &ts(now, 0),
        );
        store
            .put_manifest(&Manifest::parse(&mb).unwrap(), &mb)
            .unwrap();

        // A NAR no narinfo names, past grace. A push whose NAR the box has
        // already skips the upload and sends only the narinfo.
        let (_, ib, nb) = object('b', "pkg-b");
        let info = NarInfo::parse(&ib).unwrap();
        let nar = info.url.strip_prefix("nar/").unwrap();
        store.put_nar(nar, &nb[..]).unwrap();
        age(&store.nar_path(nar).unwrap(), now - 2 * GRACE);

        let r = sweep(&store, now, GRACE, false, || {
            store.put_narinfo(&info, ib.as_bytes()).unwrap();
        })
        .unwrap();
        assert!(r.deleted.is_empty(), "deleted {:?}", r.deleted);
        assert!(dangling(&store).is_empty());
        let _ = std::fs::remove_dir_all(&store.root);
    }

    #[test]
    fn box_sweep_spares_a_store_with_no_manifests() {
        let now = SystemTime::now();
        let dir = std::env::temp_dir().join(format!(
            "kasha-gc-empty-{}",
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::open(&dir).unwrap();
        let (h, info, nar) = object('a', "pkg-a");
        store.put_nar(&format!("{h}.nar.xz"), &nar[..]).unwrap();
        store
            .put_narinfo(&NarInfo::parse(&info).unwrap(), info.as_bytes())
            .unwrap();

        // Unreachable from any manifest and past every grace: still spared,
        // because an empty mark set means "not synced", not "retain nothing".
        let r = box_sweep(&store, now, Duration::ZERO, false).unwrap();
        assert!(r.deleted.is_empty());
        assert!(store.has(h.as_str()));
        let _ = std::fs::remove_dir_all(dir);
    }

    const DAY: Duration = Duration::from_secs(86400);

    fn app(store: Store, lru: Option<Lru>) -> std::sync::Arc<App> {
        std::sync::Arc::new(App {
            store: store.into(),
            keys: vec![],
            token: None,
            status: Default::default(),
            counters: Default::default(),
            ages: Default::default(),
            priority: 10,
            pull: None,
            lru,
        })
    }

    /// Serve `app` on localhost; returns the base URL.
    fn serve(app: std::sync::Arc<App>) -> String {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr().to_ip().unwrap());
        std::thread::spawn(move || {
            while let Ok(req) = server.recv() {
                crate::server::handle(&app, req);
            }
        });
        base
    }

    fn get(url: &str) -> u16 {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .into();
        let mut resp = agent.get(url).call().unwrap();
        let _ = resp.body_mut().read_to_vec().unwrap();
        resp.status().as_u16()
    }

    /// Unlisted paths `a`, `b`, … with NARs of `nar` bytes, `a` used
    /// longest ago; returns their store hashes.
    fn unlisted(store: &Store, now: SystemTime, names: &[char], nar: usize) -> Vec<String> {
        names
            .iter()
            .enumerate()
            .map(|(i, &c)| {
                let (h, info, _) = object(c, &format!("pkg-{c}"));
                let days = (names.len() - i) as u32;
                put_path(store, &info, &vec![b'x'; nar], now - DAY * days);
                h
            })
            .collect()
    }

    fn has_all(store: &Store, hashes: &[String]) -> Vec<bool> {
        hashes
            .iter()
            .map(|h| store.narinfo_path(h).unwrap().exists())
            .collect()
    }

    #[test]
    fn eviction_goes_least_recently_used_first_down_to_ninety_percent() {
        let now = SystemTime::now();
        let store = Store::open(tmp("lru")).unwrap();
        let h = unlisted(&store, now, &['a', 'b', 'c', 'd'], 10_000);
        let narinfo = store.narinfo_files().unwrap()[0].2;
        let total = 4 * (narinfo + 10_000);

        // Under the cap: nothing goes, and the pass reports the class.
        let lru = Lru::new(total);
        let r = evict(&store, now, &lru, &|_| false).unwrap();
        assert!(r.evicted.is_empty() && r.orphans.is_empty());
        assert_eq!(lru.bytes.load(Ordering::Relaxed), total);
        assert_eq!(lru.paths.load(Ordering::Relaxed), 4);

        // Over it: two paths take 90% of 30000 down from four.
        let lru = Lru::new(30_000);
        let r = evict(&store, now, &lru, &|_| false).unwrap();
        assert_eq!(
            r.evicted,
            [
                format!("{}.narinfo", h[0]),
                format!("nar/{}.nar.xz", h[0]),
                format!("{}.narinfo", h[1]),
                format!("nar/{}.nar.xz", h[1]),
            ]
        );
        assert_eq!(has_all(&store, &h), [false, false, true, true]);
        assert!(!store.has(&h[0]) && store.has(&h[2]));
        assert!(dangling(&store).is_empty());
        assert_eq!(lru.bytes.load(Ordering::Relaxed), 2 * (narinfo + 10_000));
        assert_eq!(lru.paths.load(Ordering::Relaxed), 2);
        assert_eq!(lru.evicted.load(Ordering::Relaxed), 2);
        assert_eq!(
            lru.evicted_bytes.load(Ordering::Relaxed),
            2 * (narinfo + 10_000)
        );
        let _ = std::fs::remove_dir_all(&store.root);
    }

    #[test]
    fn a_path_used_recently_is_not_evicted_and_that_survives_a_restart() {
        let now = SystemTime::now();
        let root = tmp("used");
        let h = unlisted(&Store::open(&root).unwrap(), now, &['a', 'b', 'c'], 3);

        // `a` is the least recently used, until a client reads it.
        let base = serve(app(Store::open(&root).unwrap(), Some(Lru::new(1))));
        assert_eq!(get(&format!("{base}/{}.narinfo", h[0])), 200);

        // A new process, as after a restart, sees the use on disk.
        let store = Store::open(&root).unwrap();
        let r = evict(&store, SystemTime::now(), &Lru::new(1), &|_| false).unwrap();
        assert_eq!(r.evicted.len(), 4, "evicted {:?}", r.evicted);
        assert_eq!(has_all(&store, &h), [true, false, false]);

        // Used longer ago than the recent window, it goes like the rest.
        let r = evict(&store, now + RECENT + DAY, &Lru::new(1), &|_| false).unwrap();
        assert_eq!(r.evicted.len(), 2);
        assert_eq!(has_all(&store, &h), [false, false, false]);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn reads_record_use_only_under_a_cap_and_at_most_once_a_minute() {
        let now = SystemTime::now();
        let root = tmp("touch");
        let h = unlisted(&Store::open(&root).unwrap(), now, &['a'], 3);
        let narinfo = Store::open(&root).unwrap().narinfo_path(&h[0]).unwrap();
        let mtime = || std::fs::metadata(&narinfo).unwrap().modified().unwrap();
        let old = mtime();

        // Without a cap a read changes nothing on disk.
        let off = serve(app(Store::open(&root).unwrap(), None));
        assert_eq!(get(&format!("{off}/{}.narinfo", h[0])), 200);
        assert_eq!(mtime(), old);

        // With one, a read of a file used a minute ago or more is recorded,
        // a read of one used since is not.
        let on = serve(app(Store::open(&root).unwrap(), Some(Lru::new(1))));
        assert_eq!(get(&format!("{on}/{}.narinfo", h[0])), 200);
        assert!(mtime() > now - crate::store::USE_RESOLUTION);
        let within = SystemTime::now() - crate::store::USE_RESOLUTION / 2;
        age(&narinfo, within);
        assert_eq!(get(&format!("{on}/{}.narinfo", h[0])), 200);
        assert!(mtime() < SystemTime::now() - crate::store::USE_RESOLUTION / 4);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn listed_paths_are_neither_evicted_nor_counted_until_they_leave() {
        let now = SystemTime::now();
        let store = Store::open(tmp("listed")).unwrap();
        // Listed, and used longer ago than anything else.
        let (hl, il, _) = object('l', "pkg-l");
        put_path(&store, &il, b"NAR", now - DAY * 100);
        let mb = manifest_at(
            "main-1-x",
            "main",
            &[format!("/nix/store/{hl}-pkg-l")],
            &ts(now, 0),
        );
        store
            .put_manifest(&Manifest::parse(&mb).unwrap(), &mb)
            .unwrap();
        let h = unlisted(&store, now, &['a', 'b'], 3);
        let unlisted_bytes: u64 = store
            .narinfo_files()
            .unwrap()
            .iter()
            .filter(|(hash, _, _)| *hash != hl)
            .map(|(_, _, size)| size + 3)
            .sum();

        let lru = Lru::new(1);
        let r = evict(&store, now, &lru, &|_| false).unwrap();
        assert_eq!(r.evicted.len(), 4);
        assert_eq!(has_all(&store, &h), [false, false]);
        assert!(store.has(&hl));
        assert_eq!(lru.evicted_bytes.load(Ordering::Relaxed), unlisted_bytes);
        assert_eq!(lru.paths.load(Ordering::Relaxed), 0);

        // Its manifest dropped, the path is unlisted, with its old use.
        store.remove_manifest("znix", "main-1-x").unwrap();
        let r = evict(&store, now, &lru, &|_| false).unwrap();
        assert_eq!(
            r.evicted,
            [format!("{hl}.narinfo"), format!("nar/{hl}.nar.xz")]
        );
        assert!(dangling(&store).is_empty());
        let _ = std::fs::remove_dir_all(&store.root);
    }

    #[test]
    fn a_shared_nar_goes_with_the_last_narinfo_naming_it() {
        let now = SystemTime::now();
        let store = Store::open(tmp("shared")).unwrap();
        let (ha, ia, na) = object('a', "pkg-a");
        let (hb, ib, _) = object('b', "pkg-b");
        let nar = format!("{ha}.nar.xz");
        // `URL` is outside the signature, so the two still verify.
        let ib = ib.replace(&format!("nar/{hb}.nar.xz"), &format!("nar/{nar}"));
        put_path(&store, &ia, &na, now - DAY * 2);
        put_path(&store, &ib, &na, now - DAY * 2);
        // `b` read just now, through its narinfo.
        age(&store.narinfo_path(&hb).unwrap(), now);

        let lru = Lru::new(1);
        let r = evict(&store, now, &lru, &|_| false).unwrap();
        assert_eq!(r.evicted, [format!("{ha}.narinfo")]);
        assert!(store.nar_path(&nar).unwrap().exists());
        assert!(dangling(&store).is_empty());

        let r = evict(&store, now + RECENT, &lru, &|_| false).unwrap();
        assert_eq!(r.evicted, [format!("{hb}.narinfo"), format!("nar/{nar}")]);
        assert!(store.narinfo_files().unwrap().is_empty());
        assert!(store.nar_files().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&store.root);
    }

    #[test]
    fn a_nar_a_fetch_needs_is_kept_and_goes_later_as_an_orphan() {
        let now = SystemTime::now();
        let store = Store::open(tmp("busy")).unwrap();
        let h = unlisted(&store, now, &['a', 'b'], 3);
        let nar = format!("{}.nar.xz", h[0]);

        let lru = Lru::new(1);
        let r = evict(&store, now, &lru, &|file| file == nar).unwrap();
        assert_eq!(
            r.evicted,
            [
                format!("{}.narinfo", h[0]),
                format!("{}.narinfo", h[1]),
                format!("nar/{}.nar.xz", h[1]),
            ]
        );
        assert!(store.nar_path(&nar).unwrap().exists());
        assert!(r.orphans.is_empty());

        // Named by no narinfo and past grace: the next pass removes it,
        // unless a fetch still needs it.
        let r = evict(&store, now, &lru, &|file| file == nar).unwrap();
        assert!(r.orphans.is_empty());
        let r = evict(&store, now, &lru, &|_| false).unwrap();
        assert_eq!(r.orphans, [format!("nar/{nar}")]);
        let _ = std::fs::remove_dir_all(&store.root);
    }

    #[test]
    fn no_narinfo_is_left_without_its_nar_when_a_writer_cuts_in() {
        let now = SystemTime::now();
        for writer in ["publish", "push", "read"] {
            let store = Store::open(tmp("cut-in")).unwrap();
            let (ha, ia, na) = object('a', "pkg-a");
            put_path(&store, &ia, &na, now - DAY * 2);
            let nar = format!("{ha}.nar.xz");
            // Another path with the same NAR, written after the pass listed
            // the store: by pull-through reusing the NAR, or by a push
            // that skipped its upload. Or a client reads `a` meanwhile.
            let (hb, ib, _) = object('b', "pkg-b");
            let ib = ib.replace(&format!("nar/{hb}.nar.xz"), &format!("nar/{nar}"));
            let info = NarInfo::parse(&ib).unwrap();
            let r = evict_with(&store, now, &Lru::new(1), &|_| false, || match writer {
                "publish" => {
                    let published = store.publish_pulled(&info, ib.as_bytes(), None).unwrap();
                    assert_eq!(published, crate::store::Published::Stored);
                }
                "push" => store.put_narinfo(&info, ib.as_bytes()).unwrap(),
                _ => {
                    let f = std::fs::File::open(store.narinfo_path(&ha).unwrap()).unwrap();
                    crate::store::mark_used(&f);
                }
            })
            .unwrap();
            assert!(r.evicted.is_empty(), "{writer}: evicted {:?}", r.evicted);
            assert!(store.nar_path(&nar).unwrap().exists(), "{writer}");
            assert!(dangling(&store).is_empty(), "{writer}");
            let _ = std::fs::remove_dir_all(&store.root);
        }
    }

    #[test]
    fn under_a_cap_the_sweep_leaves_unlisted_paths_to_eviction() {
        let now = SystemTime::now();
        let store = Store::open(tmp("sweep-lru")).unwrap();
        let (hl, il, nl) = object('l', "pkg-l");
        put_path(&store, &il, &nl, now - DAY * 2);
        let mb = manifest_at(
            "main-1-x",
            "main",
            &[format!("/nix/store/{hl}-pkg-l")],
            &ts(now, 0),
        );
        store
            .put_manifest(&Manifest::parse(&mb).unwrap(), &mb)
            .unwrap();
        let h = unlisted(&store, now, &['a'], 3);
        // A NAR no narinfo names, past grace, still goes.
        store.put_nar("orphan.nar.xz", &b"X"[..]).unwrap();
        age(&store.nar_path("orphan.nar.xz").unwrap(), now - DAY * 2);

        let r = box_sweep(&store, now, GRACE, true).unwrap();
        assert_eq!(r.deleted, ["nar/orphan.nar.xz"]);
        assert!(store.has(&h[0]) && store.has(&hl));
        // Without the cap the same store loses the unlisted path.
        let r = box_sweep(&store, now, GRACE, false).unwrap();
        let mut deleted = r.deleted.clone();
        deleted.sort();
        assert_eq!(
            deleted,
            [format!("{}.narinfo", h[0]), format!("nar/{}.nar.xz", h[0])]
        );
        let _ = std::fs::remove_dir_all(&store.root);
    }

    #[test]
    fn a_box_without_remote_or_manifests_evicts_in_process() {
        let now = SystemTime::now();
        let store = Store::open(tmp("in-process")).unwrap();
        let h = unlisted(&store, now, &['a', 'b', 'c'], 10_000);
        let box_ = app(store, Some(Lru::new(15_000)));
        let maintained = box_.clone();
        std::thread::spawn(move || maintain(&maintained, Duration::from_millis(10)));
        for _ in 0..500 {
            if box_.lru.as_ref().unwrap().evicted.load(Ordering::Relaxed) == 2 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(has_all(&box_.store, &h), [false, false, true]);
        assert!(box_.lru.as_ref().unwrap().bytes.load(Ordering::Relaxed) <= 15_000);
        let _ = std::fs::remove_dir_all(&box_.store.root);
    }

    #[test]
    fn a_failing_pass_fails_no_request_and_metrics_report_the_class() {
        let now = SystemTime::now();
        let store = Store::open(tmp("failing")).unwrap();
        let h = unlisted(&store, now, &['a'], 3);
        // A manifest that cannot be read fails every pass.
        std::fs::create_dir_all(store.root.join("roots/znix/broken.json")).unwrap();
        assert!(evict(&store, now, &Lru::new(1), &|_| false).is_err());

        let box_ = app(store, Some(Lru::new(1)));
        let maintained = box_.clone();
        std::thread::spawn(move || maintain(&maintained, Duration::from_millis(10)));
        let base = serve(box_.clone());
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(get(&format!("{base}/{}.narinfo", h[0])), 200);
        assert_eq!(get(&format!("{base}/nar/{}.nar.xz", h[0])), 200);

        // Readable again: a pass evicts nothing used just now, and the
        // metrics say what is left.
        std::fs::remove_dir(box_.store.root.join("roots/znix/broken.json")).unwrap();
        let lru = box_.lru.as_ref().unwrap();
        let r = evict(&box_.store, SystemTime::now(), lru, &|_| false).unwrap();
        assert!(r.evicted.is_empty());
        let size: u64 = box_.store.narinfo_files().unwrap()[0].2 + 3;
        let agent = ureq::Agent::new_with_defaults();
        let metrics = agent
            .get(format!("{base}/metrics"))
            .call()
            .unwrap()
            .body_mut()
            .read_to_string()
            .unwrap();
        for line in [
            "# TYPE kasha_unlisted_bytes gauge".to_string(),
            format!("kasha_unlisted_bytes {size}"),
            "kasha_unlisted_paths 1".to_string(),
            "# TYPE kasha_unlisted_evictions_total counter".to_string(),
            "kasha_unlisted_evictions_total 0".to_string(),
            "kasha_unlisted_evicted_bytes_total 0".to_string(),
        ] {
            assert!(
                metrics.lines().any(|l| l == line),
                "missing {line:?} in:\n{metrics}"
            );
        }
        let _ = std::fs::remove_dir_all(&box_.store.root);
    }
}
