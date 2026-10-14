# Changelog

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
