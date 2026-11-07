# Architecture

PKdw is a small Rust binary with three layers.

```
cli.rs / commands/*        user-facing commands and filtering
api/*                      HTTP, OAuth, captcha, share and drive models
download.rs                range planning, resumable writes, progress
paths.rs / config.rs       portable paths and the local .env file
```

## Startup

1. `Paths::discover()` finds the directory containing the real executable.
   Development builds under `target/{debug,release}` are mapped back to the
   project root, so `cargo run` and tests also use `<project>/downloads`.
2. `PikPak::new()` loads `.env`, creates a device id if needed, and builds one
   `reqwest::Client` with connection pooling.
3. The requested command runs. Public share commands never touch the refresh
   token; drive commands call `require_session()`.

## HTTP layer

`api::client::PikPak` is cloneable and cheap to share. It owns:

- the `reqwest::Client`;
- the local `ConfigFile` behind a mutex;
- the current `Session` (access token, profile, user id) behind a mutex;
- an in-memory captcha cache keyed by action and client.

Every API call:

1. picks the profile (Android for anonymous, the token's `aud` profile for
   authenticated calls);
2. gets or refreshes the action captcha token;
3. sends the request;
4. retries once on `error_code 9` (captcha) and once on 401/invalid token;
5. turns non-success bodies into an error with a short actionable hint.

Token refresh is serialized in memory. The rotated refresh token is written
before the old one can be used again. If a newly supplied token is rejected,
`auth` restores the previous token instead of leaving a broken `.env`.

## Share resolution

```
share URL
  -> parse_target()
  -> GET /drive/v1/share            (pass_code_token)
  -> GET /drive/v1/share/detail     (recursive tree, paginated)
  -> GET /drive/v1/share/file_info  (medias / web_content_link)
  -> LinkSource                     (re-resolves links on demand)
```

`ShareLinkSource` is wrapped in `CachedLink`, which reuses a URL for 90 seconds
and forces a new one after a failed range. Media selection prefers the original
stream, then the requested transcode, then the nearest allowed transcode. It
never selects a stream marked `need_more_quota` or `is_visible: false`.

## Download engine

For a file of size `S` and chunk size `C`:

1. The destination is `downloads/<sanitized name>`.
2. The sparse data file is `<name>.part`, reserved to `S` bytes.
3. The sidecar `<name>.part.pkdw.json` stores `source_id`, `S`, and completed
   byte ranges.
4. Segments are a fixed grid `[0, C), [C, 2C), ...`.
5. Up to `--connections` tasks run concurrently. Each task requests
   `Range: bytes=start-end`, verifies `Content-Range`, writes with
   `write_all_at`, then merges the range into the sidecar.
6. A failed range is retried four times with exponential backoff and a forced
   link refresh. Surviving failures are retried in additional rounds.
7. When every segment is complete, the file is synced, renamed to its final
   name, and the sidecar is removed. Otherwise the sidecar and `.part` remain.

The sidecar is created before the first range request, so even a crash during
the first second is resumable. A sidecar without its `.part` is reset, because
skipping ranges whose bytes are not on disk would corrupt the result.

Writes use OS positional I/O (`write_all_at` on Unix, `seek_write` on Windows)
so concurrent tasks never share a file cursor.

## Portable paths

```
program_dir  = directory of the real executable
downloads    = $PKDW_DOWNLOADS or program_dir/downloads
config file  = $PKDW_ENV_FILE or program_dir/.env
```

`$PKDW_HOME` overrides `program_dir`. This keeps the default install portable
while allowing a read-only binary directory to point somewhere writable.

## Testing

- Unit tests cover URL parsing, filename sanitization, size parsing, captcha
  signing, media selection, config parsing, range planning, and state merging.
- `tests/download_ranges.rs` starts a local range server and verifies parallel
  assembly plus resume with a pre-existing sidecar.
