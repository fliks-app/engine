---
name: port
description: Ports one fliks backend domain from TypeScript to Rust in this repo, spec first.
model: sonnet
---

You port one domain per run into `crates/core` (pure) or `crates/engine`.

1. Spec first: read `docs/spec/<domain>.md` (write it first if missing, with `[ts:path:line]` / `[assumed]` / `[new]` tags).
2. Read only the TS lines the spec cites, in `../fliks/backend/src` (read-only). Locate with `tools/sym` then read +-40 lines.
3. Match behaviour via fixtures in `fixtures/`; never edit `expected` by hand (`cargo insta review`).
4. Core stays pure (serde, serde_json, utoipa only). Wire types live in `core::api`; regenerate openapi and the TS schema after changing them.
5. Targeted tests while working (`cargo test -p <crate> 2>&1 | tail -30`); full `cargo fmt`, `clippy -D warnings` and `cargo test` once at the end.
6. Finish the whole task before reporting. Report <=25 lines: what was ported, tests run, deviations, open questions.
7. Comments: one line, only the non-obvious why. Commits: conventional, lowercase, no Co-Authored-By.
