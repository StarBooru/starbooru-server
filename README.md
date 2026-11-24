<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/brand/mark-gold.svg">
    <img src="docs/brand/mark.svg" alt="StarBooru logo" width="120">
  </picture>
</p>

<h1 align="center">StarBooru server</h1>

<p align="center">The server behind StarBooru, a self-hosted booru for your private image and video collection.</p>

<p align="center">
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-AGPL--3.0--or--later-f1c45e?style=flat-square" alt="License"></a>
  <a href="https://github.com/StarBooru/starbooru-server/actions/workflows/ci.yml"><img src="https://github.com/StarBooru/starbooru-server/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="https://www.rust-lang.org"><img src="https://img.shields.io/badge/Rust-1b1342?style=flat-square&logo=rust&logoColor=white" alt="Rust"></a>
  <a href="https://github.com/tokio-rs/axum"><img src="https://img.shields.io/badge/axum-1b1342?style=flat-square" alt="axum"></a>
  <a href="https://www.postgresql.org"><img src="https://img.shields.io/badge/PostgreSQL-1b1342?style=flat-square&logo=postgresql&logoColor=white" alt="PostgreSQL"></a>
</p>

It stores the files, searches them and answers a JSON API: tags, imports from other boorus, OCR, and AI on local models or any OpenAI-compatible API. It has no interface of its own, but its Docker image serves the web app. The apps each have a repository:

| App | Repository |
|---|---|
| Web | [starbooru-web](https://github.com/StarBooru/starbooru-web) |
| Android | [starbooru-android](https://github.com/StarBooru/starbooru-android) |
| Windows | [starbooru-windows](https://github.com/StarBooru/starbooru-windows) |
| Linux | [starbooru-linux](https://github.com/StarBooru/starbooru-linux) |

The first StarBooru, one Dioxus app for every platform, is archived at [StarBooru/StarBooru](https://github.com/StarBooru/StarBooru).

## Run it

```sh
git clone https://github.com/StarBooru/starbooru-server.git
cd starbooru-server
cp .env.example .env   # set POSTGRES_PASSWORD and STARBOORU_SECRET
docker compose -f docker-compose.yml -f docker-compose.local.yml up -d
```

StarBooru is then on http://localhost:8080, web app included. The first account to register becomes admin. Dokploy, graphics cards and every setting are in [docs/deployment.md](docs/deployment.md).

From source, with [Rust](https://rustup.rs) 1.88 or newer and PostgreSQL 13 or newer:

```sh
cargo build --release -p starbooru-server
export DATABASE_URL=postgres://starbooru:password@127.0.0.1:5432/starbooru
export STARBOORU_SECRET=at-least-32-characters-kept-the-same
./target/release/starbooru-server
```

## Docs

[Architecture](docs/architecture.md), [HTTP API](docs/api.md), [development](docs/development.md) and [roadmap](docs/roadmap.md). [AGENTS.md](AGENTS.md) has the rules for contributors, human or not.

## License

[AGPL-3.0-or-later](LICENSE). The AI models the server downloads are listed in [ATTRIBUTION.md](ATTRIBUTION.md).

StarBooru is an unofficial personal project, not affiliated with or endorsed by TYPE-MOON or Aniplex.
