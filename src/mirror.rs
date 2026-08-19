//! Eager bidirectional replica workers.
//!
//! Down: discover remote manifests, fetch the listed closure paths that exist
//! (remote first, then upstream caches), record per-path misses as gaps —
//! expected steady state, never an error. Local manifests absent from the
//! remote listing are removed (reflecting the remote retention decision),
//! except unmirrored local-origin gens.
//!
//! Up: everything the remote lacks from local-origin generations; manifest
//! published last, then the gen is marked mirrored (GC guard lifts).

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::io::Read;

use crate::manifest::Manifest;
use crate::narinfo::{store_hash_of, NarInfo, PubKey};
use crate::remote::Remote;
use crate::store::Store;

pub struct Mirror<'a> {
    pub store: &'a Store,
    pub remote: &'a dyn Remote,
    /// Upstream binary-cache base URLs (e.g. https://cache.nixos.org).
    pub upstreams: Vec<String>,
    pub keys: &'a [PubKey],
    pub agent: ureq::Agent,
}

#[derive(Debug, Default, PartialEq)]
pub struct DownReport {
    /// flake -> unresolved gap count across its manifests.
    pub gaps: HashMap<String, usize>,
    pub fetched_paths: usize,
}

fn manifest_key(flake: &str, gen_id: &str) -> String {
    format!("roots/{flake}/{gen_id}.json")
}

impl Mirror<'_> {
    pub fn down(&self) -> Result<DownReport> {
        let listing = self.remote.list("roots/")?;
        let remote_keys: std::collections::HashSet<&str> =
            listing.iter().map(|(k, _)| k.as_str()).collect();

        // New manifests: fetch, keep only valid v3.
        for (key, _) in &listing {
            let Some((flake, gen_id)) = parse_manifest_key(key) else { continue };
            let local = self.store.manifest_path(flake, gen_id)?;
            if local.exists() {
                continue;
            }
            let Some(bytes) = self.remote.get(key)? else { continue };
            match Manifest::parse(&bytes) {
                Ok(m) => {
                    self.store.put_manifest(&m, &bytes)?;
                    tracing::info!(flake, gen = gen_id, "new remote generation");
                }
                Err(e) => tracing::debug!(key, error = %e, "ignoring non-v3 manifest"),
            }
        }

        // Deletions: reflect remote retention, guard unmirrored local pushes.
        for (path, m) in self.store.manifests()? {
            if remote_keys.contains(manifest_key(&m.flake, &m.gen_id).as_str()) {
                continue;
            }
            if self.store.is_local_origin(&m.flake, &m.gen_id)
                && !self.store.is_mirrored(&m.flake, &m.gen_id)
            {
                continue; // box may hold the only copy
            }
            tracing::info!(path = %path.display(), "manifest gone remotely, dropping");
            self.store.remove_manifest(&m.flake, &m.gen_id)?;
        }

        // Gap fill: fetch what exists, count what doesn't.
        let mut report = DownReport::default();
        for (_, m) in self.store.manifests()? {
            let mut unresolved = 0;
            for path in self.store.gaps(&m) {
                match self.fetch_path(&path) {
                    Ok(true) => report.fetched_paths += 1,
                    Ok(false) => unresolved += 1,
                    Err(e) => {
                        tracing::warn!(path, error = %e, "fetch failed");
                        unresolved += 1;
                    }
                }
            }
            *report.gaps.entry(m.flake.clone()).or_default() += unresolved;
        }
        Ok(report)
    }

    /// Try each source (remote, then upstreams) for a narinfo+nar *pair* —
    /// nar keys are compression-specific, so the pair must come from one
    /// source. Returns false when no source has it (a gap, not an error).
    fn fetch_path(&self, store_path: &str) -> Result<bool> {
        let hash = store_hash_of(store_path);
        let narinfo_key = format!("{hash}.narinfo");
        for source in self.sources() {
            let Some(raw) = source.get(&narinfo_key)? else { continue };
            let text = match std::str::from_utf8(&raw) {
                Ok(t) => t,
                Err(_) => continue,
            };
            let info = match NarInfo::parse(text) {
                Ok(i) => i,
                Err(e) => {
                    tracing::warn!(hash, error = %e, "bad narinfo from source");
                    continue;
                }
            };
            if !info.verify(self.keys) {
                tracing::warn!(hash, "narinfo lacks trusted signature, skipping source");
                continue;
            }
            let Some(mut nar) = source.get_stream(&info.url)? else {
                tracing::debug!(hash, url = info.url, "narinfo without nar on source");
                continue;
            };
            let file = info
                .url
                .strip_prefix("nar/")
                .with_context(|| format!("unexpected nar URL {}", info.url))?;
            self.store.put_nar(file, &mut nar)?;
            self.store.put_narinfo(&info, &raw)?; // nar first: index never dangles
            return Ok(true);
        }
        Ok(false)
    }

    fn sources(&self) -> Vec<Box<dyn ObjectSource + '_>> {
        let mut v: Vec<Box<dyn ObjectSource>> = vec![Box::new(RemoteSource(self.remote))];
        for up in &self.upstreams {
            v.push(Box::new(HttpSource { base: up, agent: &self.agent }));
        }
        v
    }

    /// Returns the number of still-pending local-origin generations.
    pub fn up(&self) -> Result<usize> {
        let mut pending = 0;
        for (_, m) in self.store.manifests()? {
            if !self.store.is_local_origin(&m.flake, &m.gen_id)
                || self.store.is_mirrored(&m.flake, &m.gen_id)
            {
                continue;
            }
            match self.push_gen(&m) {
                Ok(()) => {}
                Err(e) => {
                    tracing::warn!(flake = m.flake, gen = m.gen_id, error = %e, "mirror-up deferred");
                    pending += 1;
                }
            }
        }
        Ok(pending)
    }

    fn push_gen(&self, m: &Manifest) -> Result<()> {
        let gaps = self.store.gaps(m);
        anyhow::ensure!(
            gaps.is_empty(),
            "local push incomplete ({} paths missing)",
            gaps.len()
        );
        for path in &m.closure {
            let hash = store_hash_of(path);
            let key = format!("{hash}.narinfo");
            if self.remote.exists(&key)? {
                continue;
            }
            let raw = self
                .store
                .read_narinfo(hash)?
                .context("indexed narinfo vanished")?;
            let info = NarInfo::parse(&raw)?;
            // ponytail: nar buffered in memory; stream the upload if local
            // pushes ever carry multi-GB nars.
            let file = info.url.strip_prefix("nar/").context("nar URL")?;
            let nar = std::fs::read(self.store.nar_path(file)?)
                .with_context(|| format!("local nar {file}"))?;
            self.remote.put(&info.url, &nar)?;
            self.remote.put(&key, raw.as_bytes())?; // nar first, narinfo second
            tracing::info!(hash, "mirrored up");
        }
        // Manifest last: readers/GC only see complete generations.
        let raw = std::fs::read(self.store.manifest_path(&m.flake, &m.gen_id)?)?;
        self.remote.put(&manifest_key(&m.flake, &m.gen_id), &raw)?;
        self.store.mark_mirrored(&m.flake, &m.gen_id)?;
        tracing::info!(flake = m.flake, gen = m.gen_id, "generation mirrored up");
        Ok(())
    }
}

fn parse_manifest_key(key: &str) -> Option<(&str, &str)> {
    let rest = key.strip_prefix("roots/")?;
    let (flake, file) = rest.split_once('/')?;
    Some((flake, file.strip_suffix(".json")?))
}

/// One place a narinfo+nar pair can come from.
trait ObjectSource {
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>>;
    fn get_stream(&self, key: &str) -> Result<Option<Box<dyn Read + '_>>>;
}

struct RemoteSource<'a>(&'a dyn Remote);
impl ObjectSource for RemoteSource<'_> {
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        self.0.get(key)
    }
    fn get_stream(&self, key: &str) -> Result<Option<Box<dyn Read + '_>>> {
        self.0.get_stream(key)
    }
}

struct HttpSource<'a> {
    base: &'a str,
    agent: &'a ureq::Agent,
}
impl ObjectSource for HttpSource<'_> {
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        match self.get_stream(key)? {
            Some(mut r) => {
                let mut buf = Vec::new();
                r.read_to_end(&mut buf)?;
                Ok(Some(buf))
            }
            None => Ok(None),
        }
    }
    fn get_stream(&self, key: &str) -> Result<Option<Box<dyn Read + '_>>> {
        match self.agent.get(format!("{}/{}", self.base, key)).call() {
            Ok(resp) => Ok(Some(Box::new(resp.into_body().into_reader()))),
            Err(ureq::Error::StatusCode(404)) => Ok(None),
            Err(e) => Err(e).with_context(|| format!("upstream GET {}/{}", self.base, key)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::fake::FakeRemote;
    use data_encoding::BASE64;
    use ed25519_dalek::{Signer, SigningKey};
    use std::path::PathBuf;

    fn keypair() -> (SigningKey, PubKey) {
        let sk = SigningKey::from_bytes(&[5u8; 32]);
        let pk = PubKey::parse(&format!(
            "test-1:{}",
            BASE64.encode(sk.verifying_key().as_bytes())
        ))
        .unwrap();
        (sk, pk)
    }

    /// A signed narinfo + nar body for a fake store path `<c*32>-<name>`.
    fn object(sk: &SigningKey, c: char, name: &str) -> (String, String, Vec<u8>) {
        let hash: String = std::iter::repeat_n(c, 32).collect();
        let body = format!(
            "StorePath: /nix/store/{hash}-{name}\n\
URL: nar/{hash}.nar.xz\n\
Compression: xz\n\
NarHash: sha256:00g966jlz9h37xkb9pmr3rc700i4k19mkyqm3gmwvlaik16qam5x\n\
NarSize: 8\n\
References: \n"
        );
        let info = NarInfo::parse(&body).unwrap();
        let sig = sk.sign(info.fingerprint().as_bytes());
        (
            hash.clone(),
            format!("{body}Sig: test-1:{}\n", BASE64.encode(&sig.to_bytes())),
            format!("NAR-{c}").into_bytes(),
        )
    }

    fn manifest(gen_id: &str, branch: &str, paths: &[&str]) -> Vec<u8> {
        serde_json::json!({
            "version": 3, "flake": "znix", "gen": gen_id, "branch": branch,
            "attr": "x", "timestamp": "2026-08-20T10:00:00Z",
            "closure": paths,
        })
        .to_string()
        .into_bytes()
    }

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "kasha-mirror-{:?}-{}",
            std::thread::current().id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn mirror<'a>(store: &'a Store, remote: &'a FakeRemote, keys: &'a [PubKey]) -> Mirror<'a> {
        Mirror {
            store,
            remote,
            upstreams: vec![],
            keys,
            agent: ureq::Agent::new_with_defaults(),
        }
    }

    #[test]
    fn down_fetches_new_generation_and_reports_gaps() {
        let (sk, pk) = keypair();
        let keys = [pk];
        let dir = tmp();
        let store = Store::open(&dir).unwrap();
        let remote = FakeRemote::default();

        let (ha, ia, na) = object(&sk, 'a', "pkg-a");
        remote.insert(&format!("{ha}.narinfo"), ia.as_bytes());
        remote.insert(&format!("nar/{ha}.nar.xz"), &na);
        // Manifest lists a second path nobody has: a gap, not a failure.
        let missing = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-pkg-b";
        remote.insert(
            "roots/znix/main-1-x.json",
            &manifest("main-1-x", "main", &[&format!("/nix/store/{ha}-pkg-a"), missing]),
        );

        let m = mirror(&store, &remote, &keys);
        let report = m.down().unwrap();
        assert!(store.has(&ha));
        assert_eq!(report.fetched_paths, 1);
        assert_eq!(report.gaps["znix"], 1);

        // Second cycle: idempotent, gap still quietly reported.
        let report = m.down().unwrap();
        assert_eq!(report.fetched_paths, 0);
        assert_eq!(report.gaps["znix"], 1);

        // The missing object appears later: gap fills.
        let (hb, ib, nb) = object(&sk, 'b', "pkg-b");
        remote.insert(&format!("{hb}.narinfo"), ib.as_bytes());
        remote.insert(&format!("nar/{hb}.nar.xz"), &nb);
        let report = m.down().unwrap();
        assert_eq!(report.fetched_paths, 1);
        assert_eq!(report.gaps["znix"], 0);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn down_rejects_untrusted_and_syncs_deletions() {
        let (_, pk) = keypair();
        let other = SigningKey::from_bytes(&[9u8; 32]);
        let keys = [pk];
        let dir = tmp();
        let store = Store::open(&dir).unwrap();
        let remote = FakeRemote::default();

        // Object signed by an untrusted key: never ingested.
        let (ha, ia, na) = object(&other, 'a', "evil");
        remote.insert(&format!("{ha}.narinfo"), ia.as_bytes());
        remote.insert(&format!("nar/{ha}.nar.xz"), &na);
        remote.insert(
            "roots/znix/main-1-x.json",
            &manifest("main-1-x", "main", &[&format!("/nix/store/{ha}-evil")]),
        );
        let m = mirror(&store, &remote, &keys);
        let report = m.down().unwrap();
        assert!(!store.has(&ha));
        assert_eq!(report.gaps["znix"], 1);

        // Remote sweep removes the manifest: local copy follows.
        remote.delete("roots/znix/main-1-x.json").unwrap();
        m.down().unwrap();
        assert_eq!(store.manifests().unwrap().len(), 0);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn down_never_drops_unmirrored_local_push() {
        let (sk, pk) = keypair();
        let keys = [pk];
        let dir = tmp();
        let store = Store::open(&dir).unwrap();
        let remote = FakeRemote::default();

        let (ha, ia, na) = object(&sk, 'a', "pkg-a");
        let info = NarInfo::parse(&ia).unwrap();
        store.put_nar(&format!("{ha}.nar.xz"), &na[..]).unwrap();
        store.put_narinfo(&info, ia.as_bytes()).unwrap();
        let mbytes = manifest("local-1-x", "feature", &[&format!("/nix/store/{ha}-pkg-a")]);
        store
            .put_manifest(&Manifest::parse(&mbytes).unwrap(), &mbytes)
            .unwrap();
        store.mark_local_origin("znix", "local-1-x").unwrap();

        // Not on the remote at all — down() must keep it.
        let m = mirror(&store, &remote, &keys);
        m.down().unwrap();
        assert_eq!(store.manifests().unwrap().len(), 1);

        // up() publishes nar, narinfo, then manifest; marks mirrored.
        let pending = m.up().unwrap();
        assert_eq!(pending, 0);
        assert!(store.is_mirrored("znix", "local-1-x"));
        assert_eq!(
            remote.keys(),
            vec![
                format!("{ha}.narinfo"),
                format!("nar/{ha}.nar.xz"),
                "roots/znix/local-1-x.json".to_string(),
            ]
        );

        // Now that it is mirrored, a later remote deletion is honored.
        remote.delete("roots/znix/local-1-x.json").unwrap();
        m.down().unwrap();
        assert_eq!(store.manifests().unwrap().len(), 0);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn up_defers_incomplete_local_push() {
        let (_, pk) = keypair();
        let keys = [pk];
        let dir = tmp();
        let store = Store::open(&dir).unwrap();
        let remote = FakeRemote::default();

        let mbytes = manifest(
            "local-2-x",
            "main",
            &["/nix/store/cccccccccccccccccccccccccccccccc-pkg-c"],
        );
        store
            .put_manifest(&Manifest::parse(&mbytes).unwrap(), &mbytes)
            .unwrap();
        store.mark_local_origin("znix", "local-2-x").unwrap();

        let m = mirror(&store, &remote, &keys);
        assert_eq!(m.up().unwrap(), 1); // pending, nothing uploaded
        assert!(remote.keys().is_empty());
        assert!(!store.is_mirrored("znix", "local-2-x"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn down_falls_back_to_upstream_pair() {
        let (sk, pk) = keypair();
        let keys = [pk];
        let dir = tmp();
        let updir = tmp();
        let store = Store::open(&dir).unwrap();
        let remote = FakeRemote::default();

        // Upstream = a second kasha store served over HTTP.
        let upstore = Store::open(&updir).unwrap();
        let (ha, ia, na) = object(&sk, 'a', "pkg-a");
        upstore.put_nar(&format!("{ha}.nar.xz"), &na[..]).unwrap();
        upstore
            .put_narinfo(&NarInfo::parse(&ia).unwrap(), ia.as_bytes())
            .unwrap();
        let app = std::sync::Arc::new(crate::server::App {
            store: upstore,
            keys: vec![],
            token: None,
            status: std::sync::Mutex::new(crate::server::Status::default()),
        });
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr().to_ip().unwrap());
        std::thread::spawn(move || {
            while let Ok(req) = server.recv() {
                crate::server::handle(&app, req);
            }
        });

        remote.insert(
            "roots/znix/main-1-x.json",
            &manifest("main-1-x", "main", &[&format!("/nix/store/{ha}-pkg-a")]),
        );
        let m = Mirror {
            store: &store,
            remote: &remote,
            upstreams: vec![base],
            keys: &keys,
            agent: ureq::Agent::new_with_defaults(),
        };
        let report = m.down().unwrap();
        assert_eq!(report.fetched_paths, 1);
        assert!(store.has(&ha));
        assert_eq!(report.gaps["znix"], 0);
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(updir);
    }
}
