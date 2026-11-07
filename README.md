# PKdw

A self-contained PikPak downloader for the terminal, written in Rust.

```bash
cargo build --release
./target/release/pkdw get "https://mypikpak.com/s/SHARE_ID"
```

## Features

- Public share links work without an account.
- Resumable parallel downloads with a configurable connection count.
- Quality selection: `original`, `1080p`, `720p`, `480p`.
- Optional login for drive paths and full-original `--restore`.
- Filenames and download folders are derived from the executable location.
- `--json`, shell completions, `pkdw doctor`, and `pkdw install`.

## Download location

By default: `<folder containing pkdw>/downloads/`. Override with `--output`,
`PKDW_DOWNLOADS`, or `PKDW_HOME`.

## Commands

```bash
pkdw get <link> [--largest] [--min-size 100] [--name TEXT] [--quality 1080p]
pkdw get <link> --restore --delete-after   # account and enough drive quota
pkdw get /Drive/Path/video.mkv
pkdw info <link>
pkdw ls [PATH]
pkdw auth --from-clipboard
pkdw whoami
pkdw doctor
pkdw install
pkdw completions bash
```

See `--help` for details. Exit codes: `0` success, `1` hard failure,
`2` incomplete, kept for resume.

## Authentication

Copy the `refresh_token` field from the `credentials_*` Local Storage entry at
`https://mypikpak.com`, then run `pkdw auth --from-clipboard`, `--stdin`, or
`--token`. Tokens are single-use; PKdw writes each rotation to `.env`
immediately. Keep the web app closed while using the CLI.

## Notes

- Anonymous share links can return partial originals; use a transcode or
  `--restore` when you need the complete file.
- `.env` is stored next to the binary (or in `PKDW_HOME`) with mode `0600`.
- Never commit `.env`, partial files, `target/`, or `*.log`.

## Documentation

- `docs/API.md` — researched PikPak endpoints and limits.
- `docs/ARCHITECTURE.md` — module and download design.

## License

GNU General Public License v3.0 or later. See [LICENSE](LICENSE).
