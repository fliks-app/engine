# fliks-engine

Video engine for Fliks (decisions, ffmpeg sessions, segment serving). Standalone: any host in any language talks to it over the `/v1` HTTP protocol. Personal project: no employer names, tools or domains anywhere.

## What it is / is not

- Is: a pure decision core + a tokio/axum binary that supervises a host process and supervises ffmpeg.
- Is not: a database, an ACL, a JWT verifier. Access is delegated to the host resolver (PROTOCOL.md).
- Never import Fliks-specific concepts (users, libraries) into the engine.

## Crates

- `crates/core` (`fliks_engine_core`): PURE. Deps: serde, serde_json, utoipa only. No tokio, fs, process, clock, rand.
- `crates/engine` (`fliks-engine` bin + `fliks_engine` lib): axum control plane, supervisor, later hw/proc/sessions/cache/serve.
- `crates/engine/src/bin/gen-openapi.rs`: prints the OpenAPI document.
- `fixtures/`: golden vectors (regression tests, `BLESS=1` rewrites `expected`). No client package: hosts generate their types from `openapi.json` (published with each release).

## Rules

- Wire types (`core::api`) are the single source of truth: serde struct = JSON = persisted blob = OpenAPI. Regenerate `openapi.json` after changing them: `cargo run -q --bin gen-openapi > openapi.json`, commit it.
- `MediaInfo` mirrors the host's `MediaFileInfo` jsonb: optional stays absent, `| null` stays explicit null, integers as u32/i32, seconds f64.
- Fixtures: never edit `expected` by hand: change the code, `BLESS=1 cargo test -p <crate> --test <suite>`, review the diff.
- Protocol: additive-only in v1, unknown fields ignored, no 64-bit ints, breaks go to `/v2`. Bump `PROTOCOL_VERSION` only on a new major.
- The TS reference is gone: `docs/spec/*.md` and their `[ts:...]` tags stay as documentation, the fixtures are Rust-owned regression tests.
- Record intentional differences in `docs/README.md` "Deviations", unresolved ones in "Open divergences".

## Working economically

- Quiet commands: `cargo test -p <crate> 2>&1 | tail -30`, `cargo clippy --all-targets -- -D warnings 2>&1 | tail -30`.
- Locate with `tools/sym NAME` (or `-p`, `-f FILE`, `-g REGEX`) or rust-analyzer, then read +-40 lines. Do not read `target/`, `openapi.json`.
- Targeted tests while working, the full suite once at the end.
- Before pushing code touching `cfg(windows)`/`cfg(unix)`/macOS paths: `tools/cross-check` (clippy for Windows and macOS, no toolchain needed).

## Commits

Conventional commits, lowercase subject, wrapped body (<=100 cols), no `Co-Authored-By` lines. release-please reads them. Git identity: clementdelestre@gmail.com.
