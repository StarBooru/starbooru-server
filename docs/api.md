# HTTP API (v1)

All endpoints are under `/api/v1`. Bodies are JSON unless noted. The Rust types are in `crates/core/src/model.rs`, and `starbooru-client` wraps every call.

**Auth.** Send `Authorization: Bearer <token>`. Every endpoint except `info`, `auth/register` and `auth/login` requires it, but on a public server people without an account may also read posts and tags (see [Guests](#guests)). The token is a session token from sign-in (`sb_…`) or an [API key](#api-keys) (`sbk_…`).

**Ratings.** A rating is `safe`, `sensitive`, `questionable` or `explicit`. `general` is also accepted in requests and means `safe`; answers always say `safe`.

**Errors.** Errors are returned as `{"code": "...", "message": "..."}` with a matching status:

| Status | Meaning |
|---|---|
| 400 | bad input (`bad_request`, or `weak_password` for a new account whose password is too easy to guess) |
| 401 | missing or expired token |
| 403 | not allowed (`forbidden`, `quota_exceeded` for an upload past the account's storage limit, `remote_off` for other boorus on a public server, or `profiles_off`, `forum_off` and `tag_votes_off` for what only a public server has) |
| 404 | not found |
| 409 | conflict (`duplicate`, `username_taken`, `deleted`, `not_deleted`, `implication_cycle`, `name_taken`, and those of [Modes](#modes), the [Forum](#forum) and [Share links](#share-links)) |
| 413 | upload too large |
| 507 | `storage_full`: the server's disk is full |
| 415 | unsupported file type |
| 424 | `fetch_failed`: another site the request depends on failed, such as an address to import or an AI provider. |
| 429 | rate limited (`rate_limited`: sign-in limits, guests and forum posting); `starbooru-client` waits out `Retry-After` (up to 30 s) and retries twice |
| 500 | internal error (details only in the server log) |

## Server

| Method | Path | |
|---|---|---|
| GET | `/info` | `ServerInfo`: `name`, `version`, `has_users`, `registration_open`, `max_upload_bytes`, `url_import` (whether `POST /posts/import` is on), `oidc` (the identity provider's name for a "Sign in with …" button, `null` when OpenID Connect sign-in is off), `video_editing` (whether `POST /posts/{id}/edit` can trim videos: the server has ffmpeg), `mode` (`private` or `public`, see [Modes](#modes)), `score_kind` (`likes` or `stars`, see [Scores](#scores)), `guests` (people without an account may browse), `home` (the [home page](#home-page) is on), `link_previews` (the admin's switch for [link previews](#link-previews)) |
| GET | `/healthz` (not under `/api/v1`) | `ok` when the database answers |

`name` and `registration_open` follow the admin settings (see [Administration](#administration)). A server older than modes leaves out `mode`, `score_kind` and `guests`; read them as `private`, `likes` and `false`. One older than the home page leaves out `home`, and one older than link previews leaves out `link_previews`: read both as `false`.

```json
{ "name": "StarBooru", "version": "0.2.0", "has_users": true, "registration_open": false,
  "max_upload_bytes": 536870912, "url_import": true, "oidc": null, "video_editing": true,
  "mode": "public", "score_kind": "likes", "guests": true, "home": true, "link_previews": false }
```

## Accounts

| Method | Path | Body | Returns |
|---|---|---|---|
| POST | `/auth/register` | `{username, password}` | `AuthResponse {token, user}`. The first account becomes admin. Later accounts need `STARBOORU_ALLOW_REGISTRATION`. 400 `weak_password` if the password is too easy to guess. 429 after 10 attempts from one address in an hour. |
| POST | `/auth/login` | `{username, password}` | `AuthResponse`. 401 for a wrong username or password. 429 `rate_limited` with `Retry-After` once an account has 10 failures in 15 minutes (the right password waits too) or an address has 50. A successful sign-in resets the account's count. |
| POST | `/auth/logout` | | 204; revokes this token |
| GET | `/auth/me` | | `User` |
| GET | `/auth/usage` | | `Usage {posts, bytes, quota_bytes}`: what the account uploaded (recycle bin included) and its storage limit (`null` for none) |
| PUT | `/auth/password` | `PasswordChange {current, new}` | 204. Signs out every other session of the account. 400 if `current` is wrong or `new` breaks the password rules. 429 after 10 wrong `current` passwords in 15 minutes. An account made by a provider sign-in has no password until it sets one; `current` is then ignored. |
| GET | `/auth/sessions` | | `SessionInfo[] {id, client, created_at, last_used_at, expires_at, current}`: the account's live sessions, this one first. `client` is a short description such as "Firefox on Linux", derived from the User-Agent at sign-in (the header itself is not stored). `last_used_at` is updated at most every 5 minutes. |
| DELETE | `/auth/sessions/{id}` | | 204; signs that session out. 404 if it isn't one of yours. |
| DELETE | `/auth/sessions` | | `RevokeResult {revoked}`; signs out every session but this one |
| GET | `/auth/profile` | | `Profile`, the caller's own (see [Profiles](#profiles)) |
| PATCH | `/auth/profile` | `ProfileUpdate {display_name?, about?}` | `Profile` |
| PUT | `/auth/avatar?x=&y=&size=` | the picture's bytes | `Profile` with the new picture |
| DELETE | `/auth/avatar` | | `Profile` without a picture |
| GET | `/auth/blacklist` | | `Blacklist {entries}` (see [Blacklist](#blacklist)) |
| PUT | `/auth/blacklist` | `Blacklist {entries}` | `Blacklist`, as kept |
| GET | `/auth/sharing` | | `SharingSettings {previews}`: whether the account's share links show a [link preview](#link-previews) when they don't choose for themselves. `false` until changed. |
| PUT | `/auth/sharing` | `SharingSettings {previews}` | `SharingSettings`, as kept |

Account rules:
- Usernames are 2 to 32 characters of `A–Z a–z 0–9 _ . -` and are unique regardless of case.
- Passwords are 8 characters or more.
- A new account's password must also not be too easy to guess. `starbooru_core::password_is_weak` decides, and the app uses the same function, so the two always agree. It looks for common passwords and words (letter swaps like `P@ssw0rd` included), keyboard runs, sequences, repeats, years and the username. Changing a password or an administrator's reset only checks the length.
- Tokens last 90 days.
- With an API key, the `/auth/logout`, `/auth/password` and `/auth/sessions` endpoints return 403 `forbidden`.
- A disabled account gets 403 `forbidden` at sign-in (after the right password), and its sessions and keys answer 401.

### Provider sign-in (OpenID Connect)

On when the server has `STARBOORU_OIDC_*` set (`/info` then names the provider in `oidc`); otherwise these endpoints answer 404.

1. `POST /auth/oidc/begin` with `OidcBegin {return_to}` answers `OidcStart {url, handle}`. Keep `handle` secret and open `url` in a browser.
2. The provider sends the browser back to `GET /auth/oidc/callback` on the server, which sends it on to `return_to` with `oidc_code=<code>` added to the query, or `oidc_error=denied` (the user said no) or `oidc_error=failed`.
3. `POST /auth/oidc/finish` with `OidcFinish {code, handle}` answers `AuthResponse`, like `/auth/login`.

`return_to` must be a page of the server's own web app (under `STARBOORU_PUBLIC_URL`) or a loopback address (`http://127.0.0.1:<port>/…`, `http://[::1]…`, `http://localhost…`) a native app listens on; anything else is 400. A sign-in must be finished within 10 minutes, once. A code with the wrong handle, used twice or too late is 400.

At `finish`, a provider account seen before signs in to the account it is linked to. A new one makes an account, if registration is open or there are no accounts yet (the first becomes admin); its username comes from the provider's `preferred_username` or email, made unique with a number. With registration closed it is 403 `forbidden`, as for a disabled account.

| Method | Path | Body | Returns |
|---|---|---|---|
| POST | `/auth/oidc/begin` | `OidcBegin {return_to}` | `OidcStart {url, handle}`. 429 when too many sign-ins are pending. |
| POST | `/auth/oidc/begin-link` | `OidcBegin` | `OidcStart`, to link a provider account to the signed-in one |
| GET | `/auth/oidc/callback` | | 303 to `return_to`. An unknown or expired sign-in gets a short HTML page (400) instead. |
| POST | `/auth/oidc/finish` | `OidcFinish {code, handle}` | `AuthResponse` |
| POST | `/auth/oidc/link` | `OidcFinish` | `Identity`. Only the account that called `begin-link` can finish it. 409 `identity_taken` if that provider account is linked already. |
| GET | `/auth/identities` | | `SignInMethods {password, identities: Identity[] {id, label, created_at, last_used_at}}`: whether the account has a password, and its linked provider accounts. `label` is the provider's username or email. |
| DELETE | `/auth/identities/{id}` | | 204. 400 if it is the account's last way to sign in (no password, no other identity). 404 if it isn't yours. |

The `begin-link`, `link` and `identities` endpoints need a session; API keys get 403.

## API keys

Long-lived tokens for scripts and other tools. A key acts as the user who made it, with these limits:
- It can't manage sign-ins, the password or keys. Those endpoints answer 403 `forbidden`, so a leaked key can't take over the account.
- A read-only key answers 403 `forbidden` ("this API key is read-only") to anything but `GET` and `HEAD`.

Managing keys needs a sign-in session. Changing the password keeps the account's keys; remove any you no longer trust.

| Method | Path | Body | Returns |
|---|---|---|---|
| GET | `/keys` | | `ApiKey[] {id, name, prefix, read_only, created_at, last_used_at, expires_at}`, newest first. `prefix` is the first 12 characters of the token. `last_used_at` is `null` until first use and is updated at most every 5 minutes. `expires_at` is `null` for keys that don't expire. Expired keys stay listed until removed. |
| POST | `/keys` | `NewApiKey {name, read_only?, expires_days?}` | `CreatedApiKey {key, token}`. The token is shown only here; the server keeps only its SHA-256. The name is 1–64 characters with whitespace collapsed. `expires_days` is 1–3650, or omitted for never. 400 past 50 keys. |
| DELETE | `/keys/{id}` | | 204; the key stops working at once. 404 if it isn't the caller's. |

## Administration

Admins only; others get 403. API keys may read these but not change anything.

| Method | Path | Body | Returns |
|---|---|---|---|
| GET | `/admin/stats` | | `ServerStats`: `version`, `started_at`, `users`, `disabled_users`, live `images`/`animations`/`videos`, `bin_posts`, `tags`, `media_bytes` (originals, bin included), `bin_bytes`, `database_bytes`, `uploads_by_day` (`[{day, posts}]`, the last 30 UTC days oldest first), `max_upload_bytes`, `video_thumbnails`, `url_import` |
| GET | `/admin/settings` | | `ServerSettings {name, allow_registration, trash_days, auto_meta_tags, mode, score_kind, tag_vote_threshold?, default_blacklist, home, link_previews}`, the values in force. `tag_vote_threshold` is there on a public server only (see [Tag votes](#tag-votes)). `default_blacklist` is what new accounts start with and guests get (see [Blacklist](#blacklist)); `home` is the [home page](#home-page). `link_previews` turns [link previews](#link-previews) on; it is `false` until an admin changes it. |
| PUT | `/admin/settings` | `ServerSettings` | The saved settings. They apply at once. Each starts from its environment variable (`STARBOORU_NAME`, `STARBOORU_ALLOW_REGISTRATION`, `STARBOORU_TRASH_DAYS`, `STARBOORU_AUTO_TAGS`), and a value equal to the variable's keeps following it. The mode and the score kind are stored as they are and don't follow the environment. `name` is 1–64 characters with whitespace collapsed; `trash_days` is 0–3650. `auto_meta_tags` (automatic meta tags such as `highres` and `video` on new posts) may be left out by older clients and then means `true`. `mode` changes only through `POST /admin/mode`: the mode in force is accepted, another one is 409 `use_mode_switch`, and leaving it out is fine. `score_kind` left out stays as it is; a new one converts the votes (see [Scores](#scores)) and the answer lists what it did in `changes`, as `GET /admin/scores/preview` would have. `stars` on a public server is 409 `public_counts_likes`. `tag_vote_threshold` is 0 to 100; left out, it stays as it is. `default_blacklist` is cleaned like an account's list, at most 100 entries; `home` is checked as [Home page](#home-page) says. Either left out stays as it is, and so does `link_previews`. |
| GET | `/admin/users` | | `AdminUser[] {id, username, role, created_at, disabled, posts, bytes, quota_bytes, last_active, display_name?, about?, avatar_url?}`, oldest first. `posts` and `bytes` count uploads, recycle bin included. `last_active` is the latest use of any session or key. The profile fields are left out when not set; `avatar_url` is the signed 64 px picture. |
| PATCH | `/admin/users/{id}` | `UserUpdate {role?, disabled?, quota_bytes?, clear_avatar?, clear_about?, clear_display_name?}` | The updated `AdminUser`. Missing fields stay as they are; `"quota_bytes": null` removes the limit. Disabling signs the account out everywhere. The `clear_*` flags (`true`) remove that part of the profile, for moderation. 400 for a role, disabled or limit on your own account (so an admin always remains); clearing your own profile is fine. 404 for an unknown id. |
| POST | `/admin/users/{id}/password` | `PasswordReset {new}` | 204. Signs the account out everywhere; its API keys keep working. 400 for your own account (use `PUT /auth/password`). |
| DELETE | `/admin/users/{id}` | | 204. Removes the account with its sessions, keys, votes (taken off the scores), votes on tags, saved searches and profile picture. Its posts, threads and forum posts stay, without an author. 400 for your own account. |
| GET | `/admin/ai` | | `AiAdminStatus {hardware, runtime {ready, reason, gpu}, autotag, auto_translate, tools, more_tools}`: per tool (`tagging`, `translation`, `comics` in `tools`; `people` in `more_tools`, which apps from before it don't know and skip) its `settings {mode, preset, model, api_url, api_model, api_key_set}`, `presets [{preset, available, model, est_ms}]`, `models [{id, name, size, ram_mb, high_accuracy, est_ms, measured, gpu, installed, fits, progress {done, total}, error}]`, `active`, `state` (`off`, `ready`, `installing`, `failed`) and `error`. Keys are never returned. |
| PUT | `/admin/ai` | `AiGeneralUpdate {autotag, auto_translate}` | `AiAdminStatus`. `auto_translate` is a language code, or `""` for off, that new images with OCR text are translated into; 400 when comics are off. |
| PUT | `/admin/ai/tools/{tool}` | `AiToolUpdate {mode, preset, model, api_url, api_model, api_key?}` | `AiAdminStatus`. `mode` is `off`, `local` or `api`; `preset` is `fast`, `balanced` or `accurate`; an empty `model` follows the preset. API mode needs `api_url` and `api_model`. `api_key` missing keeps the stored key, `""` clears it. The models it needs start downloading. 400 for invalid values or another tool's model. |
| POST | `/admin/ai/tools/{tool}/test` | | `AiTestResult {ms}`: a short text chat with the tool's saved API. 400 outside API mode; 424 `fetch_failed` when it errors. |
| POST | `/admin/ai/models/{id}/install` | | `AiAdminStatus`. Queues the download; poll `GET /admin/ai` for `progress` and `error`. |
| DELETE | `/admin/ai/models/{id}` | | `AiAdminStatus`. 400 while a tool uses the model. Of its files, removes those the fewest other installed models need, so models sharing a file with it can stop being installed. |

An upload or URL import that would take the uploader past `quota_bytes` answers 403 `quota_exceeded`.

### Backups

Database backups (`pg_dump` archives, custom format). They hold every account's password hash, so these need an admin signed in with a session: API keys get 403 even for `GET`.

| Method | Path | Body | Returns |
|---|---|---|---|
| GET | `/admin/backups` | | `Backups {available, running, schedule, items}`. `available` is false when `pg_dump` is missing or turned off. `schedule` is `BackupSchedule {every_days, keep}`. `items` is `Backup[] {name, bytes, created_at, automatic, url}`, newest first; `automatic` marks the schedule's backups. |
| POST | `/admin/backups` | | The new `Backup`, once written (this can take a while). 403 when backups are unavailable, 409 `backup_running` while another one is being made. |
| PUT | `/admin/backups/schedule` | `BackupSchedule` | The saved schedule. `every_days` is 0–365 (0 turns automatic backups off); `keep` is 1–100. Each starts from its variable (`STARBOORU_BACKUP_DAYS`, `STARBOORU_BACKUP_KEEP`). Automatic backups beyond `keep` are removed at once; ones made by hand stay. |
| DELETE | `/admin/backups/{name}` | | 204; 404 for an unknown name. |

A `Backup`'s `url` is a signed path relative to the server, valid for an hour. Fetch it without the bearer token; it answers with the file as an attachment.

### Updates

The server asks GitHub for the newest published release (not a draft or a pre-release) at start and every 6 hours, unless the checks are off. Admins only; API keys may read `GET /admin/updates` but not change anything.

| Method | Path | Body | Returns |
|---|---|---|---|
| GET | `/admin/updates` | | `UpdateStatus`, below |
| POST | `/admin/updates/check` | | `UpdateStatus` after asking GitHub now. 429 `rate_limited` with `Retry-After` within a minute of the last one; 403 when the checks are off. A failed check is not an error here: `check_error` says what went wrong. |
| PUT | `/admin/updates/settings` | `UpdateSettings {check, auto, auto_hour}` | `UpdateStatus`. `auto_hour` is 0 to 23; 400 otherwise. Each starts from its variable (`STARBOORU_UPDATE_CHECK`, `STARBOORU_AUTO_UPDATE`, `STARBOORU_AUTO_UPDATE_HOUR`). |
| POST | `/admin/updates/install` | `UpdateInstall {backup}`, or no body | 202 with `UpdateStatus` once the update has started; follow `run` with `GET /admin/updates`. `backup` (default `true`) backs the database up first. 409 `up_to_date` when nothing newer is known, `cannot_update` when `method.can_update` is false, `backup_unavailable` when a backup was asked for and `pg_dump` is missing or off, `update_running` while one is under way. |

`UpdateStatus`:

| Field | |
|---|---|
| `current` | The version running, as `/info` says |
| `latest` | `Release {version, name, published_at, notes, url}` from the last check that worked: `notes` is Markdown, `url` the release's page. `null` before the first check, and when GitHub has no release yet (it answers 404 while every release is a pre-release). |
| `available` | `latest` is newer than `current` (Semantic Versioning: `0.1.0-beta.6` < `0.2.0` < `0.10.0`) |
| `checked_at`, `check_error` | When the last check ran, and why it failed (`null` when it worked). A failed check keeps the `latest` found before it. |
| `settings` | `UpdateSettings`: `check` (ask GitHub; off sends nothing), `auto` (install new releases by themselves, with a backup, at `auto_hour` in the server's time) |
| `method` | `UpdateMethod {kind, can_update, reason?, message?, image?, pinned}`, below |
| `server_time` | The server's clock with its offset, such as `2026-12-01T03:12:00+01:00`, which `auto_hour` is in |
| `run` | `UpdateRun {state, from, to, automatic, started_at, finished_at, error, backup}`: the update under way, or the last one's result. `state` is `checking`, `downloading`, `backing_up`, `restarting`, `done` or `failed`. `backup` names the backup made before it. The server goes away while `restarting`; the version that starts next sets `done` or `failed`, so it survives the restart. |

`method.kind` says how the server runs:

| `kind` | How it updates |
|---|---|
| `docker` | A container. With the Docker socket it pulls the new image and replaces itself through a short-lived helper container, keeping the name, volumes, networks, labels and the rest; if the new container doesn't become healthy, the old one starts again. `image` is the image it runs. `pinned` is true for a version tag (`…:0.2.0`): the update pulls the new version's tag, but the compose file still names the old one, so a redeploy goes back to it until `STARBOORU_VERSION` is changed. |
| `swarm` | A Docker Swarm task: the service is moved to the new image and Swarm replaces it. |
| `package` | The server package under a service manager: it downloads the release's tarball, checks it against `SHA256SUMS`, swaps the binary (keeping `starbooru-server.old`) and exits to be started again. |
| `manual` | Neither, such as a development build. |

When `can_update` is false, `reason` says why and `message` says the same in English: `no_docker_socket` (a container without the socket: run `docker compose pull && docker compose up -d`, or Redeploy in Dokploy), `docker_unreachable` (the socket is there but Docker refused or didn't know the container), `unknown_image` (a tag that is neither `latest` nor a version, such as a local build, or an image pinned by digest), `removed_on_stop` (`docker run --rm`), `development_build`, `not_supervised` (no systemd to start it again), `read_only_folder`, `unsupported`.

Example:

```json
{
  "current": "0.2.0",
  "latest": {
    "version": "0.3.0",
    "name": "StarBooru server 0.3.0",
    "published_at": "2026-12-01T09:00:00Z",
    "notes": "## What's new\n\n- …",
    "url": "https://github.com/StarBooru/starbooru-server/releases/tag/v0.3.0"
  },
  "available": true,
  "checked_at": "2026-12-01T10:00:00Z",
  "check_error": null,
  "settings": { "check": true, "auto": false, "auto_hour": 4 },
  "method": { "kind": "docker", "can_update": true, "image": "ghcr.io/starbooru/starbooru-server:latest", "pinned": false },
  "server_time": "2026-12-01T11:00:00+01:00",
  "run": { "state": "done", "from": "0.1.0", "to": "0.2.0", "automatic": true,
           "started_at": "2026-11-30T03:00:12Z", "finished_at": "2026-11-30T03:01:40Z",
           "error": null, "backup": "starbooru-20261130-030014.dump" }
}
```

## Modes

A server is private (the default) or public. A private server is one person's or a household's: every reader needs an account. A public one is open to other people with their own accounts, and people without one may browse, read-only. `/info` says which, so an app can adapt before anyone signs in.

| | Private | Public |
|---|---|---|
| Reading without an account | no | posts, tags and Explore (see [Guests](#guests)) |
| Scores | likes and dislikes, or 0 to 5 stars | likes and dislikes |
| Sharing | share links | a post's own address; `POST /shares` is 409 `share_by_address` |
| Other boorus (`/remote/*`) | as set up | off: 403 `remote_off`, and it can't be turned on |
| Explore's `trending`, `tags` and `popular_characters` | empty | worked out |
| Library imports | anyone, under the URL import rules | administrators only |
| Profiles | display names only, which nobody else sees | name, text, picture and stats, for everyone; posts say who uploaded them (see [Profiles](#profiles)) |
| Forum | off: 403 `forum_off` | on (see [Forum](#forum)) |
| Votes on tags | off: 403 `tag_votes_off` | on (see [Tag votes](#tag-votes)) |

Switching deletes what the new mode has no use for. Going public deletes every share link and every saved account and search on other boorus, and turns star votes into likes; the forum starts with three categories (General, Requests, Site) if it has none. Going private deletes the profile pictures and texts, the forum's threads and posts, the votes on tags and the records of tags voted off posts. Display names and the forum's categories stay, for a server that goes public again; the tags voted off stay off posts, but edits may add them again.

| Method | Path | Body | Returns |
|---|---|---|---|
| GET | `/admin/mode/preview?to=public\|private` | | `ModePreview {to, changes}`: what switching would delete and convert, without changing anything. Empty `changes` when nothing, or when the server is in that mode already. Admins; API keys may read it. 400 for another `to`. |
| POST | `/admin/mode` | `ModeSwitch {to, backup?}` | `ModeSwitched {settings, changes, backup}` once the switch is done. `backup` (default `true`) makes a database backup first, named in the answer and listed in `GET /admin/backups`. Then everything happens in one transaction: the deletions, the votes converted, the new mode. Needs an admin signed in with a session; API keys get 403. 409 `same_mode` when the server is in that mode, `backup_unavailable` when a backup was asked for and `pg_dump` is missing or off (say `"backup": false` to switch without one), `backup_running` while another backup is being made, `backup_failed` when it failed (nothing changed then), `mode_switch_running` while another switch is under way. |
| GET | `/admin/scores/preview?to=likes\|stars` | | `ScorePreview {to, changes}`: what `PUT /admin/settings` with that `score_kind` would convert and delete. 409 `public_counts_likes` for stars on a public server. |

A `Change` is `{kind, count, message}`: a machine name, how many, and the same in an English sentence. Only kinds with something to do are listed. The kinds are in `starbooru_core::mode::changes`:

| `kind` | |
|---|---|
| `share_links` | Share links, deleted going public |
| `booru_accounts` | Saved accounts on other boorus, deleted going public |
| `remote_searches` | Saved searches on other boorus, deleted going public |
| `star_votes_converted` | Votes of 1, 2, 4 or 5 stars that become likes and dislikes |
| `star_votes_dropped` | Votes of 3 stars, which are neither and are deleted |
| `like_votes_converted` | Likes and dislikes that become stars |
| `profile_pictures` | Profile pictures, deleted going private |
| `profile_texts` | Texts accounts wrote about themselves, deleted going private |
| `forum_threads` | Forum threads, deleted going private |
| `forum_posts` | Forum posts, deleted going private |
| `tag_votes` | Votes on posts' tags, deleted going private |
| `tag_removals` | Records of tags voted off posts, deleted going private |

```json
GET /api/v1/admin/mode/preview?to=public
{ "to": "public",
  "changes": [
    { "kind": "share_links", "count": 12,
      "message": "12 share links will be deleted; on a public server a post is shared by its own address" },
    { "kind": "booru_accounts", "count": 3, "message": "3 saved accounts on other boorus will be deleted" },
    { "kind": "star_votes_converted", "count": 40,
      "message": "40 star votes will become likes and dislikes: 4 or 5 stars a like, 1 or 2 a dislike" },
    { "kind": "star_votes_dropped", "count": 6,
      "message": "6 votes of 3 stars will be deleted, as 3 stars is neither a like nor a dislike" } ] }

POST /api/v1/admin/mode   {"to": "public"}
{ "settings": { "name": "StarBooru", "allow_registration": true, "trash_days": 0,
                "auto_meta_tags": true, "mode": "public", "score_kind": "likes" },
  "changes": [ { "kind": "share_links", "count": 12,
                 "message": "12 share links were deleted; on a public server a post is shared by its own address" } ],
  "backup": "starbooru-20261124-022248.dump" }

GET /api/v1/admin/mode/preview?to=private
{ "to": "private",
  "changes": [
    { "kind": "profile_pictures", "count": 2, "message": "2 profile pictures will be deleted" },
    { "kind": "profile_texts", "count": 1, "message": "1 profile text will be deleted; display names stay" },
    { "kind": "forum_posts", "count": 14, "message": "14 forum posts will be deleted" },
    { "kind": "forum_threads", "count": 3, "message": "3 forum threads will be deleted; the categories stay" },
    { "kind": "tag_votes", "count": 9, "message": "9 votes on tags will be deleted" },
    { "kind": "tag_removals", "count": 1,
      "message": "1 record of a tag voted off a post will be deleted; the tags stay off, and edits may add them again" } ] }
```

`STARBOORU_MODE` and `STARBOORU_SCORES` set the mode and score kind of a new server. The first start stores them; after that only this API changes them (see `deployment.md` › *Private or public*).

### Guests

On a public server a request without a token may read what a browser needs, and nothing else:

| Method | Path |
|---|---|
| GET | `/posts`, `/posts/{id}`, `/posts/{id}/neighbors`, `/posts/{id}/related` |
| GET | `/tags`, `/tags/{name}`, `/tag-corrections` |
| GET | `/explore`, `/people`, `/franchises` |
| GET | `/users/{name}` |
| GET | `/forum`, `/forum/threads`, `/forum/threads/{id}` |
| GET | `/auth/blacklist` (the admin's default), `/home` |

`/info`, sign-up and sign-in, the signed `/media` URLs and the pictures of [link previews](#link-previews) are open in both modes, and so is `/home`, which on a private server shows no posts without a token. Everything else answers 401 to a guest, as it does on a private server, where none of these opens without a token. A guest has no vote of their own (`my_vote` is 0) and doesn't see the recycle bin: `status:deleted` is 401 and a binned post 404. A token that is sent but wrong is still 401.

Each client address (IPv6 by its /64, the last `X-Forwarded-For` entry with `STARBOORU_TRUST_PROXY`) may ask for 300 a minute without an account; `/explore` counts as 5 and `/people` as 2. Past that the answer is 429 `rate_limited` with `Retry-After`. Signed-in requests don't count. Page sizes are the same as for accounts.

### Scores

A vote on a post is a like or a dislike, or, on a private server set to stars, 1 to 5 stars. Admins choose with `score_kind` in `PUT /admin/settings` (likes until changed, or what `STARBOORU_SCORES` gave a new server); a public server always counts likes.

| Method | Path | Body | Returns |
|---|---|---|---|
| PUT | `/posts/{id}/vote` | `VoteRequest {value?, stars?}` | `Post` with its new tallies |

- **Likes:** `value` is 1, -1, or 0 to remove the vote; `stars` is 400. `score` is likes minus dislikes, as before.
- **Stars:** `stars` is 1 to 5, or 0 to remove the vote. An app that sends only `value` is read as likes: 1 gives 5 stars, -1 gives 1, 0 removes it.

Under stars a post also carries `stars` (the average, to two decimals), `votes` (how many) and `my_stars` (the caller's, 1 to 5). They are left out under likes and when there is nothing to say. `score` and `my_vote` keep their types for older apps: a vote of 4 or 5 counts as a like there, 1 or 2 as a dislike and 3 as neither. So `score` is the score the post would have in likes.

`order:score` under stars sorts by a fair rating, a Bayesian average: the post's stars as if it already had five votes of three,

```text
rating = (sum of stars + 3 × 5) / (number of votes + 5)
```

A post nobody voted on sits at 3, and one vote of 5 (3.33) doesn't beat fifty of 4 (3.91). Ties go by id.

Changing the kind converts the votes in place: a like becomes 5 stars and a dislike 1; 4 or 5 stars become a like, 1 or 2 a dislike, and votes of 3 are deleted. `score` doesn't move either way. Nothing else converts votes: the server never does at start.

```json
PUT /api/v1/posts/7/vote   {"stars": 4}
{ "id": 7, "score": 2, "my_vote": 1, "stars": 4.33, "votes": 3, "my_stars": 4, "…": "…" }
```

### Profiles

An account has a display name, a text about itself and a picture, besides its username, which stays the handle. On a public server anyone, guests included, sees them; a private one keeps display names only and shows them to nobody else.

| Method | Path | Body | Returns |
|---|---|---|---|
| GET | `/users/{name}` | | `Profile {name, display_name, about, avatar_url, avatar_small_url, role, joined, posts, forum_posts}`. The username in any case. `posts` counts uploads outside the recycle bin, `forum_posts` forum posts not deleted. 404 for an unknown name; 403 `profiles_off` on a private server. |
| GET | `/auth/profile` | | The caller's own `Profile`, in either mode |
| PATCH | `/auth/profile` | `ProfileUpdate {display_name?, about?}` | `Profile`. A field left out stays; `null` or `""` clears it. `display_name` is trimmed with its spaces collapsed, 1 to 40 characters, no control or invisible characters (such as right-to-left marks). `about` is plain text, its lines kept, at most 500 characters. Setting `about` on a private server is 403 `profiles_off`; clearing it works. 400 for too long or a bad character. |
| PUT | `/auth/avatar?x=&y=&size=` | The picture itself (any `Content-Type`), at most 5 MiB | `Profile`. A JPEG, PNG, WebP or GIF (its first frame), told by its content. Cut to the square at `x`, `y` with sides of `size`, in pixels of the picture as stored (give all three or none), or to the largest square in the middle; at least 16 px. Kept as two JPEGs, 256 and 64 px, and the upload itself isn't. Public servers only: 403 `profiles_off` otherwise. 400 for a square outside the picture, a picture smaller than 16 px or an empty body, 413 over 5 MiB, 415 for another format. |
| DELETE | `/auth/avatar` | | `Profile`, in either mode |

`avatar_url` (256 px) and `avatar_small_url` (64 px) are signed `/media` paths, like a post's; both are `null` without a picture, and the apps draw an initial instead. The same picture uploaded twice gets the same address.

On a public server a `Post` names its uploader in `uploader`, a `UserCard {name, display_name, avatar_url, avatar_small_url}`, left out on a private server and once the account is deleted. Forum threads and posts carry the same card as `author`.

```json
GET /api/v1/users/ann
{ "name": "ann", "display_name": "Ann", "about": "I tag skies.\nAnd cats.",
  "avatar_url": "/media/avatar/9f…c2.jpg?exp=1764000000&sig=…",
  "avatar_small_url": "/media/avatar64/9f…c2.jpg?exp=1764000000&sig=…",
  "role": "user", "joined": "2026-11-24T05:00:00Z", "posts": 12, "forum_posts": 3 }
```

### Forum

A public server has a forum: categories set by admins, threads in them, posts in threads. Guests read it; writing needs an account. On a private server every forum endpoint answers 403 `forum_off`, guests included, so an app can tell a server without a forum from a missing page.

| Method | Path | Body | Returns |
|---|---|---|---|
| GET | `/forum` | | `ForumCategory[] {id, name, description, position, threads, last_activity_at}`, by `position` then name. `last_activity_at` is the newest post in any of its threads, `null` when it has none. |
| POST | `/forum/categories` | `NewCategory {name, description?, position?}` | `ForumCategory`. Admins with a session. `name` is one line of 1 to 64 characters, unique regardless of case (409 `name_taken`); `description` up to 500. Without `position` it goes last. 409 `too_many_categories` past 100. |
| PATCH | `/forum/categories/{id}` | `CategoryUpdate {name?, description?, position?}` | `ForumCategory`. Admins. Fields left out stay. |
| DELETE | `/forum/categories/{id}` | | 204. Admins. 409 `category_not_empty` while it has threads: move or delete them first. |
| GET | `/forum/threads?category=&q=&page=&limit=` | | `ThreadPage {threads, page, limit, has_more}`. `ForumThread {id, category_id, title, author, created_at, last_activity_at, posts, pinned, locked}`. With `category`, that category's threads, pinned first; without it, every category's. Then the latest activity first. `q` finds threads whose title or a post not deleted has all its words (whole words, any case). `limit` is 1 to 100, default 30. 404 for an unknown category. |
| POST | `/forum/threads` | `NewThread {category_id, title, body}` | `ForumThread`, with its first post. `title` is one line of 1 to 120 characters. 400 for an unknown category. |
| GET | `/forum/threads/{id}?page=&limit=` | | `ThreadView {thread, posts, page, limit, has_more}`: the thread and a page of its posts, oldest first. `limit` is 1 to 100, default 50. |
| PATCH | `/forum/threads/{id}` | `ThreadUpdate {title?, pinned?, locked?, category_id?}` | `ForumThread`. The title by its author (not while locked) or an admin; `pinned`, `locked` and `category_id` (a move) by admins with a session, others get 403. |
| DELETE | `/forum/threads/{id}` | | 204. Admins: the thread with all its posts. |
| POST | `/forum/threads/{id}/posts` | `ForumPostBody {body}` | `ForumPost`, a reply. 409 `locked` in a locked thread, except for admins. |
| PATCH | `/forum/posts/{id}` | `ForumPostBody {body}` | `ForumPost` with `edited_at`. Its author only (403 otherwise); 409 `deleted` for a deleted post, `locked` in a locked thread. |
| DELETE | `/forum/posts/{id}` | | 204. Its author or an admin. The post keeps its place in the thread, empty. |

A `ForumPost` is `{id, thread_id, author, body, created_at, edited_at, deleted}`. A deleted one has an empty `body`, no `author` and `deleted: true`, so the thread still reads in order and its `posts` count doesn't change. `author` is also `null` once the account is deleted.

A body is plain text of 1 to 10,000 characters, its lines kept and trimmed at both ends. The server stores Markdown as typed and the apps render it. A post can name posts of the booru as `>>123` or `post #123`; that stays text, and `starbooru_core::post_mentions` finds them for the apps to link.

Each account may post (a thread or a reply) once every 15 seconds and 100 times in 24 hours; admins aren't held to either. Past that the answer is 429 `rate_limited` with `Retry-After` and a message saying how long.

```json
GET /api/v1/forum/threads/4?limit=2
{ "thread": { "id": 4, "category_id": 1, "title": "Best sky tags",
              "author": { "name": "ann", "display_name": "Ann",
                          "avatar_url": "/media/avatar/…", "avatar_small_url": "/media/avatar64/…" },
              "created_at": "2026-11-24T05:10:00Z", "last_activity_at": "2026-11-24T05:20:00Z",
              "posts": 3, "pinned": false, "locked": false },
  "posts": [
    { "id": 10, "thread_id": 4, "author": { "name": "ann", "display_name": "Ann",
        "avatar_url": "/media/avatar/…", "avatar_small_url": "/media/avatar64/…" },
      "body": "# Skies\n\nSee >>12.", "created_at": "2026-11-24T05:10:00Z",
      "edited_at": null, "deleted": false },
    { "id": 11, "thread_id": 4, "author": null, "body": "",
      "created_at": "2026-11-24T05:15:00Z", "edited_at": null, "deleted": true } ],
  "page": 1, "limit": 2, "has_more": true }
```

### Tag votes

On a public server, signed-in people vote on each tag of a post: it belongs there (1) or it doesn't (-1).

| Method | Path | Body | Returns |
|---|---|---|---|
| PUT | `/posts/{id}/tags/{name}/vote` | `TagVoteRequest {value: -1 \| 0 \| 1}` | `Post`. 0 takes the vote back. `name` follows aliases. 400 for a tag that isn't on the post or another value, 404 for an unknown tag or post, 403 `tag_votes_off` on a private server. |
| GET | `/admin/tag-removals?page=&limit=` | | `TagRemovalPage {removals, page, limit, has_more}`, newest first: `TagRemoval {id, post_id, tag, score, removed_at, thumb_url}`, `score` being the tag's when it came off. Admins. |
| DELETE | `/admin/tag-removals/{id}` | | `Post` with the tag back on, and no votes on it. Admins with a session. |

On a public server a `Post` carries `tag_votes`, `{tag: TagVote {score, mine}}` for the tags with votes (votes for minus votes against, and the caller's own, -1, 0 or 1). A tag missing from it has a score of 0. Both it and `voted_off` are left out when empty, and always on a private server.

When a tag's score reaches minus the threshold (3 by default; `tag_vote_threshold` in `PUT /admin/settings`, 0 to turn it off), it comes off that post at once. Its votes there are cleared, the removal is recorded, and the post lists the tag in `voted_off`. While the record is there, nothing puts the tag back on that post: not an edit, a bulk edit, an implication, an alias or the automatic tags. Only an admin's undo does, with no votes, so a tag the voters took off never comes back unnoticed.

```json
PUT /api/v1/posts/7/tags/cat/vote   {"value": -1}
{ "id": 7, "tags": ["sky"], "voted_off": ["cat"],
  "tag_votes": { "sky": { "score": 2, "mine": 0 } },
  "uploader": { "name": "ann", "display_name": "Ann", "avatar_url": null, "avatar_small_url": null },
  "…": "…" }
```

## Posts

| Method | Path | Body | Returns |
|---|---|---|---|
| GET | `/posts?q=&page=&limit=&seed=&exclude=` | | `PostPage {posts, page, limit, has_more, corrected?}`. Posts the reader's [blacklist](#blacklist) hides are left out; `exclude` is a guest's own list. `page` starts at 1 (at most 10000); `limit` is at most 200, default 60. `corrected` lists `TagCorrection {from, to}` for the unknown tags the search corrected; it is left out when there are none. `seed` (any 64-bit integer) makes `order:random` shuffle the same way every time it is given, so pages follow on from each other without repeats; without it each request shuffles anew. Other orders ignore it. |
| POST | `/posts` | multipart: `file` (required), `tags` (space-separated), `rating` (`safe`/`s`/`q`/`e`), `source`, `autotag` (`true` or `false`) | `Post`, or 409 `duplicate` if the same file exists. The message names the post, and says so when it is in the recycle bin. |
| POST | `/posts/import` | `ImportRequest {url, tags?, rating?, source?, autotag?}`. `tags` is a list; `source` defaults to `url`. An address that is a post's page on a booru, or one of that site's files, becomes the post: its original file, the site's tags after yours (with `artist:`, `character:`, `copyright:` and `meta:` prefixes where the site says), its rating, and the post's page as the source. Any other web page becomes the picture or video it names (`og:video`, `og:image`). An import that comes with a `source` of its own is downloaded as it is. | `Post`, like an upload. 400 for a non-http(s) address, credentials in the URL, a private-network address (unless the server allows them) or import turned off; 413 over the upload limit; 409 `duplicate`; 424 `fetch_failed` when the remote server errors, times out or redirects more than 5 times. |
| GET | `/posts/{id}` | | `Post`, with `ocr_text` and `tag_details: [PostTag {name, category, section}]`. `section` is the top of the tag's parent chain (`clothing` for `boots`), the tag itself when it heads a tree, or null for a tag on its own; the viewer groups general tags by it. Also `figures`, the people in the picture (see [People](#people-in-a-post)), left out when there are none. The answers of `PATCH /posts/{id}`, the vote, restore and OCR rescan carry the same fields; lists leave them out. Only this answer has `blacklisted`, the entries of the reader's [blacklist](#blacklist) the post matches (left out when none): the post opens all the same. Takes `exclude` from a guest. |
| GET | `/posts/{id}/neighbors?q=&seed=&exclude=` | | `Neighbors {prev, next}`: the ids just before and after the post in the results of search `q` (same filters and order, the blacklist included), or null at either end. For `order:random`, pass the `seed` the search was shuffled with; without one both are null. Both are also null when `q` matches nothing. |
| GET | `/posts/{id}/related?limit=&exclude=` | | `RelatedPosts {posts}`: up to `limit` live posts like this one (default 12, at most 48), most alike first, without what the reader's blacklist hides. Shared tags count by rarity, so a tag on a handful of posts outweighs one on half the collection; posts whose look (difference hash) is close count too, even without tags. Candidates come from the post's 8 rarest tags and from look-alikes, so the list is a suggestion, not an exhaustive search. Empty when nothing is alike. |
| PATCH | `/posts/{id}` | `PostUpdate {tags?, rating?, source?}` | `Post`. `tags` replaces the whole list. |
| DELETE | `/posts/{id}` | | 204; moves the post to the recycle bin (admin or uploader) |
| POST | `/posts/{id}/restore` | | `Post` |
| POST | `/posts/{id}/ocr` | | `Post` with `ocr_text`: reads the text in the image again (tesseract on the server; it can take several seconds). 400 when OCR is off on the server. |
| POST | `/posts/{id}/purge` | | 204; permanently deletes a post that is in the recycle bin, with its files (admin or uploader). 409 `not_deleted` if the post is live. |
| POST | `/posts/bulk` | `BulkEdit {ids, add_tags?, remove_tags?, rating?, delete?}` | `BulkResult {updated, skipped}`. Up to 500 distinct ids. Edits run in one transaction: tags in `remove_tags` (aliases resolve) come off, then `add_tags` go on with their implications. With `delete: true` (and no edits) each post moves to the recycle bin; posts you may not delete are skipped. Missing posts, and posts in the recycle bin when editing, are skipped. 400 for no ids, too many, an invalid tag, a post over the tag limit, no change, or delete combined with edits. |
| POST | `/posts/{id}/edit` | `MediaEdit {rotate?, flip_h?, flip_v?, straighten?, crop?, trim?, bin_original?}` | `Post`: a new post with the edited file and the original's tags, rating and source. See [Editing media](#editing-media). |
| GET | `/ai` | | `AiStatus {enabled, model, tagging, translation, comics, people, autotag}`: which AI tools are on, so clients know whether to offer *Suggest tags* (`tagging`), *Translate* (`translation`), the translate button on images (`comics`) and *Find people* (`people`). `model` is the tagging model. `autotag` says whether new uploads are tagged by the AI when the uploader doesn't say. A server older than `people` leaves it out: read it as `false`. |
| POST | `/posts/{id}/suggest-tags` | | `TagSuggestions {tags: [{name, category, exists}], rating, figures}`: up to 40 booru-style tags, the people with their tags by slot (`figures`, left out when there are none; names as the model gave them) and a rating an AI vision model suggests from the post's thumbnail. Names are normalised and resolved through aliases; `exists` says whether the tag is already on the server, and `category` is the existing tag's, or the one the model gave. Nothing on the post changes. 400 when AI is off or the post has no thumbnail; 424 `fetch_failed` when the provider errors (`AI provider answered 401`), times out or answers something unreadable. |
| POST | `/posts/{id}/find-people` | | `FoundPeople {figures, model}`: the people the people tool finds in the post's picture, as figures ready for `PUT /posts/{id}/figures`, each with its `face` box, and `model`, the model that found them. Nothing on the post changes. See [Finding people](#finding-people). 400 when the tool is off or the post has no picture it can read; 404 for a missing or binned post; 409 `ai_unavailable` while the built-in models start or download; 409 `ai_busy` when the tool has been busy with other pictures for a minute (it reads one at a time, uploads included): try again later; 424 `fetch_failed` as for suggestions. |
| POST | `/posts/{id}/translate` | `TranslateRequest {to}` | `Translation {text}`: the post's `ocr_text` translated by the AI provider into `to`, a BCP 47 code such as `en` or `pt-BR` (the clients send their UI language). Only the text is sent, not the image, and nothing is stored. At most 10,000 characters go each way. 400 for a bad code, when AI is off or when the post has no text; 404 for a missing or binned post; 424 `fetch_failed` as for suggestions, or for an empty answer. |
| GET | `/posts/{id}/translations` | | `[PanelTranslation {lang, blocks: [TextBlock {x, y, w, h, text, translation}], model, created_at}]`: the kept translations drawn over the image, newest first. Box edges are on a 0–1000 scale of the image's width and height. 404 for a missing or binned post. |
| POST | `/posts/{id}/translations` | `TranslateRequest {to}` | `PanelTranslation`: sends the still image (fit in 1536 px) to the translation model, which finds each piece of text, its box and its translation into `to`, for comic and manga pages. Kept per post and language, replacing an older one; an answer with no text (`blocks: []`) is returned but not kept. 400 for a bad code, when translation is off or for a video, GIF or unreadable format; 404 for a missing or binned post; 424 as for suggestions, or when the answer has no block list. |
| PUT | `/posts/{id}/figures` | `FiguresUpdate {figures: [Figure]}` | `Post`: replaces the post's people and updates its tags to match. See [People](#people-in-a-post). 400 for more than 20 people, more than 64 tags on one, more tags than a post can hold (512), an invalid tag or a face box outside the picture; 404 for a missing post; 409 `deleted` for a post in the recycle bin. |
| PUT | `/posts/{id}/vote` | `{value: -1 \| 0 \| 1}`, or `{stars: 0–5}` under stars | `Post` with the new `score` and `my_vote` (and `stars`, `votes`, `my_stars` under stars; see [Scores](#scores)) |

A `Post` carries `file_url`, `thumb_url` and `sample_url`. These are signed paths relative to the server, valid for at least 6 hours. Fetch them without the bearer token. `thumb_url` is null when no thumbnail exists. `sample_url` is a smaller JPEG copy of a big image, meant for viewing; it is null for most posts, and `file_url` stays the original to download. `duration_ms` is the length of a video, or null. `file_date` is the date in the file, or null: EXIF for images (its wall clock, marked UTC, because EXIF has no time zone) and the creation time for videos. `palette` lists the picture's main colours as `#rrggbb`, most common first; it is left out when there are none. `GET /posts/{id}` (and `POST /posts/{id}/ocr`) also carry `ocr_text`, the text found in a still image by OCR, lines separated by `\n`; it is left out when there is none, when the image is not read yet, and in lists. On a public server a post also carries `uploader`, `tag_votes` and `voted_off`, in lists too (see [Profiles](#profiles) and [Tag votes](#tag-votes)); a private server leaves them out, so its posts read as before.

### Chunked uploads

For a file bigger than `UPLOAD_CHUNK_SIZE` (2 MiB). One `POST /posts` of a big file can exceed a proxy's limit on request bodies or its patience; Cloudflare's free plan stops at 100 MB. Each chunk is its own request, sent in any order and in parallel. The clients send one chunk first, then up to 8 at once while answers stay quick.

| Method | Path | Body | Returns |
|---|---|---|---|
| POST | `/uploads` | `UploadStart {size, tags?, rating?, source?, autotag?}`. `tags` is a list. | `UploadSession {id, chunk_size, chunks}`. 400 for an empty file or a bad tag or source; 413 over the upload limit; 403 `quota_exceeded`. A server older than this answers 404: send the file to `POST /posts` instead. |
| PUT | `/uploads/{id}/chunks/{index}` | The chunk's raw bytes, `chunk_size` of them, or what's left for the last. | 204. Sending a chunk again is harmless. 400 for a wrong length or an index past the end; 413 for more than a chunk. Chunk 0 is checked for a known format, and a file that isn't one answers 415 and ends the upload. 507 `storage_full`. |
| POST | `/uploads/{id}/finish` | | `Post`, as `POST /posts` gives. 409 `chunks_missing` until every chunk has arrived; 409 `duplicate` if the same file exists. Asking again after an answer was lost gives the same answer, waiting for a finish still under way. Can take a while for a long video. |
| DELETE | `/uploads/{id}` | | 204, whether or not the upload still exists. What already became a post stays. |

409 `upload_gone` means the upload no longer exists, and the file has to be sent again: it sat idle for an hour, the server restarted, the account opened more than 8 at once (the one left alone longest goes), or it belongs to someone else.

### People in a post

A `Figure` is one person (or creature) in the picture: `{kind, character?, cosplay?, franchise, tags, face, face_url?}`. `kind` is `girl`, `boy` or `other`. `character` is who they are, a tag that becomes a character tag. `cosplay` is the character they are dressed as, without the suffix: the post gets `name_(cosplay)` (placed under `cosplay` in the tree when it has no parent) and `cosplay`. `franchise` is the series they come from (for a cosplay, the costume's), a tag that is made a copyright tag when it is new; a tag that exists keeps its category. `tags` maps slots to tag lists. The slots are `head`, `body`, `bottom`, `shoes` and `wear` (other clothing) for clothing, and `shape`, `eyes`, `skin`, `hair`, `pose` and `other` for the body.

`face` is where the face is: `{x, y, w, h}` from the picture's top-left corner, in thousandths of its width (`x`, `w`) and height (`y`, `h`), as panel translations' boxes are. It has to be inside the picture (`x + w` and `y + h` at most 1000) and not empty, or saving is 400. For a figure with a face the server keeps a crop of it: a 192 px square JPEG, the box's longer side plus a quarter on each side, cut from the original (from the frame the thumbnail comes from, for videos and the formats the server reads through ffmpeg). It is made in the background, usually within a second of saving; until then the figure has no `face_url`. `face_url` is the crop's signed URL, in every answer that has figures; it is ignored in a request. A figure that keeps the same box keeps its crop.

`franchise` and `face` are always in answers, `null` when there is none. Apps written before them don't send them, so a figure without the keys keeps what the same person had: the old figure at the same place in the list when it has the same kind, character and cosplay, or else the first such one not matched yet. A figure that matches none has neither. An app that knows the fields should always send them. `face: null` clears the face. `franchise: null` clears the franchise of a person already on the post, and it stays off; for a person new to the post it asks the server to fill one in, as below.

When a figure names a character (or a cosplay) and gives no franchise (`null`, or the key left out), and that person (the same character and cosplay) wasn't on the post before, the server fills it in with the franchise the collection gives the character, worked out in this order: a copyright tag the character implies (closest first), the closest copyright tag above it in the tree, the franchise other posts' figures give it most, and the copyright tag on most of its 50 newest posts. The answer shows what it filled in. A franchise taken off a figure stays off while that person is on the post: to keep a new person without one, save them, then save again with `franchise: null`.

A figure's tags are on the post too. Saving figures adds their tags (aliases resolved, missing tags created, prefixes dropped) and removes the ones only the old figures gave. Taking a tag off the post with `PATCH /posts/{id}` or a bulk edit takes it off the figures too. While a post has figures, its count tags follow them: `1girl`…`5girls`, `6+girls` and `multiple_girls`, the same for `boy` and `other`, and `solo` for one figure. A count tag the figures don't call for is dropped, and the ones they do call for come back. A tag sits in one place per figure: the first it is given, the character, the cosplay and the franchise before the slots. Count tags given in a slot are ignored. Automatic AI tagging of an upload also saves the people the model saw, when the post has none yet and the people tool (below) found nobody.

```json
PUT /api/v1/posts/42/figures
{ "figures": [ { "kind": "girl", "character": "saber", "franchise": "fate_(series)",
                 "tags": { "hair": ["blonde_hair"] }, "face": { "x": 412, "y": 96, "w": 130, "h": 180 } } ] }

GET /api/v1/posts/42   (a moment later)
{ …, "figures": [ { "kind": "girl", "character": "saber", "franchise": "fate_(series)",
                    "tags": { "hair": ["blonde_hair"] }, "face": { "x": 412, "y": 96, "w": 130, "h": 180 },
                    "face_url": "/media/face/9f…e1.jpg?exp=…&sig=…" } ] }
```

### Finding people

The people tool (`people` in `GET /ai`, set up in Administration › AI) finds the faces in a picture, drawn ones included, and fills in a figure for each: `face`, `kind`, `character`, `cosplay`, `franchise` and the tags it can read, by slot. It leaves a field empty rather than guess: a character only when the model is very sure of the face, or fairly sure and a second reading agrees (the body below it, or the post's own tags); a cosplay when that character is in a photograph or the model sees a cosplay; and `kind` `other` when it can't tell girl from boy. On photographs the built-in models only say `girl`: their taggers, trained on drawings, call many women boys. A `franchise` comes from what the collection knows of the character (as when saving), else from the series in a name such as `lumine_(genshin_impact)`.

With the built-in models, a face detector for photographs and one for drawings find the faces, and a tagger reads each face for the looks and the character and the body below it for the clothing. Tags go in the slot the tag tree puts them under (`headwear` is `head`, `hair` is `hair`, `pose` is `pose` and so on), or by their words when the tree doesn't place them; tags about the picture (setting, style) stay off. With an AI provider, the server sends the picture with the faces it found and asks for one figure per face; without the built-in face finders, the provider finds the faces too. Videos and GIFs are read from the frame their thumbnail comes from.

On upload and URL import, when the tool is on, it runs after automatic tagging, so the post's tags are there first, and saves its people when the post has none yet. An uploader who turns automatic tagging off for the upload (`autotag: false`) gets neither. One picture at a time; failures are logged and the upload is untouched.

`POST /posts/{id}/find-people` runs it on demand and saves nothing. Each figure's `face_url` there is a preview of the crop saving it would keep, a `data:image/jpeg;base64,…` URL, as nothing is stored yet; it is ignored when sent back. The people come left to right, 20 at most.

```json
POST /api/v1/posts/42/find-people
{ "model": "YuNet + anime face detector + WD SwinV2 v3",
  "figures": [ { "kind": "girl", "character": "lumine_(genshin_impact)", "franchise": "genshin_impact",
                 "tags": { "hair": ["blonde_hair", "short_hair"], "eyes": ["yellow_eyes"],
                           "body": ["dress"], "head": ["hair_flower"] },
                 "face": { "x": 412, "y": 96, "w": 130, "h": 180 },
                 "face_url": "data:image/jpeg;base64,/9j/4AAQ…" } ] }
```

### Editing media

`POST /posts/{id}/edit` never changes a file: originals are immutable, so the result is a new post with the original's people. Their face boxes move with the picture: turned, flipped, straightened (the upright box around the turned one) and cropped, cut to the new edges; a box whose middle falls outside the crop is dropped, and the person stays without it. A video's faces are on the frame its thumbnail comes from (a tenth of the way in, at most 5 s), so a trim keeps them only when it starts at 0 and both the video and the clip are at least 50 s long; otherwise the people stay without their boxes. Anyone who can see the post may edit it; `bin_original: true` also moves the original to the recycle bin and needs the rights to delete it (403 otherwise, checked before anything is made).

- **Images** (JPEG, PNG, WebP, BMP, TIFF; not GIF, AVIF or JPEG XL): `rotate` is clockwise degrees (0, 90, 180, 270), then `flip_h`/`flip_v` mirror the turned image, then `straighten` turns it a little more, clockwise in tenths of a degree (−450 to 450, so ±45°), then `crop {x, y, width, height}` cuts a rectangle in pixels of that image. Straightening keeps the size and zooms in just enough that no empty corners show (`MediaEdit::straighten_scale`). A server older than `straighten` ignores the field and saves the picture unstraightened. The result keeps the format (JPEG at quality 92, WebP lossless) and drops metadata, with EXIF orientation applied.
- **Videos**: only `trim {start_ms, end_ms}`, inside the video and at least 100 ms long. The server first copies the streams, which is lossless but starts at a keyframe; when that lands more than half a second early it re-encodes the cut (H.264/AAC, or VP9/Opus for WebM). Needs ffmpeg (`/info` → `video_editing`), otherwise 400.

400 for an edit that changes nothing, a crop outside the image, a bad angle or straightening, a trim on an image (or a turn on a video), a post in the recycle bin or a type that can't be edited; 413 when the result is over the upload limit; 409 `duplicate` when the result is a file that already exists.

### Search syntax (`q`)

Terms are separated by spaces, with at most 40 terms.

| Term | Meaning |
|---|---|
| `blue_sky` | must have the tag, or a tag under it in the tree (`footwear` finds `boots`) |
| `-cloud` | must not have the tag, nor any tag under it |
| `{girl blonde_hair boots}` / `-{boy hat}` | one person in the picture has every tag (or a tag under it in the tree); `girl`, `boy` or `other` inside the braces also sets their kind, and `{boy}` alone matches any boy. With a minus, no one does. A tag that doesn't exist makes `{…}` match nothing, and `-{…}` exclude nothing. |
| `artist:name` | the tag `name`; the prefix (`artist:`, `character:`/`char:`, `copyright:`/`copy:`/`series:`, `meta:`, `general:`/`gen:`) is dropped |
| `rating:safe` / `-rating:e` | rating is (not) safe, sensitive, questionable or explicit (`safe s q e`; `g` and `general` also mean safe); repeat to allow several, e.g. `rating:safe rating:q` |
| `type:image` / `type:animation` / `type:video` | media kind |
| `date:2024`, `date:2024-03`, `date:2024-03-15` | the post's date is in that period. The date is `file_date` (from the file), or the upload date when there is none. Dates are compared in UTC. |
| `date:2023..2024`, `date:2023-06..`, `date:..2024` | from the start of the first period to the end of the last; either end can be left open |
| `date:>2023-06`, `date:>=2023-06`, `date:<2024`, `date:<=2024` | after or before a period, without or with it |
| `date:>30d`, `date:<7d`, `date:7d` | more than 30 days old; less than 7 days old (a bare age means the same). Units are `h`, `d`, `w`, `mo` and `y`, up to 1000 years. |
| `width:1920`, `height:>1080`, `width:1000..2000` | size in pixels: a value, a comparison (`>`, `>=`, `<`, `<=`) or a range |
| `mpixels:>8`, `mpixels:2..4`, `mpixels:8` | width × height in megapixels, in the same forms. A bare value means about that, to the digits given: `8` is 7.5–8.5 and `8.3` is 8.25–8.35. |
| `ratio:16:9`, `ratio:1.78`, `ratio:>1` | width over height, in the same forms. A bare value has a margin of 0.01, so `ratio:1` is square. |
| `color:red`, `color:2060e0`, `color:#2060e0`, `color:26e` | the picture has that colour (`colour:` works too). The names are `red`, `orange`, `yellow`, `green`, `teal`, `blue`, `purple`, `pink`, `brown`, `black`, `gray` (or `grey`) and `white`. The search widens until it finds a post that fits the rest of the query, up to a limit, so it finds the nearest colours stored and can still come back empty. |
| `-date:…`, `-width:…`, `-color:…` and so on | excludes. A post of unknown size stays in. A bad value is a 400, not a tag search. |
| `order:newest` (default), `order:oldest`, `order:score`, `order:size`, `order:pixels`, `order:longside`, `order:tags`, `order:updated`, `order:random` | sort (`sort:` works too). Add `_asc` or `_desc` to set the direction, e.g. `order:size_asc` for the smallest files first. `score` is likes minus dislikes, or the fair rating under stars (see [Scores](#scores)). `pixels` (or `mpixels`) is width × height; `longside` is the longer of the two; posts of unknown size sort last in both. `tags` is the tag count; `updated` is the last edit. |
| `status:deleted` | browse the recycle bin |
| `exact:true` | take tags as typed, without correcting unknown ones. Any other value is an ordinary tag name. |
| `text:hello` / `text:"no parking"` / `-text:sale` | the text OCR read in the image has (or doesn't have) all these words, in any case. Quotes keep several words in one term; several `text:` terms all apply. Matches whole words (`simple` full-text search), at most 200 characters per term. |

Tags are normalised: trimmed, lower-cased, spaces and underscores folded to one `_`, and at most 128 characters.

A tag that is neither a tag nor an alias is corrected to the closest tag on at least one post, in included, excluded and `{...}` terms alike. It may be off by one edit up to 5 characters, two up to 11 and three beyond that, where a swap of neighbouring letters is one edit. Names under 3 characters are left alone, and digits must match exactly, so `4k` never becomes `8k`. At most 8 unknown names per query are looked up. A search and the viewer's neighbours and Explore all correct alike.

## Recycle bin

| Method | Path | Body | Returns |
|---|---|---|---|
| POST | `/trash/empty?older_than_days=` | | `PurgeResult {purged}`. Admin only. Purges every post in the bin, or only those deleted at least `older_than_days` ago. |

Browse the bin with `GET /posts?q=status:deleted`. `STARBOORU_TRASH_DAYS` purges old posts automatically.

## Duplicates

Finds posts that look alike even when the files differ (resized, re-encoded, lightly edited). Each post with a thumbnail has a fingerprint: a 64-bit difference hash of its brightness and its average colour. Two posts match when their hashes differ in at most `distance` bits and each colour channel is within 24 of the other's. Groups are transitive: A and C are grouped when both match B.

| Method | Path | Body | Returns |
|---|---|---|---|
| GET | `/duplicates?distance=&limit=` | | `Duplicates {distance, total, unhashed, groups}`. `distance` is 0–10 (default 6). `limit` is how many groups to return, 1–100 (default 30). `total` counts every group; `unhashed` counts posts that can't be compared yet (no thumbnail, or not fingerprinted yet). Each `DuplicateGroup {distance, size, posts}` gives the largest distance linking it, its size, and up to 50 posts, best first (most pixels, then largest file). Closest groups come first. Pairs marked "not duplicates" are never linked. |
| POST | `/duplicates/resolve` | `ResolveDuplicates {keep, remove, merge_tags}` | `Post` (the kept one). Moves `remove` to the recycle bin in one transaction. With `merge_tags` the kept post also gets their tags, and their source if it has none. 404 if any post is missing or already in the bin; 403 unless the caller may delete each removed post. 400 if `keep` is in `remove` or `remove` is empty or over 50. |
| POST | `/duplicates/dismiss` | `DismissDuplicates {ids}` | 204. Marks every pair among 2–50 posts as not duplicates. |

Fingerprints for posts uploaded before this feature are computed in the background at startup.

## Tags

| Method | Path | Body | Returns |
|---|---|---|---|
| GET | `/tags?q=&order=&page=&limit=` | | `Tag[]`. `order` is `count` (most used first, the default) or `name`. Without `q`, only tags in use are listed. With `q` it is the autocomplete list: tags whose name or alias starts with `q`. `limit` is 1–100 (default 20). |
| GET | `/tag-corrections?q=a+b` | | `TagCorrection[]`: "did you mean" for a whitespace-separated list being typed. The same rules as a search, except that tags on no post can be suggested. Names that are known, have a category prefix (`artist:x`) or aren't valid are skipped. |
| GET | `/tags/{name}` | | `TagDetail {tag, aliases, implies, implied_by, ancestors, children}`. `ancestors` is the parent chain, top first; `children` the tags directly under it. An alias resolves to its tag. 404 if neither exists. |
| PATCH | `/tags/{name}` | `TagUpdate {category?, parent?}` | `Tag`. Categories: `general`, `artist`, `character`, `copyright`, `meta`. `parent` names the tag to put it under (created if needed); `""` moves it to the top. Missing fields stay as they are, and a missing `{name}` is created. 409 `parent_cycle` if the parent is already under it; 400 for itself. Any signed-in user. |
| POST | `/tags/{name}/aliases` | `{alias}` | `TagDetail`. Admin only. If a tag called `alias` exists, it is merged in: its posts, aliases, implications and child tags move to `{name}`, which also takes its parent if it has none, and it is deleted. 400 if `alias` is the tag itself. |
| DELETE | `/tags/{name}/aliases/{alias}` | | `TagDetail`. Admin only. |
| POST | `/tags/{name}/implications` | `{implies}` | `TagDetail`. Admin only. Missing tags are created. Posts that already have `{name}` get the implied tags (transitively). 409 `implication_cycle` if `implies` already implies `{name}`; 400 if they are the same tag. |
| DELETE | `/tags/{name}/implications/{implied}` | | `TagDetail`. Admin only. Posts keep the tags they already got. |

Tags form a tree through their parents. Searching a tag finds posts with any tag below it, and the tops of the tree are the sections the viewer groups general tags under. At first start the server seeds a starter tree of Danbooru-style tags (sections `people`, `hair`, `eyes`, `skin`, `body`, `expression`, `pose`, `clothing`, `setting`, `style`, with groups such as `clothing › footwear › boots`); tags that already have a parent keep it. When tagging, a category prefix such as `artist:name` tags `name` and sets its category.

Tags are resolved the same way everywhere: an alias in a post's tags or in a search means its tag, and implied tags are added whenever tags are set on a post. Implied tags are stored on the post, so they show up in its tag list and in searches. Path segments are percent-encoded.

## Explore

| Method | Path | Body | Returns |
|---|---|---|---|
| GET | `/explore?days=&q=&exclude=` | | `Explore {days, trending, tags, on_this_day, random, people, popular_characters, corrected?}`. The reader's [blacklist](#blacklist) applies to every section: the posts it hides are left out, and `tags` doesn't count them. `days` is the window, 1 to 365 (default 7). `trending`: up to 24 posts with the most net upvotes cast or changed in the window (only those above zero). `tags`: up to 20 `TagTrend {tag, recent}`, the tags most used on posts added in the window, without the tags `q` requires. `on_this_day`: up to 12 posts added on today's month and day (UTC) in earlier years, newest first. `random`: 12 random posts. `people`: up to 20 [`Person`](#people)s, the characters on the most live posts; with a `q`, those on the most of its 5,000 newest matches, without the characters it requires (`posts` counts those matches) and, as without a `q`, without the characters the blacklist hides that `q` doesn't name. `popular_characters`: up to 20 `Person`s, the characters on the 1,000 posts with the most net upvotes in the window that `q` finds, the most upvotes first, with `votes`, the net upvotes their posts got, and `posts`, how many of those posts they are on. `q` uses the search syntax and narrows every section; its `order:` and `status:` are ignored, and `corrected` is as in `/posts`. 400 for a bad query. On a private server `trending`, `tags` and `popular_characters` are always empty: the apps don't show them there, and they are the costliest queries. A server from before people leaves `people` and `popular_characters` out, and the client reads them as empty. |

### People

The people of the collection are its characters: tags in the `character` category on live posts, with figures or without, so a post imported with `saber` among its tags counts for Saber like one whose figure names her. A figure without a character isn't anyone here. A `Person` is `{name, posts, franchise, face_url, thumb_url, votes?}`: the character's tag, its live posts, its franchise (or null), and a picture to show, a `face_url` (the crop of their face on one of their best-liked posts) or else a `thumb_url` (the thumbnail of their newest post; in Explore with a search, or in `popular_characters`, of the posts it counted), both signed, from posts the reader may see; both null when there is none. The faces to choose from are worked out with the franchise, so a new face shows a few seconds after its crop is made. Their posts are a search for `name`.

A character's franchise is worked out in the background and kept: the same rules as for [filling in a figure's](#people-in-a-post), again a day later, and soon after a figure names the character or the character is taken off a post, or the category, implications or tree of the tag, a tag it implies or one above it change. A character's face is one of a figure that still names it. Until then `franchise` is null. The reader's blacklist hides the characters it names and those of a franchise it names; their counts are the tag's own.

| Method | Path | Body | Returns |
|---|---|---|---|
| GET | `/people?q=&franchise=&order=&page=&limit=&exclude=` | | `PeoplePage {people, page, limit, has_more}`. `q` keeps the characters whose name holds it, written as a tag (`hatsune miku` finds `hatsune_miku`). `franchise` keeps a franchise's people (aliases followed); `franchise=` keeps those without one. `order` is `posts` (the default: most posts first), `name`, or `franchise`: grouped by franchise, the franchise with the most posts first and those without one last, most posts first within each. `limit` 1 to 100 (default 60). Takes `exclude` from a guest. 400 for another `order`. |
| GET | `/franchises?q=&page=&limit=&exclude=` | | `FranchisePage {franchises: [Franchise {name, people, posts}], page, limit, has_more, without}`: the franchises with people, the most people first; `people` is how many of the reader's characters have it, `posts` the franchise tag's own count. `without` is how many have none. `q` as above, on the franchise's name. `limit` 1 to 200 (default 50). |

```json
GET /api/v1/people?order=franchise&limit=2
{ "people": [
    { "name": "saber", "posts": 31, "franchise": "fate_(series)",
      "face_url": "/media/face/9f…e1.jpg?exp=…&sig=…", "thumb_url": null },
    { "name": "tohsaka_rin", "posts": 12, "franchise": "fate_(series)",
      "face_url": null, "thumb_url": "/media/thumb/c4…07.jpg?exp=…&sig=…" } ],
  "page": 1, "limit": 2, "has_more": true }
```

## Blacklist

Each account keeps a list of tags and ratings whose posts it doesn't want to see, in both modes. The server leaves those posts out of `GET /posts` (colour searches and a tag's posts included), `/posts/{id}/neighbors`, `/posts/{id}/related`, every section of `/explore`, the [people lists](#people) and the [home page](#home-page), as if the search had a `-tag` or `-rating:` term for each entry. An entry hides the posts with its tag or a tag under it in the tree, so `footwear` hides `boots`, and an alias stands for its tag.

| Method | Path | Body | Returns |
|---|---|---|---|
| GET | `/auth/blacklist` | | `Blacklist {entries}`. A guest gets the admin's default. |
| PUT | `/auth/blacklist` | `Blacklist {entries}` | `Blacklist`, the list as kept. Replaces the whole list; `[]` clears it. 400 for an entry that isn't a tag or a rating, or more than 500 entries. |

- An entry is a tag, or a rating in any form a search takes (`rating:e`), kept as `rating:explicit`. A tag is normalised as when tagging (`Blue Sky` is `blue_sky`), loses a category prefix (`artist:name` is `name`) and is kept as the tag its alias names. A tag that doesn't exist yet is kept and hides posts once it does. Blank entries and repeats are dropped. Other search terms (`type:video`, `order:score`, `-cat`, `{girl}`) are 400.
- What a search names wins over the list. `q=cat` shows posts tagged `cat` with `cat` on the list, `q=boots` shows boots with `footwear` on it, and `q=rating:explicit` shows explicit posts. Only what it names comes back: with `rating:explicit` on the list too, `q=boots` still leaves out explicit posts with boots. The home page's search is the admin's, so the list applies to it whole.
- The recycle bin (`status:deleted`) shows everything it holds, as emptying it purges all of it.
- A post opened by id (`GET /posts/{id}`) still opens, with `blacklisted` listing the entries it matches, such as `["footwear", "rating:explicit"]`, so the app can cover it with "Hidden by your blacklist · Show". When the search it was opened from names the tag, the app may leave it uncovered.
- New accounts start with the admin's default (`default_blacklist` in `PUT /admin/settings`, at most 100 entries, empty unless set). Changing it doesn't change existing accounts.
- Guests have no list on the server. Their reads use the admin's default, which `GET /auth/blacklist` gives them, unless they send their own as `exclude`: entries separated by spaces (`exclude=cat+rating:explicit`), at most 100; `exclude=` hides nothing. An app keeps a guest's list on the device and sends it with `GET /posts`, `/posts/{id}`, `/posts/{id}/neighbors`, `/posts/{id}/related`, `/explore`, `/people`, `/franchises` and `/home` (`Client::with_guest_blacklist` does). A bad `exclude` is 400. Signed-in requests ignore it.

```json
PUT /api/v1/auth/blacklist   {"entries": ["Kitty", "footwear", "rating:e"]}
{ "entries": ["cat", "footwear", "rating:explicit"] }

GET /api/v1/posts/7
{ "id": 7, "tags": ["boots", "sky"], "blacklisted": ["footwear"], "…": "…" }
```

## Home page

A page an app can show at start: a title and a line under it over a moving background of posts, by default the most popular posts rated safe. Off until an admin turns it on; `/info` says whether it is (`home`).

| Method | Path | Body | Returns |
|---|---|---|---|
| GET | `/home?exclude=` | | `Home {enabled, title, subtitle, posts}` |

- `title` is the admin's, or the server's name; `subtitle` is `null` without one.
- `posts` are `HomePost {id, thumb_url, sample_url, width, height, palette}`, first the ones the search puts first. `thumb_url` (512 px at most) and `sample_url` (a bigger copy of a big still image, or `null`) are signed media paths, as for a post. Only posts with a thumbnail and a known size are there.
- `posts` is empty while the page is off, and on a private server without a token: a private collection shows no picture before signing in. Everyone else gets them: accounts, and guests on a public server, whose limit counts it as 1.
- The background is the admin's `query`, in the search syntax, with `rating:safe` added unless it names a rating (`rating:` or `-rating:`) and sorted by `order:score` (the score kind in force) unless it names an order; live posts only. It shows `posts` of them.
- The search runs again at most every 5 minutes, so new votes reorder the background within that. Each request checks its posts again by id, so a post binned, rated otherwise or retagged since drops out at once. The reader's [blacklist](#blacklist) applies, and a guest's `exclude`.

Admins set it up with `home` in `PUT /admin/settings`, `HomeSettings {enabled, title, subtitle, query, posts}`. `title` is one line of at most 64 characters, `null` or `""` for the server's name; `subtitle` at most 140, `null` for none. `query` is at most 1000 characters, must parse, and can't be `status:deleted`; `""` is the most popular posts. `posts` is 12 to 60, 36 by default. 400 otherwise.

```json
GET /api/v1/home
{ "enabled": true, "title": "StarBooru", "subtitle": "Skies, seas and cats",
  "posts": [ { "id": 3, "thumb_url": "/media/thumb/c8…41.jpg?exp=1764000000&sig=…",
               "sample_url": null, "width": 1920, "height": 1080,
               "palette": ["#2a6fb0", "#e8e4d8"] } ] }

GET /api/v1/admin/settings   (part)
{ "default_blacklist": ["cat"],
  "home": { "enabled": true, "title": null, "subtitle": "Skies, seas and cats",
            "query": "", "posts": 36 } }
```

## Other boorus

| Method | Path | Body | Returns |
|---|---|---|---|
| POST | `/remote/search` | `RemoteSearch {api, base, tags?, page?, login?, api_key?}` | `RemotePage {posts, more}` |
| POST | `/remote/tags` | `RemoteTagQuery {api, base, term}` | `RemoteTag[]`, at most 10: `{name, category, count?}`, the site's tags that start with `term`, most used first. Empty when the site has none to offer or can't be reached (e621 completes from three letters). |
| POST | `/remote/post` | `RemotePostQuery {url}` | One `RemotePost` in full, from a link to it (a post's page, or a file whose address carries its MD5). 404 when the link isn't to a post or nothing could be read of it. |
| GET | `/remote/media?u=&exp=&sig=` | | A picture or video from another site, passed on by the server. No sign-in: the address is signed, as for `/media`, and only the server makes such addresses. 403 for any other; 415 when what the site sent is neither a picture nor a video. |
| GET | `/remote/searches` | | `SavedRemoteSearch[]` by name: `{id, name, query, sites: [{api, base}], created_at}` |
| POST | `/remote/searches` | `SaveRemoteSearch {name, query?, sites}` | `SavedRemoteSearch`. 1 to 8 sites; an empty `query` is a site's newest posts. 409 `too_many_searches` past 200. |
| DELETE | `/remote/searches/{id}` | | 204; 404 if it isn't yours |
| GET | `/remote/accounts` | | `BooruAccount[]`: the caller's saved accounts on other boorus, `{id, api, base, login, usable, updated_at}`. Never the key. |
| PUT | `/remote/accounts` | `SaveBooruAccount {api, base, login, api_key}` | `BooruAccount`. Saves the account for the site at `base`, in place of the one saved for it before. 400 without both `login` and `api_key`; 409 `too_many_accounts` past 50. |
| DELETE | `/remote/accounts/{id}` | | 204; 404 if it isn't yours |

The server searches another booru for you, so the app needs no cross-site access and the same address rules as `POST /posts/import` apply (public addresses only unless the server allows private ones; off when URL import is off).

On a public server every endpoint here answers 403 `remote_off`, and switching to public deletes the saved accounts and searches. URL imports still work there, without saved accounts, and so do [library imports](#library-imports) by administrators: they bring files in and don't browse other boorus.

- `api` is `danbooru`, `gelbooru` (Gelbooru 0.2 and forks such as Safebooru and Rule34's `api.rule34.xxx`), `moebooru` (yande.re, Konachan), `e621` (e621, e926) or `blombooru` (Danbooru's API, but its `s` rating means safe).
- `base` is the site's http(s) address without query or credentials; a trailing slash is dropped. `tags` is in the site's own syntax. `page` starts at 1. Each page asks for 40 posts.
- `login` and `api_key` are the account on that site, sent only to it, in the query as it names them (`user_id` for Gelbooru, `password_hash` for Moebooru). They are used only when both are given, and are never logged. Each field is at most 512 bytes. A search without them uses the account saved for that `base`, if there is one; so does a URL import of a link to a post there.
- A saved account's key is encrypted before it reaches the database (ChaCha20-Poly1305, with a key derived from `STARBOORU_SECRET` and bound to the user and the site) and is only ever opened to be sent to its site: no answer of the API carries it. `usable` is false when it no longer opens, which is what changing `STARBOORU_SECRET` does; save it again.
- A `RemotePost` is `{id, site, page_url, preview_url, sample_url?, file_url, tags, groups?, rating, width, height, md5?, source?, score?, created_at?, file_size?}`. `tags` holds only tags valid here (the rest are dropped); `groups` is `{artist?, copyright?, character?, meta?}`, the names among `tags` that aren't general ones, where the site says (Danbooru, e621, Moebooru). `rating` is mapped to StarBooru's scale. `sample_url` is a copy sized for a screen, when the site makes one. Posts without a file the account may see are left out. `more` is true when the site sent a full page and has a next one.
- The server keeps to each site's limits (`starbooru_core::Limits`): how deep a search goes (Danbooru 1000 pages, e621 750, Gelbooru 500, as it stops 20,000 posts in), how many tags one takes without an account (Danbooru 2, e621 40; `rating:` doesn't count), and the time between two requests to a site, which every account's searches and imports share (Danbooru, Gelbooru and Moebooru half a second, e621 a second). A request waits up to 8 seconds for its turn.
- `tags` may be written the StarBooru way and goes out in the site's own: `rating:safe`, `rating:sensitive`, `rating:questionable` and `rating:explicit` become that site's values (`g`/`s`/`q`/`e` on Danbooru, the full words on Gelbooru, `s`/`q`/`e` elsewhere, where sensitive is `q`), and `order:score` and `sort:score:desc` each other. A site's own short forms (`rating:s`) are left as typed. One search can so go to several sites; the app sends one request per site and merges the answers.
- `post_id` is the post here whose source is that post's page: it was imported before.
- Gelbooru's search answers list tags without their categories. `POST /remote/post`, and an import by the post's page, ask its tag list for them when there is an account (a hundred names a request, three requests at most), so `groups` is filled there and not in a search.
- Gelbooru's API wants a key. Without one, the search is read from the site's own list pages: 42 posts a page, with tags, rating and score, but no file, size or categories. Such a post has an empty `file_url`; import it by its `page_url` alone (no `source`), or ask `POST /remote/post` for the rest.
- Gelbooru's file servers answer only their own pages, and Danbooru's and e621's mark their files so that a browser shows them on those sites alone. For posts of these, `preview_url` and `sample_url` are `/remote/media` addresses on this server (relative, like a post's `/media` ones); `file_url` stays the site's, for importing. `sample_url` is always there for them, as the page has nothing else to show.
- Import results with `POST /imports` (in the background) or `POST /posts/import` (one, waiting for it), `url` = `file_url` and `source` = `page_url`.

Errors:
- 400 `bad_request`: a bad request, an address the server won't reach, URL import turned off, a search past one of the site's limits, a site that wants an account (401/403 from it), or a search it refused. What the site says about a refusal is passed on: `that site says: You cannot search for more than 2 tags at a time.`
- 429 `rate_limited` with `Retry-After` when the site says it is busy (429, 503, or Moebooru's 421), for as long as it asks (15 s when it doesn't say, 10 minutes at most), and for every request to that site until then; also when the wait for a turn would be over 8 seconds.
- 424 `fetch_failed` when the site errors (with its own words when it gave any), times out, or answers with something that isn't that API's search.

## Background imports

| Method | Path | Body | Returns |
|---|---|---|---|
| POST | `/imports` | `ImportBatch {items: [ImportRequest]}`, 1 to 500 items | `ImportJob[]`, all `queued` |
| GET | `/imports` | | `ImportJob[]`: the caller's imports still waiting or running, and those finished in the last 24 hours, newest first |
| DELETE | `/imports/{id}` | | 204; cancels a waiting import or hides a finished one. 409 `running` while it downloads; 404 if it isn't yours. |
| DELETE | `/imports` | | 204; hides the caller's finished imports |

The server downloads each address on its own, so an import goes on when every app is closed and survives a restart (an import a restart interrupts starts over). Each item follows the rules of `POST /posts/import`; the whole batch is refused (400) when one address is not http(s), has credentials, is on a private network (unless the server allows it), has a bad tag, or URL import is off. 409 `too_many_imports` past 2000 waiting imports per account. Two imports run at once, server-wide, oldest first. An import whose site says it is busy goes back to `queued` with a note in `error`, and tries again after the time the site asked for, or 15 seconds, twice as long each time, up to six times. Finished imports are deleted after 7 days.

`autotag` on an upload or import says whether the AI tags the new post once it is in. Left out, the administrator's setting decides. Either way it needs AI tagging to be on; `GET /ai` tells.

`ImportJob` is `{id, url, source, tags, rating, state, post_id, error, created_at, finished_at, progress?}`. `progress` is there while the import is `running` and its download has started: `{done, total?, speed}`, bytes so far, the file's size when the site says it, and bytes per second over the last moments (0 until measured). `state` is `queued`, `running`, `done` (`post_id` is the new post), `duplicate` (the file was already here; `error` says which post) or `failed` (`error` says why, e.g. the site answered 404, the file is over the upload limit, the storage limit, a disabled account). File uploads (`POST /posts`) are sent by the app, so they stop when it closes.

## Library imports

Moves a whole library from another self-hosted booru. The server walks that site by itself, one import at a time and oldest first, so an import goes on with every app closed and picks up where it was after a restart.

| Method | Path | Body | Returns |
|---|---|---|---|
| POST | `/library-imports/check` | `LibrarySite` | `LibraryCheck {total?, rules}`. Reads the first page, once. `total` is missing when the site doesn't say. `rules` says whether aliases and implications would come across. |
| POST | `/library-imports` | `LibrarySite` | 201, `LibraryImport`, `queued`. 403 past 5 imports waiting or running per account. |
| GET | `/library-imports` | | `LibraryImport[]`: the caller's latest 50, newest first |
| POST | `/library-imports/{id}/stop` | | `LibraryImport`. What came across stays. |
| POST | `/library-imports/{id}/retry` | `LibraryRetry {login?, secret?}` | `LibraryImport`, `queued`. Walks the site again, trying the failed posts and finding new ones, and skips what already came across. The key has to be given again. 409 `running` while it's going. |
| DELETE | `/library-imports/{id}` | | 204; forgets the import, its posts stay. 409 `running` while it's going. |
| GET | `/library-imports/{id}/failures` | | `LibraryFailure[] {remote_id, error}`, at most 500 |

- `LibrarySite` is `{source, base, login?, secret?, unrated}`. `source` is `szurubooru`, `shimmie2`, `danbooru`, `stash`, `blombooru`, `monbooru` or `lanbooru`. `base` is the site's http(s) address. `unrated` is the rating for posts that come without one.
- `login` and `secret` are sent only to that site and never returned or logged. They stay in the database only while the import is waiting or running, so a restart can go on, and are erased when it ends.
- `LibraryImport` is `{id, source, base, state, phase, total?, imported, merged, failed, rules, note?, error?, created_at, finished_at?}`. `state` is `queued`, `running`, `done`, `stopped` or `failed`. `phase` is `rules`, `posts` or `groups`. `merged` counts files that were already here, whose tags and likes are added to the post. `note` says what was left out and `error` why it stopped; the server writes both in English.
- An administrator's import can reach the server's own network. Anyone else's follows the rules of `POST /posts/import`. Only an administrator's import brings aliases and implications, as they apply to everyone. With URL import off, checking, starting or retrying one answers 400.
- On a public server only administrators may check, start or retry one; anyone else gets 403 `forbidden`. They stay available there although `/remote/*` is off, as they bring files in rather than browse another booru.

Errors: 400 `bad_request` for a bad address or key, or an address the server won't reach; 424 `fetch_failed` when the site can't be reached, refuses the account or doesn't answer like that system.

## Saved searches

Each user's own list; other users never see it.

| Method | Path | Body | Returns |
|---|---|---|---|
| GET | `/searches` | | `SavedSearch[] {id, name, query, created_at}`, by name |
| POST | `/searches` | `SaveSearchRequest {name, query}` | `SavedSearch`. Whitespace is collapsed. The name is 1–64 characters; the query must parse and is at most 1000 characters. 409 `name_taken` if the user already has that name (case-insensitive); 400 past 200 saved searches. |
| DELETE | `/searches/{id}` | | 204. 404 if it isn't the caller's. |

## Share links

A share link shows chosen posts to anyone who has it, without an account. It shows only the media: no tags, votes, ids or account details. Any signed-in user can share any post they can see.

Share links are for private servers. On a public one a post is shared by its own address, as guests can open it: `POST /shares` answers 409 `share_by_address`, `GET /shares` an empty list, and `PATCH /shares/{id}` and `GET /shared/{token}` 404. Switching to public deletes the links.

| Method | Path | Body | Returns |
|---|---|---|---|
| GET | `/shares` | | `Share[] {id, token, title, posts, cover_url, created_at, expires_at, views, last_viewed_at, preview, preview_on}`, the caller's links, newest first. `posts` counts the posts the link was made with. `cover_url` is the signed thumbnail of the first post still shown, or `null`. `views` counts opens of the public view. `preview` is the link's own choice of [link preview](#link-previews), `null` when it follows the account's (`GET /auth/sharing`); `preview_on` says whether it shows one now, after the admin's switch. Expired links stay listed until removed. |
| POST | `/shares` | `NewShare {post_ids, title?, days?, preview?}` | `Share`. `post_ids` keeps its order, repeats are dropped, 1–500 posts. The title is trimmed, at most 100 characters, and empty means none. `days` is 1–365, or omitted to keep the link until it is removed. `preview` is `true`, `false`, or left out to follow the account. 400 if a post doesn't exist or is in the recycle bin, and past 500 links. |
| PATCH | `/shares/{id}` | `ShareUpdate {preview?}` | The `Share` as kept. `"preview": null` makes the link follow the account again; left out, it stays as it is. 404 if it isn't the caller's. |
| DELETE | `/shares/{id}` | | 204; the link stops working at once. 404 if it isn't the caller's. Media URLs already handed out stay valid until their signature expires (up to 12 hours). |
| GET | `/shared/{token}` | | **No auth.** `SharedView {title, server, expires_at, posts}`, where `posts` are `SharedPost {kind, mime, width, height, duration_ms, file_url, thumb_url, sample_url}` in the link's order. Posts moved to the recycle bin since are left out. 404 for a malformed, unknown, expired or removed link, or one whose owner is disabled. Answers with `Cache-Control: no-store` and `Referrer-Policy: no-referrer`. |

The page to give out is `/s/{token}` on the server, which the web app serves (`starbooru_core::share::share_path`). Its page always carries `<meta name="robots" content="noindex, nofollow">` and the header `X-Robots-Tag: noindex, nofollow`, so search engines leave it alone.

## Link previews

Chat apps and sites (WhatsApp, Telegram, Discord, Facebook, Slack, iMessage, Google Chat and others) show a pasted link with a title, a line of text and a picture, read from Open Graph tags in the page. Their bots don't run the app, so the server writes the tags into the page itself when it serves the web app (`STARBOORU_WEB_DIR`). Every client gets the same page, whatever its User-Agent.

What has a preview:

- On a public server, a post's own address, `/post/{id}`, when the admin's `link_previews` is on and the post is out of the recycle bin. Every rating.
- On a private server, a share link, `/s/{token}`, when `link_previews` is on, the link is live (not expired, removed, or of a disabled account) and it says yes: its own `preview`, or the account's `previews` when it leaves that to the account (`starbooru_core::preview_on`).
- Nothing else. With `link_previews` off, no page has one.

Opening the page or its pictures doesn't count as a view of the link.

The tags come right after `<head>`, as WhatsApp reads only the first 300 KB:

| Tag | Content |
|---|---|
| `og:type` | `website` |
| `og:site_name` | The server's name |
| `og:title` | A link's title, or the server's name. A post's first three characters, else its copyrights, else its artists (underscores read as spaces), else `#<id>` |
| `og:description` | What a link holds, such as "3 pictures and 1 video" or "3 immagini e 1 video". A post's general tags up to about 200 characters, or the server's name. Never empty, as WhatsApp shows nothing without it |
| `og:url` | The page's absolute address |
| `og:image`, with `og:image:width`, `og:image:height`, `og:image:type` (`image/jpeg`) and `og:image:alt` | The picture. A link names up to four, one for each of its first posts that has one, in its order: Discord shows them together, the others take the first |
| `twitter:card` | `summary_large_image` with a picture (Telegram shows a large one only then), `summary` without |

The texts are in Italian when `Accept-Language` prefers Italian to English, and in English otherwise; the page says `Vary: Accept-Language`. Absolute addresses start from `STARBOORU_PUBLIC_URL`. Without it they take the request's host: `X-Forwarded-Host` and `X-Forwarded-Proto` first with `STARBOORU_TRUST_PROXY`, then `Host`, with `http` for a local name or a private address and `https` for the rest. A host that isn't a plain name or address, with an optional port, gets no tags.

The pictures are outside `/api/v1`, need no token and don't count against a guest's limit:

| Method | Path | Returns |
|---|---|---|
| GET, HEAD | `/preview/s/{token}/{n}.jpg` | The picture of the link's `n`th post (0 to 3) among those that have one |
| GET, HEAD | `/preview/p/{id}.jpg` | A post's picture |

Each answers 404 unless its page has a preview at that moment, so removing or expiring a link, binning a post or turning the switch off stops them at once. The tags add `?v=` and the first 8 hex digits of the post's SHA-256 to each address, so an edited picture, which is another post, gets another address; the server ignores it. The answer has `Content-Type: image/jpeg`, `Cache-Control: public, max-age=3600` and `X-Content-Type-Options: nosniff`, and a link's picture `X-Robots-Tag: noindex` as well.

A picture is a JPEG that fits in 1200 × 1200 and is never enlarged, transparency on white. It comes from a big image's sample, else the original for a picture or a GIF's first frame (AVIF and JPEG XL through ffmpeg), else the thumbnail (videos, and AVIF or JPEG XL without ffmpeg); a post without any of these, or without a stored size, has none. It is saved at quality 82, or 75, 68 or 60 when that keeps it under 550 KB, as WhatsApp drops pictures over 600 KB; the size stays the one the tags name. The server makes it the first time it is asked for and keeps it with the post's other files.

Chat apps keep their own copy of a preview, so a message sent before a link was removed may go on showing its picture and title in that app. Only new messages see the change.

## Media

```text
GET /media/{original|thumb|sample}/{sha256}.{ext}?exp=<unix>&sig=<hex>
GET /media/{avatar|avatar64|face}/{sha256}.jpg?exp=<unix>&sig=<hex>
```

Media URLs come from the API, so never build one by hand. Invalid or expired signatures get a 403. Range requests are supported. Responses are cacheable (`private, max-age=21600, immutable`).

Link previews' pictures have routes of their own (see [Link previews](#link-previews)), never a `/media` URL.
