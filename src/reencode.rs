//! Re-encoding stored NARs as zstd, after the fact (ADR-0015).
//!
//! A path whose narinfo names an `xz`, `bzip2` or uncompressed NAR file has
//! it decoded, checked against `NarHash` and `NarSize`, and stored again as
//! zstd under `nar/<nix32 NarHash>.nar.zst`, the name pull-through gives it
//! (ADR-0012), so the two share files. The narinfo is then replaced by the
//! box's rendering of it naming the new file, but only while the stored one
//! is still the one read. The old file is left to orphan removal.
//!
//! Two callers: a pass on `serve`'s maintenance thread, `BUDGET` bytes of
//! NARs at a time, and mirror-up, before it uploads a path of a local-origin
//! generation.

use anyhow::{Context, Result, anyhow, bail};
use std::collections::HashSet;
use std::fs::{self, File};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use crate::gc::GRACE;
use crate::narinfo::{NarInfo, nix32_encode};
use crate::pull::{self, FileMeta, TempFile};
use crate::store::{Published, Store, compression_of};

/// `NarSize` bytes one pass decodes, bar the path that crosses it: about
/// 9 s of one core at the rates ADR-0012 measured, per 5-minute pass.
pub const BUDGET: u64 = 1 << 30;

/// An unlisted path is re-encoded only when used this recently, and no file
/// the re-encoder writes is dated earlier: half the grace window by which a
/// sweep and orphan removal judge files, so neither takes one of them.
const USED_WITHIN: Duration = Duration::from_secs(GRACE.as_secs() / 2);

/// Monotonic counters surfaced at /metrics.
#[derive(Default)]
pub struct Stats {
    pub reencoded: AtomicU64,
    /// Paths whose NAR failed its check or cannot be decoded.
    pub failed: AtomicU64,
    /// Paths whose narinfo changed while their NAR was re-encoded.
    pub lost: AtomicU64,
    /// Store errors; the path is tried again.
    pub errors: AtomicU64,
    /// Bytes of the files re-encoded, and of the zstd files naming them.
    pub bytes_old: AtomicU64,
    pub bytes_new: AtomicU64,
}

#[derive(Default)]
pub struct Reencoder {
    /// Store hashes not tried again until restart: their NAR failed, or
    /// their narinfo says zstd already.
    done: Mutex<HashSet<String>>,
    pub stats: Stats,
}

enum Outcome {
    Swapped {
        size: u64,
    },
    Lost,
    /// The NAR, or a stored file of the zstd name, is not the narinfo's.
    Bad(anyhow::Error),
}

impl Reencoder {
    /// Re-encode paths whose narinfo names a file not named as zstd, until
    /// `budget` bytes of NARs are decoded.
    pub fn pass(&self, store: &Store, budget: u64) -> Result<()> {
        let candidates = store.hashes_by_url(|url| compression_of(url) != pull::STORED);
        let candidates: Vec<String> = {
            let done = self.done.lock().unwrap();
            candidates
                .into_iter()
                .filter(|h| !done.contains(h))
                .collect()
        };
        if candidates.is_empty() {
            return Ok(());
        }
        let listed = crate::gc::marked(store, SystemTime::now())?
            .map(|(_, mark)| mark)
            .unwrap_or_default();
        let mut decoded = 0;
        for hash in candidates {
            if decoded >= budget {
                break;
            }
            decoded += self.path(store, &hash, listed.contains(&hash));
        }
        if decoded > 0 {
            tracing::info!(decoded, "re-encode pass done");
        }
        Ok(())
    }

    /// Re-encode one path's NAR as zstd; `listed` when a retained manifest
    /// lists the path. Returns the `NarSize` decoded. What goes wrong is
    /// logged and counted, and leaves the path as it was.
    pub fn path(&self, store: &Store, hash: &str, listed: bool) -> u64 {
        self.path_with(store, hash, listed, || ())
    }

    /// `path`, with `encoded` called once the zstd file is ready and before
    /// the narinfo is swapped: the point where a test lets a writer in.
    fn path_with(&self, store: &Store, hash: &str, listed: bool, encoded: impl FnOnce()) -> u64 {
        if self.done.lock().unwrap().contains(hash) {
            return 0;
        }
        let raw = match store.read_narinfo(hash) {
            Ok(Some(raw)) => raw,
            Ok(None) => return 0,
            Err(e) => {
                self.error(hash, e);
                return 0;
            }
        };
        // Ingest and the boot scan already refuse an unparseable narinfo.
        let Ok(info) = NarInfo::parse(&raw) else {
            return 0;
        };
        if info.compression == pull::STORED {
            self.done.lock().unwrap().insert(hash.to_string());
            return 0;
        }
        let (Some(nar_hash), true, Some(old)) = (
            info.nar_sha256(),
            pull::decodable(&info.compression),
            info.url.strip_prefix("nar/"),
        ) else {
            let e = anyhow!(
                "cannot verify Compression {}, NarHash {} or URL {}",
                info.compression,
                info.nar_hash,
                info.url
            );
            self.fail(hash, e);
            return 0;
        };
        // No NAR yet: a push may send its narinfo first.
        let Some((used, old_size)) = last_use(store, hash, old) else {
            return 0;
        };
        // A cold unlisted path is the next to be evicted or swept: re-encoding
        // it costs CPU, and disk until orphan removal, for a path nobody reads.
        if !listed && used.elapsed().is_ok_and(|age| age >= USED_WITHIN) {
            return 0;
        }
        match reencode(store, &raw, &info, &nar_hash, used, encoded) {
            Ok(Outcome::Swapped { size }) => {
                let s = &self.stats;
                s.reencoded.fetch_add(1, Ordering::Relaxed);
                s.bytes_old.fetch_add(old_size, Ordering::Relaxed);
                s.bytes_new.fetch_add(size, Ordering::Relaxed);
                tracing::info!(
                    hash,
                    compression = info.compression,
                    old_size,
                    size,
                    "re-encoded as zstd"
                );
            }
            Ok(Outcome::Lost) => {
                self.stats.lost.fetch_add(1, Ordering::Relaxed);
                tracing::info!(hash, "narinfo changed while re-encoding, left as it is");
            }
            Ok(Outcome::Bad(e)) => self.fail(hash, e),
            Err(e) => self.error(hash, e),
        }
        info.nar_size
    }

    fn fail(&self, hash: &str, e: anyhow::Error) {
        self.done.lock().unwrap().insert(hash.to_string());
        self.stats.failed.fetch_add(1, Ordering::Relaxed);
        tracing::warn!(
            hash,
            error = format!("{e:#}"),
            "cannot re-encode, not tried again until restart"
        );
    }

    fn error(&self, hash: &str, e: anyhow::Error) {
        self.stats.errors.fetch_add(1, Ordering::Relaxed);
        tracing::warn!(hash, error = format!("{e:#}"), "re-encoding failed");
    }
}

/// The newer of a path's narinfo and NAR mtimes (ADR-0013), and the NAR's
/// size; None while the NAR is not stored.
fn last_use(store: &Store, hash: &str, nar: &str) -> Option<(SystemTime, u64)> {
    let nar = fs::metadata(store.nar_path(nar).ok()?).ok()?;
    let narinfo = fs::metadata(store.narinfo_path(hash).ok()?)
        .and_then(|m| m.modified())
        .ok()?;
    Some((nar.modified().ok()?.max(narinfo), nar.len()))
}

/// Store the NAR `info` names as zstd, then swap in the narinfo `raw`
/// rendered for it, while the stored one still reads `raw`.
fn reencode(
    store: &Store,
    raw: &str,
    info: &NarInfo,
    nar_hash: &[u8; 32],
    used: SystemTime,
    encoded: impl FnOnce(),
) -> Result<Outcome> {
    let old = store.nar_path(info.url.strip_prefix("nar/").context("nar URL")?)?;
    let file = format!("{}.nar.zst", nix32_encode(nar_hash));
    let dest = store.nar_path(&file)?;
    let check = |out: Option<&mut File>| -> Result<FileMeta> {
        let body = File::open(&old)?;
        Ok(pull::verify(body, out, &info.compression, nar_hash, info.nar_size)?.0)
    };
    let stored = || -> Result<FileMeta> {
        let body = File::open(&dest)?;
        let (meta, _) = pull::verify(body, None, pull::STORED, nar_hash, info.nar_size)
            .with_context(|| format!("stored NAR {file} is not this path's NAR"))?;
        Ok(meta)
    };
    // As in pull-through, a file of that name is reused once decoding shows
    // it holds this NAR; zstd output need not be deterministic.
    let (mut tmp, mut meta) = if dest.is_file() {
        match stored() {
            Ok(meta) => (None, meta),
            Err(e) => return Ok(Outcome::Bad(e)),
        }
    } else {
        let (path, mut f) = store.nar_temp(&file)?;
        let tmp = TempFile(path);
        match check(Some(&mut f)) {
            Ok(meta) => {
                f.sync_all()?;
                (Some(tmp), meta)
            }
            // A check that writes nothing tells a bad NAR from a failed
            // write, and a NAR removed meanwhile is no verdict on it.
            Err(e) => {
                return match check(None) {
                    Err(bad) if old.is_file() => Ok(Outcome::Bad(bad)),
                    _ => Err(e),
                };
            }
        }
    };
    encoded();
    loop {
        let new = pull::render(raw, &file, Some(&meta));
        let new_info = NarInfo::parse(&new)?;
        let nar_tmp = tmp.as_ref().map(|t| t.0.as_path());
        let used = used.max(SystemTime::now() - USED_WITHIN);
        match store.publish_reencoded(&new_info, new.as_bytes(), raw.as_bytes(), nar_tmp, used)? {
            Published::Stored => return Ok(Outcome::Swapped { size: meta.size }),
            Published::Lost => return Ok(Outcome::Lost),
            Published::NarExists => {
                tmp = None;
                meta = match stored() {
                    Ok(meta) => meta,
                    Err(e) => return Ok(Outcome::Bad(e)),
                };
            }
            Published::NarMissing => bail!("stored NAR {file} vanished"),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::manifest::Manifest;
    use crate::narinfo::PubKey;
    use data_encoding::BASE64;
    use ed25519_dalek::{Signer, SigningKey};
    use sha2::{Digest, Sha256};
    use std::io::Write;
    use std::path::PathBuf;
    use std::time::UNIX_EPOCH;

    const HOUR: Duration = Duration::from_secs(3600);

    fn key() -> SigningKey {
        SigningKey::from_bytes(&[13u8; 32])
    }

    fn keys() -> Vec<PubKey> {
        let pk = BASE64.encode(key().verifying_key().as_bytes());
        vec![PubKey::parse(&format!("test-1:{pk}")).unwrap()]
    }

    pub(crate) fn tmp() -> PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "kasha-reencode-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn encode(compression: &str, nar: &[u8]) -> Vec<u8> {
        match compression {
            "none" => nar.to_vec(),
            "xz" => {
                let mut e = liblzma::write::XzEncoder::new(Vec::new(), 1);
                e.write_all(nar).unwrap();
                e.finish().unwrap()
            }
            "bzip2" => {
                let mut e = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::fast());
                e.write_all(nar).unwrap();
                e.finish().unwrap()
            }
            other => panic!("no encoder for {other}"),
        }
    }

    /// A path as a push or mirror-down stores it.
    pub(crate) struct Stored {
        pub hash: String,
        pub narinfo: String,
        /// The NAR file as stored, named as Nix names it.
        pub file: String,
        pub nar: Vec<u8>,
        /// The name of its zstd file.
        pub zst: String,
    }

    pub(crate) fn age(path: &std::path::Path, t: SystemTime) {
        File::open(path).unwrap().set_modified(t).unwrap();
    }

    fn mtime(path: &std::path::Path) -> SystemTime {
        fs::metadata(path).unwrap().modified().unwrap()
    }

    /// Store the path `<c*32>-pkg-<c>`, signed, with its NAR as
    /// `compression`, NAR first; both files last modified at `t`.
    pub(crate) fn put(store: &Store, c: char, compression: &str, t: SystemTime) -> Stored {
        let nar = format!("nar of {c} ").repeat(500).into_bytes();
        let body = encode(compression, &nar);
        let ext = match compression {
            "xz" => ".xz",
            "bzip2" => ".bz2",
            _ => "",
        };
        let file = format!("{}.nar{ext}", nix32_encode(&Sha256::digest(&body)));
        let hash: String = std::iter::repeat_n(c, 32).collect();
        let nar_hash = nix32_encode(&Sha256::digest(&nar));
        let text = format!(
            "StorePath: /nix/store/{hash}-pkg-{c}\n\
URL: nar/{file}\n\
Compression: {compression}\n\
FileHash: sha256:{}\n\
FileSize: {}\n\
NarHash: sha256:{nar_hash}\n\
NarSize: {}\n\
References: \n\
Deriver: {hash}-pkg-{c}.drv\n",
            nix32_encode(&Sha256::digest(&body)),
            body.len(),
            nar.len(),
        );
        let sig = key().sign(NarInfo::parse(&text).unwrap().fingerprint().as_bytes());
        let narinfo = format!("{text}Sig: test-1:{}\n", BASE64.encode(&sig.to_bytes()));
        store.put_nar(&file, &body[..]).unwrap();
        store
            .put_narinfo(&NarInfo::parse(&narinfo).unwrap(), narinfo.as_bytes())
            .unwrap();
        age(&store.nar_path(&file).unwrap(), t);
        age(&store.narinfo_path(&hash).unwrap(), t);
        Stored {
            hash,
            narinfo,
            file,
            nar,
            zst: format!("{nar_hash}.nar.zst"),
        }
    }

    /// Replace the stored NAR file of `p` with other bytes, keeping its time.
    fn corrupt(store: &Store, p: &Stored, compression: &str) {
        let path = store.nar_path(&p.file).unwrap();
        let t = mtime(&path);
        store
            .put_nar(&p.file, &encode(compression, b"another NAR")[..])
            .unwrap();
        age(&path, t);
    }

    fn list(store: &Store, manifest_gen: &str, paths: &[&Stored]) {
        let closure: Vec<String> = paths
            .iter()
            .map(|p| NarInfo::parse(&p.narinfo).unwrap().store_path)
            .collect();
        let raw = serde_json::json!({
            "version": 3, "flake": "znix", "gen": manifest_gen, "branch": "main",
            "attr": "x", "timestamp": humantime::format_rfc3339_seconds(SystemTime::now()).to_string(),
            "closure": closure,
        })
        .to_string();
        store
            .put_manifest(&Manifest::parse(raw.as_bytes()).unwrap(), raw.as_bytes())
            .unwrap();
    }

    fn field<'a>(narinfo: &'a str, key: &str) -> Option<&'a str> {
        narinfo
            .lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix(": "))
    }

    fn lines<'a>(narinfo: &'a str, key: &str) -> Vec<&'a str> {
        narinfo
            .lines()
            .filter(|l| l.split_once(": ").is_some_and(|(k, _)| k == key))
            .collect()
    }

    fn nar_dir(store: &Store) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(store.root.join("nar"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// Narinfos on disk whose NAR is not.
    pub(crate) fn dangling(store: &Store) -> Vec<String> {
        store
            .narinfo_files()
            .unwrap()
            .into_iter()
            .map(|(hash, _, _)| hash)
            .filter(|hash| {
                let url = NarInfo::parse(&store.read_narinfo(hash).unwrap().unwrap())
                    .unwrap()
                    .url;
                !store
                    .nar_path(url.strip_prefix("nar/").unwrap())
                    .unwrap()
                    .exists()
            })
            .collect()
    }

    fn n(c: &AtomicU64) -> u64 {
        c.load(Ordering::Relaxed)
    }

    #[test]
    fn every_other_encoding_is_stored_as_zstd_and_the_signatures_still_verify() {
        let store = Store::open(tmp()).unwrap();
        let r = Reencoder::default();
        let mut old_bytes = 0;
        let mut new_bytes = 0;
        for (c, compression) in ['a', 'b', 'c'].into_iter().zip(["xz", "bzip2", "none"]) {
            let p = put(&store, c, compression, SystemTime::now() - HOUR);
            old_bytes += fs::metadata(store.nar_path(&p.file).unwrap())
                .unwrap()
                .len();
            assert_eq!(r.path(&store, &p.hash, false), p.nar.len() as u64);

            let stored = store.read_narinfo(&p.hash).unwrap().unwrap();
            assert_eq!(field(&stored, "URL"), Some(&*format!("nar/{}", p.zst)));
            assert_eq!(field(&stored, "Compression"), Some("zstd"));
            for kept in [
                "StorePath",
                "NarHash",
                "NarSize",
                "References",
                "Deriver",
                "Sig",
            ] {
                assert_eq!(lines(&stored, kept), lines(&p.narinfo, kept), "{kept}");
            }
            assert!(NarInfo::parse(&stored).unwrap().verify(&keys()));
            let file = fs::read(store.nar_path(&p.zst).unwrap()).unwrap();
            new_bytes += file.len() as u64;
            assert_eq!(zstd::decode_all(&file[..]).unwrap(), p.nar);
            let file_hash = format!("sha256:{}", nix32_encode(&Sha256::digest(&file)));
            assert_eq!(field(&stored, "FileHash"), Some(&*file_hash));
            assert_eq!(field(&stored, "FileSize"), Some(&*file.len().to_string()));
            assert_eq!(store.url_of(&p.hash), Some(format!("nar/{}", p.zst)));
            // A client holding the old narinfo still gets its file.
            assert!(store.nar_path(&p.file).unwrap().exists(), "{compression}");
            // Done: nothing is decoded again.
            assert_eq!(r.path(&store, &p.hash, false), 0);
        }
        assert_eq!(n(&r.stats.reencoded), 3);
        assert_eq!(n(&r.stats.bytes_old), old_bytes);
        assert_eq!(n(&r.stats.bytes_new), new_bytes);
        assert_eq!(
            n(&r.stats.failed) + n(&r.stats.lost) + n(&r.stats.errors),
            0
        );
        let _ = fs::remove_dir_all(&store.root);
    }

    #[test]
    fn a_narinfo_replaced_meanwhile_is_not_reverted() {
        let store = Store::open(tmp()).unwrap();
        let r = Reencoder::default();
        let p = put(&store, 'a', "xz", SystemTime::now() - HOUR);
        // A push of the same path, outside the signed fields.
        let pushed = p
            .narinfo
            .replace("Deriver: ", "System: x86_64-linux\nDeriver: ");
        r.path_with(&store, &p.hash, false, || {
            let info = NarInfo::parse(&pushed).unwrap();
            store.put_narinfo(&info, pushed.as_bytes()).unwrap();
        });
        assert_eq!(store.read_narinfo(&p.hash).unwrap().unwrap(), pushed);
        assert_eq!(store.url_of(&p.hash), Some(format!("nar/{}", p.file)));
        assert_eq!(n(&r.stats.lost), 1);
        // Nothing stored, no temp file left.
        assert_eq!(nar_dir(&store), [p.file.as_str()]);

        // Not remembered: the next try re-encodes the narinfo it reads then.
        r.path(&store, &p.hash, false);
        let stored = store.read_narinfo(&p.hash).unwrap().unwrap();
        assert_eq!(field(&stored, "Compression"), Some("zstd"));
        assert_eq!(field(&stored, "System"), Some("x86_64-linux"));
        let _ = fs::remove_dir_all(&store.root);
    }

    #[test]
    fn a_stored_zstd_file_of_the_same_nar_is_reused() {
        let store = Store::open(tmp()).unwrap();
        let r = Reencoder::default();
        let now = SystemTime::now();
        let p = put(&store, 'a', "xz", now - HOUR);
        // A level the box would not have chosen, and past grace: an orphan
        // of a pulled-through path evicted since.
        let theirs = zstd::encode_all(&p.nar[..], 19).unwrap();
        store.put_nar(&p.zst, &theirs[..]).unwrap();
        age(&store.nar_path(&p.zst).unwrap(), now - 2 * GRACE);

        r.path(&store, &p.hash, false);
        let stored = store.read_narinfo(&p.hash).unwrap().unwrap();
        assert_eq!(field(&stored, "URL"), Some(&*format!("nar/{}", p.zst)));
        assert_eq!(fs::read(store.nar_path(&p.zst).unwrap()).unwrap(), theirs);
        let file_hash = format!("sha256:{}", nix32_encode(&Sha256::digest(&theirs)));
        assert_eq!(field(&stored, "FileHash"), Some(&*file_hash));
        // Touched, as by every writer: never older than it was.
        assert!(mtime(&store.nar_path(&p.zst).unwrap()) > now - HOUR);
        assert_eq!(n(&r.stats.reencoded), 1);

        // A file of that name holding something else is left alone, and so
        // is the path.
        let q = put(&store, 'b', "xz", now - HOUR);
        store.put_nar(&q.zst, &b"not this NAR"[..]).unwrap();
        assert_eq!(r.path(&store, &q.hash, false), q.nar.len() as u64);
        assert_eq!(store.read_narinfo(&q.hash).unwrap().unwrap(), q.narinfo);
        assert_eq!(
            fs::read(store.nar_path(&q.zst).unwrap()).unwrap(),
            b"not this NAR"
        );
        assert_eq!(n(&r.stats.failed), 1);
        let _ = fs::remove_dir_all(&store.root);
    }

    #[test]
    fn a_nar_that_fails_its_check_is_left_alone_and_not_retried() {
        let store = Store::open(tmp()).unwrap();
        let r = Reencoder::default();
        let now = SystemTime::now();
        // Another NAR, and bytes that are no xz at all.
        let p = put(&store, 'a', "xz", now - HOUR);
        corrupt(&store, &p, "xz");
        let q = put(&store, 'b', "xz", now - HOUR);
        corrupt(&store, &q, "none");
        for s in [&p, &q] {
            assert_eq!(r.path(&store, &s.hash, false), s.nar.len() as u64);
            assert_eq!(store.read_narinfo(&s.hash).unwrap().unwrap(), s.narinfo);
            assert_eq!(r.path(&store, &s.hash, false), 0, "tried again");
        }
        assert_eq!(nar_dir(&store), {
            let mut v = [p.file.clone(), q.file.clone()];
            v.sort();
            v
        });
        // An encoding the box cannot decode costs nothing to find out.
        let b = put(&store, 'c', "xz", now - HOUR);
        let br = b.narinfo.replace("Compression: xz", "Compression: br");
        store
            .put_narinfo(&NarInfo::parse(&br).unwrap(), br.as_bytes())
            .unwrap();
        assert_eq!(r.path(&store, &b.hash, false), 0);
        assert_eq!(store.read_narinfo(&b.hash).unwrap().unwrap(), br);
        assert_eq!(n(&r.stats.failed), 3);
        assert_eq!(n(&r.stats.reencoded), 0);

        // A pass skips all three.
        r.pass(&store, BUDGET).unwrap();
        assert_eq!(n(&r.stats.failed), 3);
        let _ = fs::remove_dir_all(&store.root);
    }

    #[test]
    fn a_cold_unlisted_path_is_skipped_and_a_listed_one_is_not() {
        let store = Store::open(tmp()).unwrap();
        let r = Reencoder::default();
        let now = SystemTime::now();
        let cold = now - USED_WITHIN - HOUR;
        let listed = put(&store, 'a', "xz", cold);
        let unlisted = put(&store, 'b', "xz", cold);
        let warm = put(&store, 'c', "xz", now - HOUR);
        list(&store, "main-1-x", &[&listed]);

        r.pass(&store, BUDGET).unwrap();
        let compression = |p: &Stored| {
            let stored = store.read_narinfo(&p.hash).unwrap().unwrap();
            field(&stored, "Compression").unwrap().to_string()
        };
        assert_eq!(compression(&listed), "zstd");
        assert_eq!(compression(&unlisted), "xz");
        assert_eq!(compression(&warm), "zstd");
        assert!(!store.nar_path(&unlisted.zst).unwrap().exists());

        // A read of its NAR warms it up.
        age(&store.nar_path(&unlisted.file).unwrap(), now);
        r.pass(&store, BUDGET).unwrap();
        assert_eq!(compression(&unlisted), "zstd");
        let _ = fs::remove_dir_all(&store.root);
    }

    #[test]
    fn the_new_files_carry_the_last_use_over() {
        let store = Store::open(tmp()).unwrap();
        let r = Reencoder::default();
        let secs = |t: SystemTime| {
            UNIX_EPOCH + Duration::from_secs(t.duration_since(UNIX_EPOCH).unwrap().as_secs())
        };
        let now = SystemTime::now();
        // Its NAR read three hours ago, the narinfo an hour earlier.
        let used = secs(now - 3 * HOUR);
        let p = put(&store, 'a', "xz", used - HOUR);
        age(&store.nar_path(&p.file).unwrap(), used);
        r.path(&store, &p.hash, false);
        assert_eq!(mtime(&store.narinfo_path(&p.hash).unwrap()), used);
        assert_eq!(mtime(&store.nar_path(&p.zst).unwrap()), used);

        // A listed path used long ago gets no time by which a sweep or
        // orphan removal would take a file before its narinfo is in place.
        let q = put(&store, 'b', "xz", now - 30 * 24 * HOUR);
        r.path(&store, &q.hash, true);
        for f in [
            store.narinfo_path(&q.hash).unwrap(),
            store.nar_path(&q.zst).unwrap(),
        ] {
            let t = mtime(&f);
            assert!(
                t >= now - USED_WITHIN && t < now - USED_WITHIN + HOUR,
                "{f:?}"
            );
        }
        let _ = fs::remove_dir_all(&store.root);
    }

    #[test]
    fn a_pass_stops_once_its_budget_is_spent() {
        let store = Store::open(tmp()).unwrap();
        let r = Reencoder::default();
        for c in ['a', 'b', 'c'] {
            put(&store, c, "xz", SystemTime::now() - HOUR);
        }
        // The path that crosses the budget is done whole.
        r.pass(&store, 1).unwrap();
        assert_eq!(n(&r.stats.reencoded), 1);
        r.pass(&store, 1).unwrap();
        assert_eq!(n(&r.stats.reencoded), 2);
        r.pass(&store, BUDGET).unwrap();
        r.pass(&store, BUDGET).unwrap();
        assert_eq!(n(&r.stats.reencoded), 3);
        let _ = fs::remove_dir_all(&store.root);
    }

    #[test]
    fn a_pass_cutting_in_before_the_swap_leaves_no_narinfo_without_its_nar() {
        let store = Store::open(tmp()).unwrap();
        let r = Reencoder::default();
        let now = SystemTime::now();
        // Eviction takes the path while its NAR is re-encoded.
        let p = put(&store, 'a', "xz", now - 2 * HOUR);
        r.path_with(&store, &p.hash, false, || {
            let lru = crate::gc::Lru::new(1);
            let evicted = crate::gc::evict(&store, SystemTime::now(), &lru, &|_| false).unwrap();
            assert_eq!(evicted.evicted.len(), 2, "{:?}", evicted.evicted);
        });
        assert_eq!(n(&r.stats.lost), 1);
        assert!(store.narinfo_files().unwrap().is_empty());
        assert!(nar_dir(&store).is_empty());

        // A sweep removes the zstd file the re-encoder checked for reuse.
        let q = put(&store, 'b', "xz", now - 30 * 24 * HOUR);
        list(&store, "main-1-x", &[&q]);
        let theirs = zstd::encode_all(&q.nar[..], 19).unwrap();
        store.put_nar(&q.zst, &theirs[..]).unwrap();
        age(&store.nar_path(&q.zst).unwrap(), now - 2 * GRACE);
        r.path_with(&store, &q.hash, true, || {
            let swept = crate::gc::box_sweep(&store, SystemTime::now(), GRACE, false).unwrap();
            assert_eq!(swept.deleted, [format!("nar/{}", q.zst)]);
        });
        assert_eq!(n(&r.stats.errors), 1);
        assert_eq!(store.read_narinfo(&q.hash).unwrap().unwrap(), q.narinfo);
        assert!(dangling(&store).is_empty());
        // The next try stores a file of its own.
        r.path(&store, &q.hash, true);
        assert_eq!(n(&r.stats.reencoded), 1);
        assert!(dangling(&store).is_empty());
        let _ = fs::remove_dir_all(&store.root);
    }

    #[test]
    fn mirror_up_uploads_a_pushed_xz_path_as_zstd() {
        let store = Store::open(tmp()).unwrap();
        let r = Reencoder::default();
        let now = SystemTime::now();
        let p = put(&store, 'a', "xz", now - 30 * 24 * HOUR);
        let bad = put(&store, 'b', "xz", now - HOUR);
        corrupt(&store, &bad, "xz");
        list(&store, "local-1-x", &[&p, &bad]);
        store.mark_local_origin("znix", "local-1-x").unwrap();
        let remote = crate::remote::fake::FakeRemote::default();
        let keys = keys();
        let m = crate::mirror::Mirror {
            store: &store,
            remote: &remote,
            upstreams: vec![],
            keys: &keys,
            agent: ureq::Agent::new_with_defaults(),
            misses: Default::default(),
            counters: Box::leak(Box::default()),
            reencode: Some(&r),
        };
        assert_eq!(m.up().unwrap(), 0);

        let mut expected = vec![
            format!("{}.narinfo", p.hash),
            format!("nar/{}", p.zst),
            format!("{}.narinfo", bad.hash),
            format!("nar/{}", bad.file),
            "roots/znix/local-1-x.json".to_string(),
        ];
        expected.sort();
        assert_eq!(remote.keys(), expected);
        use crate::remote::Remote;
        let up = String::from_utf8(remote.get(&format!("{}.narinfo", p.hash)).unwrap().unwrap())
            .unwrap();
        assert_eq!(field(&up, "Compression"), Some("zstd"));
        assert!(NarInfo::parse(&up).unwrap().verify(&keys));
        let nar = remote.get(&format!("nar/{}", p.zst)).unwrap().unwrap();
        assert_eq!(zstd::decode_all(&nar[..]).unwrap(), p.nar);
        // The one it could not re-encode went up as it is, and counts.
        let up = remote
            .get(&format!("{}.narinfo", bad.hash))
            .unwrap()
            .unwrap();
        assert_eq!(up, bad.narinfo.as_bytes());
        assert_eq!(n(&r.stats.failed), 1);
        let _ = fs::remove_dir_all(&store.root);
    }

    #[test]
    fn maintain_re_encodes_only_with_the_flag_on() {
        for on in [false, true] {
            let store = Store::open(tmp()).unwrap();
            let p = put(&store, 'a', "xz", SystemTime::now() - HOUR);
            let app = std::sync::Arc::new(crate::server::App {
                store: store.into(),
                keys: vec![],
                token: None,
                status: Default::default(),
                counters: Default::default(),
                ages: Default::default(),
                priority: 10,
                pull: None,
                lru: None,
                reencode: on.then(Reencoder::default),
            });
            let maintained = app.clone();
            std::thread::spawn(move || crate::gc::maintain(&maintained, Duration::from_millis(10)));
            let zstd = || {
                let stored = app.store.read_narinfo(&p.hash).unwrap().unwrap();
                field(&stored, "Compression") == Some("zstd")
            };
            for _ in 0..if on { 500 } else { 20 } {
                if zstd() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            assert_eq!(zstd(), on);
            if !on {
                assert_eq!(app.store.read_narinfo(&p.hash).unwrap().unwrap(), p.narinfo);
                assert_eq!(nar_dir(&app.store), [p.file.as_str()]);
            }
            let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
            let base = format!("http://{}", server.server_addr().to_ip().unwrap());
            let serving = app.clone();
            std::thread::spawn(move || {
                while let Ok(req) = server.recv() {
                    crate::server::handle(&serving, req);
                }
            });
            let metrics = ureq::Agent::new_with_defaults()
                .get(format!("{base}/metrics"))
                .call()
                .unwrap()
                .body_mut()
                .read_to_string()
                .unwrap();
            let reencoded = "kasha_reencode_paths_total{result=\"reencoded\"} 1";
            assert_eq!(metrics.lines().any(|l| l == reencoded), on, "{metrics}");
            assert_eq!(
                metrics.contains("kasha_reencode_bytes_total{file=\"new\"}"),
                on
            );
            let _ = fs::remove_dir_all(&app.store.root);
        }
    }
}
