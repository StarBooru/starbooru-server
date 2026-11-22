# AGENTS.md: guide for AI agents and contributors

StarBooru is a self-hosted personal booru: organise and tag a private image and video collection. This repository is its server, in **Rust** (axum and PostgreSQL), and the two crates that define its API. It has no user interface.

The apps live in their own repositories under [StarBooru](https://github.com/StarBooru): `starbooru-web`, `starbooru-android`, `starbooru-windows` and `starbooru-linux`. They talk to this server over its JSON API only. The archived [StarBooru/StarBooru](https://github.com/StarBooru/StarBooru) holds the first app (Dioxus) and is kept as their visual reference; never push to it.

The product name is written **StarBooru** in user-facing text. Machine names stay lowercase: crates `starbooru-*`, binaries, ids (`eu.filipnone.starbooru`).

Read this file first, then `docs/`.

## 1. Orientation

| Read | Why |
|---|---|
| `docs/architecture.md` | Crate map, data model, how search, storage and auth work |
| `docs/development.md` | Toolchain, running the server, tests, releases |
| `docs/deployment.md` | Docker image, Dokploy setup, configuration, backups |
| `docs/api.md` | The HTTP API the apps use |
| `docs/roadmap.md` | What is left on the server, and known gaps |
| `docs/checklist.md` | The owner's feature backlog for every repository (source of truth for scope; don't rewrite it) |
| `log/devlog.md` | Local dev log (gitignored): what landed, what's still open |

## 2. Workspace map

```text
crates/
  core      shared types (API models, tag rules, search query parser); no I/O, builds for wasm
  client    typed async HTTP client for the API (reqwest; wasm + native), used by the Rust apps
apps/
  server    starbooru-server: axum API, media storage, thumbnails, can serve a web app's bundle
  ai-worker starbooru-ai-worker: runs local AI models in a process of its own (ONNX Runtime)
tools/
  ai-catalog  pins each AI model file to a commit and a SHA-256 (writes crates/core/src/ai_files.rs)
```

Dependency direction: `core` ← `client`, `core` ← `server`, `core` ← `ai-worker`. The server never depends on `client`. `core` and `client` must keep compiling for `wasm32-unknown-unknown`, as starbooru-web runs them in the browser.

## 3. Golden rules

### Never crash

People trust StarBooru with their collection. A malformed upload, a bad query, a corrupt settings file or a full disk must produce an error the user can act on, never a panic.

- **Non-test code never panics.** No `unwrap()`, `expect()`, `panic!`, `unreachable!`, `todo!` or `unimplemented!`. The workspace lints deny them, and `clippy.toml` allows them in tests only. Return the error type and propagate with `?`. Use `unwrap_or*` only where a fallback is truly correct.
- **No `unsafe`.** The workspace sets `unsafe_code = "forbid"`.
- **Input-derived numbers are hostile.** Use `get()` instead of indexing, `checked_*`/`saturating_*` arithmetic, and cap allocations by configuration. The image decoder runs with an allocation limit.
- **Server handlers return `AppError`.** Internal errors are logged and returned as a generic 500 without details. `CatchPanicLayer` is a last-resort net, not permission to panic.
- **Every crash fix comes with a regression test** that failed before the fix.

### Project rules

1. **The API is the contract.** Request and response types live in `starbooru-core`. Add or change an endpoint in `core`, the server and `client` in the same change, and update `docs/api.md`. The apps pin these crates to a tag of this repository, so tag a release when the API changes and say what changed in its notes.
2. **Database changes are new migrations** (`apps/server/migrations/NNNN_name.sql`). Never edit an applied migration. They run automatically at startup.
3. **Media is content-addressed and immutable.** Files are named by SHA-256. Deletion is soft (the recycle bin). Clients reach media only through signed `/media` URLs returned by the API.
4. **No interface code here.** Anything an app shows belongs in its repository. What the apps share (models, parsers, validators like `password_is_weak`) goes in `core`, so every app and the server agree.
5. **Tests are the gate.** Every change comes with tests. Parsers and validators (`core`) get unit tests. Server behaviour gets tests or a documented manual check in the dev log.
6. **Privacy.** Collections are private. Never log file contents, tokens or passwords. Network requests from tooling use a generic User-Agent and never carry personal details. Test images are generated or public domain, never personal photos.
7. **Assets.** Third-party assets must be permissively licensed and get a row in `ATTRIBUTION.md`.
8. **Time goes through `chrono`.**

## 4. Before you finish a task

```sh
cargo fmt --all
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy -p starbooru-core -p starbooru-client --target wasm32-unknown-unknown -- -D warnings
docker build -t starbooru-server:dev .   # if you touched the Dockerfile, migrations or build setup
```

Server tests need `STARBOORU_TEST_DATABASE_URL` (`docs/development.md` › Tests and lints); without it they are skipped, not passed.

Then append a terse entry to `log/devlog.md`: what landed, what you verified, what's still open. Also update `docs/roadmap.md` when a checklist item changes state.

## 5. Tooling notes

- The server needs PostgreSQL. The quickest local one is a throwaway container (`docs/development.md`).
- ffmpeg, tesseract and `pg_dump` are optional at run time; tests that need ffmpeg skip without it.
- The wiki of the archived repository still has the user guides, read-only.
