//! Manifest v3: one generation = full build-closure store-path list plus
//! explicit grouping fields. Replaces roots-only v2 (`{outPath, drvPath}`).

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::time::SystemTime;

use crate::narinfo::STORE_DIR;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    pub flake: String,
    #[serde(rename = "gen")]
    pub gen_id: String,
    pub branch: String,
    pub attr: String,
    /// ISO-8601 UTC.
    pub timestamp: String,
    /// Full build closure, absolute store paths.
    pub closure: Vec<String>,
}

impl Manifest {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let m: Manifest = serde_json::from_slice(bytes).context("manifest JSON")?;
        m.validate()?;
        Ok(m)
    }

    pub fn validate(&self) -> Result<()> {
        if self.version != 3 {
            bail!("manifest version {} (want 3)", self.version);
        }
        for f in [
            ("flake", &self.flake),
            ("gen", &self.gen_id),
            ("branch", &self.branch),
            ("attr", &self.attr),
        ] {
            if f.1.is_empty() {
                bail!("manifest missing {}", f.0);
            }
        }
        if self.closure.is_empty() {
            bail!("manifest has empty closure");
        }
        for p in &self.closure {
            if !p.starts_with(STORE_DIR) {
                bail!("closure path outside {STORE_DIR}: {p}");
            }
        }
        self.time()?;
        Ok(())
    }

    pub fn time(&self) -> Result<SystemTime> {
        humantime::parse_rfc3339(&self.timestamp)
            .with_context(|| format!("manifest timestamp {:?}", self.timestamp))
    }
}

/// A manifest's fields minus `closure`, which is nearly all of its bytes.
#[derive(Debug, Deserialize)]
pub struct Head {
    pub version: u32,
    pub flake: String,
    #[serde(rename = "gen")]
    pub gen_id: String,
    pub branch: String,
    pub attr: String,
    pub timestamp: String,
}

impl Head {
    /// Parse from the first bytes of a manifest. `kasha emit` serializes
    /// `closure` last, so a prefix reaching it holds every other field; a
    /// whole object parses in any field order. None when the prefix is not
    /// enough or the fields fail validation; the caller can then retry with
    /// the whole object.
    pub fn from_prefix(bytes: &[u8]) -> Option<Self> {
        const CUT: &[u8] = b",\"closure\"";
        let cut = bytes
            .windows(CUT.len())
            .position(|w| w == CUT)
            .and_then(|i| serde_json::from_slice(&[&bytes[..i], b"}"].concat()).ok());
        cut.or_else(|| serde_json::from_slice::<Head>(bytes).ok())?
            .valid()
    }

    fn valid(self) -> Option<Self> {
        let filled = [&self.flake, &self.gen_id, &self.branch, &self.attr]
            .iter()
            .all(|f| !f.is_empty());
        (self.version == 3 && filled && self.time().is_ok()).then_some(self)
    }

    pub fn time(&self) -> Result<SystemTime> {
        humantime::parse_rfc3339(&self.timestamp)
            .with_context(|| format!("manifest timestamp {:?}", self.timestamp))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Manifest {
        Manifest {
            version: 3,
            flake: "znix".into(),
            gen_id: "main-8a741bd-checks.x86_64-linux.foo".into(),
            branch: "main".into(),
            attr: "checks.x86_64-linux.foo".into(),
            timestamp: "2026-08-20T10:00:00Z".into(),
            closure: vec!["/nix/store/v4f0jj9sz97ckskvacf40llz4nfr19jf-hello-2.12.3".into()],
        }
    }

    #[test]
    fn valid_roundtrip() {
        let m = base();
        let bytes = serde_json::to_vec(&m).unwrap();
        let back = Manifest::parse(&bytes).unwrap();
        assert_eq!(back.gen_id, m.gen_id);
        assert_eq!(back.time().unwrap(), m.time().unwrap());
    }

    #[test]
    fn head_from_prefix() {
        let bytes = serde_json::to_vec(&base()).unwrap();
        let cut = bytes.windows(9).position(|w| w == b"\"closure\"").unwrap() + 15;
        let head = Head::from_prefix(&bytes[..cut]).unwrap();
        assert_eq!(
            (head.branch.as_str(), head.attr.as_str()),
            ("main", base().attr.as_str())
        );
        // Prefix ends before `closure`: not enough to tell, caller retries whole.
        assert!(Head::from_prefix(&bytes[..40]).is_none());
        assert!(Head::from_prefix(&bytes).is_some());
        let mut v2 = base();
        v2.version = 2;
        assert!(Head::from_prefix(&serde_json::to_vec(&v2).unwrap()).is_none());
    }

    #[test]
    fn rejects_v2_and_bad_fields() {
        let mut v2 = base();
        v2.version = 2;
        assert!(v2.validate().is_err());

        let mut empty = base();
        empty.closure.clear();
        assert!(empty.validate().is_err());

        let mut escape = base();
        escape.closure = vec!["/etc/passwd".into()];
        assert!(escape.validate().is_err());

        let mut nobranch = base();
        nobranch.branch = String::new();
        assert!(nobranch.validate().is_err());

        // v2 wire shape (roots, no closure/branch/attr) must not parse.
        assert!(Manifest::parse(
            br#"{"version":2,"flake":"znix","gen":"g","timestamp":"2026-01-01T00:00:00Z","roots":[{"outPath":"/nix/store/x","drvPath":"/nix/store/y.drv"}]}"#
        )
        .is_err());
    }
}
