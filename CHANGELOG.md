# Changelog

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
