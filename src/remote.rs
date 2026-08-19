//! Remote cache access. `Remote` is the seam the mirror/GC logic is tested
//! through; `S3Remote` is the real implementation (R2 or any S3 endpoint),
//! speaking presigned requests via ureq — no AWS SDK.

use anyhow::{bail, Context, Result};
use rusty_s3::{Bucket, Credentials, S3Action, UrlStyle};
use std::time::{Duration, SystemTime};

pub trait Remote: Send + Sync {
    /// Keys under a prefix with their LastModified stamps.
    fn list(&self, prefix: &str) -> Result<Vec<(String, SystemTime)>>;
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>>;
    /// Streaming variant for large objects (nars).
    fn get_stream(&self, key: &str) -> Result<Option<Box<dyn std::io::Read + '_>>>;
    fn exists(&self, key: &str) -> Result<bool>;
    fn put(&self, key: &str, body: &[u8]) -> Result<()>;
    fn delete(&self, key: &str) -> Result<()>;
}

const SIGN_TTL: Duration = Duration::from_secs(600);

pub struct S3Remote {
    bucket: Bucket,
    creds: Credentials,
    agent: ureq::Agent,
}

impl S3Remote {
    /// Parse a nix-style S3 target: `s3://bucket?endpoint=HOST&region=R`.
    /// Credentials from AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY.
    pub fn from_url(url: &str) -> Result<Self> {
        let rest = url
            .strip_prefix("s3://")
            .with_context(|| format!("remote must be s3://…, got {url}"))?;
        let (name, query) = rest.split_once('?').unwrap_or((rest, ""));
        if name.is_empty() || name.contains('/') {
            bail!("remote bucket must be bare (s3://bucket?…): {url}");
        }
        let mut endpoint = None;
        let mut region = "auto".to_string();
        for p in query.split('&').filter(|p| !p.is_empty()) {
            match p.split_once('=') {
                Some(("endpoint", v)) => {
                    endpoint = Some(if v.contains("://") {
                        v.to_string()
                    } else {
                        format!("https://{v}")
                    })
                }
                Some(("region", v)) => region = v.to_string(),
                _ => {}
            }
        }
        let endpoint = endpoint.context("remote URL needs ?endpoint=…")?;
        let bucket = Bucket::new(
            endpoint.parse().context("endpoint URL")?,
            UrlStyle::Path,
            name.to_string(),
            region,
        )
        .context("bucket")?;
        let key = std::env::var("AWS_ACCESS_KEY_ID").context("AWS_ACCESS_KEY_ID")?;
        let secret = std::env::var("AWS_SECRET_ACCESS_KEY").context("AWS_SECRET_ACCESS_KEY")?;
        Ok(Self {
            bucket,
            creds: Credentials::new(key, secret),
            agent: ureq::Agent::new_with_defaults(),
        })
    }
}

fn status_of(e: &ureq::Error) -> Option<u16> {
    match e {
        ureq::Error::StatusCode(c) => Some(*c),
        _ => None,
    }
}

impl Remote for S3Remote {
    fn list(&self, prefix: &str) -> Result<Vec<(String, SystemTime)>> {
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut action = self.bucket.list_objects_v2(Some(&self.creds));
            action.with_prefix(prefix);
            if let Some(t) = &token {
                action.with_continuation_token(t);
            }
            let url = action.sign(SIGN_TTL);
            let text = self
                .agent
                .get(url.as_str())
                .call()
                .context("list")?
                .body_mut()
                .read_to_string()?;
            let parsed = rusty_s3::actions::ListObjectsV2::parse_response(&text)
                .context("list XML")?;
            for c in parsed.contents {
                let t = humantime::parse_rfc3339(&c.last_modified)
                    .unwrap_or(SystemTime::UNIX_EPOCH);
                out.push((c.key, t));
            }
            token = parsed.next_continuation_token;
            if token.is_none() {
                break;
            }
        }
        Ok(out)
    }

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

    fn get_stream(&self, key: &str) -> Result<Option<Box<dyn std::io::Read + '_>>> {
        let url = self.bucket.get_object(Some(&self.creds), key).sign(SIGN_TTL);
        match self.agent.get(url.as_str()).call() {
            Ok(resp) => Ok(Some(Box::new(resp.into_body().into_reader()))),
            Err(e) if status_of(&e) == Some(404) => Ok(None),
            Err(e) => Err(e).with_context(|| format!("GET {key}")),
        }
    }

    fn exists(&self, key: &str) -> Result<bool> {
        let url = self.bucket.head_object(Some(&self.creds), key).sign(SIGN_TTL);
        match self.agent.head(url.as_str()).call() {
            Ok(_) => Ok(true),
            Err(e) if matches!(status_of(&e), Some(404) | Some(403)) => Ok(false),
            Err(e) => Err(e).with_context(|| format!("HEAD {key}")),
        }
    }

    fn put(&self, key: &str, body: &[u8]) -> Result<()> {
        let url = self.bucket.put_object(Some(&self.creds), key).sign(SIGN_TTL);
        self.agent
            .put(url.as_str())
            .send(body)
            .with_context(|| format!("PUT {key}"))?;
        Ok(())
    }

    fn delete(&self, key: &str) -> Result<()> {
        let url = self
            .bucket
            .delete_object(Some(&self.creds), key)
            .sign(SIGN_TTL);
        self.agent
            .delete(url.as_str())
            .call()
            .with_context(|| format!("DELETE {key}"))?;
        Ok(())
    }
}

/// In-memory remote for tests.
#[cfg(test)]
pub mod fake {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    #[derive(Default)]
    pub struct FakeRemote {
        pub objects: Mutex<BTreeMap<String, (Vec<u8>, SystemTime)>>,
    }

    impl FakeRemote {
        pub fn insert(&self, key: &str, body: &[u8]) {
            self.insert_at(key, body, SystemTime::now());
        }
        pub fn insert_at(&self, key: &str, body: &[u8], t: SystemTime) {
            self.objects
                .lock()
                .unwrap()
                .insert(key.into(), (body.to_vec(), t));
        }
        pub fn keys(&self) -> Vec<String> {
            self.objects.lock().unwrap().keys().cloned().collect()
        }
    }

    impl Remote for FakeRemote {
        fn list(&self, prefix: &str) -> Result<Vec<(String, SystemTime)>> {
            Ok(self
                .objects
                .lock()
                .unwrap()
                .iter()
                .filter(|(k, _)| k.starts_with(prefix))
                .map(|(k, (_, t))| (k.clone(), *t))
                .collect())
        }
        fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
            Ok(self.objects.lock().unwrap().get(key).map(|(b, _)| b.clone()))
        }
        fn get_stream(&self, key: &str) -> Result<Option<Box<dyn std::io::Read + '_>>> {
            Ok(self
                .get(key)?
                .map(|b| Box::new(std::io::Cursor::new(b)) as Box<dyn std::io::Read>))
        }
        fn exists(&self, key: &str) -> Result<bool> {
            Ok(self.objects.lock().unwrap().contains_key(key))
        }
        fn put(&self, key: &str, body: &[u8]) -> Result<()> {
            self.insert(key, body);
            Ok(())
        }
        fn delete(&self, key: &str) -> Result<()> {
            self.objects.lock().unwrap().remove(key);
            Ok(())
        }
    }
}
