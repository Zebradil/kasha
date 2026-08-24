//! Remote cache access. `Remote` is the seam the mirror/GC logic is tested
//! through; `S3Remote` is the real implementation (R2 or any S3 endpoint),
//! speaking presigned requests via ureq — no AWS SDK.

use anyhow::{Context, Result, bail};
use rusty_s3::actions::{DeleteObjects, DeleteObjectsResponse, ObjectIdentifier};
use rusty_s3::{Bucket, Credentials, S3Action, UrlStyle};
use std::time::{Duration, SystemTime};

/// Concurrent requests a caller may fan out over one remote. Remote sweeps are
/// round-trip-latency bound, not CPU or bandwidth bound, so this sits far above
/// the core count; the connection pool below is sized to match.
pub const FANOUT: usize = 32;

/// Keys per `DeleteObjects` request; the S3 API caps it at 1000.
const DELETE_BATCH: usize = 1000;

pub trait Remote: Send + Sync {
    /// Keys under a prefix with their LastModified stamps.
    fn list(&self, prefix: &str) -> Result<Vec<(String, SystemTime)>>;
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>>;
    /// Streaming variant for large objects (nars).
    fn get_stream(&self, key: &str) -> Result<Option<Box<dyn std::io::Read + '_>>>;
    fn exists(&self, key: &str) -> Result<bool>;
    fn put(&self, key: &str, body: &[u8]) -> Result<()>;
    fn delete(&self, key: &str) -> Result<()>;
    /// Delete many keys. One round trip per key by default; S3 overrides it
    /// with batched `DeleteObjects`.
    fn delete_many(&self, keys: &[String]) -> Result<()> {
        for key in keys {
            self.delete(key)?;
        }
        Ok(())
    }
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
            agent: ureq::Agent::config_builder()
                .max_idle_connections(FANOUT)
                .max_idle_connections_per_host(FANOUT)
                .build()
                .into(),
        })
    }
}

fn status_of(e: &ureq::Error) -> Option<u16> {
    match e {
        ureq::Error::StatusCode(c) => Some(*c),
        _ => None,
    }
}

/// Attempts after the first before a request is given up on.
const RETRIES: u32 = 6;
const BACKOFF_BASE: Duration = Duration::from_millis(500);

/// A sweep of a large bucket fans out enough requests to trip the endpoint's
/// rate limit (R2 answers 429), and a rate limit is not a failure — it is a
/// request to come back later.
fn retryable(e: &ureq::Error) -> bool {
    match e {
        ureq::Error::StatusCode(c) => matches!(c, 429 | 500 | 502 | 503 | 504),
        ureq::Error::Io(_) | ureq::Error::Timeout(_) | ureq::Error::ConnectionFailed => true,
        _ => false,
    }
}

/// Presigned URLs stay valid for `SIGN_TTL`, well past the total backoff, so a
/// retry can replay the same signed request.
fn with_retry<T>(
    what: &str,
    key: &str,
    mut f: impl FnMut() -> Result<T, ureq::Error>,
) -> Result<T, ureq::Error> {
    for attempt in 0..RETRIES {
        match f() {
            Err(e) if retryable(&e) => {
                let wait = BACKOFF_BASE * 2u32.pow(attempt);
                tracing::warn!(
                    what,
                    key,
                    attempt = attempt + 1,
                    wait_ms = wait.as_millis(),
                    error = format!("{e}"),
                    "request failed, retrying"
                );
                std::thread::sleep(wait);
            }
            r => return r,
        }
    }
    f()
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
            let text = with_retry("LIST", prefix, || self.agent.get(url.as_str()).call())
                .context("list")?
                .body_mut()
                .read_to_string()?;
            let parsed =
                rusty_s3::actions::ListObjectsV2::parse_response(&text).context("list XML")?;
            for c in parsed.contents {
                let t =
                    humantime::parse_rfc3339(&c.last_modified).unwrap_or(SystemTime::UNIX_EPOCH);
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
        let url = self
            .bucket
            .get_object(Some(&self.creds), key)
            .sign(SIGN_TTL);
        match with_retry("GET", key, || self.agent.get(url.as_str()).call()) {
            Ok(resp) => Ok(Some(Box::new(resp.into_body().into_reader()))),
            Err(e) if status_of(&e) == Some(404) => Ok(None),
            Err(e) => Err(e).with_context(|| format!("GET {key}")),
        }
    }

    fn exists(&self, key: &str) -> Result<bool> {
        let url = self
            .bucket
            .head_object(Some(&self.creds), key)
            .sign(SIGN_TTL);
        match with_retry("HEAD", key, || self.agent.head(url.as_str()).call()) {
            Ok(_) => Ok(true),
            Err(e) if matches!(status_of(&e), Some(404) | Some(403)) => Ok(false),
            Err(e) => Err(e).with_context(|| format!("HEAD {key}")),
        }
    }

    fn put(&self, key: &str, body: &[u8]) -> Result<()> {
        let url = self
            .bucket
            .put_object(Some(&self.creds), key)
            .sign(SIGN_TTL);
        with_retry("PUT", key, || self.agent.put(url.as_str()).send(body))
            .with_context(|| format!("PUT {key}"))?;
        Ok(())
    }

    fn delete(&self, key: &str) -> Result<()> {
        let url = self
            .bucket
            .delete_object(Some(&self.creds), key)
            .sign(SIGN_TTL);
        with_retry("DELETE", key, || self.agent.delete(url.as_str()).call())
            .with_context(|| format!("DELETE {key}"))?;
        Ok(())
    }

    fn delete_many(&self, keys: &[String]) -> Result<()> {
        for chunk in keys.chunks(DELETE_BATCH) {
            let ids: Vec<ObjectIdentifier> = chunk
                .iter()
                .map(|k| ObjectIdentifier::new(k.clone()))
                .collect();
            let (url, body, md5) = sign_delete_objects(&self.bucket, &self.creds, &ids);
            let text = with_retry("DeleteObjects", &chunk[0], || {
                self.agent
                    .post(&url)
                    .header("content-md5", &md5)
                    .send(body.as_bytes())
            })
            .context("DeleteObjects")?
            .body_mut()
            .read_to_string()?;
            match DeleteObjectsResponse::parse(&text) {
                Ok(resp) if resp.errors.is_empty() => {}
                Ok(resp) => {
                    let e = &resp.errors[0];
                    bail!(
                        "DeleteObjects: {} of {} keys failed, first {}: {} {}",
                        resp.errors.len(),
                        chunk.len(),
                        e.key,
                        e.code,
                        e.message
                    );
                }
                // A 2xx with an unparseable body means the deletes went
                // through but per-key errors can't be read; the sweep is
                // idempotent, so a later run catches whatever survived.
                Err(err) => tracing::warn!(
                    error = format!("{err}"),
                    body = text.chars().take(200).collect::<String>(),
                    "DeleteObjects response unparseable"
                ),
            }
        }
        Ok(())
    }
}

/// Sign one batched `DeleteObjects` POST, returning the presigned URL, the XML
/// body, and its Content-MD5. The body must be known before signing: S3 covers
/// Content-MD5 in the signature, so it is built from a clone and signed after.
/// The header name must be lowercase — rusty-s3 copies it verbatim into
/// `X-Amz-SignedHeaders`, and SigV4 only accepts lowercase names there.
fn sign_delete_objects(
    bucket: &Bucket,
    creds: &Credentials,
    ids: &[ObjectIdentifier],
) -> (String, String, String) {
    let mut action = DeleteObjects::new(bucket, Some(creds), ids.iter());
    let (body, md5) = action.clone().body_with_md5();
    action.headers_mut().insert("content-md5", md5.clone());
    (action.sign(SIGN_TTL).to_string(), body, md5)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn retries_rate_limits_then_succeeds() {
        let calls = Cell::new(0);
        let out = with_retry("DELETE", "x.narinfo", || {
            calls.set(calls.get() + 1);
            if calls.get() < 3 {
                Err(ureq::Error::StatusCode(429))
            } else {
                Ok(calls.get())
            }
        })
        .unwrap();
        assert_eq!((out, calls.get()), (3, 3));
    }

    #[test]
    fn gives_up_on_non_retryable_status() {
        let calls = Cell::new(0);
        let err = with_retry("DELETE", "x.narinfo", || {
            calls.set(calls.get() + 1);
            Err::<(), _>(ureq::Error::StatusCode(403))
        })
        .unwrap_err();
        assert_eq!((status_of(&err), calls.get()), (Some(403), 1));
    }

    #[test]
    fn delete_objects_signs_lowercase_header_name() {
        let bucket = Bucket::new(
            "https://acc.r2.cloudflarestorage.com".parse().unwrap(),
            UrlStyle::Path,
            "b".to_string(),
            "auto".to_string(),
        )
        .unwrap();
        let ids = [ObjectIdentifier::new("a.narinfo".to_string())];
        let (url, _, _) = sign_delete_objects(&bucket, &Credentials::new("k", "s"), &ids);
        assert!(
            url.contains("X-Amz-SignedHeaders=content-md5%3Bhost"),
            "SigV4 rejects uppercase signed header names: {url}"
        );
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
            Ok(self
                .objects
                .lock()
                .unwrap()
                .get(key)
                .map(|(b, _)| b.clone()))
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
