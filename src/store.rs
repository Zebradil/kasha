//! Flat binary-cache store on disk, same layout as the R2 bucket:
//! `<storehash>.narinfo`, `nar/<filehash>.nar.*`, `roots/<flake>/<gen>.json`,
//! plus `state/` bookkeeping. The filesystem is the database; the in-memory
//! index is rebuilt by scanning narinfos at boot.

use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use crate::manifest::Manifest;
use crate::narinfo::NarInfo;

pub struct Store {
    pub root: PathBuf,
    /// storehash -> nar URL (relative key like `nar/<filehash>.nar.xz`).
    index: RwLock<HashMap<String, String>>,
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
        let store = Store { root, index: RwLock::new(HashMap::new()) };
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
            let Some(name) = name.to_str() else { continue };
            let Some(hash) = name.strip_suffix(".narinfo") else { continue };
            let text = fs::read_to_string(entry.path())?;
            match NarInfo::parse(&text) {
                Ok(n) => {
                    idx.insert(hash.to_string(), n.url);
                }
                Err(e) => tracing::warn!(file = name, error = %e, "skipping unparseable narinfo"),
            }
        }
        *self.index.write().unwrap() = idx;
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.index.read().unwrap().len()
    }

    pub fn has(&self, store_hash: &str) -> bool {
        self.index.read().unwrap().contains_key(store_hash)
    }

    pub fn narinfo_path(&self, store_hash: &str) -> Result<PathBuf> {
        Ok(self.root.join(format!("{}.narinfo", safe_component(store_hash)?)))
    }

    pub fn nar_path(&self, file: &str) -> Result<PathBuf> {
        Ok(self.root.join("nar").join(safe_component(file)?))
    }

    /// Store a verified narinfo byte-identical and index it. Caller verifies
    /// the signature first.
    pub fn put_narinfo(&self, info: &NarInfo, raw: &[u8]) -> Result<()> {
        let path = self.narinfo_path(info.store_hash())?;
        write_atomic(&path, |f| f.write_all(raw))?;
        self.index
            .write()
            .unwrap()
            .insert(info.store_hash().to_string(), info.url.clone());
        Ok(())
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

    /// All parseable v3 manifests under `roots/`, with their file paths.
    pub fn manifests(&self) -> Result<Vec<(PathBuf, Manifest)>> {
        let mut out = Vec::new();
        let roots = self.root.join("roots");
        for flake in fs::read_dir(&roots)? {
            let flake = flake?;
            if !flake.file_type()?.is_dir() {
                continue;
            }
            for f in fs::read_dir(flake.path())? {
                let path = f?.path();
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                match Manifest::parse(&fs::read(&path)?) {
                    Ok(m) => out.push((path, m)),
                    Err(e) => {
                        tracing::warn!(path = %path.display(), error = %e, "skipping bad manifest")
                    }
                }
            }
        }
        Ok(out)
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

    fn marker(&self, kind: &str, flake: &str, gen_id: &str) -> Result<PathBuf> {
        Ok(self
            .root
            .join("state")
            .join(kind)
            .join(format!("{}--{}", safe_component(flake)?, safe_component(gen_id)?)))
    }

    /// A gen whose manifest arrived via the authenticated push API.
    pub fn mark_local_origin(&self, flake: &str, gen_id: &str) -> Result<()> {
        fs::write(self.marker("local-origin", flake, gen_id)?, b"")?;
        Ok(())
    }

    pub fn is_local_origin(&self, flake: &str, gen_id: &str) -> bool {
        self.marker("local-origin", flake, gen_id).map(|p| p.exists()).unwrap_or(false)
    }

    /// Confirmed fully present in the remote cache (manifest included).
    pub fn mark_mirrored(&self, flake: &str, gen_id: &str) -> Result<()> {
        fs::write(self.marker("mirrored", flake, gen_id)?, b"")?;
        Ok(())
    }

    pub fn is_mirrored(&self, flake: &str, gen_id: &str) -> bool {
        self.marker("mirrored", flake, gen_id).map(|p| p.exists()).unwrap_or(false)
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
        let p = self.narinfo_path(store_hash)?;
        if p.exists() {
            fs::remove_file(p)?;
        }
        self.index.write().unwrap().remove(store_hash);
        Ok(())
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

/// Write via temp file + rename so readers never see partial objects.
fn write_atomic(path: &Path, fill: impl FnOnce(&mut fs::File) -> std::io::Result<()>) -> Result<()> {
    let dir = path.parent().context("no parent dir")?;
    let tmp = dir.join(format!(
        ".tmp-{}-{}",
        std::process::id(),
        path.file_name().unwrap().to_string_lossy()
    ));
    let mut f = fs::File::create(&tmp)?;
    let res = fill(&mut f).and_then(|()| f.sync_all());
    drop(f);
    match res {
        Ok(()) => {
            fs::rename(&tmp, path)?;
            Ok(())
        }
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e).with_context(|| format!("writing {}", path.display()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos(),
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
        assert_eq!(s.read_narinfo("v4f0jj9sz97ckskvacf40llz4nfr19jf").unwrap().unwrap(), REAL);
        s.remove_narinfo("v4f0jj9sz97ckskvacf40llz4nfr19jf").unwrap();
        assert!(!s.has("v4f0jj9sz97ckskvacf40llz4nfr19jf"));
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
        s.put_manifest(&m, &serde_json::to_vec(&m).unwrap()).unwrap();
        assert_eq!(s.manifests().unwrap().len(), 1);
        assert_eq!(s.gaps(&m).len(), 2);
        let n = NarInfo::parse(REAL).unwrap();
        s.put_narinfo(&n, REAL.as_bytes()).unwrap();
        assert_eq!(s.gaps(&m), vec![
            "/nix/store/jspv3c5l2zx4kiwzhq0zgxcwp34cqifz-libiconv-115.100.1".to_string()
        ]);

        s.mark_local_origin("znix", "main-abc-x").unwrap();
        assert!(s.is_local_origin("znix", "main-abc-x"));
        assert!(!s.is_mirrored("znix", "main-abc-x"));
        s.mark_mirrored("znix", "main-abc-x").unwrap();
        assert!(s.is_mirrored("znix", "main-abc-x"));
        s.remove_manifest("znix", "main-abc-x").unwrap();
        assert!(!s.is_local_origin("znix", "main-abc-x"));
        assert_eq!(s.manifests().unwrap().len(), 0);
        fs::remove_dir_all(root).unwrap();
    }
}
