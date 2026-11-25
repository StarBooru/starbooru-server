# Roadmap

_Status as of 2026-11-25. Scope comes from `docs/checklist.md`, and the "Next" sections from the owner's lists of 2026-11-23. They come in this order: updates, private and public instances, people. Releases are no longer betas: the next is v0.2.0, and Cargo's version follows them._

The server left the [StarBooru](https://github.com/StarBooru/StarBooru) repository at `v0.1.0-beta.6`, with every feature it had then. That repository is archived, and its `docs/roadmap.md` has the status of each checklist item at the split, apps included.

## Server

Done: the API, PostgreSQL search with tags, aliases, implications, the tag tree, figures, dates, sizes, colours and typo correction; uploads in chunks, URL and library imports, other boorus; thumbnails, samples, video probing and trims, image edits; OCR, meta tags, AI tagging and translation on local models or an API; accounts, OpenID Connect, API keys, limits, shares, duplicates, the recycle bin and backups; the Docker image and the server package.

Left:

| Item | Milestone |
|---|---|
| Embeddings with pgvector, for similar-image search and related posts by content | M5 |
| Upscaling and background removal | M5 |
| Sync between devices | M4 |

## Next: updates

The server knows only its own version. Installs run the image's `latest` tag, so an administrator finds out about a new release only by looking. starbooru-web's `docs/roadmap.md` (section 10) has the app's side.

- [x] The server checks for a new release now and then (it can be turned off) and tells administrators, with the release notes
- [x] Updating from the app: the Docker image replaces itself when the administrator has given it the Docker socket; the server package downloads, checks and swaps itself
- [x] Automatic updates, an option, with a backup first
- [ ] Docker Swarm updates, tested on a real swarm (unit-tested only)
- [ ] The server package rolls back by itself when a new binary won't start
- [x] The image serves starbooru-web, a pinned release checked against its `SHA256SUMS`, and `web.rs` knows Trunk's file names (long cache, preloads)

## Next: private and public instances

An instance runs in one of two modes: private, as today, or public, open to other people with their own accounts. Today every endpoint but `/info`, sign-up and sign-in needs a token, and only share links open without one. starbooru-web's `docs/roadmap.md` (section 4) has the app's side.

The owner's answers (2026-11-23): in public mode, people without an account can browse everything, read-only. A post is shared by its own address there; share links stay for private mode. Switching deletes what the new mode doesn't use, after a preview of it and a backup made for the switch.

- [x] The mode, a server setting that `/info` reports
- [x] Switching is destructive, so the switch first offers a backup made for it, on by default
- [x] Private mode: 0 to 5 star scores as an option, with likes and dislikes as the default
- [x] Public mode: accounts with a profile and a profile picture
- [x] Public mode: a forum
- [ ] A moderator role between users and admins, and forum notifications
- [x] Public mode: votes on tags
- [x] Public mode: the other-booru endpoints (`/remote/*`) are off and can't be turned on
- [x] Public mode: browsing without an account, read-only, and Explore's Trending and Popular tags
- [x] Share links set up to suit each mode: share links in private mode, a post's own address in public mode
- [ ] The guests' rate limit shared between replicas (it's per process, in memory, like the sign-in limits)
- [x] A tag blacklist for each account
- [x] A home page setting: on or off, and how it's customised. Its background shows the most popular posts rated safe

## Next: people

A figure has a kind, a character, a franchise, a cosplay flag, tag slots for clothing and looks, and a face box with a kept crop. An AI tool finds the people in a picture. starbooru-web's `docs/roadmap.md` (section 5) has the app's side.

- [x] An AI tool that finds faces on upload, anime ones included, keeps a crop of each, and fills in the figure: name, franchise, cosplay or not, clothing and looks
- [x] A franchise on each figure, from the `copyright` tag category, so people group by franchise
- [x] People in Explore, and popular characters in public mode

## Apps

Each app starts from its docs and is rebuilt natively, with the archived Dioxus app as the visual reference.

| App | Repository | Status |
|---|---|---|
| Web (Leptos) | [starbooru-web](https://github.com/StarBooru/starbooru-web) | v0.23.0: everything the Dioxus app did, plus browsing, sharing, design and install work; installable |
| Android (Compose, Rust core) | [starbooru-android](https://github.com/StarBooru/starbooru-android) | docs only |
| Windows (windows-reactor) | [starbooru-windows](https://github.com/StarBooru/starbooru-windows) | docs only |
| Linux (GTK 4 and libadwaita) | [starbooru-linux](https://github.com/StarBooru/starbooru-linux) | docs only |

The image serves starbooru-web, a release pinned in the `Dockerfile`.

## Known gaps and risks

- **The server package has no local AI.** It holds no `starbooru-ai-worker` or ONNX Runtime, so the built-in models need the Docker image. API-based AI works.
- **Orphan files.** Failed inserts remove their files unless another post owns them, and `tmp/` is cleared at startup. Files left by a crash between the write and the insert are not swept yet.
- **AI key in the database.** An API key entered in Administration › AI is stored as plain text in `ai_tools`, so it is also in database backups. Setting it with `STARBOORU_AI_KEY` keeps it out of both.
- **OCR coverage.** GIFs and videos are not read, and a scan that failed or timed out counts as done until `POST /posts/{id}/ocr` redoes it. The Docker image has English and Japanese; other languages need their `tesseract-ocr-*` package.
- **Panel translation.** Small local models miss text and place boxes loosely, and may translate sound effects literally. Long webtoon strips are shrunk to 1536 px, so their text may be too small to read; there is no splitting yet.
- **Sideways photos from before EXIF orientation.** Posts uploaded earlier keep the thumbnail and the size they got without it, so a crop can be refused as "outside the image". Rebuilding them needs a backfill.
