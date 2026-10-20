# Changelog

## [0.5.2](https://github.com/fliks-app/engine/compare/v0.5.1...v0.5.2) (2026-10-20)


### Miscellaneous Chores

* release 0.5.2 ([#73](https://github.com/fliks-app/engine/issues/73)) ([62f92d0](https://github.com/fliks-app/engine/commit/62f92d0b5311e7cea166d12be736c388d7a80707))

## [0.5.1](https://github.com/fliks-app/engine/compare/v0.5.0...v0.5.1) (2026-10-20)


### Bug Fixes

* **cache:** rank eviction by when an entry was served, not written ([#62](https://github.com/fliks-app/engine/issues/62)) ([af984ce](https://github.com/fliks-app/engine/commit/af984ce8d59004b69c0cdeaaabb0ea2ac5f2d017))
* **engine:** bound ffmpeg stderr reads, map the host path once, trim axum features ([#65](https://github.com/fliks-app/engine/issues/65)) ([4f84626](https://github.com/fliks-app/engine/commit/4f846262a3e90f4c2722bc17e3bc4a9419527ee8))
* **engine:** bound the resolver, init-track and lock caches ([#58](https://github.com/fliks-app/engine/issues/58)) ([ad9a90a](https://github.com/fliks-app/engine/commit/ad9a90ac4194523f986ea8242aac50de7a1038f5))
* **engine:** keep serving after a panic under a lock ([#66](https://github.com/fliks-app/engine/issues/66)) ([a1f1181](https://github.com/fliks-app/engine/commit/a1f1181720743aa0597ffb0f541bf3fca34a3fae))
* **serve:** bound on-demand ffmpeg runs and extract an embedded subtitle once ([#57](https://github.com/fliks-app/engine/issues/57)) ([36ec337](https://github.com/fliks-app/engine/commit/36ec337be2c22a3753980b075040424d1b598c1c))


### Performance Improvements

* **jobs:** index the job queue instead of scanning it ([#59](https://github.com/fliks-app/engine/issues/59)) ([b4e0723](https://github.com/fliks-app/engine/commit/b4e07233d96fff793161fde8410858c24fe14fde))
* **runtime:** list a run's directories in one blocking hop ([#60](https://github.com/fliks-app/engine/issues/60)) ([1073d12](https://github.com/fliks-app/engine/commit/1073d1298aac52e6041e233686ad9b266005afe0))
* **runtime:** read remux progress and the config without blocking ([#61](https://github.com/fliks-app/engine/issues/61)) ([1878656](https://github.com/fliks-app/engine/commit/18786568ef8e489afd8cc84857d1e7e606cc5314))
* **serve:** stop copying segments up to three times per request ([#55](https://github.com/fliks-app/engine/issues/55)) ([5cd9639](https://github.com/fliks-app/engine/commit/5cd9639521dd044545d88e16d12f5e0c23d04fe5))

## [0.5.0](https://github.com/fliks-app/engine/compare/v0.4.1...v0.5.0) (2026-10-18)


### Features

* **probe:** report every track disposition the player can mark ([#49](https://github.com/fliks-app/engine/issues/49)) ([03cb443](https://github.com/fliks-app/engine/commit/03cb443f03084f3e04b1e490aaa1bcc3ddef7810))

## [0.4.1](https://github.com/fliks-app/engine/compare/v0.4.0...v0.4.1) (2026-10-17)


### Bug Fixes

* **live:** a cancelled open no longer blocks its channel ([#47](https://github.com/fliks-app/engine/issues/47)) ([a1ece5c](https://github.com/fliks-app/engine/commit/a1ece5c6fc3a256223686c36ce4c492b81c683a9))

## [0.4.0](https://github.com/fliks-app/engine/compare/v0.3.2...v0.4.0) (2026-10-17)


### Features

* **live:** capacity veto per upstream, owner-only viewer denial ([#44](https://github.com/fliks-app/engine/issues/44)) ([805f908](https://github.com/fliks-app/engine/commit/805f908520dcc90aef4a271dafe4fa8d7fa87b1b))

## [0.3.2](https://github.com/fliks-app/engine/compare/v0.3.1...v0.3.2) (2026-10-15)


### Bug Fixes

* **supervise:** kill the host when its supervisor dies ([#40](https://github.com/fliks-app/engine/issues/40)) ([30e6be5](https://github.com/fliks-app/engine/commit/30e6be52e6e5b4587bafa50a905788e2c664ff5a))

## [0.3.1](https://github.com/fliks-app/engine/compare/v0.3.0...v0.3.1) (2026-10-15)


### Bug Fixes

* **release:** bump Cargo.lock with the workspace version ([#38](https://github.com/fliks-app/engine/issues/38)) ([88776ce](https://github.com/fliks-app/engine/commit/88776ce8fa7cd2935bec14f1c26fca361bdab8f3))

## [0.3.0](https://github.com/fliks-app/engine/compare/v0.2.0...v0.3.0) (2026-10-15)


### Features

* **core:** port hls playlists and mp4 segment packaging ([#16](https://github.com/fliks-app/engine/issues/16)) ([1e9dbb3](https://github.com/fliks-app/engine/commit/1e9dbb33c547aa6262b703377d48ed0f3c44fcb9))
* **core:** port session logic (seek matrix, throttle, cache, lifetimes) ([#12](https://github.com/fliks-app/engine/issues/12)) ([9367f33](https://github.com/fliks-app/engine/commit/9367f33e0c7f63760b410c97d46d67110ce4731e))
* **core:** port the ffmpeg argv builders as core::args ([#14](https://github.com/fliks-app/engine/issues/14)) ([739731e](https://github.com/fliks-app/engine/commit/739731edc04a72585b75814c00caa54ec5cb9d94))
* **core:** pure scan, fingerprint and subtitle extraction for background jobs ([#27](https://github.com/fliks-app/engine/issues/27)) ([31cbfad](https://github.com/fliks-app/engine/commit/31cbfad1d31c5d600678a79e70103ced2efa3937))
* **engine:** add the front-door gateway role (--upstream) ([#18](https://github.com/fliks-app/engine/issues/18)) ([e7736f0](https://github.com/fliks-app/engine/commit/e7736f0f3b5f34097a2401ab20febe472e773299))
* **engine:** background jobs on one ffmpeg budget, sprite job ([#28](https://github.com/fliks-app/engine/issues/28)) ([97bc8b5](https://github.com/fliks-app/engine/commit/97bc8b55899d856e05bf81deca5b1e74714e2229))
* **engine:** land intro matching and the subtitle jobs ([d968a11](https://github.com/fliks-app/engine/commit/d968a11858af6b98c694196b59d4e64c3d0f5992))
* **engine:** run a remote engine behind the nas gateway ([#37](https://github.com/fliks-app/engine/issues/37)) ([7398859](https://github.com/fliks-app/engine/commit/73988596a436bda9b9a892d8380093a357e4f69b))
* **engine:** session runtime, data plane, resolver client and events ([#19](https://github.com/fliks-app/engine/issues/19)) ([54054de](https://github.com/fliks-app/engine/commit/54054deb78d05aac1723112f79415c067b4f895c))
* **jobs:** demux MPEG-TS in rust for the scan's AAC walk ([#30](https://github.com/fliks-app/engine/issues/30)) ([50fd477](https://github.com/fliks-app/engine/commit/50fd4777fe3f7ec238221366aab2ad14bf43b38d))
* **jobs:** scan, fingerprint and subtitle extraction jobs ([#29](https://github.com/fliks-app/engine/issues/29)) ([dec9f49](https://github.com/fliks-app/engine/commit/dec9f492417a3f2721bedbc8d71bc543fa7413c6))
* **live:** expose the feeding upstream to hosts ([#33](https://github.com/fliks-app/engine/issues/33)) ([20148ff](https://github.com/fliks-app/engine/commit/20148ff2c2cd1d96eb8b2126187782674f200e4a))
* **live:** run live tv sessions in the engine ([#32](https://github.com/fliks-app/engine/issues/32)) ([7b17218](https://github.com/fliks-app/engine/commit/7b17218de512298e3e12c5ca413e75555f3d071b))
* **runtime:** remux runs, early companion and dolby vision codecs ([#20](https://github.com/fliks-app/engine/issues/20)) ([c6ba76c](https://github.com/fliks-app/engine/commit/c6ba76c285a2ff973ffe12ea6bb0e9b0e0a14520))


### Bug Fixes

* **api:** keep the lifetime config optional on the wire ([#15](https://github.com/fliks-app/engine/issues/15)) ([f543e0c](https://github.com/fliks-app/engine/commit/f543e0c63e04eafe9a594f3f9fccc90da1e50924))
* **api:** unique operationIds, deflake watchdog tests ([#31](https://github.com/fliks-app/engine/issues/31)) ([b8c823b](https://github.com/fliks-app/engine/commit/b8c823b0fbfdb046f2029c2c1bfe76129334e542))
* **runtime:** stop a session's jobs when it ends and kill hung ffmpeg runs ([#23](https://github.com/fliks-app/engine/issues/23)) ([1f61e62](https://github.com/fliks-app/engine/commit/1f61e621b8017783fcdae3e915021936c1ecb603))
* **serve:** answer 410 for an ended sid before any resolver call ([#26](https://github.com/fliks-app/engine/issues/26)) ([35cd8fc](https://github.com/fliks-app/engine/commit/35cd8fc729a94949790c8b7654efaa5682f5c831))

## [0.2.0](https://github.com/fliks-app/engine/compare/v0.1.0...v0.2.0) (2026-10-14)


### Features

* **api:** expose the detected hostCaps on /v1/capabilities ([#11](https://github.com/fliks-app/engine/issues/11)) ([0348692](https://github.com/fliks-app/engine/commit/03486927c223f90768444d2a4c9e6970352a1c67))
* **core:** port the playback decision and size estimates ([#4](https://github.com/fliks-app/engine/issues/4)) ([9f94201](https://github.com/fliks-app/engine/commit/9f94201fd81aed4a8cc7deb6ea18f5eda462d50c))
* **hw:** detect host capabilities in the engine ([#6](https://github.com/fliks-app/engine/issues/6)) ([7a64407](https://github.com/fliks-app/engine/commit/7a644073ae30d28f55b369fa957924336502c460))
* **probe:** decode crop samples on the gpu with a software fallback ([#8](https://github.com/fliks-app/engine/issues/8)) ([6e3bdca](https://github.com/fliks-app/engine/commit/6e3bdca1e0e0781e37ff52877958beffbf77e748))
* **probe:** port media probing to the engine ([#7](https://github.com/fliks-app/engine/issues/7)) ([2b02adc](https://github.com/fliks-app/engine/commit/2b02adca51bb815cbdd1d62cfac4213d69200504))

## 0.1.0 (2026-10-14)


### Features

* **api:** add wire types with media info round-trip ([32a8fd4](https://github.com/fliks-app/engine/commit/32a8fd486cf5f519d55fb959c3fdd8b8c89580f3))
* **engine:** add control plane, supervisor and openapi generator ([5bb60f1](https://github.com/fliks-app/engine/commit/5bb60f1d4c81cf94b740cc8e9babc82b4cd7297b))
* **supervise:** kill the child tree with the engine on windows ([#2](https://github.com/fliks-app/engine/issues/2)) ([69678fd](https://github.com/fliks-app/engine/commit/69678fdc751e65dd460c3813034cb0611961b388))
