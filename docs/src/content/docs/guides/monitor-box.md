---
title: Monitor a box
description: Read the box's /status and /metrics endpoints and logs to tell a healthy box from a stuck one.
---

The box reports its state in a JSON endpoint, a Prometheus endpoint and structured logs.

## Read /status

```sh
curl -s http://box.lan:5000/status
```

```json
{"flakes":{},"objects":3,"pending_mirror_up":0,"store_bytes":854474}
```

| Field | Meaning |
| --- | --- |
| `objects` | narinfos in the box's index |
| `store_bytes` | bytes on disk under the store root |
| `pending_mirror_up` | local-origin generations whose mirror-up failed or is waiting on missing paths |
| `flakes.<flake>.last_sync` | end of the last successful mirror-down cycle that covered this flake, RFC 3339 UTC |
| `flakes.<flake>.gaps` | closure paths listed in this flake's manifests that no source could supply |

`flakes` stays empty until the box has a remote cache and has finished a sync cycle with at least one manifest.
`/status` needs no authentication.

## Scrape /metrics

`/metrics` serves the same state in the Prometheus text format (`text/plain; version=0.0.4`), plus a few counters. It
needs no authentication.

| Metric | Type | Meaning |
| --- | --- | --- |
| `kasha_objects` | gauge | narinfos in the box's index |
| `kasha_pending_mirror_up` | gauge | same as `pending_mirror_up` in `/status` |
| `kasha_flake_last_sync_timestamp_seconds{flake}` | gauge | `last_sync` in `/status`, as Unix seconds |
| `kasha_flake_gaps{flake}` | gauge | `gaps` in `/status` |
| `kasha_narinfo_requests_total{result}` | counter | narinfo lookups, `result` is `hit` or `miss` |
| `kasha_ingest_requests_total{result}` | counter | pushes, `result` is `accepted`, `rejected` (bad content or signature) or `unauthorized` |
| `kasha_last_sweep_timestamp_seconds` | gauge | start of the last box GC sweep attempt; absent until the first sweep |
| `kasha_sweep_deleted_objects_total` | counter | narinfos and NARs the box GC deleted |

Counters start at zero when the box restarts. `store_bytes` has no metric: it walks the whole store, which is too
costly to repeat on every scrape; read it from `/status`.

```yaml
scrape_configs:
  - job_name: kasha
    static_configs:
      - targets: ["box.lan:5000"]
```

Alerts that match the checks below:

```yaml
groups:
  - name: kasha
    rules:
      - alert: KashaSyncStale
        expr: time() - kasha_flake_last_sync_timestamp_seconds > 3600
      - alert: KashaMirrorUpStuck
        expr: kasha_pending_mirror_up > 0
        for: 1h
      - alert: KashaSweepStale
        expr: time() - kasha_last_sweep_timestamp_seconds > 2 * 86400
```

The narinfo hit ratio, `rate(kasha_narinfo_requests_total{result="hit"}[1h]) / rate(kasha_narinfo_requests_total[1h])`,
shows how much of the clients' demand the box serves itself.

## Tell healthy from stuck

- **`last_sync` falls behind.** A healthy box updates it every `KASHA_SYNC_INTERVAL` seconds (300 by default), plus
  the cycle's own run time. A stale value means mirror-down is failing; the logs say why (`mirror-down failed`).
- **`gaps` is nonzero.** Usually expected: a manifest can list paths that neither the remote cache nor any upstream
  holds. A missing path is retried at most once an hour, and a gap is never an error.
- **`pending_mirror_up` stays above zero.** A local push is not reaching the remote cache. Look for
  `mirror-up deferred` in the logs: either the push is incomplete on the box, or the remote write failed.

## Read the logs

Logs go to stderr: human-readable on a terminal, one JSON object per line otherwise (for example in a container).
`RUST_LOG` sets the level, `info` by default:

```sh
RUST_LOG=warn kasha serve …
```

Lines worth alerting on:

| Message | Level | Meaning |
| --- | --- | --- |
| `mirror-down failed` | warn | the remote cache could not be listed or read this cycle |
| `mirror-up deferred` | warn | a local-origin generation could not be pushed up yet |
| `box sweep failed` | warn | the in-process GC sweep hit an error |
| `rejected ingest` | warn | a push was refused, e.g. an untrusted signature |
| `unauthorized write` | warn | a write arrived without a valid token |

Each successful cycle logs `synced` with `fetched` and `gaps` counts. On startup the box warns when `KASHA_TOKEN` or
`KASHA_REMOTE` is unset, since writes or mirroring are then off.

:::caution[Unverified]
`/status`, `/metrics`, the log levels and the startup warnings were checked against a local box without a remote cache;
the sync and mirror-up messages were read from the source, not observed. The scrape config and alert rules were not run
against a Prometheus server; `promtool check metrics` accepts the endpoint's output.
:::
