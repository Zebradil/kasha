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
| `kasha_unlisted_bytes` | gauge | bytes of paths no retained manifest lists (their narinfos, and NARs only they name) after the last eviction pass |
| `kasha_unlisted_paths` | gauge | number of those paths after the last eviction pass |
| `kasha_unlisted_evictions_total` | counter | paths eviction removed |
| `kasha_unlisted_evicted_bytes_total` | counter | bytes of the narinfos and NARs eviction removed |
| `kasha_generation_published_timestamp_seconds{flake,branch}` | gauge | `timestamp` of the newest manifest on the box for the branch: when its producer last published |
| `kasha_generation_arrived_timestamp_seconds{flake,branch}` | gauge | when the box last stored a new manifest for the branch (the manifest file's mtime, so it survives restarts) |
| `kasha_nar_bytes_total{direction,source}` | counter | NAR bytes moved: `down` (mirror-down, `source` is `remote` or `upstream`), `up` (mirror-up), `served` (GETs by clients), `ingest` (pushes) or `pull` (pull-through, `source` as for `down`; bytes as the source sent them, before re-encoding) |
| `kasha_nar_age_objects{le}` | gauge | NARs on disk stored (with `KASHA_UNLISTED_MAX_BYTES` set: last used) at most `le` seconds ago, cumulative like a histogram |
| `kasha_nar_age_bytes{le}` | gauge | bytes of those NARs |
| `kasha_nar_oldest_timestamp_seconds`, `kasha_nar_newest_timestamp_seconds` | gauge | mtime of the oldest and newest NAR on disk; absent on an empty store |
| `kasha_pull_through_lookups_total{result}` | counter | narinfo misses handled by pull-through: `found` (a source had the path), `miss` (no source had it), `negative` (answered from a remembered miss), `failed` (a source could not answer), `capped` (over `KASHA_PULL_THROUGH_MAX_PENDING`) or `busy` (a quarter of `KASHA_MAX_INFLIGHT` narinfo requests already wait on a lookup) |
| `kasha_pull_through_nar_waits_total{result}` | counter | NAR requests for a download in flight: `waited` (held until it ended) or `busy` (turned away with `404`: a quarter of `KASHA_MAX_INFLIGHT` requests already wait) |
| `kasha_pull_through_fetches_total{result}` | counter | background fetches ended: `stored`, `failed` or `abandoned` (a push or mirror-down stored the path first) |
| `kasha_pull_through_pending` | gauge | paths answered whose NAR is not stored yet |
| `kasha_pull_through_queued` | gauge | pending paths whose download has not started |

Counters start at zero when the box restarts. `store_bytes` has no metric: it walks the whole store, which is too
costly to repeat on every scrape; read it from `/status`. The NAR age metrics read only `nar/` and are rescanned at most
every 5 minutes. Byte counters move when a transfer completes, so one large NAR shows as a spike rather than a plateau.

The `kasha_pull_through_*` metrics and the `pull` byte counters exist only on a box with pull-through on. A narinfo
answered by pull-through counts as a `hit` in `kasha_narinfo_requests_total`. A `kasha_pull_through_queued` that stays
high means the downloads cannot keep up: raise `KASHA_PULL_THROUGH_WORKERS`. A rising `failed` count is explained in
the logs (`narinfo lookup failed`, `pull-through fetch failed`). A rising `busy` count in either metric means consumers
are being sent away: raise `KASHA_MAX_INFLIGHT`.

The `kasha_unlisted_*` metrics exist only with `KASHA_UNLISTED_MAX_BYTES` set. `kasha_unlisted_bytes` sits at or below
the cap after a pass; above it, paths used in the last 10 minutes alone exceed the cap, which no pass can fix: raise the
cap. A steady rise of `kasha_unlisted_evictions_total` is the cap working; a steep one, with clients pulling the same
paths through again, means the cap is smaller than their working set.

The generation metrics and the `down`/`up` byte counters, like `last_sync`, need a remote cache: they come from the sync
cycle. Last sync says the box checked; the generation timestamps say whether there was anything new to fetch.

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

A Grafana dashboard over these metrics ships in the repository as
[`deploy/grafana/kasha.json`](https://github.com/Zebradil/kasha/blob/main/deploy/grafana/kasha.json). It picks the
Prometheus data source and the scrape `job` from dashboard variables, and its thresholds match the alerts above.

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
| `eviction pass failed`, `eviction failed` | warn | an eviction pass, or the removal of one file in it, hit an error; the next pass retries |
| `rejected ingest` | warn | a push was refused, e.g. an untrusted signature |
| `unauthorized write` | warn | a write arrived without a valid token |

Each successful cycle logs `synced` with `fetched` and `gaps` counts. On startup the box warns when `KASHA_TOKEN` or
`KASHA_REMOTE` is unset, since writes or mirroring are then off.

:::caution[Unverified]
`/status`, `/metrics`, the log levels and the startup warnings were checked against a local box without a remote cache;
the sync and mirror-up messages were read from the source, not observed. The scrape config and alert rules were not run
against a Prometheus server; `promtool check metrics` accepts the endpoint's output.
:::
