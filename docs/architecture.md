# Architecture

```text
 starbooru-web ──────┐
 starbooru-android ──┼── HTTPS, JSON API (/api/v1) + signed media URLs ──▶ starbooru-server ──▶ PostgreSQL
 starbooru-windows ──┤                                                           │
 starbooru-linux ────┘                                                           └──▶ /data (originals, thumbnails)
```

One server process does three jobs:
- it serves the JSON API under `/api/v1`;
- it serves media files under `/media`;
- it serves a compiled web app from `STARBOORU_WEB_DIR` when one is set (any other path falls back to `index.html`).

Each app lives in its own repository under [StarBooru](https://github.com/StarBooru) and talks to the server only through the API. The apps written in Rust use `starbooru-core` and `starbooru-client` from this repository. Native apps ask for the server URL when they sign in.

## Crates

| Crate | Role |
|---|---|
| `crates/core` (`starbooru-core`) | API models (`Post`, `Tag`, `User`, …), tag normalisation, the starter tag tree, spelling variants and category prefixes (`vocab.rs`), which tag a typo meant (`typo.rs`), the search query parser, the other-booru request and its checks, and which post a link points at (`remote/`), the accepted file formats, how a file's first bytes tell them apart and a picture's upright size from its first 64 KiB (`media.rs`: PNG, GIF, JPEG with its EXIF orientation, WebP, BMP). Pure Rust, no I/O. |
| `crates/client` (`starbooru-client`) | `Client`: one async method per endpoint, with a bearer token. Maps HTTP errors to `ClientError`. On Android it trusts the system's CA folder (`roots.rs`), because reqwest's platform verifier needs JNI setup and a Java helper in the APK. A `429` is retried twice after the `Retry-After` wait (at most 30 s) before it surfaces as `RateLimited { retry_after }`. A read (`GET`) that failed before any answer came is sent again after 300 ms and after 1 s, as the first request of a phone that just woke up or changed network goes out on a dead connection; a write is never sent twice. The native build asks for gzip or brotli answers, gives a connection 10 s to open, and checks a connection in use every 20 s, so a dead one fails the request in half a minute. Runs on wasm and native. |
| `apps/server` (`starbooru-server`) | axum router, auth, posts, tags, storage, thumbnails, signed media. |
| `apps/ai-worker` (`starbooru-ai-worker`) | Runs AI models in a process of its own, on ONNX Runtime. Depends only on `core`. |

### Server modules

| Module | What it does |
|---|---|
| `config.rs` | Environment variables (see `deployment.md`) |
| `web.rs` | A compiled web app (`STARBOORU_WEB_DIR`; the image has starbooru-web in `/app/web`). Files named after their content go out with `Cache-Control: public, max-age=31536000, immutable`: Trunk's at the top (`<name>-<hex>.<ext>` and `<name>-<hex>_bg.wasm`; Trunk drops the hash's leading zeros, so 12 to 16 digits count) and `dx`'s under `/assets/` (`<name>-dxh<hex>.<ext>`). Everything else, `index.html`, `sw.js`, the manifest (`application/manifest+json`) and the icons included, goes out with `no-cache`. Any path that isn't a file answers with the page, except a missing asset, which is a 404: anything under `/assets/`, and a script, stylesheet, wasm or manifest at the top. Trunk's page preloads its script and wasm itself. For a `dx` page the server adds a preload for the wasm (named in the loader script) and for any stylesheet named in the wasm that the page doesn't link itself, so everything downloads together. `starbooru-server precompress <dir>` writes a `.br` and a `.gz` beside each text file, which are sent as they are; without them the files are compressed as they go out. |
| `routes/` | One file per resource: `auth`, `posts`, `tags`, `trash`, `duplicates`, `admin`, `backups`, `shares`, and more. `mod.rs` builds the router and its layers (CORS, compression, tracing, panic catcher, SPA fallback). Every request runs in its own task, so a client that disconnects (a closed tab, a phone going to sleep) doesn't cancel a translation, OCR, an edit or an upload that has already arrived; the result is stored as usual and the client sees it on its next load. Accepted connections have `TCP_NODELAY` set (`main.rs`), or a short answer could wait 40 ms for the client's delayed ACK. |
| `auth.rs` | Bearer-token extractor for sessions (`sb_…`) and API keys (`sbk_…`); a read-only key is refused anything but `GET`/`HEAD`, and `AuthUser::session_id()` refuses keys on the account endpoints. Argon2id password hashing (on the blocking pool). Routes that guests may read take `guests::Viewer` instead. |
| `oidc.rs` | OpenID Connect client: discovery (cached for an hour), the authorization URL with PKCE (S256) and nonce, the token request (`client_secret_basic`), and ID-token claim checks (issuer, audience, expiry, nonce). The token comes straight from the provider over TLS, so its signature is not checked. Also the `return_to` rule (own web app or loopback). `routes/oidc.rs` keeps the sign-ins in progress in `oidc_logins` and hands results over as one-time codes bound to a secret handle. |
| `backup.rs` | Database backups: runs `pg_dump` (password in the environment, not the arguments) into the backup folder, one at a time, lists and prunes them, signs their download links, and makes the scheduled ones (checked hourly). |
| `update/` | New releases and updates (see *Updates*). `github.rs` asks for the newest release, `docker.rs` is a small Docker Engine API client over the Unix socket (hyper, which axum and reqwest bring anyway), `container.rs` works out from Docker's description of the container what to pull and how to copy it, `helper.rs` is the `update-helper` process that replaces the container, `package.rs` updates the server package, and `mod.rs` keeps the state and runs the checks, automatic updates and installs. |
| `settings.rs` | The server settings in force: the `server_settings` row over the environment variables. Read per request, so admin changes apply at once. The mode and the score kind are only what is stored (private with likes before it is); a public server's score kind is likes, whatever is stored. |
| `modes.rs` | Private and public (see *Modes*). `UNUSED` is the one list of what each mode has no use for, a table or a column of `users`; the preview counts it and the switch deletes it. `settle` stores the mode and score kind on the first start. Also the advisory lock that switches, changes of score kind and votes share, and `hold_public`, which takes it for a write that only a public server keeps. |
| `avatars.rs` | Profile pictures: cuts an upload to a square, scales it to 256 and 64 px JPEGs named by the SHA-256 of the larger, and removes the files once no account uses them (see *Media storage*). |
| `faces.rs` | Crops of figures' faces: a background task, woken when people are saved, cuts each new face box out of its post's picture, and removes the files once no figure names them (see *Media storage*). |
| `routes/people.rs` | The collection's people (character tags in use) for `GET /people`, `/franchises` and Explore's `people` and `popular_characters`, and the background task that works out each character's franchise into `tags.franchise_id` (see *People*). |
| `scores.rs` | Likes or stars: what a vote request asks for under each kind, how a post's tallies move, and the conversion from one kind to the other. |
| `guests.rs` | People without an account on a public server. `GUEST_READS` is the one list of routes they may read, with what each costs them. A middleware lets such a request through within its address's limit and marks it, and the `Viewer` extractor those handlers take is the signed-in user or a guest. `Option<Viewer>` is `None` for someone without an account on a private server, for `/home`. |
| `routes/blacklist.rs` | Tag blacklists. `hidden_for` reads the reader's list (theirs, a guest's `exclude`, or the admin's default) and resolves each tag, through an alias, to its tree. Searches apply it in `posts::tag_filter` as extra `-tag` and `-rating:` terms, less what the search names (`TagFilter::hide`), so every read built on the search, Explore and the colour search included, leaves the same posts out. Related posts filter their candidates the same way. |
| `routes/home.rs` | The home page. Its background search runs at most every 5 minutes and is kept in `HomeCache` (`AppState::home`) as post ids with its filter; each request runs the filter again on those ids, so binned or retagged posts and the reader's blacklist apply at once. |
| `throttle.rs` | Sign-in limits: failed attempts per account and per client address (IPv6 by /64) in fixed windows, in memory and capped at 10,000 keys each. The address is the socket peer, or the last `X-Forwarded-For` entry with `STARBOORU_TRUST_PROXY`. The guests' limit is one more of these, counting requests instead of failures. |
| `storage.rs` | Format sniffing, content-addressed storage, thumbnails |
| `uploads.rs` | Chunked uploads under way, in memory: each is a file of its final size in the scratch directory that chunks are written into at their place, with a flag per chunk. A read-write lock per upload keeps a chunk from changing a file while it is hashed, and serialises repeated finishes. Up to 8 per account, dropped after an hour idle. `routes/uploads.rs` has the handlers; finishing goes through `Storage::ingest_file`, which moves a video into place without reading it into memory, then the same `record_post` as `POST /posts`. A restart clears the scratch directory and with it any upload under way, which clients hear as `upload_gone`. |
| `similar.rs` | A picture's colours for `color:`, and the duplicate finder fingerprints: a 64-bit difference hash (9×8 brightness grid) plus the average colour, and grouping by Hamming distance through a 4×16-bit multi-index, so it never compares every pair. Fine high-frequency patterns blur to flat at 9×8 and can match when their colours do. |
| `ai.rs` | AI provider client: `chat(provider, prompt, image)` posts to an OpenAI-compatible `/chat/completions` (no proxies, 5 min timeout, 1 MiB answer cap, any address since the admin sets it). `AiSetup` routes each tool (tagging, translation, comics) to its API, the local models or nowhere. Tag suggestions from a post's thumbnail, translation of a post's OCR text (text only; `routes/ai.rs`), and background auto-tagging of uploads (two at a time), when the uploader asks for it (`autotag` on the upload or import) or, if they don't say, when the administrator's setting does. The key is never logged or returned; the answer parser is `starbooru_core::ai`. |
| `local_ai/` | Local AI: `hardware.rs` reads the machine (`/proc`, `/sys/class/drm`, `nvidia-smi`, cgroup limits), `worker.rs` runs the worker process, `install.rs` downloads and verifies model files, `mod.rs` keeps it all working (see *Local AI*). |
| `fetch.rs` | URL import download: http(s) only, public addresses only (checked for literal IPs, on every redirect and in a filtering DNS resolver), no proxies, streaming size cap, timeouts. `get_from` names the site a file belongs to in `Referer` (the site only, never a whole address), as Gelbooru's file servers answer nothing else. |
| `routes/imports.rs` | Background URL imports: `import_jobs` rows claimed oldest first with `FOR UPDATE SKIP LOCKED` by two worker tasks, woken by a `Notify` when a batch is queued (or every minute), each run through the same path as `POST /posts/import`. At startup rows left `running` go back to `queued`; finished rows are deleted after 7 days. While a job downloads, `transfers.rs` keeps how far it has got in memory, by job (`Fetcher::get_from` reports each piece; `starbooru_core::Meter` turns the totals into a speed, a reading every half second, each counting for a third), and `GET /imports` puts it on the running jobs. An import that meets a busy site goes back to `queued` with `not_before` set (the site's `Retry-After`, or 15 s doubling each time, 10 minutes at most) and fails after six such waits; an idle worker sleeps until the first one is due. |
| `remote/` | Other boorus. `mod.rs`: the search URL and the one-post URL for each API (Danbooru, Gelbooru 0.2, Moebooru, e621, Blombooru), with the user's account in the query as that site names it, and the answer turned into `RemotePost`s (absolute http(s) links, valid tags only and which of them are artists, characters, copyrights or meta, each site's rating scale, MD5, sample, source, score, date). `html.rs` reads a web page for the picture it is about, without an HTML parser: `og:video`/`og:image`, and on a Gelbooru-style post page the original file, the tags by category, the rating and the source. `lookup.rs` is what a URL import goes through: an address `starbooru_core::post_link` recognises (a post's page, or a file whose address carries the MD5) is asked of the site's API; when the API refuses, as Gelbooru's does without a key, or the address is any other page, the page itself is read. `routes/remote.rs` fetches searches through `fetch.rs`, so the same address rules apply, and never puts the URL (which may carry a key) in an error or log. `pace.rs` keeps the server within what each site allows: a request to a site's API books the next turn for that host and port, the API's interval after the one before (`starbooru_core::Limits`), and waits for it, or answers 429 when the turn is over 8 seconds away. A site that says it is busy (429, 503, 421) is left alone by everything for as long as it asks, and one whose API refused a request without an account isn't asked again for an hour. `Fetcher::ask` returns refusals with their body, so the site's own words about a limit reach the user (`site_message`). `accounts.rs` is the saved accounts: `Vault` seals a key with ChaCha20-Poly1305 (`ring`) under a key derived from `STARBOORU_SECRET` by HMAC, with the user and the site as associated data, so a row moved to another user or site doesn't open; `load` opens a user's accounts for one search or one import, and `list` answers the API without keys. For the browser: `tags_url`/`parse_tags` are each site's tag suggestions; `html::read_list` reads Gelbooru's own list pages, which is how it is searched without a key; `lookup::post` gives one post in full, from the API or from its page, and for a Gelbooru post with an account its tags' categories from the site's tag list (`sort_tags`); `relay.rs` passes on pictures from sites that refuse hotlinking (`starbooru_core::guarded_by`), behind addresses signed with an HMAC of `STARBOORU_SECRET` that expire like `/media` ones, after checking that what came is a picture or a video. `starbooru_core::for_site` turns a search written the StarBooru way into each site's. |
| `sources/` | Reading a whole library from another self-hosted booru, a module per system (szurubooru, Shimmie2, Danbooru, Stash, Blombooru, Monbooru, LANBooru). Each says what to ask for next and turns the answers into `Item`s and `Rule`s. No I/O, so all of it is tested against recorded answers. |
| `routes/library.rs` | Library imports: one worker walks `library_imports` oldest first through the phases rules, posts and groups, saving a position (`"step:rest"`, up to the source) after each page, so a restart goes on from there. Two files download at once, through `store_post`, or `merge_into` when the file is already here. Waits out a 429 (eight times, 15 s each) and retries a struggling site; 50 failed downloads in a row end the import. An administrator's import uses `trusted_fetcher`, which may reach the server's own network; anyone else's uses the URL import rules. |
| `ocr.rs` | Text in images: runs `tesseract <file> stdout -l <langs>` as a child process (time-limited, local files only, capped output) on a still image's sample or original, cleans the text (whitespace, noise lines, 10,000 characters at most) into `posts.ocr_text`; after each upload in the background and for unscanned posts at start. The text is never logged |
| `panels.rs` | Comic and manga translation, kept in `post_translations`: with an API, fits a still image in 1536 px and asks a vision model for each piece of text with its box and translation (`starbooru_core::panels` builds the prompt and parses the answer); with local models, `local_ai::comics`. One page at a time, and after OCR finds text in a new upload when automatic translation is on. |
| `probe.rs` | Size, length and a still frame through `ffprobe`/`ffmpeg` child processes (time-limited, local files only, capped output); video trims (stream copy, re-encoded when the copy would start too early) |
| `edit.rs` | Image edits for `POST /posts/{id}/edit`: turn, flip and crop, written back in the original format. `routes/edit.rs` makes the result a new post through the normal upload path, with the original's people, their face boxes moved by `MediaEdit::move_face`. |
| `media.rs` | HMAC-signed media URLs and the file handler (supports Range for video seeking) |
| `error.rs` | `AppError` → JSON `{code, message}` with the right status |
| `testing.rs` | Test-only harness: a fresh database per test and the real router |

## Data model

PostgreSQL schema: `apps/server/migrations/`.

| Table | Contents |
|---|---|
| `import_jobs` | Background URL imports per user: address, source, tags, rating, `state` (`queued`, `running`, `done`, `duplicate`, `failed`), the new `post_id` and a readable `error`. `attempts` and `not_before` are for a site that says it is busy: the tries so far, and when the next may start |
| `booru_accounts` | Per user: an account on another booru, one per site address. `login` in the clear, the key in `sealed_key` (a 12-byte nonce, then the ciphertext and its tag). A backup holds them sealed; they open only with the same `STARBOORU_SECRET` |
| `remote_searches` | Per user: a search on other boorus, kept with its name and the sites it goes to (a JSON list of `{api, base}`) |
| `users` | Usernames are case-insensitively unique. Roles are `admin` and `user`; the first account becomes admin. `disabled_at` blocks sign-in and every token; `quota_bytes` caps the bytes of originals the account may upload (checked under a row lock at insert). The profile: `display_name`, `about` and `avatar_sha`, the picture's name (NULL for none). `blacklist`, a `TEXT[]` of tag names and `rating:` entries, starts as `server_settings.default_blacklist`. |
| `user_identities` | Provider accounts (`issuer`, `subject`, unique together) linked to a user, with the provider's `label` and `last_used_at`. `users.password_hash` is NULL for accounts that only sign in this way. |
| `oidc_logins` | Provider sign-ins in progress, at most 10 minutes: hashes of the `state`, the client's handle and the result code, the nonce and PKCE verifier, `return_to`, the account linking it (if any), then the provider's issuer, subject and label once the browser is back |
| `server_settings` | One row of admin overrides (name, open registration, recycle-bin days, automatic meta tags, backup schedule, update checks and automatic updates); NULL follows the environment variable. `mode` and `score_kind` are NULL only until the first start with modes, which stores them (`modes::settle`); the environment doesn't move them after that. Also the default blacklist, the home page (`home_*`, NULL for the defaults) and `forum_seeded`, set once the forum has had its default categories, so a public server gets them at start once |
| `update_state` | One row: the newest release from the last check that worked (`latest`, JSONB, with its files), when the last check ran and why it failed, and the update under way or the last one's result (`run`, JSONB) |
| `post_translations` | Text drawn over an image, per post and language: blocks (box, original, translation) as JSONB, and the model that made them |
| `sessions` | The SHA-256 of each bearer token (`sb_<64 hex>`) and its expiry, 90 days. The plain token is never stored. Also an `id` for the settings page, `last_used_at` (refreshed at most every 5 minutes) and `client`, a short description like "Firefox on Linux" (`starbooru_core::agent`); the raw User-Agent is not kept. Native apps send `StarBooru-app/<version> (<os>)`. |
| `tags` | Normalised name, category (general / artist / character / copyright / meta), `parent_id` (the tag tree; loops are refused) and a `post_count` kept in step inside the same transaction as post edits. For a character, `franchise_id`, the copyright tag the server worked out as its franchise, `face_figures`, the figures whose faces it shows, and `franchise_checked_at`, when (NULL: to do). `server_settings.vocab_version` records which starter vocabulary (`starbooru_core::vocab`) was seeded at startup, including its spelling variants as aliases. A GIN `pg_trgm` index on the name finds tags spelled alike. |
| `posts` | One per file: `sha256` (unique, so duplicates are refused), format, size, dimensions, rating, source, `score` (likes minus dislikes; a star vote counts as the like it stands for), `star_sum` and `star_count` (the star votes added up and counted), `tag_ids BIGINT[]`, `deleted_at` (soft delete), `phash`/`pcolor`, the duplicate finder's fingerprint of the thumbnail, `file_date`, the date in the file (`file_date_scanned` says it was looked at), `meta_tagged_at`, when the automatic meta tags were added (NULL: not yet), and `meta_version`, the version of their rules the post has had |
| `figures` | The people in a post, in order (`position`), with a `kind`: girl, boy or other, and the face box (`face_x`, `face_y`, `face_w`, `face_h`, thousandths of the picture, all NULL for none). `face_sha` names the crop of the face once it is made; `face_failed` marks one that couldn't be, tried again at the next start. Replaced as a whole by `PUT /posts/{id}/figures`, which carries a crop over to the figure with the same box. |
| `figure_tags` | A figure's tags with their `slot` (0 character, 1 cosplay, 2–12 head to other, 13 franchise). These tags are on `posts.tag_ids` too; this table answers "one figure has all of these" and the viewer. |
| `duplicate_dismissals` | Pairs of posts (`post_a < post_b`) marked as not duplicates |
| `post_votes` | One vote per user and post, with `voted_at` (set when the vote is cast or changed). A like or dislike has `value` 1 or −1 and no `stars`. A star vote has `stars` 1 to 5 and, in `value`, the like it counts as (1 for 4 or 5, −1 for 1 or 2, 0 for 3); a constraint holds the two together. `posts.score` is the sum of `value`, so it means the same under both kinds. |
| `tag_aliases` | `alias → tag_id`. Aliases are resolved when tagging, searching and in autocomplete. |
| `shares` | Per user: a random 128-bit `token` (kept as is, so the owner can copy the link again), optional `title`, `post_ids BIGINT[]` in display order, optional `expires_at`, `views` and `last_viewed_at`. The public view resolves posts live, so binned posts drop out and come back on restore. |
| `api_keys` | Per user: a name, the SHA-256 of the token, its first 12 characters (`prefix`, for telling keys apart), `read_only`, `last_used_at` (null until used) and an optional `expires_at` |
| `saved_searches` | Per user: a name (unique per user, case-insensitively) and a query string |
| `library_imports` | Per user: a whole-library import with its source, address, phase, position and counts. `login` and `secret` are that site's account, and a constraint holds them to `queued` and `running` rows. |
| `library_import_items` | Every post an import has dealt with: `post_id`, or `error` when it failed, so a retry tries it again and skips the rest. |
| `tag_implications` | `tag_id → implied_id`. Expanded transitively (a recursive CTE) when tags are set, so implied tags are stored on posts. Cycles are refused. Edits take an advisory lock, since they may rewrite many posts. |
| `forum_categories` | The forum's categories: a name unique regardless of case, a description and a `position` to list them by. They stay when the server goes private. |
| `forum_threads` | A title, its category and author, `pinned` and `locked`, and `post_count` and `last_post_at`, kept in step by each reply so lists count nothing. Indexed for a category's list and the latest activity, and by `to_tsvector('simple', title)` for search. |
| `forum_posts` | A thread's posts in id order: author, body, `edited_at`, and `deleted_at` for a post that keeps its place with an empty body. A GIN index on `to_tsvector('simple', body)` covers the posts not deleted; `(author_id, created_at)` serves the posting limits. Deleting an account keeps its threads and posts, without an author. |
| `tag_votes` | One vote per user, post and tag, `value` 1 or −1. The primary key starts with the post, so a post's tag scores are an index lookup. |
| `tag_removals` | Tags voted off a post, with the score they went at. While a row is there `posts::set_tags`, implications, alias merges and the automatic meta tags leave the tag off that post; an admin's undo deletes it. |

### Automatic meta tags

Like Danbooru, the server tags each post with facts it reads from the file, in the `meta` category. The rules are a pure function, `starbooru_core::autotag::meta_tags`, with Danbooru's names and thresholds:

| Tag | When |
|---|---|
| `lowres` | width ≤ 500 and height ≤ 500 |
| `highres` | width ≥ 1600 or height ≥ 1200 |
| `absurdres` | width ≥ 3200 or height ≥ 2400 (also `highres`) |
| `incredibly_absurdres` | width or height ≥ 10000 (also the two above) |
| `wide_image` / `tall_image` | the long side is ≥ 1024 px and ≥ 4 × the short one |
| `animated` | GIFs (every GIF counts, the server doesn't count frames) and videos |
| `video` | videos |
| `sound` | videos with an audio stream (ffprobe; unknown without it) |
| `long_video` | videos of 30 s or more |
| `hd` | long side ≥ 1280 and short side ≥ 480 |
| `full_hd` | long side ≥ 1920 and short side ≥ 720 |
| `4k` | long side ≥ 3840 and short side ≥ 1440 |
| `8k` | long side ≥ 7680 and short side ≥ 2880 |
| `horizontal_image` | wider than tall, by more than 1% |
| `vertical_image` | taller than wide, by more than 1% |
| `square_image` | the sides within 1% of each other |

Every new post goes through `insert_post` (uploads, URL and booru imports, editor copies), which adds them in the same transaction as the post's own tags: a missing tag is made in `meta`, an existing one keeps its category, aliases and implications apply, `post_count` is kept in step. Only the highest of `hd`, `full_hd`, `4k` and `8k` applies. Their short sides are two thirds of the standard's, so cropped widescreen still counts. The names `landscape` and `portrait` are left alone, as Danbooru uses them for what a picture shows.

`posts.meta_version` is set then, so a post gets each rule only once and a tag someone removes stays removed. A new rule raises `autotag::META_VERSION`, and `autotag::added_in` says which version each tag came with. At startup a backfill brings live posts behind the current version up to date (older posts, or ones uploaded while this was off): a post at version 1 gets only the newer tags, and only a post at version 0 is probed for audio. New rules reaching a post don't change its `updated_at`, so they don't reorder `order:updated`. `STARBOORU_AUTO_TAGS` or the admin toggle turns both off.

### File dates

`posts.file_date` is the date in the file: EXIF `DateTimeOriginal`, then `DateTimeDigitized`, then `DateTime` for images, and the container's `creation_time` from ffprobe for videos. GIFs have none. EXIF has no time zone, so its wall clock is stored as if it were UTC: `date:2024-03-15` matches the day the photo shows. Clocks that were never set don't count (a year before 1826 in EXIF, before 1990 in a video). An editor copy keeps the original's date, as editing drops the metadata.

Uploads read the date at ingest. A startup backfill (`backfill_file_dates`) reads it once for older posts, recycle bin included, and doesn't change `updated_at`. Videos wait for a server with ffprobe. `date:` compares `COALESCE(file_date, created_at)`, which has its own partial index.

### Colours

`crates/core/src/color.rs` cuts OKLab into a grid (13 lightness lines, 17 each of a and b; lightness counts for half as much as hue in distances). `similar::palette` counts a picture's thumbnail on a 48×48 grid of pixels. `posts.colors` keeps its most common cells: up to eight with at least 3% of the pixels, and the top three whatever their share. `posts.palette` keeps up to six colours for display, at least a visible step apart.

Uploads work these out at ingest. A startup backfill (`backfill_colors`) reads them once for older posts and doesn't change `updated_at`. A post with no thumbnail, or one that can't be read, waits for a later start.

`color:` turns into `colors && <cells near the colour>`, which the partial GIN index `posts_live_colors` serves. `tag_filter` tries the reaches in `COLOR_REACHES` (0.10, 0.14, 0.18) narrowest first and keeps the first where `EXISTS` finds a post with the whole search, so one search costs at most three extra queries, however many colours it names. The widest reach stops short of a neutral grey for a saturated colour. `-color:` has one fixed reach (0.14), so what it hides doesn't depend on what else is stored.

### Search

Tags are stored on each post as a `BIGINT[]` with a GIN index. A query like `blue_sky night -cloud rating:safe order:score` becomes:

1. Unknown tags are corrected, unless the query says `exact:true` (`correct_typos` in `routes/posts.rs`). One query finds the names that are neither a tag nor an alias. For each, `tags::spelled_like` fetches candidates in use: the top 20 by `pg_trgm` similarity, plus up to 200 of about the same length with the same first letter, as short words share few trigrams. `starbooru_core::closest_tag` then picks one by edit distance. The corrected query is what the rest of the search uses, so the corrections are the same in the gallery, the viewer's neighbours and Explore, and known names cost only that one query.
2. The tag names are resolved to ids, following aliases, and each is expanded to the tags below it in the tree (a recursive CTE on `parent_id`, at most 32 levels). If an included tag doesn't exist, the result is empty, with no scan.
3. The SQL is `tag_ids @> {leaf tags} AND tag_ids && {each parent's tree} AND NOT tag_ids && {excluded trees} AND rating = ANY(...)`.
   A `{girl blonde_hair boots}` term adds `EXISTS` over `figures` and `figure_tags`. A figure's tags are on the post too, so each of its tags also goes into the `tag_ids &&` filter and the GIN index narrows the search first.
4. Rows are ordered by id, score or random, and paginated by limit and offset (at most 200 per page).
5. The ordered, paginated query selects only post ids. The full rows, with tag names and the caller's vote, are built for the ids on the page (`select_page` in `routes/posts.rs`). So rows an `OFFSET` skips cost nothing extra. On a public server each row also gets its uploader (by primary key), its tags' votes (the `tag_votes` primary key) and the tags voted off it (`tag_removals`), per post on the page and never by scanning.

Partial indexes cover live posts, so the recycle bin doesn't slow normal browsing. Explore runs its sections in parallel. Its random picks come from a sample of the table's pages (`TABLESAMPLE SYSTEM`) once the collection has more than 20,000 posts. When the sample has too few matches for a rare search, all the matches are sorted instead. Timings at a million posts are in `development.md` › *Speed at scale*. The query syntax is in `api.md`.

### People

The people lists read character tags, not figures: `tags.post_count` already counts a character's live posts, and most posts from other boorus have character tags without figures. Lists and Explore's section without a search walk `tags` by category and count (the `tags_popular` index) and filter names with the `pg_trgm` index. Each person's picture is one lookup: the first of the character's `tags.face_figures` on a post the reader may see, or else their newest such post through the GIN index, one `UNION ALL` branch per character so each is planned with its own id. `face_figures` holds the faces on the two best-liked live posts of each rating, best first, so a reader who hides a rating still finds one, and no request sorts all of a popular character's figures.

A character's franchise is in `tags.franchise_id`, so lists group, filter and count by it without working it out. `routes::people::run_franchises` fills it in, with `face_figures`, at start, when woken (a figure names a character, a crop of its face is made, or a tag's category, implications, aliases or parent change, which clear `franchise_checked_at`), every 10 minutes for what's due, and again a day after each check. The order: a copyright tag the character implies (fewest steps first), the nearest copyright ancestor in the tree, the franchise its figures (not cosplays) are given most, and the copyright tag most common on its 50 newest live posts. Saving a figure without a franchise uses the same rules at once (`franchises_by_name`).

With a search, Explore's `people` counts characters on the search's 5,000 newest matches, and `popular_characters` on the 1,000 posts with the most net upvotes in the window, the same votes Trending reads.

### Related posts

`GET /posts/{id}/related` ranks live posts by what they share with the post. Each shared tag adds its inverse document frequency, `ln((N + 1) / (post_count + 1))`. So a tag on three posts says a lot and one on half the collection almost nothing. `N` is the planner's row estimate once the table has 10,000 rows, and an exact count below that. A difference hash within 12 bits adds up to 9 more, about what a tag on one post in 10,000 is worth.

Candidates come from two cheap lookups, never a scan:

- the newest 200 posts of each of the post's 8 rarest tags, through the GIN index;
- up to 400 look-alikes whose hash has a 16-bit chunk within one bit of the post's, through four expression indexes (migration 0015). By the pigeonhole rule, that catches every hash within 7 bits.

Only those candidates are scored, and only the top ones get full rows.

## Media storage

```text
/data/original/ab/cd/<sha256>.<ext>
/data/thumb/ab/cd/<sha256>.jpg
/data/sample/ab/cd/<sha256>.jpg
/data/avatar/ab/cd/<sha256>.jpg     (profile pictures, 256 px)
/data/avatar64/ab/cd/<sha256>.jpg   (the same, 64 px)
/data/face/ab/cd/<sha256>.jpg       (crops of figures' faces, 192 px)
/data/tmp/            (staging; writes are atomic renames)
```

Uploads are sniffed by content, not by name. Accepted formats:

| Formats | Kind |
|---|---|
| JPEG, PNG, WebP, BMP, TIFF, AVIF, JPEG XL | images |
| GIF | animation |
| MP4, WebM, MKV, MOV | video |

Thumbnails are JPEGs at most 512px on the long side, except for pictures more than twice as long as they are wide: those get 384px across, up to 3072px along, as a tile shows them cropped to their width. `posts.thumb_stale` marks long pictures whose thumbnail is from before this, and a background task at each start makes them again. The `image` crate decodes the common formats. For video, AVIF and JPEG XL the server asks ffmpeg for a frame (10% into a video, at most 5 s) and ffprobe for the size and length (`posts.duration_ms`). Without ffmpeg those posts have no thumbnail and the client shows a placeholder. Images are decoded with their EXIF orientation applied, so thumbnails, sizes and edits match what a browser shows. At each start a background task retries posts that are still missing one. The gallery plays a muted preview of a video while the mouse is over it.

Originals are kept byte for byte. Big still images also get a **sample**, a smaller copy that the viewer and share pages show instead. The rules:
- The original is a JPEG, PNG, WebP, BMP or TIFF of at least 1 MiB, with no transparency.
- The copy is a JPEG at quality 85, at most 2560 px on the long side, with the original's colour profile.
- It is kept only when it is at most 75% of the original's size. Already compact JPEGs have none.

`posts.has_sample` is null until a post has been looked at. A background task at each start makes samples for older posts. `STARBOORU_SAMPLES=false` stops making them. The viewer swaps in the original when you zoom in or tap *Show original*, or always with Settings › *Always open originals*. Downloads and the editor use the original.

**Profile pictures.** `PUT /auth/avatar` takes a JPEG, PNG, WebP or GIF of up to 5 MiB, sniffed by content, and decodes it within 256 MiB of pixels (a GIF gives its first frame). It cuts the square the client chose, or the largest one in the middle, and scales it to 256 and 64 px JPEGs. Both are named by the SHA-256 of the 256 px one, which goes in `users.avatar_sha`; the upload isn't kept. Two accounts with the same picture share the files. A changed or removed picture's files go once no account names them, under an advisory lock so a picture someone just took up stays. A sweep at each start, after a switch to private and after an account is deleted removes any left over. They are served like thumbnails, through signed `/media/avatar/…` and `/media/avatar64/…` URLs.

**Faces.** A figure with a face box gets a crop of it: the square around the box's middle, its longer side plus a quarter each way, moved or shrunk to stay inside the picture, scaled to a 192 px JPEG (sharp at twice a 96 px circle). `PUT /posts/{id}/figures` only stores the box and wakes `faces::run`, so saving doesn't wait; the task reads each post's picture once (`Storage::still`: the original, or for what `image` can't read the frame the thumbnail comes from) and cuts every face waiting on it. A crop is named by its SHA-256 in `figures.face_sha`, so the same pixels share a file. A box that changed while its crop was being made doesn't take the crop. A replaced, removed or purged face's file goes once no figure names it, under an advisory lock. At each start the task tries again the crops that failed, forgets those whose files are missing (as after restoring a database without its data folder) so they are made again, and removes files nothing names. Served through signed `/media/face/…` URLs.

**Text in images (OCR).** `posts.ocr_at` is null until a still image has been read by tesseract (`ocr.rs`); `posts.ocr_text` holds what it found, or null. A GIN index on `to_tsvector('simple', ocr_text)` for live posts serves `text:` search. Uploads are read in the background, so OCR never delays or fails one; a task at each start reads older posts one at a time, and `POST /posts/{id}/ocr` reads one again. With an AI provider set up, the viewer's *Translate* sends that text (never the image) to it and shows the answer in the app's language, without storing it. For comics and manga, `panels.rs` sends the image itself and keeps the result in `post_translations` (post, language, JSONB blocks with boxes on a 0–1000 scale, model); the viewer draws the blocks over the image with the same zoom transform.

Media URLs look like `/media/thumb/<sha>.jpg?exp=…&sig=…`:
- The signature is an HMAC-SHA256 keyed by `STARBOORU_SECRET`.
- The expiry is rounded to 6-hour buckets, so URLs stay stable and browsers cache them.
- `<img>` and `<video>` tags work without sending the bearer token.

## Modes

A server is private or public (`api.md` › *Modes*). The mode is read from `server_settings` per request, like the other settings, so a switch applies at once.

A switch (`modes::switch`) runs one at a time (a lock in memory and an advisory lock in the database). It makes the backup first, through `backup::create`, then in one transaction deletes what `UNUSED` lists for the new mode (whole tables, or a column of `users` set to NULL), writes the mode, and converts the votes when the score kind in force changes, which going public does. Going public also stores likes as the score kind, so a private server that comes back later starts from likes, and gives a forum without categories the three of `starbooru_core::forum::DEFAULT_CATEGORIES`. Going private removes the profile pictures' files after the transaction. Anything else that gives either mode something of its own adds a row to `UNUSED`.

What only a public server keeps (forum threads and posts, tag votes, profile texts and pictures) is written in a transaction that first calls `modes::hold_public`: it takes the advisory lock shared and checks the mode, so a switch to private can't miss a write that started before it. The forum's routes sit behind `forum::gate`, which answers 403 `forum_off` on a private server, and the posting limits count an account's posts under a lock on its `users` row.

`modes::settle` runs at start, before the server serves anything. When `mode` or `score_kind` is NULL (a new server, or the first start after an upgrade) it stores what `STARBOORU_MODE` and `STARBOORU_SCORES` ask for, if `UNUSED` and `scores::plan` say that deletes and converts nothing, and private with likes otherwise, with a warning. It converts nothing itself. Once both are stored it only compares them with the variables and returns one warning when they differ, which `main.rs` logs.

Votes say what they are row by row (`post_votes.stars` set or not), and posts keep both tallies, so a conversion is two or three `UPDATE`s and a `DELETE`, and nothing reads a vote differently depending on when it was cast. A vote takes the advisory lock shared, so the kind it reads holds until it is saved. `order:score` under stars uses the partial expression index `posts_live_stars` on `(star_sum + 15) / (star_count + 5)`, `starbooru_core::fair_rating`.

On a public server, `guests::gate` runs after routing, so it knows the route: a `GET` without a token to one of `GUEST_READS` is counted against its address and marked, and its handler's `Viewer` is a guest, whose user id (0) belongs to no account. `/remote/*` sits behind a layer of its own that answers 403 on a public server. Explore skips its trending and tag queries on a private one.

## Updates

`update/github.rs` asks `https://api.github.com/repos/StarBooru/starbooru-server/releases/latest` (no credentials, `StarBooru/<version>` as User-Agent, a 20 s timeout, at most 1 MiB) at start and every 6 hours. A check at start is skipped when one ran in the last 10 minutes, so a server restarting in a loop doesn't spend GitHub's 60 an hour. A 404 means no published release yet; a 403 or 429 waits for the next turn. The answer is kept in memory and in `update_state`, and versions compare as Semantic Versioning says (`starbooru_core::Version`).

At start the server works out how it runs (`update::detect`):

- In a container (`/.dockerenv`, `/run/.containerenv` or Docker's folders in `/proc/self/mountinfo`), with the socket from `DOCKER_HOST` (`unix://` only) or `/var/run/docker.sock`: its own id comes from `/proc/self/mountinfo` (Docker mounts the container's `hostname` and `resolv.conf` from a folder named after it), then the cgroup, then the hostname, and Docker confirms it. A container with `com.docker.swarm.service.id` is a Swarm task. Without the socket it can't update itself.
- Otherwise the server package can, when it is a release build (`STARBOORU_VERSION` set at build time), on Linux, its folder is writable, and systemd runs it (`INVOCATION_ID`).

An update runs one at a time and saves its state in `update_state.run` at each step: `checking`, `downloading`, `backing_up` (`backup::create`, unless the request turned it off), `restarting`. The download comes before the backup, so the backup is as recent as it can be when the server switches.

- **Docker.** The server pulls the new image: the same tag for `latest`, `latest-vulkan` and `latest-nvidia`, or the new version's tag with the same suffix for a version tag. It refuses an image that is still the running one (the new version isn't published yet) or whose `org.opencontainers.image.version` label names another version. It then starts a helper container on the image it runs itself (always there, and known to start), as `starbooru-server update-helper`, with the old container's mounts (the socket and the data folder among them), its groups plus the socket's, no network, and `AutoRemove`. The helper stops the old container (30 s to shut down), renames it to `<name>-old-<time>`, creates the new one under the old name from the old one's description with the image's own values taken out (`container::create_body`: environment, labels, volumes including anonymous ones, ports, restart policy, devices and graphics card requests stay; Compose's `com.docker.compose.image` label gets the new image's id), joins its other networks, starts it and waits up to 5 minutes for its health check. Then it removes the old container. If anything fails it removes the new one, gives the old one its name back, writes why to `<data>/update-result.json` and then starts the old one, which reads it as it starts.
- **Swarm.** The server moves the service's image to the new tag (and bumps `ForceUpdate`); Swarm replaces the task.
- **Package.** The server downloads `starbooru-server-<version>-linux-<arch>.tar.gz` and `SHA256SUMS` from the release, checks the sum, unpacks the binary as `starbooru-server.new`, runs it with `version` to see that it starts here and is the right release, keeps a hard link of the old one as `starbooru-server.old`, renames the new one over it and shuts down with exit code 75 for systemd to start it again.

The version that starts next reads `run`: `restarting` and it is the version it was going to, `done`; otherwise `failed`, with the helper's reason when there is one. A server still running 10 minutes after starting the helper or moving the service marks the update failed. Migrations run with `ignore_missing`, so an old version brought back after a failed update starts on a database the new one has migrated.

Automatic updates (off by default) look every 5 minutes: at the chosen hour of the server's clock (`chrono::Local`), when a newer release is known and the server can update itself, they start the same install with a backup. A version that failed is tried again after 20 hours.

## Local AI

Local models run in `starbooru-ai-worker`, a child process that speaks one JSON object per line over stdin and stdout (`starbooru_core::ai_worker`). When memory runs out or native code fails, it dies and the server starts another; its raised `oom_score_adj` makes the kernel pick it before the server or the database. It holds no state worth keeping and exits after 10 idle minutes unless a tool is on Fast.

`starbooru_core::ai_catalog` lists the models. Each has a time and a memory footprint measured on two Zen 3 cores, and its files are pinned to a commit and a SHA-256 in `ai_files.rs` (generated by `tools/ai-catalog/pin.py`). Models were chosen for their job: Danbooru-trained taggers instead of general vision models, dedicated translators, and a comic-text detector with Manga OCR.

Which model runs:

- `Machine` scales the reference times by a calibration: two tiny models embedded in the worker, one float and one quantized, timed for at least a second, so a container's CPU limit shows, and stored with a hardware fingerprint in `server_settings.ai_calibration`. The fingerprint includes the limit.
- Once a model is installed, `measure_next` times it for real, in a worker with nothing else loaded and the model itself already loaded (the budgets don't count reading it from disk); that replaces the estimate. Graphics card timings count only when the card gave the same results as the processor and was faster. A measured time over the preset's budget moves the tool to another model by itself, and a model whose timing fails isn't chosen until it's installed again.
- Fast is the most accurate model within 1 s; none greys it out. Balanced is the most accurate within 5 s, or else the quickest, so it's only greyed out when nothing fits in memory (keeping 1.5 GB free). Accurate is the most accurate `high_accuracy` model within 30 s; none greys it out, and a tool set to a preset that went out of reach runs Balanced. `high_accuracy` marks what published benchmarks put at the top of its field: the WD Large taggers and Danbooru Tagger v1 on a shared benchmark, TranslateGemma on WMT24++.
- `sweep` runs at startup, every 5 minutes and whenever a setting changes. It prepares the runtime, downloads what enabled tools need (one at a time, resumable) and measures. The tool stays usable on what it has and retries failures by itself.

Packaging: the default build loads Microsoft's `libonnxruntime.so` from next to the worker. `AI_GPU=vulkan` adds a statically linked WebGPU worker (`libwebgpu_dawn.so`, Mesa); `AI_GPU=nvidia` adds a CUDA worker and NVIDIA's libraries. The GPU worker is named `starbooru-ai-worker-gpu` and is used only when a card really runs a small model through it; otherwise the processor's takes over.

