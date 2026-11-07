# PikPak API notes

This document records the protocol used by PKdw and the public research it is
based on. It is a field guide, not an official API specification. PikPak can
change these endpoints at any time.

## Hosts

| Purpose | Base URL |
|---|---|
| OAuth / captcha | `https://user.mypikpak.com` |
| Drive API | `https://api-drive.mypikpak.com` |
| Share pages | `https://mypikpak.com/s/<share_id>` |

## Required headers

Most drive requests need:

```http
X-Device-Id: <stable device id>
X-Captcha-Token: <action-scoped captcha token>
x-client-id: <OAuth client id>
Accept: application/json
```

Authenticated requests add `Authorization: Bearer <access_token>`.

The device id is a stable random value. PKdw stores it in `.env` and creates it
on first run. Reusing the same device id avoids some `riskLimited` responses.

## OAuth refresh-token flow

PikPak clients receive a long-lived refresh token. It is **single-use**: each
exchange returns a new refresh token and invalidates the one that was sent.

```http
POST https://user.mypikpak.com/v1/auth/token
Content-Type: application/json

{
  "client_id": "YNxT9w7GMdWvEOKa",
  "client_secret": "dbw2OtmVEeuUvIptb1Coyg",
  "grant_type": "refresh_token",
  "refresh_token": "os..."
}
```

Response:

```json
{
  "access_token": "<JWT>",
  "refresh_token": "os...",
  "expires_in": 7200,
  "sub": "<user id>"
}
```

Persist the new refresh token immediately. If the process crashes between the
exchange and the write, the old token is already dead.

PKdw supports two client profiles because the access token is tied to the OAuth
client that issued it (the JWT `aud` claim):

| Profile | `client_id` | Version | Package | Salt chain |
|---|---|---|---|---|
| Android | `YNxT9w7GMdWvEOKa` | `1.21.0` | `com.pikcloud.pikpak` | 9 salts from the Android client |
| Web | `YUMx5nI8ZU8Ap8pm` | `undefined` | `drive.mypikpak.com` | 10 salts from `mypikpak.com` |

Refresh is attempted with the Android client first, then the web client. The
profile is re-selected from the `aud` claim of the returned access token.

Relevant errors:

- `invalid_grant` / `error_code 4126`: the refresh token is dead, already used,
  or belongs to another client.
- `client_id not match`: the captcha was requested with a different client than
  the access token.
- `invalid captcha_sign`: wrong salt chain, package, version, or device id.

## Captcha flow

Every drive action has its own captcha token. The token is obtained from:

```http
POST https://user.mypikpak.com/v1/shield/captcha/init?client_id=<client_id>
```

Body (anonymous):

```json
{
  "action": "GET:/drive/v1/share/detail",
  "captcha_token": "",
  "client_id": "YNxT9w7GMdWvEOKa",
  "device_id": "<device id>",
  "meta": {
    "captcha_sign": "1.<md5 chain>",
    "user_id": "",
    "package_name": "com.pikcloud.pikpak",
    "client_version": "1.21.0",
    "timestamp": "<unix milliseconds>"
  },
  "redirect_uri": "ttps://api.mypikpak.com/v1/auth/callback"
}
```

The `ttps://` misspelling is intentional: it is what real clients send.

The signature is a chained MD5:

```text
s = client_id + client_version + package_name + device_id + timestamp_ms
for salt in salts:
    s = md5(s + salt)
sign = "1." + s
```

The salts are hard-coded per client version. They are not secrets, but mixing
sets breaks the signature.

Anonymous captcha requests work without an access token, which is why public
share links can be resolved without an account. Authenticated requests send the
bearer token and the user id in `meta.user_id`.

Captcha tokens are cached per action in memory. When the API returns
`error_code: 9`, PKdw invalidates that action token and retries once.

## Public share flow

### 1. Resolve the share

```http
GET /drive/v1/share?share_id=<id>&pass_code=<code>&thumbnail_size=SIZE_LARGE&limit=100
```

Response fields used:

```json
{
  "share_status": "OK",
  "pass_code_token": "...",
  "share_status_text": ""
}
```

If `share_status` is `PASS_CODE_EMPTY` or `PASS_CODE_ERROR`, ask for the pass
code and try again. Other statuses mean the share is unavailable.

### 2. List the share tree

```http
GET /drive/v1/share/detail
  ?share_id=<id>
  &pass_code_token=<token>
  &parent_id=<folder id, omitted at root>
  &thumbnail_size=SIZE_LARGE
  &limit=100
  &with_audit=true
  &filters={"phase":{"eq":"PHASE_TYPE_COMPLETE"},"trashed":{"eq":false}}
```

Paginate with `next_page_token`. Folders have `kind: "drive#folder"` and are
walked recursively; files have `kind: "drive#file"`.

### 3. Resolve a file link

```http
GET /drive/v1/share/file_info
  ?share_id=<id>
  &file_id=<file id>
  &pass_code_token=<token>
  &with_audit=true
  &thumbnail_size=SIZE_LARGE
```

The interesting fields are:

```json
{
  "file_info": {
    "name": "movie.mp4",
    "size": "123456789",
    "hash": "GCID...",
    "web_content_link": "...",
    "medias": [
      {
        "media_name": "Original",
        "is_origin": true,
        "need_more_quota": false,
        "link": {"url": "https://dl-.../download/?..."}
      },
      {
        "media_name": "1080P",
        "link": {"url": "https://..."}
      }
    ]
  }
}
```

`size` and other byte counts are frequently strings. PKdw parses both strings
and numbers.

`hash` is not a SHA-1 of the file. It is PikPak's internal GCID and appears in
the CDN URL as `g=`. Do not use it for integrity verification.

`web_content_link` is the download link for a file that is already in a drive.
For a share it may be empty; in that case use `medias[*].link.url`.

## Download semantics

CDN links accept HTTP range requests:

```http
GET <media url>
Range: bytes=start-end
```

Expected response:

```http
HTTP/1.1 206 Partial Content
Content-Range: bytes start-end/total
```

Observed behavior:

- Range requests are supported on media links.
- A range outside the currently served region returns `416` with
  `X-Xos-Err-Desc: 10`.
- The CDN can serve a contiguous prefix and a suffix of an object while leaving
  a gap in the middle. The served map can change over time and is shared by the
  CDN nodes tested.
- Anonymous links carry a signed `po=0` parameter. The server does not accept
  modified or extra query parameters (`403`), so the range map cannot be
  bypassed by editing the URL.
- `web_content_link` for a file in a drive can serve the complete object and is
  charged to the account's transfer quota.

PKdw therefore downloads fixed-size ranges in parallel, retries failed ranges
with a fresh link, and keeps the partial file plus a byte-range sidecar if some
ranges cannot be served. It never renames a partial file to its final name.

## Copy-to-drive flow (`--restore`)

```http
POST /drive/v1/share/restore
Content-Type: application/json

{
  "share_id": "<id>",
  "pass_code_token": "<token>",
  "file_ids": ["<file id>"]
}
```

If the response includes a task id, poll:

```http
GET /drive/v1/tasks/<task id>
```

until `phase` is `PHASE_TYPE_COMPLETE`. Then find the file in the drive and read
its `web_content_link`:

```http
GET /drive/v1/files/<file id>?with_audit=true&thumbnail_size=SIZE_LARGE
```

The copy consumes drive quota. Free accounts have a small quota, so a file must
fit before it can be restored. Delete the copy afterwards with:

```http
DELETE /drive/v1/files/<file id>
```

## Drive listing

```http
GET /drive/v1/files?parent_id=<id>&limit=200&thumbnail_size=SIZE_MEDIUM
```

Paginate with `next_page_token`. Folders use `kind: "drive#folder"`. Root has no
`parent_id`.

Quota:

```http
GET /drive/v1/about
```

```json
{
  "quota": {
    "limit": "10737418240",
    "usage": "2147483648",
    "usage_in_trash": "0"
  }
}
```

VIP:

```http
GET /drive/v1/privilege/vip
```

## Limits reported by PikPak and by the field research

- Premium: 40 TB/month cloud download, 4 TB/month downstream, 1 TB/month upload.
- Free accounts: roughly 20 GB/day of downstream traffic.
- "Connected apps" (third-party APIs, WebDAV) can be limited to 25% of the
  monthly allowance; free accounts may see ~5 GB/day and speed throttling.
- Anonymous media links are not guaranteed to serve the full object.
- The original of a share can require more quota than a free account has; the
  API then returns `need_more_quota` and omits the link.

## Sources

- [`youming-ai/pikpak-downloader`](https://github.com/youming-ai/pikpak-downloader)
  — Rust CLI with refresh-token rotation, captcha handling, resumable downloads,
  and a JSON contract.
- [`Bengerthelorf/pikpaktui`](https://github.com/Bengerthelorf/pikpaktui) — Rust
  TUI/CLI with drive, share, upload, and download implementations.
- [`Quan666/PikPakAPI`](https://github.com/Quan666/PikPakAPI) — widely used
  Python client and endpoint reference.
- [`YJBeetle/PikpakGet`](https://github.com/YJBeetle/PikpakGet) — restore one
  file at a time, download, verify, delete.
- [`rozx/pikpak-dl-cli`](https://github.com/rozx/pikpak-dl-cli) — clipboard,
  resume, GCID checks, and recycling temporary drive copies.
- [`deepdarkpaw/pikpak-sdk`](https://github.com/deepdarkpaw/pikpak-sdk) — SDK
  reference for `web_content_link` vs `medias[]`.
- [`TeamBreakerr/gopeed-extension-pikpak`](https://github.com/TeamBreakerr/gopeed-extension-pikpak)
  — notes on anonymous video vs login/restore for other files.
- [PikPak referral help](https://mypikpak.com/zh-CN/help-center/referral_program/referral_reward/code-no-reward)
  — official wording on invitation rewards.

The CDN range-map measurements in this repository came from the earlier
`INVESTIGACION.md` field notes: the original object was observed serving roughly
55% from the start plus a suffix, with transcodes showing similar gaps. Those
numbers are a snapshot, not a fixed rule.

## Security notes

- Never log access tokens or refresh tokens. PKdw redacts the refresh token from
  server error text.
- `.env` is written atomically with mode `0600`.
- Access tokens live only in memory. Refresh tokens rotate on every exchange and
  are persisted immediately.
- The browser and the CLI must not use the same refresh token concurrently.
