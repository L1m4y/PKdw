# PKdw

A self-contained PikPak downloader for the terminal, written in Rust.

PKdw downloads public share links and files from your own PikPak drive. It is a
single binary: no Python, no `fish`, no `curl`, no `tqdm`, and no browser
automation. Downloads are resumable and parallel, and the default output folder
travels with the program.

```
pkdw get <share-link>          # download a public share
pkdw info <share-link>         # inspect it first
pkdw get /Movies/movie.mkv      # download from your own drive (login required)
pkdw whoami                    # account and quota
```

## Features

- Public share links work without an account.
- Login is optional and only needed for drive paths and full-original restore.
- Parallel HTTP range requests with a configurable connection count.
- Resume after Ctrl-C, crashes, or network loss (`.part` + sidecar state).
- Selectable stream quality: `original`, `1080p`, `720p`, `480p`.
- Filter a share by name, minimum size, or largest file.
- Copy a share to your drive and download the complete original when quota
  allows (`--restore`).
- Progress bar with speed and ETA.
- English output, `--json` for scripting, and shell completions.
- The download folder is resolved from the executable location, so moving the
  `PKdw` folder does not break anything.

## Build

```bash
cargo build --release
./target/release/pkdw --help
```

Put the binary next to the project folder if you want a portable install:

```bash
cp target/release/pkdw .
./pkdw get "https://mypikpak.com/s/..."
```

A symlink works too:

```bash
mkdir -p ~/.local/bin
ln -sf "$PWD/target/release/pkdw" ~/.local/bin/pkdw
```

`pkdw` resolves symlinks to the real executable when choosing the program
folder, so the symlinked command still uses this project's `downloads/`.

## Download location

By default:

```text
<folder containing the pkdw executable>/downloads/
```

For this repository that is:

```text
/home/limay/Documentos/Proyectos/PKdw/downloads/
```

The path is dynamic: move or rename the `PKdw` folder and downloads follow it.
You can override it per command or globally:

```bash
pkdw get <link> --output /mnt/disk/videos
PKDW_DOWNLOADS=/mnt/disk/videos pkdw get <link>
PKDW_HOME=/opt/pkdw pkdw get <link>
```

`PKDW_HOME` changes where the program looks for `.env` and the default
`downloads/` folder. This is useful if the binary lives in a read-only location
such as `/usr/local/bin`.

## Commands

### `get` (alias: `download`)

```bash
pkdw get "https://mypikpak.com/s/SHARE_ID"
pkdw get "https://mypikpak.com/s/SHARE_ID" --largest
pkdw get "https://mypikpak.com/s/SHARE_ID" --min-size 100
pkdw get "https://mypikpak.com/s/SHARE_ID" --name episode
pkdw get "https://mypikpak.com/s/SHARE_ID" --quality 1080p
pkdw get "https://mypikpak.com/s/SHARE_ID" --restore --delete-after
pkdw get "https://mypikpak.com/s/SHARE_ID" --dry-run
```

Useful flags:

| Flag | Meaning |
|---|---|
| `-o, --output DIR` | Destination folder |
| `-q, --quality Q` | `original`, `1080p`, `720p`, or `480p` |
| `--largest` | Keep only the largest file |
| `--min-size MIB` | Skip smaller files |
| `--name TEXT` | Case-insensitive name filter |
| `--pass-code CODE` | Pass code for protected shares |
| `--restore` | Copy to drive first, then download the full original |
| `--delete-after` | Delete the temporary drive copy after success |
| `-j, --connections N` | Parallel ranges per file (default 16) |
| `--chunk-size MIB` | Range size (default 16) |
| `--rounds N` | Extra retry rounds (default 3) |
| `--overwrite` | Discard resume state and start again |
| `--dry-run` | Resolve and show what would be downloaded |

Exit codes: `0` success, `1` hard failure, `2` download incomplete (partial
data kept for resume).

### `info`

```bash
pkdw info "https://mypikpak.com/s/SHARE_ID"
pkdw info "https://mypikpak.com/s/SHARE_ID/FILE_ID"
pkdw --json info "https://mypikpak.com/s/SHARE_ID"
```

### `ls`

```bash
pkdw ls
pkdw ls "/Movies"
pkdw --json ls "/Movies"
```

### `auth`

PikPak uses a single-use refresh token. Get one from the web app:

1. Open `https://mypikpak.com` and sign in.
2. DevTools -> **Application** -> **Local Storage** -> `https://mypikpak.com`.
3. Find the `credentials_*` entry and copy its **`refresh_token`** field (not
   `access_token`).
4. In the browser console:

   ```js
   copy(JSON.parse(localStorage.getItem(
     Object.keys(localStorage).find(k => k.startsWith('credentials'))
   )).refresh_token)
   ```

5. Save it:

   ```bash
   pkdw auth --from-clipboard
   # or
   pkdw auth --stdin
   pkdw auth --token 'os...'
   ```

The token rotates every time it is exchanged. `pkdw` writes the replacement to
`.env` immediately. Keep the web app closed while using the CLI, otherwise the
web app can rotate the token first and leave the CLI copy stale.

Remove the stored token with:

```bash
pkdw auth --logout
```

### `whoami`, `config`, `completions`

```bash
pkdw whoami
pkdw config
pkdw completions bash > ~/.local/share/bash-completion/completions/pkdw
```

## Quality and the CDN

PikPak usually exposes several streams per video:

- `original` is the uploaded file.
- `1080p`, `720p`, `480p` are server-side transcodes.

For anonymous share links, PikPak's CDN can serve only part of the original
object. `pkdw` still tries every range and keeps the file as `.part` if some
ranges never become available; run the same command again later to resume. If
you need a complete file without an account, pick a transcode:

```bash
pkdw get <link> --quality 1080p
```

With an account and enough drive quota, `--restore` copies the share to your
drive and downloads the original through `web_content_link`:

```bash
pkdw auth --from-clipboard
pkdw get <link> --restore --delete-after
```

## Resume files

While downloading, PKdw creates:

```text
movie.mkv.part             sparse file with the downloaded bytes
movie.mkv.part.pkdw.json   completed byte ranges
```

Both are reused on the next run. A completed download is renamed to
`movie.mkv` and the sidecars are removed. `--overwrite` deletes them and starts
again.

Changing `--chunk-size` between runs is safe but may re-download the boundary
segments; the sidecar tracks byte ranges, not segment numbers.

## Configuration

`.env` is stored next to the executable (or in `PKDW_HOME`):

```dotenv
PIKPAK_REFRESH_TOKEN=os...
PIKPAK_DEVICE_ID=...
PIKPAK_PROXY=
```

`PIKPAK_PROXY` supports HTTP and SOCKS proxies. If it is empty, the normal
`HTTP_PROXY` / `HTTPS_PROXY` environment variables still apply.

The file is written with mode `0600`. Never commit it. The repository's
`.gitignore` excludes `.env`, partial files, and `target/`.

## Troubleshooting

**`invalid_grant`** — the refresh token was already used. Copy a fresh token
from the browser and run `pkdw auth --from-clipboard`. Refresh tokens are
single-use; a second browser tab or another CLI can consume them.

**`error_code 9` / captcha errors** — the action captcha expired or the request
was rate-limited. `pkdw` retries automatically. If it persists, wait a moment
and retry.

**HTTP 416 / missing ranges** — the CDN does not serve that part of the object
right now. The `.part` is kept. Try again later, use a lower quality, or use
`--restore` with an account.

**The original needs more quota** — the account cannot copy a file larger than
its free drive space. Free space, choose a transcode, or use a Premium account.

**Downloads in the wrong place** — run `pkdw config` to see the resolved paths.
Use `--output`, `PKDW_DOWNLOADS`, or `PKDW_HOME`.

## Legal

PKdw is an unofficial client. Use it only for content you have the right to
download. PikPak's terms and local law apply.

## License

MIT. See [LICENSE](LICENSE).
