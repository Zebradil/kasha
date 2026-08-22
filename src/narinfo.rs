//! Minimal narinfo parse + ed25519 signature verification.
//!
//! In-repo instead of tvix `nix-compat`: crates.io only carries a 0.0.0-pre
//! placeholder; the real crate is a git dep on the snix monorepo. The subset
//! kasha needs (parse a few fields, verify the fingerprint signature) is small
//! enough to own.

use anyhow::{Context, Result, bail};
use data_encoding::BASE64;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};

pub const STORE_DIR: &str = "/nix/store";

/// A nix binary-cache public key: `name:base64(32-byte ed25519 pubkey)`.
pub struct PubKey {
    pub name: String,
    key: VerifyingKey,
}

impl PubKey {
    pub fn parse(s: &str) -> Result<Self> {
        let (name, b64) = s
            .split_once(':')
            .with_context(|| format!("public key missing ':': {s}"))?;
        let bytes = BASE64.decode(b64.as_bytes()).context("public key base64")?;
        let arr: [u8; 32] = bytes
            .as_slice()
            .try_into()
            .map_err(|_| anyhow::anyhow!("public key must be 32 bytes"))?;
        Ok(Self {
            name: name.to_string(),
            key: VerifyingKey::from_bytes(&arr).context("invalid ed25519 key")?,
        })
    }
}

/// The fields kasha needs from a narinfo. `raw` keeps the exact bytes so the
/// store stays byte-identical to what writers produced.
pub struct NarInfo {
    pub store_path: String,
    pub url: String,
    pub nar_hash: String,
    pub nar_size: u64,
    /// Reference basenames as listed (nix emits them sorted).
    pub references: Vec<String>,
    /// `name:base64sig` lines, unparsed.
    pub sigs: Vec<String>,
}

impl NarInfo {
    pub fn parse(text: &str) -> Result<Self> {
        let mut store_path = None;
        let mut url = None;
        let mut nar_hash = None;
        let mut nar_size = None;
        let mut references = Vec::new();
        let mut sigs = Vec::new();
        for line in text.lines() {
            let Some((k, v)) = line.split_once(": ") else {
                if line.trim().is_empty() {
                    continue;
                }
                bail!("malformed narinfo line: {line:?}");
            };
            match k {
                "StorePath" => store_path = Some(v.to_string()),
                "URL" => url = Some(v.to_string()),
                "NarHash" => nar_hash = Some(v.to_string()),
                "NarSize" => nar_size = Some(v.parse::<u64>().context("NarSize")?),
                "References" => references = v.split_whitespace().map(str::to_string).collect(),
                "Sig" => sigs.push(v.to_string()),
                _ => {}
            }
        }
        let store_path: String = store_path.context("narinfo missing StorePath")?;
        if !store_path.starts_with(STORE_DIR) {
            bail!("StorePath outside {STORE_DIR}: {store_path}");
        }
        Ok(Self {
            store_path,
            url: url.context("narinfo missing URL")?,
            nar_hash: nar_hash.context("narinfo missing NarHash")?,
            nar_size: nar_size.context("narinfo missing NarSize")?,
            references,
            sigs,
        })
    }

    /// The 32-char base32 hash part of the store path basename.
    pub fn store_hash(&self) -> &str {
        store_hash_of(&self.store_path)
    }

    /// The string nix signs: `1;<path>;<narHash>;<narSize>;<full-path refs, comma-joined>`.
    pub fn fingerprint(&self) -> String {
        let refs: Vec<String> = self
            .references
            .iter()
            .map(|r| format!("{STORE_DIR}/{r}"))
            .collect();
        format!(
            "1;{};{};{};{}",
            self.store_path,
            self.nar_hash,
            self.nar_size,
            refs.join(",")
        )
    }

    /// True iff any Sig line verifies against a trusted key of the same name.
    pub fn verify(&self, keys: &[PubKey]) -> bool {
        let fp = self.fingerprint();
        self.sigs.iter().any(|sig| {
            let Some((name, b64)) = sig.split_once(':') else {
                return false;
            };
            let Ok(bytes) = BASE64.decode(b64.as_bytes()) else {
                return false;
            };
            let Ok(arr) = <[u8; 64]>::try_from(bytes.as_slice()) else {
                return false;
            };
            let sig = Signature::from_bytes(&arr);
            keys.iter()
                .any(|k| k.name == name && k.key.verify(fp.as_bytes(), &sig).is_ok())
        })
    }
}

/// Hash part (first 32 chars of the basename) of a full store path.
pub fn store_hash_of(store_path: &str) -> &str {
    let base = store_path.rsplit('/').next().unwrap_or(store_path);
    &base[..base.len().min(32)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    // Real object from cache.nixos.org (hello-2.12.3, aarch64-darwin).
    const REAL: &str = "StorePath: /nix/store/v4f0jj9sz97ckskvacf40llz4nfr19jf-hello-2.12.3\n\
URL: nar/00g966jlz9h37xkb9pmr3rc700i4k19mkyqm3gmwvlaik16qam5x.nar.zst\n\
Compression: zstd\n\
FileHash: sha256:0v8pmkqx2ipvv41a2m8p39qsp3z3ffzy904mrqsi4mikjvypvryg\n\
FileSize: 31063\n\
NarHash: sha256:00g966jlz9h37xkb9pmr3rc700i4k19mkyqm3gmwvlaik16qam5x\n\
NarSize: 113096\n\
References: jspv3c5l2zx4kiwzhq0zgxcwp34cqifz-libiconv-115.100.1\n\
Deriver: lvr08sbgczxiy7299l5a1adss05fn4r7-hello-2.12.3.drv\n\
Sig: cache.nixos.org-1:KEsNsSW3fMW5Izf4ZtjDbvSy/IO7al066kF52gutYtw/wJ8PopYTyu2aAjm86Rw+h0orEJD6ShbKFf63ThBpDw==\n";

    const NIXOS_KEY: &str = "cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY=";

    #[test]
    fn parses_fields() {
        let n = NarInfo::parse(REAL).unwrap();
        assert_eq!(n.store_hash(), "v4f0jj9sz97ckskvacf40llz4nfr19jf");
        assert_eq!(
            n.url,
            "nar/00g966jlz9h37xkb9pmr3rc700i4k19mkyqm3gmwvlaik16qam5x.nar.zst"
        );
        assert_eq!(n.nar_size, 113096);
        assert_eq!(n.references.len(), 1);
        assert_eq!(n.sigs.len(), 1);
    }

    #[test]
    fn verifies_real_cache_nixos_org_sig() {
        let n = NarInfo::parse(REAL).unwrap();
        let key = PubKey::parse(NIXOS_KEY).unwrap();
        assert!(n.verify(&[key]));
    }

    #[test]
    fn rejects_wrong_key_and_tampered_body() {
        let n = NarInfo::parse(REAL).unwrap();
        // Right name, wrong key material.
        let wrong = SigningKey::from_bytes(&[9u8; 32]).verifying_key();
        let bogus = PubKey::parse(&format!(
            "cache.nixos.org-1:{}",
            BASE64.encode(wrong.as_bytes())
        ))
        .unwrap();
        assert!(!n.verify(&[bogus]));
        // Untrusted key name.
        let other =
            PubKey::parse(&format!("other-1:{}", NIXOS_KEY.split(':').nth(1).unwrap())).unwrap();
        assert!(!n.verify(&[other]));
        // Tampered NarSize breaks the fingerprint.
        let tampered = REAL.replace("NarSize: 113096", "NarSize: 113097");
        let t = NarInfo::parse(&tampered).unwrap();
        assert!(!t.verify(&[PubKey::parse(NIXOS_KEY).unwrap()]));
    }

    #[test]
    fn roundtrip_with_local_key() {
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let pk_b64 = BASE64.encode(sk.verifying_key().as_bytes());
        let mut n = NarInfo::parse(REAL).unwrap();
        n.sigs.clear();
        let sig = sk.sign(n.fingerprint().as_bytes());
        n.sigs
            .push(format!("test-1:{}", BASE64.encode(&sig.to_bytes())));
        let key = PubKey::parse(&format!("test-1:{pk_b64}")).unwrap();
        assert!(n.verify(&[key]));
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(NarInfo::parse("not a narinfo").is_err());
        assert!(NarInfo::parse("URL: nar/x.nar\nNarHash: sha256:0\nNarSize: 1\n").is_err());
        assert!(
            NarInfo::parse(
                "StorePath: /etc/passwd\nURL: nar/x.nar\nNarHash: sha256:0\nNarSize: 1\n"
            )
            .is_err()
        );
    }
}
