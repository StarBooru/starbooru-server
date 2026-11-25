# Development

## Toolchain

- Rust 1.88 or newer (developed on 1.98). For the wasm check of the shared crates: `rustup target add wasm32-unknown-unknown`.
- Docker, for PostgreSQL and the image.
- Optional: ffmpeg (video, AVIF and JPEG XL thumbnails), tesseract (OCR) and the PostgreSQL client (`pg_dump`, backups).

## Run the server

```sh
# throwaway database
docker run -d --rm --name sb-pg -p 55432:5432 \
  -e POSTGRES_USER=starbooru -e POSTGRES_PASSWORD=devpass -e POSTGRES_DB=starbooru \
  pgvector/pgvector:pg17

export DATABASE_URL=postgres://starbooru:devpass@127.0.0.1:55432/starbooru
export STARBOORU_SECRET=dev-secret-dev-secret-dev-secret-0000
export STARBOORU_BIND=127.0.0.1:8080
cargo run -p starbooru-server
```

Migrations run at startup. The first account you register becomes admin.

To serve a web app's bundle from the server, set `STARBOORU_WEB_DIR` to its folder. `cargo run -p starbooru-server -- precompress <dir>` writes `.br` and `.gz` copies beside its files. It's optional: without them the server compresses each file as it sends it.

Local AI needs the worker: `cargo build -p starbooru-ai-worker` (`target/debug/` is next to the server). The server downloads the pinned ONNX Runtime into `<data>/ai/runtime/` by itself on Linux x86-64 and ARM64. It comes from Microsoft's NuGet package, as in the `Dockerfile`: the server reads just the library out of it with range requests (about 12 MB), unpacks no more than its pinned size and checks its SHA-256, or downloads the whole package (160 MB) and checks that when ranges don't work. Elsewhere, point `STARBOORU_ORT_LIB` at a `libonnxruntime` 1.31.

## Tests and lints

The gate before finishing a task is in `AGENTS.md` › *Before you finish a task*. While iterating:

Server integration tests run the real router against PostgreSQL. Each test creates its own database and drops it afterwards. Point them at a server whose user may create databases; without the variable they are skipped:

```sh
export STARBOORU_TEST_DATABASE_URL=postgres://starbooru:devpass@127.0.0.1:55432/starbooru
```

The AI worker's tests in `apps/ai-worker/tests/worker.rs` also run models when `STARBOORU_ORT_LIB` and `STARBOORU_AI_MODELS` are set. The people test also needs `STARBOORU_TEST_FACES`, a folder with two public-domain pictures from Wikimedia Commons: `p11.jpg` ([Apollo 11 Crew](https://commons.wikimedia.org/wiki/File:Apollo_11_Crew.jpg)) and `d02.png` ([Moe anime girl in sailor fuku](https://commons.wikimedia.org/wiki/File:Moe_anime_girl_in_sailor_fuku.png), CC0). `STARBOORU_TEST_NUGET=1` fetches the runtime from NuGet to check the pin.

The harness is `apps/server/src/testing.rs`. Tests for video, AVIF and JPEG XL thumbnails need `ffmpeg` on `PATH` and are skipped without it.

```sh
cargo test -p starbooru-core
cargo test -p starbooru-server --bins
cargo clippy -p starbooru-core -p starbooru-client --target wasm32-unknown-unknown -- -D warnings
```

## Speed at scale

`apps/server/src/bench.rs` is an ignored test. It seeds generated posts straight into a test database (one million by default):
- 5 to 24 tags a post, taken from 5,000 tags, with popularity falling off steeply;
- 2% of the posts in the bin;
- votes on 5%;
- a random difference hash on each post, except 1% that share a blank one (hash 0);
- 3 to 8 colour cells a post, drawn towards white, black and grey, with teal the rarest;
- a fifth of the tags characters and a fifth copyrights, and a figure on one post in 5 naming its first character, half of them with a kept face.

It then works out every character's franchise (12 s for 1,000 characters and 200,000 figures, in the background on a real server) and times the API calls the clients make most, through the real router, and fails if any median is over 400 ms. The last three calls run after a switch to public, where Explore also works out trending posts, popular tags and popular characters. Run it in release:

```sh
STARBOORU_BENCH_POSTS=1000000 cargo test --release -p starbooru-server search_at_scale -- --ignored --nocapture
```

Set `STARBOORU_BENCH_KEEP=1` to keep the seeded database for `EXPLAIN ANALYZE` (the name is printed; drop it afterwards).

Results at 1,000,000 posts (Ryzen 9 5900X, PostgreSQL 17 in Docker, default settings; median of 5 runs; seeding took 113 s). In the call column, `page=1000` is 60,000 posts deep, and `tag_1` is on half the posts.

| Call | Median |
|---|---|
| `/posts`, `/posts?page=50`, every `order:` but random | 2.5–2.8 ms |
| `/posts?page=1000` | 5.4 ms |
| `/posts?q=tag_1`, `tag_1 tag_2 tag_3`, `tag_2 -tag_1`, `rating:e type:video` | 3.2–4.0 ms |
| `order:longside`, `width:>3900`, `mpixels:>12`, `-width:>1000`, `date:<7d` | 2.4–2.7 ms |
| `ratio:16:9`, `tag_1 width:>1920`, `date:>30d`, `date:2026-10` | 3.8–19 ms |
| `tag_300 date:>30d` | 30 ms |
| `/posts?q=tag_4000` (a rare tag) | 18.8 ms |
| `color:white`, `color:red`, `color:teal`, `color:000080`, `-color:white`, `tag_1 color:blue`, `color:red color:blue`, `color:red order:score` | 3.1–4.1 ms |
| `color:ff00ff` (nothing stored nearby, so it widens the search) | 6.6 ms |
| `tag_300 color:teal` (a rare tag and a rare colour; it varies between runs) | 13–190 ms |
| `tgag_300` (corrected to `tag_300`) | 9.3 ms |
| `qqqqqqqq` (nothing close) | 1.1 ms |
| `/tag-corrections` with three names, two of them unknown | 4.5 ms |
| `/posts?q=order:random` | 141 ms |
| `/posts/{id}`, `/posts/{id}/neighbors`, tag search | under 1.2 ms |
| `/posts/{id}/related` (a random hash, and a blank one shared by 10,000 posts) | 24–37 ms |
| `/explore`, `/explore?q=tag_1` on a private server | 22–31 ms |
| `/explore`, `/explore?q=tag_1` on a public server (Trending is most of it) | 74–120 ms |
| `/people` (every order and filter), `/franchises` | 2.1–4.5 ms |
| `/admin/stats` | 112 ms |

`order:random` sorts every match by `random()`, so it grows with the collection. The admin stats count the whole table.

## Making a release

Releases are plain versions (`v0.2.0`); there are no more betas. Cargo's version follows them: the workspace `Cargo.toml` holds the version of the next release (now `0.2.0`), and the commit you tag must have the tag's version there.

Before tagging, move the web app's pin in the `Dockerfile` to the newest starbooru-web release: `WEB_VERSION`, and `WEB_SHA256` from that release's `SHA256SUMS` (`gh release download v<version> -R StarBooru/starbooru-web -p SHA256SUMS -O -`).

`.github/workflows/release.yml` builds the server package (`starbooru-server-<version>-linux-x86_64.tar.gz`) on a push of a `v*` tag and puts it in a draft release. Running the workflow by hand builds without releasing, which is how to try a change to the workflow; the filename then says `main`.

What the server shows comes from `STARBOORU_VERSION`, which both workflows set to the tag at build time (`starbooru_core::VERSION`). A local build without it shows Cargo's version and counts as a development build, which doesn't update itself.

`.github/workflows/image.yml` builds the image from the same tag and pushes it to `ghcr.io/starbooru/starbooru-server` under its version: x86-64 and ARM64, plus `<version>-vulkan` and `<version>-nvidia`. Publishing the release moves `latest`, and `latest-vulkan` and `latest-nvidia` with it, which is what every install runs, so a draft reaches nobody. Running it by hand builds an existing tag again, with `latest` moved only if you tick that.

Write the notes (the workflow attaches a `SHA256SUMS`, which servers that update themselves check the package against), wait for the Image workflow to finish, as publishing needs its images, then publish with `gh release edit <tag> --draft=false`. Servers see the release within 6 hours, and the notes are what their admins read before updating.

The apps release from their own repositories. A release that changes the API says so in its notes, so the apps can follow.

### Trying an update

Two variables point a server at something other than GitHub and the image's own repository: `STARBOORU_UPDATE_URL`, an address that answers like `releases/latest` (a JSON file served by any web server will do), and `STARBOORU_UPDATE_IMAGE`, an image repository such as a local registry (`docker run -d -p 5000:5000 registry:2`). Build the image twice with different `STARBOORU_VERSION` build arguments, push both, run the older one with the Docker socket, and serve a release JSON that names the newer version. For the package, build it as `release.yml` does, serve a second tarball and its `SHA256SUMS` from the release JSON's `assets`, and run the first under `systemd-run --user -p Restart=always`.
