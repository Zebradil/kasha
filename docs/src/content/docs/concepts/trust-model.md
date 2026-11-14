---
title: Trust model
description: Who signs what in kasha, what the box accepts, and why the box holds no signing key.
---

kasha separates three things: who may **write** to a box, which paths the box **accepts**, and which paths a host
**trusts**. Only producers sign. The box verifies and passes signatures through unchanged.

## The box never signs

Every path the box serves was signed before it reached the box: by CI, by a developer's machine, or by the cache it
was pulled through from. The box stores pushed and mirrored narinfos byte-identical as received, and keeps every
signature line of a pulled-through one, so the original signatures reach every host that substitutes from it.

The box does hold read/write credentials for the remote cache, to mirror both ways. It deliberately does not hold a
private signing key. It is the most exposed, always-on component; if it is compromised, the worst outcome is a desynced
mirror, never a forged trusted store path. Delete-capable remote credentials stay off the box too: only the remote GC
sweep, run from CI, has them.

Rationale: [ADR-0004](https://github.com/Zebradil/kasha/blob/main/docs/adr/0004-single-signing-key.md).

## What the box accepts

Every narinfo the box stores, whether pushed to it, fetched by mirror-down or pulled through, must pass one of two
checks:

- **Trusted signature.** A `Sig` line verifies (ed25519, over the narinfo fingerprint) against a key in
  `KASHA_TRUSTED_KEYS` with the same key name.
- **Content address.** The narinfo's `CA` field, with its references and name, reproduces its store path. Such a path
  commits to its own content, so it needs no signature; this is the rule Nix itself applies. It covers the unsigned
  `.drv` recipes and sources that CI pushes, since CI signs only what it builds.

Anything else is refused. A push gets `400` with
`neither a trusted signature nor a valid content address on /nix/store/…`; mirror-down skips that source and tries the
next. A malformed or unsupported `CA` fails the check rather than passing it.

Neither rule lets anyone forge an input-addressed output: without a trusted key, the only paths that pass are ones whose
path is derived from their own content.

## What pull-through adds

Reads are unauthenticated, and with [pull-through](../eager-replica/#pull-through-on-a-miss) on a read can make the box
download and store a path. It can only be a path one of the box's sources serves, and it has to pass more than the
check above:

- The narinfo must name the path that was asked for. A source cannot answer a request for one path with another.
- The NAR must decompress to exactly `NarSize` bytes that hash to `NarHash`. A corrupt or truncated download is dropped,
  not stored and served to every later consumer. The box can check `none`, `xz`, `zstd` and `bzip2`; a source that
  offers the path in another encoding is skipped.
- The NAR is stored under a name derived from `NarHash`, never under the name the source gave it, and it never replaces
  a stored object.

The box rewrites four fields of a pulled-through narinfo: `URL`, `Compression`, `FileHash` and `FileSize`. Signatures
cover the store path, `NarHash`, `NarSize` and the references, so they stay valid, and the box still signs nothing. An
upstream whose key is not in `KASHA_TRUSTED_KEYS` can supply content-addressed paths only.

Rationale: [ADR-0011](https://github.com/Zebradil/kasha/blob/main/docs/adr/0011-optional-pull-through.md).

## Writing is a separate gate

The write token (`KASHA_TOKEN`) decides who may push; it proves nothing about content. A holder of the token can upload
objects, but the box still rejects any narinfo that fails the checks above. With no token configured, the box refuses
every write.

## What hosts trust

Hosts verify signatures themselves: the [consumer module](../../guides/consumer-host/) keeps Nix's `require-sigs` on
and adds the remote cache's public key to `trusted-public-keys`. A box cannot make a host accept a path its keys do not
cover.

## A separate key for CI builds of kasha

kasha's own CI publishes its build outputs to the remote cache signed with a CI-only key, `kasha-ci-1`, not the remote
cache's main key. A compromised Actions run can then write objects, but nothing outside CI trusts them until you add
that key. Add it to a box's `KASHA_TRUSTED_KEYS` to let the box mirror kasha's own builds; leave it out and the box skips
them.
