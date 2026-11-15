# Changelog

## [0.13.0](https://github.com/Zebradil/kasha/compare/v0.12.2...v0.13.0) (2026-11-15)


### Features

* **metrics:** count NARs by encoding on disk and by arrival route ([1f6ff68](https://github.com/Zebradil/kasha/commit/1f6ff68e26b71ec2eaf1b67655954f17aa0ca4bd))
* **reencode:** re-encode stored non-zstd NARs as zstd ([5bee1fe](https://github.com/Zebradil/kasha/commit/5bee1feff6cfdb5fc722e332fcaf1f4c43ac5328)), closes [#126](https://github.com/Zebradil/kasha/issues/126)


### Fixes

* **ci:** run the NixOS VM test on x86_64-linux only ([#131](https://github.com/Zebradil/kasha/issues/131)) ([ef17288](https://github.com/Zebradil/kasha/commit/ef17288da455d1d61a1fa30118e30b7c19250a63))
* **reencode:** keep the old NAR a full grace window after the swap ([36de7e1](https://github.com/Zebradil/kasha/commit/36de7e1a6d0f50674b9d713e318888b411418e1a)), closes [#126](https://github.com/Zebradil/kasha/issues/126)


### Documentation

* record the re-encoder in ADR-0015 and the user docs ([62c2a94](https://github.com/Zebradil/kasha/commit/62c2a9472cd5399ec37af99e6179b478064cc872)), closes [#126](https://github.com/Zebradil/kasha/issues/126)

## [0.12.2](https://github.com/Zebradil/kasha/compare/v0.12.1...v0.12.2) (2026-11-15)


### Fixes

* **grafana:** cap the failure-ratio axis at 100 % ([71b5935](https://github.com/Zebradil/kasha/commit/71b59350fede64db16ae9dea7211e7a82504a730))


### Documentation

* push to the box with zstd, not Nix's xz default ([#125](https://github.com/Zebradil/kasha/issues/125)) ([d31de25](https://github.com/Zebradil/kasha/commit/d31de2579cb0dfe1f6fde30b417be04744983c1f))

## [0.12.1](https://github.com/Zebradil/kasha/compare/v0.12.0...v0.12.1) (2026-11-15)


### Fixes

* **grafana:** readable age buckets, box GC under the remote row, steadier ratios ([f4ca740](https://github.com/Zebradil/kasha/commit/f4ca740bd82d01937767359976f8d0cd5815cc6f))

## [0.12.0](https://github.com/Zebradil/kasha/compare/v0.11.0...v0.12.0) (2026-11-15)


### Features

* **grafana:** dashboard for several boxes, pull-through and the cap ([d7af35b](https://github.com/Zebradil/kasha/commit/d7af35b1cac8c422752e620df433638d8e424727))
* **metrics:** export the unlisted cap as kasha_unlisted_max_bytes ([db128af](https://github.com/Zebradil/kasha/commit/db128afd25ad46c8a7509665239f6d589d218fb1))

## [0.11.0](https://github.com/Zebradil/kasha/compare/v0.10.0...v0.11.0) (2026-11-15)


### Features

* never expire release-tag generations (ADR-0014) ([#118](https://github.com/Zebradil/kasha/issues/118)) ([166e4be](https://github.com/Zebradil/kasha/commit/166e4bef37aeb0d22d324fdffefa1ba6b9a472fb))

## [0.10.0](https://github.com/Zebradil/kasha/compare/v0.9.0...v0.10.0) (2026-11-15)


### Features

* evict unlisted paths least recently used first under a size cap (ADR-0013) ([#117](https://github.com/Zebradil/kasha/issues/117)) ([75bbeec](https://github.com/Zebradil/kasha/commit/75bbeec11dada1e0573fdf1b6b67de95df2a8b2a))
* pull-through on a narinfo miss (ADR-0011) ([#114](https://github.com/Zebradil/kasha/issues/114)) ([d25f586](https://github.com/Zebradil/kasha/commit/d25f58678995497414aa44e23f5999193d8be609))
* **pull:** store pulled-through NARs as zstd (ADR-0012) ([#115](https://github.com/Zebradil/kasha/issues/115)) ([b6e2431](https://github.com/Zebradil/kasha/commit/b6e2431837576ab8476a2ef97104952c961ef2a5))


### Fixes

* **deps:** update rust crate rusty-s3 to 0.11.0 ([#98](https://github.com/Zebradil/kasha/issues/98)) ([38a7a19](https://github.com/Zebradil/kasha/commit/38a7a19a63d97664341450c30138dbe6e8aef2d9))
* **remote:** accept ListBucketResult in GCS's XML namespace ([#112](https://github.com/Zebradil/kasha/issues/112)) ([7386ea3](https://github.com/Zebradil/kasha/commit/7386ea3a22f098691bb4c6fad90eecd50223674b))


### Documentation

* **adr:** record optional pull-through mode (ADR-0011) ([#111](https://github.com/Zebradil/kasha/issues/111)) ([dee29c5](https://github.com/Zebradil/kasha/commit/dee29c5e2eb48ec56b0d713b56b161c9a45579da))
* **agents:** add deferred triage label ([8350f00](https://github.com/Zebradil/kasha/commit/8350f002a698811a3e469e76f6ef673f75c4594c))
* **agents:** refresh skill config, rename CONTEXT.md to GLOSSARY.md ([56e98db](https://github.com/Zebradil/kasha/commit/56e98db27c51be1df3e2f268abed5f54bcf1445c))
* count docs site visitors with Umami ([#103](https://github.com/Zebradil/kasha/issues/103)) ([7a4548e](https://github.com/Zebradil/kasha/commit/7a4548e400ad9f31c5cbf7ab2d140620b485fdd8))
* install docs-kit from npm ([#106](https://github.com/Zebradil/kasha/issues/106)) ([0ba6c93](https://github.com/Zebradil/kasha/commit/0ba6c93ff3b98c25c6383671a884f0b9ff19514e))
* publish docs of past releases with docs-kit 0.11.0 ([#113](https://github.com/Zebradil/kasha/issues/113)) ([3f7ba96](https://github.com/Zebradil/kasha/commit/3f7ba96d070336c26ef1e86d8673adf58cb75997))

## [0.9.0](https://github.com/Zebradil/kasha/compare/v0.8.0...v0.9.0) (2026-10-09)


### Features

* **gc:** expire stale retention groups, add kasha ls, parallel manifest reads ([#86](https://github.com/Zebradil/kasha/issues/86)) ([4817194](https://github.com/Zebradil/kasha/commit/4817194871baf4daffb280a431946fefc8165094))

## [0.8.0](https://github.com/Zebradil/kasha/compare/v0.7.0...v0.8.0) (2026-10-06)


### Features

* metrics and dashboard panels for generation freshness, NAR throughput and NAR age ([#83](https://github.com/Zebradil/kasha/issues/83)) ([978f434](https://github.com/Zebradil/kasha/commit/978f434bf0b1c9472cb47840c23af77c54314a63))

## [0.7.0](https://github.com/Zebradil/kasha/compare/v0.6.0...v0.7.0) (2026-10-05)


### Features

* ship a Grafana dashboard for the box metrics ([#82](https://github.com/Zebradil/kasha/issues/82)) ([55b62bc](https://github.com/Zebradil/kasha/commit/55b62bc7ebcf61414dc8b6e5abcef19f0b9f988c))


### Fixes

* **deps:** update docs-kit to v0.5.0 ([#81](https://github.com/Zebradil/kasha/issues/81)) ([75913cb](https://github.com/Zebradil/kasha/commit/75913cb8226e2ca31917c5a290c399a81f70a7ec))


### Documentation

* fill the new landing layout fields ([#77](https://github.com/Zebradil/kasha/issues/77)) ([e28a63e](https://github.com/Zebradil/kasha/commit/e28a63e3a981e2ad313980833ae249bf03690773))

## [0.6.0](https://github.com/Zebradil/kasha/compare/v0.5.0...v0.6.0) (2026-10-02)


### Features

* add `kasha sweep` for an on-demand box GC sweep ([#70](https://github.com/Zebradil/kasha/issues/70)) ([c56aae6](https://github.com/Zebradil/kasha/commit/c56aae6b4e3bd1c8d0b96c8b376640d5ae851f4d))
* **emit-manifest:** file pull request generations under one pr branch ([#75](https://github.com/Zebradil/kasha/issues/75)) ([bed6589](https://github.com/Zebradil/kasha/commit/bed6589bab15a55a76686df6416eda3b49f096ef))

## [0.5.0](https://github.com/Zebradil/kasha/compare/v0.4.0...v0.5.0) (2026-09-30)


### Features

* add --version flag and verify docs install commands ([#71](https://github.com/Zebradil/kasha/issues/71)) ([7298edd](https://github.com/Zebradil/kasha/commit/7298eddb733e06311ad26eb7b61eddf5cf56db09))
* serve Prometheus metrics at /metrics ([#69](https://github.com/Zebradil/kasha/issues/69)) ([a1a9854](https://github.com/Zebradil/kasha/commit/a1a98544076feefd461631fec4084fab830db0c4))


### Documentation

* bootstrap docs-kit site and close docs audit gaps ([#60](https://github.com/Zebradil/kasha/issues/60)) ([9b012b8](https://github.com/Zebradil/kasha/commit/9b012b8c698cc4c75d3e8620c0a0805a2a534bfe))
* new site design with porridge theme and stir logo ([#72](https://github.com/Zebradil/kasha/issues/72)) ([80985bc](https://github.com/Zebradil/kasha/commit/80985bc6e0826ecc3371946f6f21793a177c326a))
* prune todo.md to open investigations ([#68](https://github.com/Zebradil/kasha/issues/68)) ([906cd30](https://github.com/Zebradil/kasha/commit/906cd308e21ac9e86748503140680a9805ef9695))

## [0.4.0](https://github.com/Zebradil/kasha/compare/v0.3.1...v0.4.0) (2026-09-14)


### Features

* name signer keys when rejecting a narinfo ([f0be64a](https://github.com/Zebradil/kasha/commit/f0be64a19740bbe37e2f109bce4ef046af219947))

## [0.3.1](https://github.com/Zebradil/kasha/compare/v0.3.0...v0.3.1) (2026-09-14)


### Fixes

* accept content-addressed narinfos without a signature ([482dd1f](https://github.com/Zebradil/kasha/commit/482dd1f468c085ae4a87a2bf7aadc11a64aac6bb))
* bound box memory and stop sweep crash loop ([d7d7484](https://github.com/Zebradil/kasha/commit/d7d74840dc4c2624086ae4b2d90b445143121baf))

## [0.3.0](https://github.com/Zebradil/kasha/compare/v0.2.0...v0.3.0) (2026-09-06)


### Features

* push with zstd, read the cache URL from a variable ([#42](https://github.com/Zebradil/kasha/issues/42)) ([b5aa903](https://github.com/Zebradil/kasha/commit/b5aa903a2be8b1d1f34fe4669c21d20a3ffabb9f))

## [0.2.0](https://github.com/Zebradil/kasha/compare/v0.1.0...v0.2.0) (2026-09-05)


### Features

* publish a per-platform kasha binary consumers can substitute ([#37](https://github.com/Zebradil/kasha/issues/37)) ([5b15330](https://github.com/Zebradil/kasha/commit/5b153300f6f22e4929eb4a63b4550a43fc5e0f86))
