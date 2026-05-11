# siphon

A terminal UI for taking database dumps — Postgres, MongoDB, MySQL/MariaDB, SQLite, Redis — with auto-detection of running Docker containers and scheduled auto-backups.

No client tools to install. If `pg_dump`/`mongodump`/etc. aren't on your `PATH`, siphon transparently runs them inside ephemeral Docker containers (`docker run --rm postgres:17 pg_dump …`). Docker is the only hard requirement, and even it's optional when the client tools are installed.

## Install

### Homebrew (macOS / Linux)

```sh
brew tap takshakmudgal/tools
brew install siphon
```

### From source

```sh
git clone git@github.com:takshakmudgal/siphon.git
cd siphon
cargo install --path .
```

### Pre-built binaries

Download from [Releases](https://github.com/takshakmudgal/siphon/releases) — macOS (Apple Silicon + Intel) and Linux (x86_64 + arm64).

## Usage

```sh
siphon
```

Then:

| key            | action                                                    |
| -------------- | --------------------------------------------------------- |
| `↑` `↓` / `jk` | navigate the list                                         |
| `n`            | new connection                                            |
| `i`            | import an autodetected docker DB (credentials filled in)  |
| `d` / `enter`  | dump now                                                  |
| `a`            | toggle / configure auto-backup                            |
| `t`            | test connection                                           |
| `e`            | edit                                                      |
| `D`            | delete (saved entry only — your DB & dumps are kept)      |
| `r`            | rescan docker                                             |
| `o`            | open backup folder                                        |
| `?`            | help                                                      |
| `q`            | quit                                                      |

Backups live in `~/.siphon/backups/<name>-<id>/`. Connections (with credentials) are in `~/.siphon/config.toml` — `chmod 600`.

## How it picks a runtime

For each dump, siphon chooses the first that's available:

1. **Attached container** — if the connection is bound to a docker container (via the `Detected` panel), runs `docker exec` against it.
2. **Local client** — if the client tool (`pg_dump`, `mongodump`, …) is on `PATH`.
3. **Ephemeral container** — `docker run --rm <image> <tool>` with the URI passed in. Hostname `127.0.0.1` is rewritten to `host.docker.internal` so localhost servers still work.

## Managed-provider auto-config

When the hostname matches a known managed database service, siphon applies sensible connection defaults automatically (read-only — never modifies your DB):

| provider             | what we add                          |
| -------------------- | ------------------------------------ |
| Supabase             | `sslmode=require`                    |
| Amazon RDS           | `sslmode=require`                    |
| Neon                 | `sslmode=require`                    |
| Aiven                | `sslmode=require` / `tls=true`       |
| Render / Railway     | `sslmode=require`                    |
| MongoDB Atlas        | `tls=true`                           |
| DigitalOcean DBs     | `sslmode=require` / `tls=true`       |
| Cosmos / Azure       | `sslmode=require` / `tls=true`       |
| Timescale / Cockroach| `sslmode=require`                    |

You can always override in the URI (e.g. `?sslmode=verify-full`); siphon won't replace an explicit setting.

## Build & test

```sh
cargo build --release
cargo test            # 49 unit + integration tests
```
