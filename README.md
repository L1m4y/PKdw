<br/>

<div align="center">

<img src="assets/icon.png" alt="PKdw" width="192"/>

<h1>PKdw</h1>

**A self-contained PikPak downloader for the terminal.**

Downloads PikPak shared links and drive files in parallel, using the accessible direct streams instead of the browser web UI download limits and barriers.

[![Rust](https://img.shields.io/badge/Rust-2021-6F78A8?logo=rust&logoColor=FFFFFF)](https://www.rust-lang.org/)
[![Platform](https://img.shields.io/badge/Platform-Linux-6F78A8?logo=linux&logoColor=FFFFFF)](https://github.com/L1m4y/PKdw)
[![License: GPLv3](https://img.shields.io/badge/License-GPLv3-6F78A8?logo=gnu&logoColor=FFFFFF)](LICENSE)

</div>


## How it works

PKdw resolves a share, walks its files and downloads them with fixed-size HTTP
range requests. Each file is written to a sparse `.part` file while a JSON
sidecar records the completed byte ranges, so an interruption never discards
progress. Finished files are renamed and the sidecars are removed.

Public shares work without an account. With a refresh token, PKdw can list the
drive, fetch complete files through `web_content_link`, and copy a share to the
drive for the full original with `--restore`.

## Installation

```bash
git clone https://github.com/L1m4y/PKdw.git
cd PKdw
cargo build --release
./target/release/pkdw install
```

`install` creates a `pkdw` symlink in `~/.local/bin`, so the default download
directory stays in the source tree.

## Quick start

```bash
pkdw get "https://mypikpak.com/s/SHARE_ID"
pkdw get "https://mypikpak.com/s/SHARE_ID" --largest --quality 1080p
pkdw info "https://mypikpak.com/s/SHARE_ID"
pkdw ls
pkdw doctor
```

## Commands

| Command | Purpose |
| --- | --- |
| `get`, `download` | Download a share link, file, or drive folder |
| `info` | Inspect a share or drive path |
| `ls` | List a drive folder |
| `auth` | Save or rotate the refresh token |
| `whoami` | Show account and quota |
| `doctor` | Validate paths, configuration, and connectivity |
| `install` | Create a `pkdw` symlink in `~/.local/bin` |
| `config` | Show resolved paths |
| `completions` | Generate shell completions |

## Download location

```text
<folder containing pkdw>/downloads/
```

Override with `--output`, `PKDW_DOWNLOADS`, or `PKDW_HOME`.

## Authentication

1. Sign in at `https://mypikpak.com`.
2. DevTools -> Application -> Local Storage -> `https://mypikpak.com`.
3. Copy the `refresh_token` field from `credentials_*`.
4. Run:

```bash
pkdw auth --from-clipboard
```

Tokens are single-use and rotate on every exchange. PKdw persists each rotation to
`.env` immediately. Keep the PikPak web app closed while using the CLI.

## Behaviour notes

- `0` success, `1` hard failure, `2` incomplete data kept for resume.
- `--overwrite` discards resume state and restarts a file.
- Changing `--chunk-size` is safe and may revisit a few boundary segments.
- Anonymous shares can expose partial originals. Use a transcode or `--restore`
  when you need the complete file.
- `.env` is stored with mode `0600` and is ignored by git.

## Documentation

- `docs/API.md` — endpoints, headers, and observed limits.
- `docs/ARCHITECTURE.md` — modules, portable paths, and the download design.


