# A source whose narinfo request failed is skipped for a cooldown

Partly supersedes ADR-0011 (its **Failure behaviour**: "every failure below is logged and
not remembered", for a source that is down or slow).

Under ADR-0011 a pull-through lookup asks the sources one after another, and a source that
fails is asked again by the next lookup. A source that is down, or that accepts
connections and never answers, then costs every lookup up to
`KASHA_PULL_THROUGH_LOOKUP_TIMEOUT` (10 s by default) before the sources after it are
asked. Paths a later source holds arrive 10 s late, and each lookup holds one of the
`max_lookups` slots that long: under load the slots fill, and narinfo misses the other
sources could answer are turned away as `busy`.

**Decision.** A source whose narinfo request fails is marked down, and lookups skip it for
a cooldown of 30 s. When the cooldown ends, one lookup asks it again while the others go on
skipping it. Any answer, a `404` included, clears the mark; another failure starts a new
cooldown. A dead source then costs one lookup timeout per cooldown, not one per lookup.

- **What marks a source down.** Only a narinfo request that gets no answer: a connection or
  TLS error, a timeout, an error status other than `404`, or a body that breaks off,
  exceeds 1 MiB or is not UTF-8. A narinfo that does not parse, carries no trusted
  signature, names another path or has an encoding the box cannot verify is about one path
  and leaves the source as it is. A
  failed NAR download does not mark its source down either.
- **A skip is a failure, not a miss.** A lookup that skipped a source counts as `failed`
  and is not remembered as a miss, so a path only the skipped source holds is asked for
  again once the source is back.
- **Order stays.** Sources are still asked in configured order; skipping one moves the
  lookup to the next.
- **The cooldown is fixed**, not an operator setting.

**Consequence.** A single failed request, a blip, takes its source out of lookups for up to
30 s; paths only it holds get `404` meanwhile, and Nix falls back to its next substituter
or builds. `kasha_pull_through_sources_down` counts the sources marked down.

Considered: asking every source at once and taking the first hit in configured order
(rejected: every lookup reaches every upstream, `cache.nixos.org` included, even when the
remote cache has the path; zebradil/sito rejects its `race` strategy for the same reason);
active `GET /nix-cache-info` probes on an interval, as zebradil/sito does (rejected: sito
probes because a roaming laptop changes networks while idle; a box does not, and a trial
lookup after the cooldown answers the same question with no extra thread or requests);
ranking sources by measured latency (rejected: the order is policy, remote cache first,
ADR-0011).
