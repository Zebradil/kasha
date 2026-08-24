//! Garbage collection: box sweep (in-process timer) and remote sweep (CI).
//!
//! Both mark-sweep from surviving manifests via the shared retention
//! selector. Mark set = union of retained manifests' closure lists (no
//! transitive narinfo walk); live nar keys come from retained narinfos'
//! URL fields. A 24h grace window skips young objects, which — together
//! with manifest-published-last ordering — makes the sweep race-safe and
//! idempotent without locks.

use anyhow::Result;
use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use crate::manifest::Manifest;
use crate::narinfo::store_hash_of;
use crate::remote::{FANOUT, Remote};
use crate::retention::{Gen, Policy, retain};
use crate::store::Store;

pub const GRACE: Duration = Duration::from_secs(24 * 3600);

/// A remote sweep reads one object per retained narinfo, so a large bucket runs
/// for minutes even fanned out. Report progress this often to keep a slow sweep
/// distinguishable from a wedged one.
const PROGRESS_EVERY: usize = 1000;

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
pub fn box_sweep(store: &Store, now: SystemTime, grace: Duration) -> Result<SweepReport> {
    let manifests = store.manifests()?;
    // No manifests means no mark set, which would sweep the whole store. That
    // is never a retention decision — it is a store that has not synced yet
    // (fresh volume, restored backup), so leave it alone.
    if manifests.is_empty() {
        tracing::info!("box sweep skipped: no manifests to mark from");
        return Ok(SweepReport::default());
    }
    let gens: Vec<Gen> = manifests
        .iter()
        .map(|(_, m)| to_gen(format!("{}/{}", m.flake, m.gen_id), m))
        .collect();
    let mut keep = retain(&gens, &Policy::boxed(), now);
    for (_, m) in &manifests {
        if store.is_local_origin(&m.flake, &m.gen_id) && !store.is_mirrored(&m.flake, &m.gen_id) {
            keep.insert(format!("{}/{}", m.flake, m.gen_id));
        }
    }

    let mut mark: HashSet<String> = HashSet::new();
    for (_, m) in &manifests {
        if keep.contains(&format!("{}/{}", m.flake, m.gen_id)) {
            mark.extend(m.closure.iter().map(|p| store_hash_of(p).to_string()));
        }
    }
    let live_nars: HashSet<String> = mark
        .iter()
        .filter_map(|h| store.url_of(h))
        .filter_map(|u| u.strip_prefix("nar/").map(str::to_string))
        .collect();

    let mut report = SweepReport {
        retained_manifests: keep.len(),
        ..Default::default()
    };
    for (hash, mtime) in store.narinfo_files()? {
        if mark.contains(&hash) {
            continue;
        }
        if now.duration_since(mtime).unwrap_or(Duration::ZERO) < grace {
            report.skipped_young += 1;
            continue;
        }
        store.remove_narinfo(&hash)?;
        report.deleted.push(format!("{hash}.narinfo"));
    }
    for (file, mtime) in store.nar_files()? {
        if live_nars.contains(&file) {
            continue;
        }
        if now.duration_since(mtime).unwrap_or(Duration::ZERO) < grace {
            report.skipped_young += 1;
            continue;
        }
        store.remove_nar(&file)?;
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
    let mut gens = Vec::new();
    let mut manifests = Vec::new();
    let mut garbage_roots: Vec<(&str, SystemTime)> = Vec::new();
    for (i, (key, t)) in roots.iter().enumerate() {
        if i > 0 && i % PROGRESS_EVERY == 0 {
            tracing::info!(
                read = i,
                total = roots.len(),
                "remote sweep: reading manifests"
            );
        }
        match remote.get(key)?.as_deref().map(Manifest::parse) {
            Some(Ok(m)) => {
                gens.push(to_gen(key.to_string(), &m));
                manifests.push((key.to_string(), *t, m));
            }
            _ => garbage_roots.push((key, *t)),
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
    let next = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    let live_nars: HashSet<String> = std::thread::scope(|scope| -> Result<HashSet<String>> {
        let workers: Vec<_> = (0..FANOUT.min(marked.len()))
            .map(|_| {
                scope.spawn(|| -> Result<Vec<String>> {
                    let mut urls = Vec::new();
                    while let Some(h) = marked.get(next.fetch_add(1, Ordering::Relaxed)) {
                        if let Some(raw) = remote.get(&format!("{h}.narinfo"))?
                            && let Ok(info) =
                                crate::narinfo::NarInfo::parse(std::str::from_utf8(&raw)?)
                        {
                            urls.push(info.url);
                        }
                        let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                        if n.is_multiple_of(PROGRESS_EVERY) {
                            tracing::info!(
                                read = n,
                                marked = marked.len(),
                                "remote sweep: reading narinfos"
                            );
                        }
                    }
                    Ok(urls)
                })
            })
            .collect();
        let mut live = HashSet::new();
        for w in workers {
            live.extend(w.join().expect("narinfo reader panicked")?);
        }
        Ok(live)
    })?;
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
        let r = box_sweep(&store, now, GRACE).unwrap();
        assert!(r.deleted.is_empty());
        assert!(r.skipped_young > 0);

        // Zero grace: gen 'd' (4th newest main) swept; local-origin 'e' kept.
        let r = box_sweep(&store, now, Duration::ZERO).unwrap();
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
        assert_eq!(store.manifests().unwrap().len(), 5);

        // Once mirrored, the guard lifts; 'e' is still newest in its group
        // so it stays retained by count.
        store.mark_mirrored("znix", "local-1-x").unwrap();
        let r = box_sweep(&store, now, Duration::ZERO).unwrap();
        assert!(r.deleted.is_empty());
        let _ = std::fs::remove_dir_all(dir);
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
        let r = box_sweep(&store, now, Duration::ZERO).unwrap();
        assert!(r.deleted.is_empty());
        assert!(store.has(h.as_str()));
        let _ = std::fs::remove_dir_all(dir);
    }
}
