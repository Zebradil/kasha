//! Flat binary-cache store on disk, same layout as the R2 bucket:
//! `<storehash>.narinfo`, `nar/<filehash>.nar.*`, `roots/<flake>/<gen>.json`,
//! plus `state/` bookkeeping. The filesystem is the database; the in-memory
//! index is rebuilt by scanning narinfos at boot.

use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use crate::manifest::Manifest;
use crate::narinfo::NarInfo;

pub struct Store {
    pub root: PathBuf,
    /// storehash -> nar URL (relative key like `nar/<filehash>.nar.xz`).
    index: RwLock<HashMap<String, String>>,
}

/// What `Store::publish_pulled` did with a pulled-through path.
#[derive(Debug, PartialEq)]
pub enum Published {
    Stored,
    /// The store already holds a narinfo for the path; nothing was written.
    Lost,
    /// A NAR of that name appeared while the caller downloaded its own.
    NarExists,
    /// The caller relied on a stored NAR that is gone.
    NarMissing,
}

/// Reject path components that could escape the store root.
fn safe_component(s: &str) -> Result<&str> {
    if s.is_empty() || s.contains('/') || s.contains("..") || s.starts_with('.') {
        bail!("unsafe path component: {s:?}");
    }
    Ok(s)
}

impl Store {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        for d in ["nar", "roots", "state/local-origin", "state/mirrored"] {
            fs::create_dir_all(root.join(d))?;
        }
        let store = Store {
            root,
            index: RwLock::new(HashMap::new()),
        };
        store.scan()?;
        Ok(store)
    }

    /// Rebuild the index from `*.narinfo` files. Filename is authoritative for
    /// the hash; only the URL field is read (no signature re-check: ingest
    /// already verified, and boot must stay fast).
    fn scan(&self) -> Result<()> {
        let mut idx = HashMap::new();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let name = entry.file_name();
            // Dot-files are `write_atomic` temps, not narinfos.
            let Some(hash) = name
                .to_str()
                .filter(|n| !n.starts_with('.'))
                .and_then(|n| n.strip_suffix(".narinfo"))
            else {
                continue;
            };
            let text = fs::read_to_string(entry.path())?;
            match NarInfo::parse(&text) {
                Ok(n) => {
                    idx.insert(hash.to_string(), n.url);
                }
                Err(e) => tracing::warn!(hash, error = %e, "skipping unparseable narinfo"),
            }
        }
        *self.index.write().unwrap() = idx;
        Ok(())
    }

    /// Remove temp files a killed process left behind; returns how many.
    /// Only old ones go: a young one can belong to a writer that is still
    /// at it, in this process or in another `serve` on the volume, such as
    /// the one a rolling restart replaces. Nothing here is worth failing
    /// for, so errors are skipped.
    pub fn clean_temps(&self) -> usize {
        let mut dirs = vec![self.root.clone(), self.root.join("nar")];
        if let Ok(flakes) = fs::read_dir(self.root.join("roots")) {
            dirs.extend(flakes.flatten().map(|f| f.path()));
        }
        let stale = |e: &fs::DirEntry| {
            e.file_name().to_string_lossy().starts_with(TEMP_PREFIX)
                && e.metadata()
                    .and_then(|m| m.modified())
                    .is_ok_and(|t| t.elapsed().is_ok_and(|age| age >= TEMP_MAX_AGE))
        };
        dirs.iter()
            .filter_map(|dir| fs::read_dir(dir).ok())
            .flat_map(|entries| entries.flatten())
            .filter(|e| stale(e) && fs::remove_file(e.path()).is_ok())
            .count()
    }

    pub fn len(&self) -> usize {
        self.index.read().unwrap().len()
    }

    #[cfg(test)]
    pub fn has(&self, store_hash: &str) -> bool {
        self.index.read().unwrap().contains_key(store_hash)
    }

    pub fn narinfo_path(&self, store_hash: &str) -> Result<PathBuf> {
        Ok(self
            .root
            .join(format!("{}.narinfo", safe_component(store_hash)?)))
    }

    pub fn nar_path(&self, file: &str) -> Result<PathBuf> {
        Ok(self.root.join("nar").join(safe_component(file)?))
    }

    /// Store a verified narinfo byte-identical and index it. Caller runs
    /// `NarInfo::verify` first. Its NAR may come later: Nix can push the
    /// narinfo first.
    pub fn put_narinfo(&self, info: &NarInfo, raw: &[u8]) -> Result<()> {
        self.put_narinfo_checked(info, raw, false)
    }

    /// `put_narinfo` for a caller that saw the NAR stored: fails, writing
    /// nothing, when the NAR is gone by the time the narinfo would land,
    /// removed by eviction or a sweep since.
    pub fn put_narinfo_of_stored_nar(&self, info: &NarInfo, raw: &[u8]) -> Result<()> {
        self.put_narinfo_checked(info, raw, true)
    }

    fn put_narinfo_checked(&self, info: &NarInfo, raw: &[u8], nar_required: bool) -> Result<()> {
        let path = self.narinfo_path(info.store_hash())?;
        let nar = info.url.strip_prefix("nar/").map(|f| self.nar_path(f));
        let nar = nar.transpose()?;
        let tmp = write_temp(&path, |f| f.write_all(raw))?;
        // Rename and index update under one lock, so index and disk agree
        // when this races `publish_pulled` for the same path. The write and
        // its fsync stay outside: the lock also serves every index reader.
        let mut idx = self.index.write().unwrap();
        // The NAR this names may be past grace already: a push skips the
        // upload of a NAR the box has. As in `publish_locked`, it is made as
        // young as its new narinfo, so no sweep that listed the store before
        // takes it. Absent is fine unless the caller counted on it.
        if let Some(nar) = nar {
            match touch(&nar) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound && !nar_required => {}
                Err(e) => {
                    let _ = fs::remove_file(&tmp);
                    return Err(e).context("touching the narinfo's NAR");
                }
            }
        }
        move_into_place(&tmp, &path)?;
        idx.insert(info.store_hash().to_string(), info.url.clone());
        Ok(())
    }

    /// A new temp file beside the NAR `file`, for a download that
    /// `publish_pulled` then moves into place.
    pub fn nar_temp(&self, file: &str) -> Result<(PathBuf, fs::File)> {
        create_temp(&self.nar_path(file)?)
    }

    /// Store a pulled-through path without replacing anything: the NAR
    /// (`nar_tmp`, or one already stored under the narinfo's `URL`) and then
    /// its narinfo. An existing narinfo wins, and so does an existing NAR of
    /// that name; either way the store is left untouched and the caller keeps
    /// its temp file. Caller verified both objects.
    pub fn publish_pulled(
        &self,
        info: &NarInfo,
        raw: &[u8],
        nar_tmp: Option<&Path>,
    ) -> Result<Published> {
        self.publish(info, raw, nar_tmp, None)
    }

    /// Store a re-encoded path as `publish_pulled` does, except that its
    /// narinfo replaces the stored one, and only while that still reads
    /// `old`: a narinfo written since the caller read it wins (`Lost`). The
    /// NAR this call creates and the narinfo get mtime `used`, so the path
    /// keeps its last use; a reused NAR is touched as by every writer, since
    /// eviction may be about to remove it for another path that names it.
    /// The NAR file `old_nar` the replaced narinfo named is touched too.
    pub fn publish_reencoded(
        &self,
        info: &NarInfo,
        raw: &[u8],
        old: &[u8],
        old_nar: &Path,
        nar_tmp: Option<&Path>,
        used: SystemTime,
    ) -> Result<Published> {
        self.publish(info, raw, nar_tmp, Some((old, used, old_nar)))
    }

    fn publish(
        &self,
        info: &NarInfo,
        raw: &[u8],
        nar_tmp: Option<&Path>,
        replace: Option<(&[u8], SystemTime, &Path)>,
    ) -> Result<Published> {
        let narinfo = self.narinfo_path(info.store_hash())?;
        let file = info.url.strip_prefix("nar/").context("nar URL")?;
        let nar = self.nar_path(file)?;
        let narinfo_tmp = write_temp(&narinfo, |f| f.write_all(raw))?;
        let published = self.publish_locked(info, &narinfo, &narinfo_tmp, &nar, nar_tmp, replace);
        if !matches!(published, Ok(Published::Stored)) {
            let _ = fs::remove_file(&narinfo_tmp);
        }
        published
    }

    /// The part of `publish` that needs the index lock: checks, renames,
    /// times and the index update, no file contents.
    fn publish_locked(
        &self,
        info: &NarInfo,
        narinfo: &Path,
        narinfo_tmp: &Path,
        nar: &Path,
        nar_tmp: Option<&Path>,
        replace: Option<(&[u8], SystemTime, &Path)>,
    ) -> Result<Published> {
        // Every narinfo writer takes this lock, so the check below holds
        // until this narinfo is in place.
        let mut idx = self.index.write().unwrap();
        let lost = match replace {
            None => narinfo.exists(),
            Some((old, _, _)) => fs::read(narinfo).map_or(true, |stored| stored != old),
        };
        if lost {
            return Ok(Published::Lost);
        }
        match nar_tmp {
            // A hard link is the portable way to create without replacing.
            Some(tmp) => match fs::hard_link(tmp, nar) {
                Ok(()) => {
                    let _ = fs::remove_file(tmp);
                    // Set under the lock: no sweep or eviction judges the
                    // file by this time before its narinfo is in place.
                    if let Some((_, used, _)) = replace
                        && let Err(e) = set_mtime(nar, used)
                    {
                        let _ = fs::remove_file(nar);
                        return Err(e).with_context(|| format!("dating {}", nar.display()));
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    return Ok(Published::NarExists);
                }
                Err(e) => return Err(e).with_context(|| format!("storing {}", nar.display())),
            },
            // The sweep judges a NAR by its age. A reused one may be past
            // grace already, so it is made as young as the narinfo that
            // names it from now on.
            None => match touch(nar) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(Published::NarMissing);
                }
                Err(e) => return Err(e).with_context(|| format!("touching {}", nar.display())),
            },
        }
        if let Err(e) = fs::rename(narinfo_tmp, narinfo) {
            // No narinfo names the NAR this call just created, and none can
            // appear while the lock is held.
            if nar_tmp.is_some() {
                let _ = fs::remove_file(nar);
            }
            return Err(e).with_context(|| format!("writing {}", narinfo.display()));
        }
        idx.insert(info.store_hash().to_string(), info.url.clone());
        // On the narinfo in place, not its temp file: `clean_temps` takes a
        // temp file this old for abandoned.
        if let Some((_, used, old_nar)) = replace {
            let dated = set_mtime(narinfo, used);
            // No narinfo names the old NAR from now on, so orphan removal and
            // the sweep judge it by age: touched, it stays the grace window
            // for a client that read the old narinfo just before the swap.
            match touch(old_nar) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                    return Err(e).with_context(|| format!("touching {}", old_nar.display()));
                }
                _ => {}
            }
            dated.with_context(|| format!("dating {}", narinfo.display()))?;
        }
        Ok(Published::Stored)
    }

    /// Stream a nar object to disk byte-identical.
    pub fn put_nar(&self, file: &str, mut body: impl Read) -> Result<u64> {
        let path = self.nar_path(file)?;
        let mut n = 0;
        write_atomic(&path, |f| {
            n = std::io::copy(&mut body, f)?;
            Ok(())
        })?;
        Ok(n)
    }

    /// Stream a nar object to disk unless one of that name is stored, which
    /// is kept. Returns the bytes stored, 0 when the existing file was kept.
    pub fn put_nar_new(&self, file: &str, mut body: impl Read) -> Result<u64> {
        let path = self.nar_path(file)?;
        let mut n = 0;
        let tmp = write_temp(&path, |f| {
            n = std::io::copy(&mut body, f)?;
            Ok(())
        })?;
        let linked = fs::hard_link(&tmp, &path);
        let _ = fs::remove_file(&tmp);
        match linked {
            Ok(()) => Ok(n),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(0),
            Err(e) => Err(e).with_context(|| format!("storing {}", path.display())),
        }
    }

    pub fn manifest_path(&self, flake: &str, gen_id: &str) -> Result<PathBuf> {
        Ok(self
            .root
            .join("roots")
            .join(safe_component(flake)?)
            .join(format!("{}.json", safe_component(gen_id)?)))
    }

    pub fn put_manifest(&self, m: &Manifest, raw: &[u8]) -> Result<()> {
        let path = self.manifest_path(&m.flake, &m.gen_id)?;
        fs::create_dir_all(path.parent().unwrap())?;
        write_atomic(&path, |f| f.write_all(raw))
    }

    /// Manifest files under `roots/` as `(flake, gen, path)`, unparsed.
    /// Callers load one at a time: every closure at once is hundreds of MiB.
    pub fn manifest_files(&self) -> Result<Vec<(String, String, PathBuf)>> {
        let mut out = Vec::new();
        for flake in fs::read_dir(self.root.join("roots"))? {
            let flake = flake?;
            if !flake.file_type()?.is_dir() {
                continue;
            }
            let Some(flake_name) = flake.file_name().to_str().map(str::to_string) else {
                continue;
            };
            for f in fs::read_dir(flake.path())? {
                let f = f?;
                let name = f.file_name();
                // Dot-files are `write_atomic` temps, not manifests.
                let Some(gen_id) = name
                    .to_str()
                    .filter(|n| !n.starts_with('.'))
                    .and_then(|n| n.strip_suffix(".json"))
                else {
                    continue;
                };
                out.push((flake_name.clone(), gen_id.to_string(), f.path()));
            }
        }
        Ok(out)
    }

    /// Parse one manifest file; `None` (logged) when it is not valid v3.
    /// Absent too when mirror-down removed it since it was listed.
    pub fn load_manifest(&self, path: &Path) -> Result<Option<Manifest>> {
        let raw = match fs::read(path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        match Manifest::parse(&raw) {
            Ok(m) => Ok(Some(m)),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "skipping bad manifest");
                Ok(None)
            }
        }
    }

    /// Missing closure paths of a manifest (by narinfo presence in the index).
    pub fn gaps(&self, m: &Manifest) -> Vec<String> {
        let idx = self.index.read().unwrap();
        m.closure
            .iter()
            .filter(|p| !idx.contains_key(crate::narinfo::store_hash_of(p)))
            .cloned()
            .collect()
    }

    // --- state markers -----------------------------------------------------

    fn sweep_stamp(&self) -> PathBuf {
        self.root.join("state").join("last-sweep")
    }

    /// When the last box sweep was attempted, from the stamp's mtime. `None`
    /// on a store that has never swept.
    pub fn last_sweep(&self) -> Option<SystemTime> {
        fs::metadata(self.sweep_stamp())
            .and_then(|m| m.modified())
            .ok()
    }

    /// Stamp a sweep attempt. Call before sweeping: a sweep that fails or is
    /// killed mid-way (OOM) then keeps the cadence instead of rerunning on
    /// every restart.
    pub fn record_sweep(&self) -> Result<()> {
        fs::write(self.sweep_stamp(), b"")?;
        Ok(())
    }

    fn marker(&self, kind: &str, flake: &str, gen_id: &str) -> Result<PathBuf> {
        Ok(self.root.join("state").join(kind).join(format!(
            "{}--{}",
            safe_component(flake)?,
            safe_component(gen_id)?
        )))
    }

    /// A gen whose manifest arrived via the authenticated push API.
    pub fn mark_local_origin(&self, flake: &str, gen_id: &str) -> Result<()> {
        fs::write(self.marker("local-origin", flake, gen_id)?, b"")?;
        Ok(())
    }

    pub fn is_local_origin(&self, flake: &str, gen_id: &str) -> bool {
        self.marker("local-origin", flake, gen_id)
            .map(|p| p.exists())
            .unwrap_or(false)
    }

    /// Confirmed fully present in the remote cache (manifest included).
    pub fn mark_mirrored(&self, flake: &str, gen_id: &str) -> Result<()> {
        fs::write(self.marker("mirrored", flake, gen_id)?, b"")?;
        Ok(())
    }

    pub fn is_mirrored(&self, flake: &str, gen_id: &str) -> bool {
        self.marker("mirrored", flake, gen_id)
            .map(|p| p.exists())
            .unwrap_or(false)
    }

    /// Delete a manifest and its markers (mirror-down reflecting a remote
    /// retention decision — never call for unmirrored local-origin gens).
    pub fn remove_manifest(&self, flake: &str, gen_id: &str) -> Result<()> {
        let p = self.manifest_path(flake, gen_id)?;
        if p.exists() {
            fs::remove_file(p)?;
        }
        for kind in ["local-origin", "mirrored"] {
            let m = self.marker(kind, flake, gen_id)?;
            if m.exists() {
                fs::remove_file(m)?;
            }
        }
        Ok(())
    }

    /// Remove a narinfo (file + index entry). Sweep helper.
    pub fn remove_narinfo(&self, store_hash: &str) -> Result<()> {
        self.remove_narinfo_unless(store_hash, |_| false).map(drop)
    }

    /// Remove a narinfo unless `keep`, given its mtime as it is now, says
    /// otherwise; returns whether it was removed. Decided under the index
    /// lock, and file and entry go together, so no reader of the index sees
    /// a narinfo that is gone from disk.
    pub fn remove_narinfo_unless(
        &self,
        store_hash: &str,
        keep: impl FnOnce(SystemTime) -> bool,
    ) -> Result<bool> {
        let p = self.narinfo_path(store_hash)?;
        let mut idx = self.index.write().unwrap();
        let removed = match fs::metadata(&p).and_then(|m| m.modified()) {
            Ok(t) if keep(t) => return Ok(false),
            Ok(_) => fs::remove_file(&p).map(|()| true),
            Err(e) => Err(e),
        };
        match removed {
            Ok(removed) => {
                idx.remove(store_hash);
                Ok(removed)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                idx.remove(store_hash);
                Ok(false)
            }
            Err(e) => Err(e).with_context(|| format!("removing {}", p.display())),
        }
    }

    /// Drop index entries whose narinfo is gone from disk: deleted by a sweep
    /// in another process (`kasha sweep`), which this index cannot see. A
    /// stale entry hides the path from `gaps`, so mirror-down would never
    /// refetch it. Prune-only under the write lock, never a rebuild: a
    /// `put_narinfo` racing a rebuild could drop out of the index for good.
    pub fn prune_index(&self) -> usize {
        let mut idx = self.index.write().unwrap();
        let before = idx.len();
        idx.retain(|h, _| self.narinfo_path(h).is_ok_and(|p| p.exists()));
        before - idx.len()
    }

    /// Store hashes whose indexed nar URL passes `keep`.
    pub fn hashes_by_url(&self, keep: impl Fn(&str) -> bool) -> Vec<String> {
        let idx = self.index.read().unwrap();
        idx.iter()
            .filter(|(_, url)| keep(url))
            .map(|(hash, _)| hash.clone())
            .collect()
    }

    /// Indexed nar URL for a store hash.
    pub fn url_of(&self, store_hash: &str) -> Option<String> {
        self.index.read().unwrap().get(store_hash).cloned()
    }

    /// All narinfo hashes on disk with mtimes and sizes (sweep input).
    pub fn narinfo_files(&self) -> Result<Vec<(String, SystemTime, u64)>> {
        let mut out = Vec::new();
        for e in fs::read_dir(&self.root)? {
            let e = e?;
            let name = e.file_name();
            if let Some(hash) = name
                .to_str()
                .filter(|n| !n.starts_with('.'))
                .and_then(|n| n.strip_suffix(".narinfo"))
            {
                let m = e.metadata()?;
                out.push((hash.to_string(), m.modified()?, m.len()));
            }
        }
        Ok(out)
    }

    /// All nar object file names with mtimes and sizes (sweep and age
    /// metrics input).
    pub fn nar_files(&self) -> Result<Vec<(String, SystemTime, u64)>> {
        let mut out = Vec::new();
        for e in fs::read_dir(self.root.join("nar"))? {
            let e = e?;
            if let Some(name) = e.file_name().to_str()
                && !name.starts_with('.')
            {
                let m = e.metadata()?;
                out.push((name.to_string(), m.modified()?, m.len()));
            }
        }
        Ok(out)
    }

    /// mtime of one nar object as it is now; None when it is gone.
    pub fn nar_modified(&self, file: &str) -> Option<SystemTime> {
        fs::metadata(self.nar_path(file).ok()?)
            .and_then(|m| m.modified())
            .ok()
    }

    /// Remove a nar object unless `keep`, given its mtime as it is now, says
    /// otherwise; returns whether it was removed. Decided under the index
    /// lock, so a narinfo writer, which touches the NAR it names under that
    /// lock, either comes first and keeps it young or finds it gone.
    /// Unlinking a large file can take long, so under the lock it is only
    /// renamed away. Nothing new is created, so a full disk does not stop it.
    pub fn remove_nar_unless(
        &self,
        file: &str,
        keep: impl FnOnce(SystemTime) -> bool,
    ) -> Result<bool> {
        let p = self.nar_path(file)?;
        let dead = dead_beside(&p)?;
        let moved = {
            let _idx = self.index.write().unwrap();
            match fs::metadata(&p).and_then(|m| m.modified()) {
                Ok(t) if keep(t) => return Ok(false),
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
                Err(e) => return Err(e).with_context(|| format!("removing {}", p.display())),
            }
            // A dead name another process also used holds a NAR on its way
            // out too, so replacing it loses nothing.
            fs::rename(&p, &dead).is_ok()
                || fs::remove_file(&p)
                    .map(|()| false)
                    .with_context(|| format!("removing {}", p.display()))?
        };
        if moved {
            let _ = fs::remove_file(&dead);
        }
        Ok(true)
    }

    /// Read a narinfo's raw text by store hash, if present.
    pub fn read_narinfo(&self, store_hash: &str) -> Result<Option<String>> {
        let p = self.narinfo_path(store_hash)?;
        match fs::read_to_string(&p) {
            Ok(t) => Ok(Some(t)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).context("read narinfo"),
        }
    }

    /// Total bytes under the store root (walked; called from /status only).
    pub fn disk_usage(&self) -> u64 {
        fn walk(dir: &Path) -> u64 {
            let Ok(rd) = fs::read_dir(dir) else { return 0 };
            rd.flatten()
                .map(|e| match e.file_type() {
                    Ok(t) if t.is_dir() => walk(&e.path()),
                    Ok(t) if t.is_file() => e.metadata().map(|m| m.len()).unwrap_or(0),
                    _ => 0,
                })
                .sum()
        }
        walk(&self.root)
    }
}

/// The `Compression` a NAR file's name says, as Nix names them
/// (`.nar.xz`, `.nar.zst`, `.nar.bz2`, `.nar`); `other` for the rest.
pub fn compression_of(file: &str) -> &'static str {
    match file.rsplit_once(".nar") {
        Some((_, "")) => "none",
        Some((_, ".xz")) => "xz",
        Some((_, ".zst")) => "zstd",
        Some((_, ".bz2")) => "bzip2",
        _ => "other",
    }
}

const TEMP_PREFIX: &str = ".tmp-";

/// A temp file untouched for this long has no writer: one that is still
/// being written keeps a fresh mtime. A download may receive nothing for
/// less than this (`KASHA_PULL_THROUGH_STALL_TIMEOUT`).
pub const TEMP_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(3600);

fn touch(path: &Path) -> std::io::Result<()> {
    set_mtime(path, SystemTime::now())
}

/// Read-only: the owner may set times on a file it cannot open for writing.
fn set_mtime(path: &Path, t: SystemTime) -> std::io::Result<()> {
    fs::File::open(path)?.set_modified(t)
}

/// The resolution of recorded use: a read rewrites an mtime only when it is
/// older than this, so a hot file is not a metadata write per request.
pub const USE_RESOLUTION: std::time::Duration = std::time::Duration::from_secs(60);

/// Record a read of an open narinfo or NAR file as its use (ADR-0013). A
/// failure fails no request, but eviction then goes by when files were
/// stored, so it is reported once.
pub fn mark_used(f: &fs::File) {
    static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    // A time in the future (a clock step, restored files) is rewritten too,
    // or it would never be.
    let stale = f
        .metadata()
        .and_then(|m| m.modified())
        .is_ok_and(|t| match t.elapsed() {
            Ok(age) => age >= USE_RESOLUTION,
            Err(ahead) => ahead.duration() >= USE_RESOLUTION,
        });
    if stale && let Err(e) = f.set_modified(SystemTime::now()) {
        if WARNED.swap(true, Ordering::Relaxed) {
            tracing::debug!(error = %e, "recording use failed");
        } else {
            tracing::warn!(
                error = %e,
                "recording use failed; eviction goes by store time where this repeats"
            );
        }
    }
}

/// A temp path beside `path`, different on every call within one process.
fn temp_beside(path: &Path) -> Result<PathBuf> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = path.parent().context("no parent dir")?;
    Ok(dir.join(format!(
        "{TEMP_PREFIX}{}-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed),
        path.file_name().context("no file name")?.to_string_lossy()
    )))
}

/// A name beside `path` for a file on its way out, one `clean_temps`
/// removes should this process die before it does.
fn dead_beside(path: &Path) -> Result<PathBuf> {
    let tmp = temp_beside(path)?;
    let name = tmp.file_name().context("no file name")?.to_string_lossy();
    Ok(tmp.with_file_name(name.replacen(TEMP_PREFIX, &format!("{TEMP_PREFIX}dead-"), 1)))
}

/// Create a temp file beside `path` that no other writer has. Pid and
/// counter do not make the name unique across processes: in containers
/// every `serve` is pid 1 and counts from zero, and two can share a volume.
/// So the file is created exclusively, moving on to the next name when one
/// is taken.
fn create_temp(path: &Path) -> Result<(PathBuf, fs::File)> {
    loop {
        let tmp = temp_beside(path)?;
        match fs::File::options().write(true).create_new(true).open(&tmp) {
            Ok(f) => return Ok((tmp, f)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e).with_context(|| format!("creating {}", tmp.display())),
        }
    }
}

/// Write and sync a temp file beside `path`, for `move_into_place`.
fn write_temp(
    path: &Path,
    fill: impl FnOnce(&mut fs::File) -> std::io::Result<()>,
) -> Result<PathBuf> {
    let (tmp, mut f) = create_temp(path)?;
    let res = fill(&mut f).and_then(|()| f.sync_all());
    drop(f);
    match res {
        Ok(()) => Ok(tmp),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e).with_context(|| format!("writing {}", path.display()))
        }
    }
}

fn move_into_place(tmp: &Path, path: &Path) -> Result<()> {
    fs::rename(tmp, path).map_err(|e| {
        let _ = fs::remove_file(tmp);
        anyhow::Error::new(e).context(format!("writing {}", path.display()))
    })
}

/// Write via temp file + rename so readers never see partial objects.
fn write_atomic(
    path: &Path,
    fill: impl FnOnce(&mut fs::File) -> std::io::Result<()>,
) -> Result<()> {
    move_into_place(&write_temp(path, fill)?, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const REAL: &str = "StorePath: /nix/store/v4f0jj9sz97ckskvacf40llz4nfr19jf-hello-2.12.3\n\
URL: nar/00g966jlz9h37xkb9pmr3rc700i4k19mkyqm3gmwvlaik16qam5x.nar.zst\n\
NarHash: sha256:00g966jlz9h37xkb9pmr3rc700i4k19mkyqm3gmwvlaik16qam5x\n\
NarSize: 113096\n\
References: jspv3c5l2zx4kiwzhq0zgxcwp34cqifz-libiconv-115.100.1\n";

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("kasha-test-{}", rand_suffix()));
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn rand_suffix() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        format!(
            "{}-{:?}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            std::thread::current().id()
        )
        .replace(['(', ')', ' '], "")
    }

    #[test]
    fn put_scan_roundtrip() {
        let root = tmp();
        {
            let s = Store::open(&root).unwrap();
            let n = NarInfo::parse(REAL).unwrap();
            s.put_narinfo(&n, REAL.as_bytes()).unwrap();
            s.put_nar("f.nar.zst", &b"nar bytes"[..]).unwrap();
            assert!(s.has("v4f0jj9sz97ckskvacf40llz4nfr19jf"));
        }
        // Fresh open rebuilds the index from disk.
        let s = Store::open(&root).unwrap();
        assert_eq!(s.len(), 1);
        assert!(s.has("v4f0jj9sz97ckskvacf40llz4nfr19jf"));
        assert_eq!(
            s.read_narinfo("v4f0jj9sz97ckskvacf40llz4nfr19jf")
                .unwrap()
                .unwrap(),
            REAL
        );
        s.remove_narinfo("v4f0jj9sz97ckskvacf40llz4nfr19jf")
            .unwrap();
        assert!(!s.has("v4f0jj9sz97ckskvacf40llz4nfr19jf"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn publish_pulled_never_replaces() {
        let root = tmp();
        let s = Store::open(&root).unwrap();
        let n = NarInfo::parse(REAL).unwrap();
        let file = n.url.strip_prefix("nar/").unwrap();
        let download = |body: &[u8]| {
            let (tmp, mut f) = s.nar_temp(file).unwrap();
            f.write_all(body).unwrap();
            tmp
        };

        // A NAR the caller counted on is gone: nothing is written.
        assert_eq!(
            s.publish_pulled(&n, REAL.as_bytes(), None).unwrap(),
            Published::NarMissing
        );
        assert!(!s.has(n.store_hash()));

        let tmp = download(b"first");
        assert_eq!(
            s.publish_pulled(&n, REAL.as_bytes(), Some(&tmp)).unwrap(),
            Published::Stored
        );
        assert!(s.has(n.store_hash()) && !tmp.exists());
        assert_eq!(fs::read(s.nar_path(file).unwrap()).unwrap(), b"first");

        // The path is stored now: a second publish changes nothing and
        // leaves the caller its download.
        let tmp = download(b"second");
        assert_eq!(
            s.publish_pulled(&n, b"other narinfo", Some(&tmp)).unwrap(),
            Published::Lost
        );
        assert!(tmp.exists());
        assert_eq!(s.read_narinfo(n.store_hash()).unwrap().unwrap(), REAL);

        // Without the narinfo, the stored NAR of that name still wins.
        s.remove_narinfo(n.store_hash()).unwrap();
        assert_eq!(
            s.publish_pulled(&n, REAL.as_bytes(), Some(&tmp)).unwrap(),
            Published::NarExists
        );
        assert!(tmp.exists() && !s.has(n.store_hash()));
        assert_eq!(fs::read(s.nar_path(file).unwrap()).unwrap(), b"first");

        // Reusing it makes it young again, so no sweep takes it from under
        // the new narinfo.
        let long_ago = SystemTime::now() - Duration::from_secs(7 * 86400);
        fs::File::options()
            .write(true)
            .open(s.nar_path(file).unwrap())
            .unwrap()
            .set_modified(long_ago)
            .unwrap();
        assert_eq!(
            s.publish_pulled(&n, REAL.as_bytes(), None).unwrap(),
            Published::Stored
        );
        assert!(s.nar_modified(file).unwrap() > long_ago + Duration::from_secs(86400));

        // Only the download the caller still owns is left in a temp file.
        fs::remove_file(tmp).unwrap();
        let temps = |dir: &Path| {
            fs::read_dir(dir)
                .unwrap()
                .filter(|e| {
                    let name = e.as_ref().unwrap().file_name();
                    name.to_string_lossy().starts_with(TEMP_PREFIX)
                })
                .count()
        };
        assert_eq!(temps(&root) + temps(&root.join("nar")), 0);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn put_nar_new_keeps_an_existing_file() {
        let root = tmp();
        let s = Store::open(&root).unwrap();
        assert_eq!(s.put_nar_new("f.nar.zst", &b"first"[..]).unwrap(), 5);
        assert_eq!(s.put_nar_new("f.nar.zst", &b"second"[..]).unwrap(), 0);
        assert_eq!(
            fs::read(s.nar_path("f.nar.zst").unwrap()).unwrap(),
            b"first"
        );
        // No temp file is left either way.
        let names: Vec<_> = fs::read_dir(root.join("nar")).unwrap().collect();
        assert_eq!(names.len(), 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn remove_nar_unless_leaves_nothing_behind() {
        let root = tmp();
        let s = Store::open(&root).unwrap();
        s.put_nar("kept.nar", &b"x"[..]).unwrap();
        s.put_nar("gone.nar", &b"x"[..]).unwrap();
        assert!(!s.remove_nar_unless("kept.nar", |_| true).unwrap());
        assert!(s.remove_nar_unless("gone.nar", |_| false).unwrap());
        assert!(!s.remove_nar_unless("never.nar", |_| false).unwrap());
        let names: Vec<_> = fs::read_dir(root.join("nar"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["kept.nar"]);
        // A manifest removed after it was listed is absent, not an error.
        assert!(
            s.load_manifest(&root.join("roots/x/y.json"))
                .unwrap()
                .is_none()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn temp_files_are_unique_ignored_and_cleaned() {
        let root = tmp();
        let s = Store::open(&root).unwrap();
        let ((a, _), (b, _)) = (s.nar_temp("f.nar").unwrap(), s.nar_temp("f.nar").unwrap());
        assert_ne!(a, b, "two writers of one object share a temp file");

        // Names another process on the volume already took, as a second
        // pid-1 `serve` counting from zero would: they are skipped, never
        // opened.
        let object = s.nar_path("g.nar").unwrap();
        let next = temp_beside(&object).unwrap();
        let name = next.file_name().unwrap().to_string_lossy().into_owned();
        let seq: u64 = name.split('-').nth(2).unwrap().parse().unwrap();
        let taken: Vec<PathBuf> = (seq + 1..seq + 200)
            .map(|n| next.with_file_name(name.replacen(&format!("-{seq}-"), &format!("-{n}-"), 1)))
            .collect();
        for t in &taken {
            fs::write(t, "taken").unwrap();
        }
        let (mine, _) = create_temp(&object).unwrap();
        assert!(!taken.contains(&mine));
        for t in &taken {
            assert_eq!(fs::read(t).unwrap(), b"taken");
            fs::remove_file(t).unwrap();
        }
        fs::remove_file(mine).unwrap();

        // What a killed process leaves behind.
        let n = NarInfo::parse(REAL).unwrap();
        let narinfo = temp_beside(&s.narinfo_path(n.store_hash()).unwrap()).unwrap();
        fs::create_dir_all(root.join("roots/znix")).unwrap();
        let manifest = temp_beside(&s.manifest_path("znix", "main-1-x").unwrap()).unwrap();
        let long_ago = SystemTime::now() - 2 * TEMP_MAX_AGE;
        for tmp in [&a, &narinfo, &manifest] {
            fs::write(tmp, REAL).unwrap();
            let f = fs::File::options().write(true).open(tmp).unwrap();
            f.set_modified(long_ago).unwrap();
        }
        // A young temp file may have a live writer.
        fs::write(&b, REAL).unwrap();
        s.put_nar("kept.nar", &b"x"[..]).unwrap();

        // A temp narinfo is neither indexed nor offered to the sweep.
        let s = Store::open(&root).unwrap();
        assert_eq!(s.len(), 0);
        assert!(s.narinfo_files().unwrap().is_empty());

        assert_eq!(s.clean_temps(), 3);
        assert!(!a.exists() && !narinfo.exists() && !manifest.exists());
        assert!(b.exists() && s.nar_path("kept.nar").unwrap().exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_traversal() {
        let root = tmp();
        let s = Store::open(&root).unwrap();
        assert!(s.nar_path("../evil").is_err());
        assert!(s.narinfo_path("a/b").is_err());
        assert!(s.manifest_path("flake", "../../etc").is_err());
        assert!(s.nar_path(".hidden").is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn compression_of_reads_nix_file_names() {
        for (file, compression) in [
            ("1abc.nar.xz", "xz"),
            ("1abc.nar.zst", "zstd"),
            ("1abc.nar.bz2", "bzip2"),
            ("1abc.nar", "none"),
            ("1abc.nar.br", "other"),
            ("deadbeef", "other"),
        ] {
            assert_eq!(compression_of(file), compression, "{file}");
        }
    }

    #[test]
    fn manifest_gaps_and_markers() {
        let root = tmp();
        let s = Store::open(&root).unwrap();
        let m = Manifest {
            version: 3,
            flake: "znix".into(),
            gen_id: "main-abc-x".into(),
            branch: "main".into(),
            attr: "x".into(),
            timestamp: "2026-08-20T10:00:00Z".into(),
            closure: vec![
                "/nix/store/v4f0jj9sz97ckskvacf40llz4nfr19jf-hello-2.12.3".into(),
                "/nix/store/jspv3c5l2zx4kiwzhq0zgxcwp34cqifz-libiconv-115.100.1".into(),
            ],
        };
        s.put_manifest(&m, &serde_json::to_vec(&m).unwrap())
            .unwrap();
        // A leftover temp file is not a manifest.
        fs::write(root.join("roots/znix/.tmp-1-main-abc-x.json"), b"{").unwrap();
        let files = s.manifest_files().unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(
            (files[0].0.as_str(), files[0].1.as_str()),
            ("znix", "main-abc-x")
        );
        assert_eq!(
            s.load_manifest(&files[0].2).unwrap().unwrap().closure,
            m.closure
        );
        assert_eq!(s.gaps(&m).len(), 2);
        let n = NarInfo::parse(REAL).unwrap();
        s.put_narinfo(&n, REAL.as_bytes()).unwrap();
        assert_eq!(
            s.gaps(&m),
            vec!["/nix/store/jspv3c5l2zx4kiwzhq0zgxcwp34cqifz-libiconv-115.100.1".to_string()]
        );

        s.mark_local_origin("znix", "main-abc-x").unwrap();
        assert!(s.is_local_origin("znix", "main-abc-x"));
        assert!(!s.is_mirrored("znix", "main-abc-x"));
        s.mark_mirrored("znix", "main-abc-x").unwrap();
        assert!(s.is_mirrored("znix", "main-abc-x"));
        s.remove_manifest("znix", "main-abc-x").unwrap();
        assert!(!s.is_local_origin("znix", "main-abc-x"));
        assert_eq!(s.manifest_files().unwrap().len(), 0);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn prune_index_surfaces_a_foreign_delete_as_a_gap() {
        let root = tmp();
        let server = Store::open(&root).unwrap();
        let n = NarInfo::parse(REAL).unwrap();
        server.put_narinfo(&n, REAL.as_bytes()).unwrap();
        let m = Manifest {
            version: 3,
            flake: "znix".into(),
            gen_id: "main-abc-x".into(),
            branch: "main".into(),
            attr: "x".into(),
            timestamp: "2026-08-20T10:00:00Z".into(),
            closure: vec!["/nix/store/v4f0jj9sz97ckskvacf40llz4nfr19jf-hello-2.12.3".into()],
        };
        // A second handle on the same root, as `kasha sweep` beside `serve`.
        Store::open(&root)
            .unwrap()
            .remove_narinfo("v4f0jj9sz97ckskvacf40llz4nfr19jf")
            .unwrap();
        assert!(server.gaps(&m).is_empty());
        assert_eq!(server.prune_index(), 1);
        assert_eq!(server.gaps(&m).len(), 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn sweep_stamp_round_trips() {
        let dir = tmp();
        let store = Store::open(&dir).unwrap();
        assert!(store.last_sweep().is_none());
        store.record_sweep().unwrap();
        let t = store.last_sweep().expect("stamp recorded");
        assert!(t.elapsed().unwrap() < Duration::from_secs(60));
        // Survives a reopen: the schedule must not restart with the process.
        let reopened = Store::open(&dir).unwrap();
        assert_eq!(reopened.last_sweep().unwrap(), t);
        let _ = fs::remove_dir_all(dir);
    }
}
