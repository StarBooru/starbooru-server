# Deployment

The server ships as one ready-made image, `ghcr.io/starbooru/starbooru-server`, next to a PostgreSQL container. The repository's `docker-compose.yml` runs it and is ready for Dokploy. The image includes the web app, a pinned release of [starbooru-web](https://github.com/StarBooru/starbooru-web), served from the same address. To serve another build of it, mount its bundle and point `STARBOORU_WEB_DIR` at it.

## Dokploy

1. **Create a Compose service.** In your project, choose *Create Service › Compose*. Set *Provider* to this Git repository, the branch, and *Compose Path* `./docker-compose.yml`.
2. **Environment.** In the *Environment* tab, paste the variables from `.env.example` and fill them in:
   ```sh
   openssl rand -hex 24   # POSTGRES_PASSWORD (letters and digits only; it goes into a URL)
   openssl rand -hex 32   # STARBOORU_SECRET
   ```
3. **Domain.** In the *Domains* tab, add your domain for service **`starbooru`**, port **`8080`**, with HTTPS (Let's Encrypt). Dokploy adds the Traefik labels and network. Don't publish host ports.
4. **Deploy.** This downloads the image, so nothing is compiled on your server.
5. **Create the admin account.** Point any StarBooru app at `https://your-domain` and choose *Create account*, or call `POST /api/v1/auth/register`. The first account becomes admin. Registration then closes unless `STARBOORU_ALLOW_REGISTRATION=true`.
6. **Backups.** StarBooru backs the database up itself (see [Backups](#backups)) into the `media` volume. In *Volume Backups*, back up the `media` volume off the server: it then holds the files and the database backups. Backing up `pgdata` as well does no harm, but a copy of a running database's files may not restore cleanly; the `pg_dump` archives always do.

Every app signs in with the same URL (`https://your-domain`).

### Upload size

`STARBOORU_MAX_UPLOAD_MB` (default 512) is the server's limit. Dokploy's Traefik doesn't cap request bodies by default. The apps send anything over 2 MiB in chunks of 2 MiB, so a proxy's limit on request bodies doesn't get in their way, including Cloudflare's 100 MB on the free plan. Only other tools that use `POST /posts` with a whole file still have to fit it.

**Cloudflare and HTTP/3.** Uploads over HTTP/3 through Cloudflare were measured at 80 to 220 KB/s, against about 20 MB/s over HTTP/2 on the same connection, and Cloudflare gave up on a request after about 70 s. Chunks get through at that speed, as the apps drop to one at a time, but slowly. Browsers use HTTP/3 where they can, so turn it off under *Speed › Protocol Optimization › HTTP/3 (with QUIC)* in Cloudflare's dashboard. Browsers remember it for a day, so restart them after the change.

### Video thumbnails

The image ships ffmpeg (most of its roughly 750 MB). The server uses it for thumbnails, size and length of videos, AVIF and JPEG XL. Without it, those posts show a placeholder. Each start retries posts that still lack a thumbnail, so installing ffmpeg later fills them in.

## Backups

The server writes `pg_dump` archives (custom format) of the database to `STARBOORU_DATA_DIR/backups`:

- **Automatically,** every `STARBOORU_BACKUP_DAYS` days (default 1), keeping the newest `STARBOORU_BACKUP_KEEP` (default 7). The first check runs five minutes after start-up, then hourly.
- **By hand,** from Settings › Administration › Backups. Backups made by hand are kept until an admin deletes them.

Admins download and delete backups on the same page and change the schedule there. Archives hold every account's password hash, so only admins signed in to the app can reach them (API keys can't).

A backup taken while a library import is waiting or running also holds the key to the other site, as it's kept until the import ends.

A backup holds the database only. The media files are immutable and named by their hash, so copying the data folder (or the `media` volume) is all they need. That includes the crops of faces (`face/`); if they are missing after a restore, the server makes them again from the originals at its next start.

The image ships the PostgreSQL 17 client. `pg_dump` must be at least as new as the database server, so update the image too if you move the `db` service to a newer PostgreSQL. Elsewhere, install the client or point `STARBOORU_PG_DUMP` at it.

### Restoring

1. Stop the `starbooru` service. Leave `db` running.
2. Copy the archive into the `db` container and load it over the current database:
   ```sh
   docker cp starbooru-20261027-031500.dump <db-container>:/tmp/restore.dump
   docker exec <db-container> pg_restore --clean --if-exists --no-owner \
       -U starbooru -d starbooru /tmp/restore.dump
   ```
3. Put the media files back if they were lost, into the `media` volume, as they were.
4. Start `starbooru` again. Migrations newer than the backup run at start-up.

## Sign-in with an identity provider

StarBooru can also sign people in with an OpenID Connect provider such as Authentik, Keycloak, Authelia or Google. Passwords keep working next to it.

1. At the provider, create an OAuth2/OpenID application: confidential client, authorization code flow, scopes `openid profile email`. Set the redirect URI to `https://your-domain/api/v1/auth/oidc/callback`.
2. Set `STARBOORU_PUBLIC_URL=https://your-domain`, `STARBOORU_OIDC_ISSUER` (the provider's issuer URL, the one that serves `/.well-known/openid-configuration`), `STARBOORU_OIDC_CLIENT_ID`, `STARBOORU_OIDC_CLIENT_SECRET` and, for the button, `STARBOORU_OIDC_NAME`.
3. Redeploy. The sign-in screen shows *Sign in with …*.

Someone who signs in this way for the first time gets a new account when registration is open (or when the server has no accounts yet, as admin). With registration closed, an admin keeps control: people sign in once with a password, then link the provider account in Settings › Ways to sign in. Accounts made by the provider have no password until they set one in Settings.

The desktop and Android apps open the system browser and receive the answer on a loopback address, so nothing else needs registering at the provider.

## Private or public

A server starts private: it is yours or your household's, and everything in it needs an account. A public server is open to other people with their own accounts (turn on `STARBOORU_ALLOW_REGISTRATION` or the setting for sign-ups), and to anyone without one, who may browse.

What a public server shows to anyone who finds its address:

- every post outside the recycle bin, with its tags, rating, source, score and people, and its files through the signed media links;
- the tags, their tree, aliases and implications, and spelling suggestions;
- Explore, trending posts and popular tags included;
- the accounts' profiles: display name, the text about themselves, picture, what they uploaded and wrote, and who uploaded each post;
- the forum.

They can't vote, upload, edit, tag, post in the forum, save searches or see the recycle bin, the list of accounts or anything in Administration. Each address may make 300 requests a minute without an account (an IPv6 network counts as one address); past that it waits, with `Retry-After`. Behind a reverse proxy, set `STARBOORU_TRUST_PROXY` so the limit counts each visitor rather than the proxy. A public server counts likes and dislikes, shares posts by their own address instead of share links, and doesn't search other boorus, which can't be turned on there. Library imports stay, for administrators.

People with an account on a public server also vote on each post's tags: a tag voted down to −3 comes off that post, and edits don't put it back. Administration › Server sets that number (0 turns it off), and lists the tags taken off, each with an undo. The forum starts with three categories, General, Requests and Site, which admins rename, reorder or add to. Each account may post once every 15 seconds and 100 times a day; admins aren't limited. Admins also clear a profile's picture, text or display name from the list of accounts.

To switch, use Administration in the app (or `POST /api/v1/admin/mode`). It first shows what the other mode has no use for and will delete. Going public deletes the share links and the saved accounts and searches on other boorus, and turns star votes into likes (3 stars, neither, are dropped). Going private deletes the profile pictures and texts, the forum's threads and posts, and the votes on tags; display names and the forum's categories stay, for a later switch back. It then backs the database up, unless you say not to, and switches.

A private server can count 1 to 5 stars instead of likes (Administration › Server). Changing it converts the votes after showing how many: a like becomes 5 stars and a dislike 1, 4 or 5 stars a like and 1 or 2 a dislike.

`STARBOORU_MODE` and `STARBOORU_SCORES` set the starting mode and score kind of a new server. The first start stores them, and from then on they have no effect: changes go through Administration, which shows what they delete or convert first. If one is later set to something other than what the server keeps, the server logs a warning at start and keeps what it has. Unset and empty are the same, so the compose file passes them empty by default. When you upgrade from a version without modes, the server stores private with likes, which is what it was. It follows the variables instead only if that deletes and converts nothing (going public with no share links and no saved booru accounts or searches, or stars with no votes yet); otherwise it logs a warning naming the variable and keeps private with likes. A server that starts public gets the forum's three categories then, as a switch would give it. So does one that was public before the forum came (0.3.0), on its first start since; once a forum has had them, a start never adds them again, so categories an admin deleted stay deleted.

Administration › Server also sets the blacklist new accounts start with, which guests get too, and the home page: a title over the most popular posts rated safe, or a search of your own. Before anyone signs in, a private server's home page shows the title and no pictures.

## Configuration

Admins can change the name, open registration, the recycle-bin days, the score kind, the tag-vote threshold, link previews (off until turned on) and the backup schedule in the app (Settings › Administration). Those changes apply at once and override the variables below.

| Variable | Default | Meaning |
|---|---|---|
| `DATABASE_URL` | required | PostgreSQL URL (the compose file builds it from `POSTGRES_PASSWORD`). The migrations add the `pg_trgm` extension. That needs PostgreSQL 13 or newer, where the database's owner can add it, or a superuser on anything older. The bundled container is fine. |
| `STARBOORU_SECRET` | required | At least 32 characters; signs media URLs and encrypts the keys of saved accounts on other boorus. Changing it invalidates existing media links, which clients refresh on reload, and makes those saved accounts unusable until they are entered again. Keep it with your backups: a restored database needs the same one to open them. |
| `STARBOORU_NAME` | `StarBooru` | Instance name shown at sign-in |
| `STARBOORU_ALLOW_REGISTRATION` | `false` | Allow sign-ups after the first (admin) account |
| `STARBOORU_MODE` | `private` | The mode of a new server, `private` or `public` (see [Private or public](#private-or-public)). Stored on the first start, then changed only in Administration. |
| `STARBOORU_SCORES` | `likes` | The score kind of a new server, `likes` (likes and dislikes) or `stars` (1 to 5, private servers only). Stored on the first start, then changed only in Administration. |
| `STARBOORU_MAX_UPLOAD_MB` | `512` | Largest upload, 1 to 16384 |
| `STARBOORU_SAMPLES` | `true` | Make smaller JPEG copies of big images for viewing (see `architecture.md` › *Media storage*). Turned on later, copies are made for older posts at the next start. |
| `STARBOORU_AUTO_TAGS` | `true` | Tag posts with what the file shows, in the `meta` category: `lowres`, `highres`, `absurdres`, `wide_image`, `tall_image`, `animated`, `video`, `sound`, `long_video` (see `architecture.md` › *Automatic meta tags*). Each post is tagged once; turned on later, older posts are tagged at the next start. Admins can override it in Administration › Server. |
| `STARBOORU_TRASH_DAYS` | `0` | Purge recycle-bin posts (and their files) after this many days; `0` keeps them until emptied by hand |
| `STARBOORU_PUBLIC_URL` | unset | The address people open StarBooru at, e.g. `https://booru.example.com` (a path below it works too). Needed for provider sign-in. Link previews put it in front of the addresses chat apps fetch; unset, they take the address from the request (see `api.md` › *Link previews*). |
| `STARBOORU_OIDC_ISSUER` | unset | OpenID Connect issuer URL (https). With the client id and secret, turns provider sign-in on. |
| `STARBOORU_OIDC_CLIENT_ID` | unset | Client id from the provider |
| `STARBOORU_OIDC_CLIENT_SECRET` | unset | Client secret from the provider |
| `STARBOORU_OIDC_NAME` | `SSO` | Provider name on the *Sign in with …* button |
| `STARBOORU_BACKUP_DAYS` | `1` | Days between automatic database backups, 0 to 365; `0` turns them off |
| `STARBOORU_BACKUP_KEEP` | `7` | Automatic backups kept, 1 to 100 |
| `STARBOORU_BACKUP_DIR` | `STARBOORU_DATA_DIR/backups` | Where backups are written |
| `STARBOORU_PG_DUMP` | `pg_dump` | pg_dump (installed in the image); `off` turns backups off |
| `STARBOORU_FFMPEG` | `ffmpeg` | ffmpeg for video, AVIF and JPEG XL thumbnails (installed in the image); `off` disables it |
| `STARBOORU_FFPROBE` | `ffprobe` | ffprobe, for video size and length |
| `STARBOORU_OCR` | `true` | Read text in still images with tesseract (installed in the image), for `text:` search and the viewer's *Text in image*. Runs on the server only; no image leaves it. `off` turns it off; a path picks another tesseract. Without tesseract it is quietly off (logged once at start). Older posts are read in the background at each start. |
| `STARBOORU_OCR_LANGS` | `eng` (`eng+jpn+jpn_vert` in `docker-compose.yml`) | tesseract languages, joined with `+`, e.g. `eng+ita`. The image has English and Japanese (`jpn` across, `jpn_vert` down, for manga); fewer languages read faster. For another language add its package (`tesseract-ocr-ita`) in the `Dockerfile` and [compile the image yourself](#compiling-the-image-yourself). |
| `STARBOORU_URL_IMPORT` | `on` | Import from URL: `on` (public addresses only), `private` (also private-network addresses, e.g. a NAS; anyone with an account can then make the server fetch from your LAN), `off`. Library imports follow it, except that an admin's can always reach the server's own network; `off` turns them off for everyone too. |
| `STARBOORU_AI_URL` | unset | Base URL of an OpenAI-compatible API, such as `https://openrouter.ai/api/v1`, `https://api.openai.com/v1`, `https://generativelanguage.googleapis.com/v1beta/openai` or a local server's own (`http://192.168.1.5:11434/v1`). What a new install's tools start with; each tool can have its own in Administration › Server › AI. Thumbnails of posts, and the text of images being translated, are sent to it. |
| `STARBOORU_AI_MODEL` | unset | The API's model, such as `google/gemini-2.5-flash-lite` or `gpt-4o-mini` |
| `STARBOORU_AI_KEY` | unset | API key, sent as a bearer token and only to this address. Never shown to clients or logged. |
| `STARBOORU_AI_AUTOTAG` | `false` | Add suggested tags to every new upload in the background (two at a time) |
| `STARBOORU_LOCAL_AI` | `false` | Start the tools on StarBooru's own models unless an API is set (see *Local AI* below) |
| `STARBOORU_AI_TAGGING` | see meaning | Where tag suggestions start: `off`, `local` or `api`. Unset: `api` when `STARBOORU_AI_URL` is set, else `local` when `STARBOORU_LOCAL_AI` is on, else `off`. |
| `STARBOORU_AI_TRANSLATION` | see meaning | Where translations start, the same way |
| `STARBOORU_AI_COMICS` | see meaning | Where comic translation starts, the same way |
| `STARBOORU_AI_PEOPLE` | `off` | Where finding people starts: `off`, `local` or `api`. Off unless set, as it runs the tagger a few times for each picture. |
| `STARBOORU_AI_WORKER` | next to the server | A specific AI worker binary, instead of `starbooru-ai-worker-gpu` then `starbooru-ai-worker` |
| `STARBOORU_AI_TRANSLATE_TO` | unset | Translate new uploads in which OCR finds text into this language (`en`, `it`, `pt-BR`), drawn over the image (see *Comics and manga* below). Unset: only when someone asks in the viewer. |
| `STARBOORU_TRUST_PROXY` | `false` (`true` in `docker-compose.yml`) | Take the client address from the last `X-Forwarded-For` entry, for the sign-in limits and the limit on visitors without an account. Turn it on only behind a reverse proxy that sets the header (Traefik does); otherwise clients could pick their own address. |
| `STARBOORU_BIND` | `0.0.0.0:8080` | Listen address |
| `STARBOORU_DATA_DIR` | `/data` in the image | Media storage |
| `STARBOORU_WEB_DIR` | `/app/web` in the image, else unset | A compiled web app to serve, such as starbooru-web's bundle; unset means the server runs API-only. `starbooru-server precompress <dir>` writes a `.br` and a `.gz` of each text file, which are then sent as they are; without them the server compresses each file as it goes out. |
| `STARBOORU_UPDATE_CHECK` | `true` | Ask GitHub for new releases at start and every 6 hours, and tell admins (see [Updating](#updating)). `false` sends no request at all. Admins can override it. |
| `STARBOORU_AUTO_UPDATE` | `false` | Install new releases by themselves, with a backup first, where the server can update itself. Admins can override it. |
| `STARBOORU_AUTO_UPDATE_HOUR` | `4` | The hour (0 to 23) automatic updates start at, in the server's time zone |
| `TZ` | UTC in the image | The server's time zone, such as `Europe/Rome`, for the hour above |
| `STARBOORU_UPDATE_URL` | GitHub's newest release | For testing updates: another address that answers like `https://api.github.com/repos/StarBooru/starbooru-server/releases/latest` |
| `STARBOORU_UPDATE_IMAGE` | the image the container runs | For testing updates: another image repository to pull from, such as `localhost:5000/starbooru-server` |
| `RUST_LOG` | `info,tower_http=info,sqlx=warn` | Log filter |

## Local AI

Tag suggestions, translation, comic translation and finding people can run inside StarBooru, on models made for each job. Nothing leaves the server and there is no per-image cost. In Administration › Server › AI each tool is **Off**, **Local** or **API**.

**Local** needs nothing else: choose a speed and the server downloads the model, resumes after a failure, checks every file's SHA-256, restarts the worker if it dies, downloads again any file that gets damaged, and frees the memory after 10 idle minutes. Models live in `STARBOORU_DATA_DIR/ai/models`, so they survive updates (and are in volume backups of `/data`; leave that folder out to save space).

| Speed | Runs | Greyed out |
|---|---|---|
| Fast (1 s) | The most accurate model that answers within 1 s on this machine | When none does |
| Balanced (5 s) | The most accurate model that answers within 5 s, or else the quickest one | Only when no model fits in memory |
| Accurate (30 s) | The most accurate *High accuracy* model that answers within 30 s | When none does |

*High accuracy* marks models that published benchmarks put at the top of their field: for tags, the three that lead the shared [Danbooru Tagger v1 comparison](https://huggingface.co/Redstonexs/danbooru-tagger-v1); for translation, [TranslateGemma on WMT24++](https://arxiv.org/abs/2601.09012). The first runs time each model on this machine and replace the estimate; a model whose time is still estimated shows `≈`. A model that fails its timing isn't chosen. Longer text takes longer than the limit, which is for a typical answer. If a speed goes out of reach later, the tool runs Balanced.

Under *Models* you can install, uninstall or use any model that fits in memory; picking a speed goes back to the automatic choice.

| Tool | Model | Download | 2 cores | Radeon RX 9060 XT | Memory | License |
|---|---|---|---|---|---|---|
| Tags | ML-Danbooru TResNet (320 and 448 px) | 236 MB | 0.2 and 0.3 s | 0.03 and 0.05 s | 0.5 GB | MIT |
| Tags | JoyTag, also for photographs | 366 MB | 0.9 s | 0.07 s | 0.8 GB | Apache-2.0 |
| Tags | ML-Danbooru CAFormer | 287 MB | 1.1 s | 0.2 s | 0.9 GB | MIT |
| Tags | WD ViT v3 | 378 MB | 0.8 s | 0.07 s | 0.8 GB | Apache-2.0 |
| Tags | WD ConvNext v3 | 395 MB | 0.7 s | 0.06 s | 0.8 GB | Apache-2.0 |
| Tags | WD SwinV2 v3 | 470 MB | 0.9 s | 0.09 s | 0.9 GB | Apache-2.0 |
| Tags | WD ViT Large v3 (high accuracy) | 1.3 GB | 3.8 s | 0.2 s | 1.9 GB | Apache-2.0 |
| Tags | Danbooru Tagger v1 (high accuracy; 24,000 tags, no characters or rating) | 660 MB | 4.6 s | 0.3 s | 2.9 GB | Apache-2.0 |
| Tags | WD EVA02 Large v3 (high accuracy) | 1.3 GB | 3.7 s | 0.4 s | 2.1 GB | Apache-2.0 |
| Translation | Opus-MT | 354 MB, plus about 120 MB per extra language | 0.5 s | | 0.8 GB | Apache-2.0 |
| Translation | M2M-100 418M | 640 MB | 1.3 s | | 2.2 GB | MIT |
| Translation | MADLAD-400 3B | 3.2 GB | 1.4 s | | 5.3 GB | Apache-2.0 |
| Translation | TranslateGemma 4B (high accuracy) | 3.1 GB | 11 s | 1.5 s | 2.7 GB | Gemma Terms |
| Comics | Detector S, Manga OCR, Opus-MT | see above | 3.9 s | | 1.1 GB | Apache-2.0 |
| Comics | Detector L, Manga OCR, Opus-MT | see above | 4.5 s | | 1.2 GB | Apache-2.0 |
| Comics | Detector S, Manga OCR, M2M-100 | see above | 4.5 s | | 2.6 GB | Apache-2.0, MIT |
| Comics | Detector L, Manga OCR, M2M-100 | see above | 4.9 s | | 2.7 GB | Apache-2.0, MIT |
| Comics | Detector L, Manga OCR, MADLAD-400 | see above | 5.2 s | | 5.7 GB | Apache-2.0 |
| Comics | Detector L, Manga OCR, TranslateGemma (high accuracy) | see above | 38 s | 6 s | 3.1 GB | Apache-2.0, Gemma Terms |
| People | YuNet, anime face detector, WD SwinV2 v3 | 12 MB plus the tagger | 3.6 s | | 1.0 GB | MIT, Apache-2.0 |
| People | YuNet, anime face detector, WD EVA02 Large v3 (high accuracy) | 12 MB plus the tagger | 15 s | | 2.2 GB | MIT, Apache-2.0 |

Times are for one picture, one sentence, one page of eight balloons or a picture of two people with the model already loaded, measured on two cores of a Ryzen 5900X and on the card through Vulkan. The first answer after loading also reads the model from disk, up to about 10 s more for the big ones. An empty card column means the processor is faster and stays in use. A slower server takes longer; a Raspberry Pi hasn't been measured. So on those two cores Accurate is greyed out for comics, and with that card all three speeds run the High accuracy models for tags.

**API** sends the same work to any OpenAI-compatible address, hosted or on your network, such as Ollama (`http://host:11434/v1`), LM Studio or llama.cpp. Each tool has its own address, model and key. Tagging, comics and people need a model that reads images. For people the server still finds the faces itself when it can (it downloads the two face detectors, 12 MB, and needs the AI runtime) and sends their boxes with the picture; otherwise the model finds them too.

### Graphics cards

The default image runs on the processor (x86-64 and ARM64). A card needs an image that includes its driver layer. When the card can't be used, StarBooru falls back to the processor.

```sh
# NVIDIA (CUDA): needs the NVIDIA Container Toolkit; adds about 3 GB
docker compose -f docker-compose.yml -f docker-compose.nvidia.yml up -d
# AMD, Intel, or NVIDIA through Vulkan
docker compose -f docker-compose.yml -f docker-compose.gpu.yml up -d
```

If `/dev/dri/renderD128` isn't open to everyone on the host, set `STARBOORU_RENDER_GID` to its group (`stat -c %g /dev/dri/renderD128`). With several cards the NVIDIA image uses NVIDIA; the Vulkan image uses the one the driver layer ranks first. On Dokploy add the same keys to the compose file, including the `image` line. Both images are x86-64 only.

Upgrading from the Ollama setup: the `ollama` service is gone, and its volume is no longer used (`docker volume rm <project>_ollama` frees the space). `STARBOORU_LOCAL_AI_URL` and `STARBOORU_LOCAL_AI_MODEL` are ignored. A custom Ollama saved in the panel becomes an API with the same address and model; one set only through those variables needs `STARBOORU_AI_URL` and `STARBOORU_AI_MODEL` instead (with `/v1` on the address).

## Comics and manga

OCR is what finds text in images, for `text:` search and the viewer's *Text in image*. For comic and manga pages, translation goes further: each balloon, caption and sound effect is found and read, and its translation is drawn over it.

- In the viewer, the translate button on the image (bottom left) translates the page into the app's language and shows it over the picture; it then toggles the translation on and off. Hovering a balloon shows the original text. Translations are kept per post and language, so each page is translated once.
- To translate new uploads automatically, set *Translate new comics to* (Administration › Server › AI) or `STARBOORU_AI_TRANSLATE_TO`. A new image is translated after OCR finds text in it, one at a time, so OCR has to be on. Older posts aren't translated in bulk.
- **Local:** a detector finds the text, Manga OCR reads Japanese and tesseract reads the rest, and the translator writes it. Nothing leaves the server. Korean and Chinese depend on the tesseract languages installed.
- **API:** the page goes to the model (fit in 1536 px), so it must read images and the provider sees the page.

## Importing a library from another booru

Settings › Import from another booru moves a whole library from szurubooru, Shimmie2, Danbooru, Stash, Blombooru, Monbooru or LANBooru: every post with its tags, rating, source, date and your likes, plus aliases, implications, and pools or albums as tags. The other site has to be running, and the StarBooru server has to be able to reach it. A file that's already here merges into its post, so importing twice is safe.

| From | What it needs on that side |
|---|---|
| szurubooru | User name and a login token, or you get what guests can see. |
| Shimmie2 | The Ouroboros API extension turned on, and an API key if guests can't see everything. Pools can't come across (no API), nor aliases of several words. |
| Danbooru | User name and API key, or you get what guests can see and no favourites. Only favourites become likes. |
| Stash | The API key, if it has one. No safety ratings, so every post gets the rating you choose. Performers become characters, studios, directors and photographers artists, groups copyrights, and galleries a tag. |
| Blombooru | An admin's API key for aliases and implications. Tag-pattern implications can't come across. |
| Monbooru | A token with read access, if the site uses them. Every gallery comes across. Aliases can't (its API doesn't say which tag one stands for). |
| LANBooru | Nothing: its reads are open. It has no aliases, implications or albums. |

**Stays behind everywhere:** comments, other users and their accounts, notes on images, titles and descriptions, site-wide scores and the order inside pools. StarBooru has no place for them. szurubooru's YouTube posts are skipped, as they have no file. Files over `STARBOORU_MAX_UPLOAD_MB` or in a format StarBooru refuses are listed as failed, and *Import again* tries them after you've raised the limit.

**Who can import what:** an admin can import from the server's own network and brings aliases and implications. Anyone else reaches what `STARBOORU_URL_IMPORT` allows and brings posts only.

## Plain Docker Compose

```sh
cp .env.example .env        # fill in the two secrets
docker compose -f docker-compose.yml -f docker-compose.local.yml up -d
# → http://localhost:8080
```

## Compiling the image yourself

Only needed for a commit that isn't released yet or a changed `Dockerfile`, such as an added OCR language. It takes several minutes. Add `docker-compose.build.yml` last:

```sh
docker compose -f docker-compose.yml -f docker-compose.local.yml -f docker-compose.build.yml up -d --build
# For a graphics card, with its override and the matching AI_GPU (vulkan or nvidia)
AI_GPU=vulkan docker compose -f docker-compose.yml -f docker-compose.gpu.yml -f docker-compose.build.yml up -d --build
```

On Dokploy, set *Compose Path* to a file of your own that has the `build` section, as Dokploy reads one compose file.

## Health

- `GET /healthz` returns `ok` when the database answers.
- The image's `HEALTHCHECK` runs `starbooru-server healthcheck`, so no curl is needed.

The container runs as uid 10001. A fresh named volume gets the right ownership automatically. If you bind-mount a host directory instead, `chown 10001:10001` it.

## Updating

When a new release is out, admins see it in the app (Administration › Updates) with its notes, unless `STARBOORU_UPDATE_CHECK=false`. The server asks GitHub at start and every 6 hours, sending nothing about the install.

### By hand

Redeploy, or run `docker compose pull && docker compose up -d`: both download the newest release. Database migrations run at startup, and the media layout is stable. Back up first when the release notes mention a migration.

To stay on one version, set `STARBOORU_VERSION` (`0.2.0`). Without it you get `latest`, the newest published release.

### From the app

A container can update itself when it has the Docker socket. **The Docker socket is root on the host:** whoever controls the container could control the machine. That is why it's off unless you turn it on. Updating by hand works without it.

To turn it on, uncomment the two lines in `docker-compose.yml` that mount `/var/run/docker.sock` and add the socket's group, and set `STARBOORU_DOCKER_GID` to that group (`stat -c %g /var/run/docker.sock`). The container runs as uid 10001, so it needs the group to use the socket. On Dokploy, Compose services read the compose file from the repository, so keep a compose file of your own with those lines (or switch the service's provider to *Raw* and paste it), and add `STARBOORU_DOCKER_GID` in *Environment*.

*Update now* then:

1. backs the database up (on by default; it can be turned off for that update),
2. pulls the new image: `latest` again, or the new version's tag for a container that runs a version (`-vulkan` and `-nvidia` stay),
3. starts a short-lived helper container, which stops the server's container, makes a new one with the same name, volumes, networks and aliases, environment, labels (Compose's and Traefik's, so Dokploy and `docker compose` still know it), ports, restart policy, devices and graphics cards, and waits for it to be healthy. Only then does it remove the old container. If the new one doesn't come up, the old one starts again and the app shows why.

The server is away for a few seconds while it switches. A container that runs a version (`STARBOORU_VERSION=0.2.0`) moves to the new version's tag, but the compose file still names the old one, so a redeploy would go back: change `STARBOORU_VERSION` too. The app says so. Images updated this way stay on disk; `docker image prune` removes the old ones.

A Docker Swarm service updates the same way, except that Swarm replaces the task. The server must run on a manager node.

### Automatic updates

In Administration › Updates (or with `STARBOORU_AUTO_UPDATE=true`), the server installs new releases by itself at the hour you choose, after a backup. It needs what *Update now* needs. If an automatic update fails, the old version keeps running and the next try is a day later. The hour is in the server's time zone: UTC in the image unless you set `TZ`.

### The server package

The release's `starbooru-server-<version>-linux-x86_64.tar.gz` runs without Docker (no local AI). It can update itself when systemd runs it and it may write to its own folder. It downloads the new package, checks it against the release's `SHA256SUMS`, puts the new binary in place, keeps the old one as `starbooru-server.old`, and exits with code 75 for systemd to start it again. To go back, stop the service and rename `starbooru-server.old` to `starbooru-server`.

A unit for it, as `/etc/systemd/system/starbooru.service`:

```ini
[Unit]
Description=StarBooru server
After=network-online.target postgresql.service
Wants=network-online.target

[Service]
User=starbooru
# The binary's folder, which the starbooru user must be able to write to
WorkingDirectory=/opt/starbooru
ExecStart=/opt/starbooru/starbooru-server
# Variables from .env.example, with DATABASE_URL and STARBOORU_DATA_DIR
EnvironmentFile=/opt/starbooru/.env
Restart=always
RestartSec=2

[Install]
WantedBy=multi-user.target
```

```sh
sudo systemctl daemon-reload && sudo systemctl enable --now starbooru
```
