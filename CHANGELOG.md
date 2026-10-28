# Changelog

## [1.7.3](https://github.com/fliks-app/engine/compare/v1.7.2...v1.7.3) (2026-10-28)


### Bug Fixes

* **decide:** direct play a sample entry only for a client listing it ([#144](https://github.com/fliks-app/engine/issues/144)) ([91632db](https://github.com/fliks-app/engine/commit/91632db5566eaf0de43922ab70dd4698c77dd301))
* **runtime:** ask the host to retry an open that arrives before its config ([#142](https://github.com/fliks-app/engine/issues/142)) ([d3a08fa](https://github.com/fliks-app/engine/commit/d3a08faf8755614ef7e5098686f101ffcb864831))

## [1.7.2](https://github.com/fliks-app/engine/compare/v1.7.1...v1.7.2) (2026-10-28)


### Bug Fixes

* **remux:** scan an unscanned source before freezing its keyframe grid ([#140](https://github.com/fliks-app/engine/issues/140)) ([bcd9c91](https://github.com/fliks-app/engine/commit/bcd9c91efa6beba67e89c2215ee148353f024dd1))

## [1.7.1](https://github.com/fliks-app/engine/compare/v1.7.0...v1.7.1) (2026-10-26)


### Bug Fixes

* **decide:** serve the source when a requested eco rung saves nothing ([#138](https://github.com/fliks-app/engine/issues/138)) ([499701d](https://github.com/fliks-app/engine/commit/499701d384db04c4aedafa37bf4773a276da0f71))

## [1.7.0](https://github.com/fliks-app/engine/compare/v1.6.1...v1.7.0) (2026-10-26)


### Features

* **decide:** transcode interlaced sources ([#134](https://github.com/fliks-app/engine/issues/134)) ([d111af5](https://github.com/fliks-app/engine/commit/d111af50f783507e5d51b2017e0cda58a8419b5d))

## [1.6.1](https://github.com/fliks-app/engine/compare/v1.6.0...v1.6.1) (2026-10-25)


### Bug Fixes

* **session:** hash the burn-in subtitle into the profile hash ([#128](https://github.com/fliks-app/engine/issues/128)) ([a758a29](https://github.com/fliks-app/engine/commit/a758a29ed2d9e83c0fb13eec2aba62ed5918b72e))

## [1.6.0](https://github.com/fliks-app/engine/compare/v1.5.0...v1.6.0) (2026-10-25)


### Features

* **args:** burn in an external image subtitle file ([#126](https://github.com/fliks-app/engine/issues/126)) ([ee3a858](https://github.com/fliks-app/engine/commit/ee3a85876ede8c432b13ae35b9b6795af5701f2b))

## [1.5.0](https://github.com/fliks-app/engine/compare/v1.4.0...v1.5.0) (2026-10-25)


### Features

* **args:** deinterlace on the GPU when the backend has a deinterlacer ([#125](https://github.com/fliks-app/engine/issues/125)) ([b6f4869](https://github.com/fliks-app/engine/commit/b6f486932b689ed09b0732a0a971ac98514848d4))
* **hw:** decode VC-1 on the GPU ([#124](https://github.com/fliks-app/engine/issues/124)) ([79655f2](https://github.com/fliks-app/engine/commit/79655f2fafca89fc3877361b889306cd9299825d))
* **hw:** decode VP9 and MPEG-2 on the GPU ([#121](https://github.com/fliks-app/engine/issues/121)) ([8e29264](https://github.com/fliks-app/engine/commit/8e29264609155e9bb46c30207d7f184eb81e6aef))

## [1.4.0](https://github.com/fliks-app/engine/compare/v1.3.0...v1.4.0) (2026-10-25)


### Features

* **sessions:** let an open retire the session it replaces ([#118](https://github.com/fliks-app/engine/issues/118)) ([4f06d9a](https://github.com/fliks-app/engine/commit/4f06d9a3e778b48f9a2f4fd8ecd2a4eaf423538a))

## [1.3.0](https://github.com/fliks-app/engine/compare/v1.2.0...v1.3.0) (2026-10-25)


### Features

* **remux:** copy the video into MPEG-TS for TS clients ([#119](https://github.com/fliks-app/engine/issues/119)) ([085c443](https://github.com/fliks-app/engine/commit/085c443002e371bf2753c004f1c2bcb9eb033f09))


### Bug Fixes

* **audio:** limit the unnormalised stereo downmix at full scale ([#115](https://github.com/fliks-app/engine/issues/115)) ([45284eb](https://github.com/fliks-app/engine/commit/45284eb0234469d79bb90a020b267aaf134181a5))

## [1.2.0](https://github.com/fliks-app/engine/compare/v1.1.1...v1.2.0) (2026-10-25)


### Features

* **auth:** resolve the control token from env, then a conf file ([#114](https://github.com/fliks-app/engine/issues/114)) ([af7a872](https://github.com/fliks-app/engine/commit/af7a872d969e8ed1a5c3a562340b3541fedebf3b))


### Bug Fixes

* **runtime:** retry the gpu on every run instead of remembering a failure ([#110](https://github.com/fliks-app/engine/issues/110)) ([3b93210](https://github.com/fliks-app/engine/commit/3b93210c8e14d69344e72c9edbf27f1eba9bf3c4))
* **serve:** mark thumbnail sprites private in the cache header ([#113](https://github.com/fliks-app/engine/issues/113)) ([b043acb](https://github.com/fliks-app/engine/commit/b043acb06771b407c7eaca2ea88980e7235ecad7))

## [1.1.1](https://github.com/fliks-app/engine/compare/v1.1.0...v1.1.1) (2026-10-25)


### Bug Fixes

* **live:** refuse internal addresses for vetted upstreams ([#108](https://github.com/fliks-app/engine/issues/108)) ([dd579e0](https://github.com/fliks-app/engine/commit/dd579e06a5f6600f5066d2fa7b501767c7c940a9))

## [1.1.0](https://github.com/fliks-app/engine/compare/v1.0.2...v1.1.0) (2026-10-25)


### Features

* **args:** deinterlace interlaced sources when transcoding ([c5dc6c3](https://github.com/fliks-app/engine/commit/c5dc6c3edec52a18a8ed579a092beb8bccd785f8))


### Bug Fixes

* **args:** keep dialogue level when downmixing to stereo ([9dfa941](https://github.com/fliks-app/engine/commit/9dfa941a8afd17852382cccd8447404b7b50c461))
* **events:** close the stream of a subscriber that lagged ([e8657b3](https://github.com/fliks-app/engine/commit/e8657b3e44a64312fcb6851640bfd20eff1fe9e3))
* **runtime:** refuse a playback run below the free-space floor ([b775f6b](https://github.com/fliks-app/engine/commit/b775f6b09d82e3f814a7a7606a68c3c2488160e1))
* **runtime:** remember a hardware encoder failure for the whole process ([#105](https://github.com/fliks-app/engine/issues/105)) ([be274da](https://github.com/fliks-app/engine/commit/be274dab7a307309ee67f4e3de8b123909d2e586))


### Performance Improvements

* **runtime:** kill ffmpeg at once on seek and switch, wake a throttled run for a waiter ([#93](https://github.com/fliks-app/engine/issues/93)) ([78c5853](https://github.com/fliks-app/engine/commit/78c5853c5319f159582d7614eac826535fd8a9c5))
* **runtime:** no early companion for a session that starts at the beginning ([#107](https://github.com/fliks-app/engine/issues/107)) ([1f1e123](https://github.com/fliks-app/engine/commit/1f1e1231a9b39587e68b4985760c3e05d59f9e80))
* **runtime:** split the cores between concurrent software encodes ([#95](https://github.com/fliks-app/engine/issues/95)) ([3fbc327](https://github.com/fliks-app/engine/commit/3fbc327617ae5afe0895982d17541f49e7e1d6d5))
* **subtitles:** extract every text track of a file in one pass ([a384c86](https://github.com/fliks-app/engine/commit/a384c869bd2b468b0830a79b9efd0a095e7b6706))

## [1.0.2](https://github.com/fliks-app/engine/compare/v1.0.1...v1.0.2) (2026-10-22)


### Bug Fixes

* **hls:** never list a segment the transcode can't produce ([#90](https://github.com/fliks-app/engine/issues/90)) ([c97ac9e](https://github.com/fliks-app/engine/commit/c97ac9e19ffd171a824955fb98593477fa4ec7c3))
* **session:** bound the plan duration to the video end ([#92](https://github.com/fliks-app/engine/issues/92)) ([93a2961](https://github.com/fliks-app/engine/commit/93a2961743384bb83737cd4f3a991ec1af56dda6))

## [1.0.1](https://github.com/fliks-app/engine/compare/v1.0.0...v1.0.1) (2026-10-22)


### Bug Fixes

* **deps:** bump rustls-webpki to 0.103.16 ([#88](https://github.com/fliks-app/engine/issues/88)) ([bb29959](https://github.com/fliks-app/engine/commit/bb299595e82628329e12a481cd16c3bc85834cd7))

## [1.0.0](https://github.com/fliks-app/engine/compare/v0.5.4...v1.0.0) (2026-10-22)


### Miscellaneous Chores

* release 1.0.0 ([#86](https://github.com/fliks-app/engine/issues/86)) ([5678045](https://github.com/fliks-app/engine/commit/5678045bdb6e759d35c86b9f8723387349663cea))

## [0.5.4](https://github.com/fliks-app/engine/compare/v0.5.3...v0.5.4) (2026-10-22)


### Bug Fixes

* **decide:** resolve the requested rung among those that fit the source ([#78](https://github.com/fliks-app/engine/issues/78)) ([1e938c9](https://github.com/fliks-app/engine/commit/1e938c97ed47a734475dbd9a3340b1998248e095))

## [0.5.3](https://github.com/fliks-app/engine/compare/v0.5.2...v0.5.3) (2026-10-20)


### Performance Improvements

* **engine:** use mimalloc on linux ([#77](https://github.com/fliks-app/engine/issues/77)) ([0ecceb5](https://github.com/fliks-app/engine/commit/0ecceb55491ef8f6856a8488df6bf67f98b25e96))
* **serve:** stream files through one helper that reuses its buffer ([#75](https://github.com/fliks-app/engine/issues/75)) ([59a1b09](https://github.com/fliks-app/engine/commit/59a1b094c17fc710ea5b786d2392d4829e8732c7))

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
