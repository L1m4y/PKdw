# PKdw

![CI](https://github.com/L1m4y/PKdw/workflows/CI/badge.svg)
![License](https://img.shields.io/github/license/L1m4y/PKdw)
![Rust](https://img.shields.io/badge/Rust-2021-orange.svg)
![Last commit](https://img.shields.io/github/last-commit/L1m4y/PKdw)

PKdw is a self-contained PikPak downloader for the terminal, written in Rust.

One binary. No Python, no shell dependencies beyond the idioms of this project,
no browser automation. Downloads are parallel, resumable, and the default
download directory follows the program wherever it goes.

## Quick start

```bash
cargo build --release
./target/release/pkdw get "https://mypikpak.com/s/SHARE_ID"
```

## Feature snapshot

| Capability | Status |
| --- | --- |
| Public share links without an account | Yes |
| Parallel HTTP range downloads | Yes |
| Resume after interruption | Yes |
| Configurable connection count | Yes, `-j/--connections` |
| Quality selection | `original`, `1080p`, `720p`, `480p` |
| Drive listing and downloads | Yes, with login |
| Copy share to drive (`--restore`) | Yes |
| JSON output | `--json` |
| Shell completions | bash, zsh, fish, powershell, elvish |

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

### Common download options

| Flag | Meaning |
| --- | --- |
| `-o, --output DIR` | Destination folder |
| `-q, --quality Q` | Stream quality |
| `--largest` | Keep only the largest file |
| `--min-size MIB` | Skip smaller files |
| `--name TEXT` | Case-insensitive name filter |
| `--pass-code CODE` | Protected share code |
| `--restore` | Copy to drive, then download the full original |
| `--delete-after` | Delete the temporary drive copy |
| `-j, --connections N` | Download workers per file, default 16 |
| `--chunk-size MIB` | Range size, default 16 |
| `--overwrite` | Start from scratch |
| `--dry-run` | Resolve and show selections only |

Exit codes: `0` success, `1` hard failure, `2` incomplete data kept for resume.

## Download location

```text
<folder containing pkdw>/downloads/
```

Override per run or globally:

```bash
pkdw get <link> --output /mnt/storage/videos
PKDW_DOWNLOADS=/mnt/storage PKDW_HOME=/opt/pkdw pkdw get <link>
```

## Authentication

1. Open `https://mypikpak.com` and sign in.
2. DevTools -> Application -> Local Storage -> `https://mypikpak.com`.
3. Find `credentials_*` and copy its `refresh_token`.
4. Save it:

```bash
pkdw auth --from-clipboard
# or
pkdw auth --stdin
```

Tokens are single-use. PKdw persists every rotation to `.env` immediately.
Keep the web app closed while using the CLI.

## Quality and missing ranges

Server links can expose `original`, `1080p`, `720p`, and `480p` streams.
For anonymous shares, the CDN may serve only part of the original object;
PKdw preserves the partial file and resumes it later. Use a transcode or
`--restore` for a complete file.

## Resume files

```text
movie.mkv.part              sparse bytes on disk
movie.mkv.part.pkdw.json    completed byte ranges
```

After a successful download these are removed and the bytes are renamed to
`movie.mkv`. `--overwrite` discards them. Changing `--chunk-size` is safe but
may revisit a few boundary segments.

## Documentation

- `docs/API.md` — researched endpoints, headers, and limits.
- `docs/ARCHITECTURE.md` — modules and download algorithm design.

## License

GNU General Public License v3.0 or later. See [LICENSE](LICENSE).
