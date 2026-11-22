# Roadmap

_Status as of 2026-11-22. Scope comes from `docs/checklist.md`._

The server left the [StarBooru](https://github.com/StarBooru/StarBooru) repository at `v0.1.0-beta.6`, with every feature it had then. That repository is archived, and its `docs/roadmap.md` has the status of each checklist item at the split, apps included.

## Server

Done: the API, PostgreSQL search with tags, aliases, implications, the tag tree, figures, dates, sizes, colours and typo correction; uploads in chunks, URL and library imports, other boorus; thumbnails, samples, video probing and trims, image edits; OCR, meta tags, AI tagging and translation on local models or an API; accounts, OpenID Connect, API keys, limits, shares, duplicates, the recycle bin and backups; the Docker image and the server package.

Left:

| Item | Milestone |
|---|---|
| Embeddings with pgvector, for similar-image search and related posts by content | M5 |
| Upscaling and background removal | M5 |
| Sync between devices | M4 |

## Apps

Each app starts from its docs and is rebuilt natively, with the archived Dioxus app as the visual reference.

| App | Repository | Status |
|---|---|---|
| Web (Leptos) | [starbooru-web](https://github.com/StarBooru/starbooru-web) | docs only |
| Android (Compose, Rust core) | [starbooru-android](https://github.com/StarBooru/starbooru-android) | docs only |
| Windows (windows-reactor) | [starbooru-windows](https://github.com/StarBooru/starbooru-windows) | docs only |
| Linux (GTK 4 and libadwaita) | [starbooru-linux](https://github.com/StarBooru/starbooru-linux) | docs only |

Until starbooru-web is ready, the image serves no web app. Installs that need one stay on `ghcr.io/filipnone/starbooru`, the last image with the Dioxus web app inside.

## Known gaps and risks

- **ONNX Runtime is gone from GitHub.** Since 2026-11-17, `github.com/microsoft/onnxruntime` and its release downloads answer 404. The image now takes it from Microsoft's NuGet packages, whose libraries are byte for byte those of the GitHub releases. The server's own runtime download (`local_ai/install.rs`) still fetches from GitHub, so it fails until the repository comes back or that pin moves to NuGet too.
- **The server package has no local AI.** It holds no `starbooru-ai-worker` or ONNX Runtime, so the built-in models need the Docker image. API-based AI works.
- **Orphan files.** Failed inserts remove their files unless another post owns them, and `tmp/` is cleared at startup. Files left by a crash between the write and the insert are not swept yet.
- **AI key in the database.** An API key entered in Administration › AI is stored as plain text in `ai_tools`, so it is also in database backups. Setting it with `STARBOORU_AI_KEY` keeps it out of both.
- **OCR coverage.** GIFs and videos are not read, and a scan that failed or timed out counts as done until `POST /posts/{id}/ocr` redoes it. The Docker image has English and Japanese; other languages need their `tesseract-ocr-*` package.
- **Panel translation.** Small local models miss text and place boxes loosely, and may translate sound effects literally. Long webtoon strips are shrunk to 1536 px, so their text may be too small to read; there is no splitting yet.
- **Sideways photos from before EXIF orientation.** Posts uploaded earlier keep the thumbnail and the size they got without it, so a crop can be refused as "outside the image". Rebuilding them needs a backfill.
- **`web.rs` knows only `dx` bundles.** It caches files for a year only when they are named `<name>-dxh<hex>.<ext>`, and finds the wasm to preload in `dx`'s loader script. starbooru-web's bundle is served, but checked on every visit and without preloads, until the module learns its names.
