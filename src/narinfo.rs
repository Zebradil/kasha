//! Minimal narinfo parse + trust check: an ed25519 signature from a trusted
//! key, or a content address that reproduces the store path.
//!
//! In-repo instead of tvix `nix-compat`: crates.io only carries a 0.0.0-pre
//! placeholder; the real crate is a git dep on the snix monorepo. The subset
//! kasha needs (parse a few fields, verify the fingerprint signature, recompute
//! a content-addressed store path) is small enough to own.

use anyhow::{Context, Result, bail};
use data_encoding::{BASE64, BitOrder, Encoding, HEXLOWER, Specification};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use sha2::{Digest, Sha256};

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
    /// `CA` line, e.g. `text:sha256:<nix32>`; absent on input-addressed paths.
    pub ca: Option<String>,
}

impl NarInfo {
    pub fn parse(text: &str) -> Result<Self> {
        let mut store_path = None;
        let mut url = None;
        let mut nar_hash = None;
        let mut nar_size = None;
        let mut references = Vec::new();
        let mut sigs = Vec::new();
        let mut ca = None;
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
                "CA" => ca = Some(v.to_string()),
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
            ca,
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

    /// True iff the content address reproduces the store path, or any Sig line
    /// verifies against a trusted key of the same name. A valid CA needs no
    /// signature: the path itself commits to the content, and nix rejects a
    /// NAR whose content does not match the CA when importing it.
    pub fn verify(&self, keys: &[PubKey]) -> bool {
        if self.ca_store_path().as_deref() == Some(self.store_path.as_str()) {
            return true;
        }
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

    /// Key names of the `Sig` lines, so a rejection can say who signed it.
    pub fn sig_key_names(&self) -> Vec<&str> {
        self.sigs
            .iter()
            .filter_map(|s| s.split_once(':').map(|(name, _)| name))
            .collect()
    }

    /// The store path nix derives from `CA`, `References` and the name
    /// (nix's `makeTextPath` / `makeFixedOutputPath`). None when the CA is
    /// absent, malformed, contradicts the other fields, or is a kind kasha
    /// does not model (git hashing) — all of which fail closed.
    fn ca_store_path(&self) -> Option<String> {
        let ca = self.ca.as_deref()?;
        let base = self.store_path.strip_prefix(STORE_DIR)?.strip_prefix('/')?;
        let name = base.get(33..)?;
        let mut others: Vec<&str> = self
            .references
            .iter()
            .map(String::as_str)
            .filter(|r| *r != base)
            .collect();
        others.sort_unstable();
        let has_self = others.len() != self.references.len();
        let refs: String = others.iter().map(|r| format!(":{STORE_DIR}/{r}")).collect();

        if let Some(h) = ca.strip_prefix("text:sha256:") {
            if has_self {
                return None;
            }
            return Some(make_store_path(
                &format!("text{refs}"),
                &nix32_decode(h, 32)?,
                name,
            ));
        }
        let (recursive, rest) = match ca.strip_prefix("fixed:r:") {
            Some(rest) => (true, rest),
            None => (false, ca.strip_prefix("fixed:")?),
        };
        let (algo, h) = rest.split_once(':')?;
        if recursive && algo == "sha256" {
            // A recursive sha256 hashes the NAR itself, so NarHash must agree;
            // this also binds the NAR the box stores to the path.
            if self.nar_hash != format!("sha256:{h}") {
                return None;
            }
            let self_ref = if has_self { ":self" } else { "" };
            let ty = format!("source{refs}{self_ref}");
            return Some(make_store_path(&ty, &nix32_decode(h, 32)?, name));
        }
        // Any other fixed-output path cannot carry references: nix leaves them
        // out of the path, so accepting some here would let them be forged.
        if !self.references.is_empty() {
            return None;
        }
        let len = match algo {
            "md5" => 16,
            "sha1" => 20,
            "sha256" => 32,
            "sha512" => 64,
            _ => return None,
        };
        let hash = nix32_decode(h, len)?;
        let r = if recursive { "r:" } else { "" };
        let inner = Sha256::digest(format!("fixed:out:{r}{algo}:{}:", HEXLOWER.encode(&hash)));
        Some(make_store_path("output:out", &inner, name))
    }
}

/// Nix's `makeStorePath`: sha256 over the descriptor, XOR-folded to 20 bytes.
fn make_store_path(ty: &str, hash: &[u8], name: &str) -> String {
    let digest = Sha256::digest(format!(
        "{ty}:sha256:{}:{STORE_DIR}:{name}",
        HEXLOWER.encode(hash)
    ));
    let mut folded = [0u8; 20];
    for (i, b) in digest.iter().enumerate() {
        folded[i % 20] ^= b;
    }
    let hash_part: String = nix32().encode(&folded).chars().rev().collect();
    format!("{STORE_DIR}/{hash_part}-{name}")
}

/// Nix's base32: its own alphabet, least-significant bits first, written back
/// to front — so a reversed string is plain LSB-first base32.
fn nix32() -> Encoding {
    let mut spec = Specification::new();
    spec.symbols.push_str("0123456789abcdfghijklmnpqrsvwxyz");
    spec.bit_order = BitOrder::LeastSignificantFirst;
    spec.encoding().expect("valid nix32 spec")
}

fn nix32_decode(s: &str, len: usize) -> Option<Vec<u8>> {
    let reversed: String = s.chars().rev().collect();
    nix32()
        .decode(reversed.as_bytes())
        .ok()
        .filter(|b| b.len() == len)
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
    fn sig_key_names_skips_malformed_lines() {
        let text = format!("{REAL}Sig: garbage\nSig: other-1:AAAA\n");
        let n = NarInfo::parse(&text).unwrap();
        assert_eq!(n.sig_key_names(), ["cache.nixos.org-1", "other-1"]);
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

    // Real unsigned objects CI pushed to the znix cache: a text-CA `.drv` with
    // references, and a recursive-sha256 source.
    const TEXT_DRV: &str = "StorePath: /nix/store/6wgyxlqx39sf48qxyzc3jm0g61k5nw1z-lint.drv\n\
URL: nar/0m2339xfs3spmr2wyk27xpwxs0n6scdmgjq348aqnva46pzpqj34.nar.zst\n\
Compression: zstd\n\
FileHash: sha256:0m2339xfs3spmr2wyk27xpwxs0n6scdmgjq348aqnva46pzpqj34\n\
FileSize: 1498\n\
NarHash: sha256:1rdpcwk76b3xkc50hz1z2f8wk58mc9vy111crp7r5aw27jlfrva3\n\
NarSize: 3768\n\
References: 5wxhjx3ryms32bmx7xmbspid15ygil4g-source d1h6nfwgqq43drkzzszmryjvcfrs1s5p-jq-1.8.2.drv fvbc4g2s1pqvkrmrnq2ycd5kg5f2zsyy-python3-3.14.7.drv hplnhqsmnpr4gv35yf4cxvbalki3k308-bash-5.3p15.drv kv2m32vy9pnbp4kgkzfbclsbyam14dvg-stdenv-linux-no-cc.drv l0jdfv2mv07y12knwrq6gvr0jgy1819l-shellcheck-0.11.0.drv l622p70vy8k5sh7y5wizi5f2mic6ynpg-source-stdenv.sh r74f4mh034y45qaacan5cp9nrsiysyj1-yq-go-4.53.3.drv s626xa23pqgl9ckcc9bj4gf245abaz2s-actionlint-1.7.12.drv shkw4qm9qcw5sc5n1k5jznc83ny02r39-default-builder.sh w1rqkj3sd9z542x1zv474rkh1186ysxm-git-2.55.0.drv\n\
CA: text:sha256:1s6jdd4jlmlxk7bgrqwlcvqnb7rzqlisy1b62wz3drj1i7jrfss9\n";

    const SOURCE: &str = "StorePath: /nix/store/5wxhjx3ryms32bmx7xmbspid15ygil4g-source\n\
URL: nar/1impghh7wqi8f1nfnwsch7fb8xf055qn24xiq037yv64g81zqzjv.nar.zst\n\
Compression: zstd\n\
FileHash: sha256:1impghh7wqi8f1nfnwsch7fb8xf055qn24xiq037yv64g81zqzjv\n\
FileSize: 384300\n\
NarHash: sha256:1gsfphd7gqaflfwxah0bd9gh2kmkf0rmcwgg630w03cz6asakzp7\n\
NarSize: 1434336\n\
References: \n\
CA: fixed:r:sha256:1gsfphd7gqaflfwxah0bd9gh2kmkf0rmcwgg630w03cz6asakzp7\n";

    // cache.nixos.org fetchurl output (flat fixed-output), Sig line dropped.
    const FLAT: &str = "StorePath: /nix/store/wj7phsmi7ncidl8k00p489krqss7n9sd-hello-2.12.3.tar.gz\n\
URL: nar/0fsxp2bgcinx2wmqw6w5x82j2bmsvl87ls3szxvffn4f5za9ipyi.nar.zst\n\
Compression: zstd\n\
FileHash: sha256:0hd5z0948qq6kci2l1j3iybz6q34ji5p9s19yb5fmspwwsh4yqm8\n\
FileSize: 1189356\n\
NarHash: sha256:0fsxp2bgcinx2wmqw6w5x82j2bmsvl87ls3szxvffn4f5za9ipyi\n\
NarSize: 1189320\n\
Deriver: 9xj3yy4cw1irk4wypbdqrn2w8jpjyvnr-hello-2.12.3.tar.gz.drv\n\
CA: fixed:sha256:183a6rxnhixiyykd7qis0y9g9cfqhpkk872a245y3zl28can0pqd\n";

    #[test]
    fn content_address_verifies_without_sig() {
        for text in [TEXT_DRV, SOURCE, FLAT] {
            let n = NarInfo::parse(text).unwrap();
            assert!(n.sigs.is_empty());
            assert!(n.verify(&[]), "{}", n.store_path);
        }
    }

    #[test]
    fn content_address_rejects_tampering() {
        let cases = [
            ("CA hash", TEXT_DRV.replace("sha256:1s6j", "sha256:1s6k")),
            (
                "dropped reference",
                TEXT_DRV.replace(" d1h6nfwgqq43drkzzszmryjvcfrs1s5p-jq-1.8.2.drv", ""),
            ),
            ("renamed", TEXT_DRV.replace("-lint.drv", "-lint2.drv")),
            (
                "NarHash",
                SOURCE.replace("NarHash: sha256:1gsf", "NarHash: sha256:1gsg"),
            ),
            (
                "forged reference",
                FLAT.replace(
                    "NarSize: 1189320\n",
                    "NarSize: 1189320\nReferences: 5wxhjx3ryms32bmx7xmbspid15ygil4g-source\n",
                ),
            ),
            ("hash mode", FLAT.replace("CA: fixed:", "CA: fixed:r:")),
            (
                "git hashing",
                SOURCE.replace("CA: fixed:r:", "CA: fixed:git:"),
            ),
            (
                "CA on input-addressed path",
                format!(
                    "{REAL}CA: text:sha256:1s6jdd4jlmlxk7bgrqwlcvqnb7rzqlisy1b62wz3drj1i7jrfss9\n"
                ),
            ),
        ];
        for (what, text) in cases {
            let n = NarInfo::parse(&text).unwrap();
            assert!(!n.verify(&[]), "{what} should not verify");
        }
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
