# EmpireBT web API

The web UI is built entirely on a JSON API that scripts can use too. The
API is served by `empirebt serve` or `empirebt ui --web`. See
[WEB_UI.md](WEB_UI.md) for addresses, HTTPS, and passwords.

The API is not yet versioned. It follows the web UI and may change between
releases.

## Signing in

Scripts use an **API token**:

```bash
empirebt web token backup-script     # prints the token once
empirebt web revoke-tokens           # revokes every token
```

Send the token in a header with every request:

```bash
curl -H "Authorization: Bearer $TOKEN" http://127.0.0.1:8080/api/state
```

Tokens are stored hashed, so a lost token cannot be shown again; make a new
one. A token made while the web UI is running works after the next restart.

Browsers sign in with a session cookie instead:

- `POST /api/login` with `{"password": "..."}`, or with `{"token": "..."}`
  to redeem a one-time login link.
- `POST /api/logout` signs out.

Requests that change something with a cookie also need the header
`X-EmpireBT: 1`, and a matching `Origin` header when one is sent. This
protects against cross-site requests. Token requests skip both checks.

Five failed sign-ins from one address lock that address out for five
minutes. During the lockout, sign-in answers `429` with `Retry-After`, even
for the right password.

## Conventions

- **Torrents** are named by their info hash: 40 hexadecimal characters, in
  the URL as `{hash}`. For v2-only torrents, this is the first 20 bytes of
  the SHA-256 info hash.
- **Bodies** are JSON (`Content-Type: application/json`), at most 64 KiB.
  The exception is uploads, which take a raw `.torrent` file of up to
  16 MiB.
- **Field names** are camelCase. A field with no value is `null`.
- **Errors** are `{"error": "message"}`. The status codes are:

| Status | Meaning |
| --- | --- |
| `200` | Here is the result |
| `202` | Accepted, finishing in the background (adding by URL) |
| `204` | Done, nothing to return |
| `400` | The request was understood but refused; `error` says why |
| `401` | Not signed in, or a wrong password |
| `403` | Missing `X-EmpireBT`, a cross-site request, a locked setting, or a save folder outside the allowed ones |
| `404` | No such torrent or endpoint |
| `413` | Body too large |
| `421` | Unknown host name (add it to `web.allowed_hosts`) |
| `429` | Too many failed sign-ins |
| `503` | Too many live-update streams open |

## Endpoints

| Method and path | Body | Answer |
| --- | --- | --- |
| `GET /api/session` | | `{"signedIn": true}`, or `401` |
| `GET /api/state` | | Everything, see [State](#state) |
| `GET /api/events` | | Live updates, see [Live updates](#live-updates) |
| `POST /api/torrents` | `{"source": "magnet:…"}` or an `http(s)://` URL of a `.torrent` file; optionally `"savePath"`, `"category"`, `"tags": [...]` | `{"added": hash}`, or `202 {"fetching": true}` for URLs |
| `POST /api/torrents/upload?savePath=…&category=…&tags=a,b` | the `.torrent` file's bytes (each query value optional, URL-encoded) | `{"added": hash}` |
| `GET /api/torrents/{hash}` | | [Details](#details) |
| `GET /api/torrents/{hash}/peers` | | [Peers](#peers) |
| `POST /api/torrents/{hash}/start` | | `204` |
| `POST /api/torrents/{hash}/pause` | | `204` |
| `POST /api/torrents/{hash}/toggle` | | `204` (start if paused, else pause) |
| `POST /api/torrents/{hash}/recheck` | | `204` |
| `DELETE /api/torrents/{hash}` | | `204`; downloaded files stay on disk |
| `PUT /api/torrents/{hash}/category` | `{"category": "ISOs"}` (`null` for none) | `204`; a new name creates the category |
| `PUT /api/torrents/{hash}/tags` | `{"tags": ["linux", "web"]}` | `204`; replaces all tags |
| `POST /api/categories` | `{"name": "ISOs"}` | `204` |
| `PUT /api/categories/folder` | `{"name": "Films", "savePath": "/media/films"}` (`null` or empty takes the folder away) | `204`; makes the category if new |
| `POST /api/categories/remove` | `{"name": "ISOs"}` | `204`; torrents keep their other labels |
| `POST /api/tags` | `{"name": "linux"}` | `204` |
| `POST /api/tags/remove` | `{"name": "linux"}` | `204` |
| `PUT /api/settings` | `{"key": "network.upload_limit", "value": "300"}` | `204` |

Torrents can't be added from a path on the server. Use a magnet link, a
URL, or an upload.

`savePath` picks the folder a new torrent is saved in. Without it, the
torrent goes in its category's folder, if the category has one, and
otherwise in the download folder. New categories and tags are created as
needed. The API accepts only these folders,
and answers `403` for anything else, including paths with `..`:
- the download folder;
- folders listed in `storage.allowed_dirs`;
- category folders (all of these appear as `lists.saveDirs` in the
  state);
- any folder inside one of those.

Settings take the same text values as `empirebt config set`. Settings
under `storage.*` and `web.*`, and `network.interface`, answer `403`. They
can only be changed on the machine itself (`empirebt config set`, the
dashboard, or `config.toml`).

## State

`GET /api/state` answers with a full update; live updates have the same
shape.

```json
{
  "full": true,
  "header": {
    "generation": 1842,
    "totals": { "down": 5242880.0, "up": 131072.0 },
    "limits": { "down": 0, "up": 307200 },
    "activeCount": 1,
    "seedingCount": 12,
    "network": {
      "port": 6881,
      "seeding": { "state": "listening", "torrents": 12 },
      "peers": 31,
      "dht": { "state": "running", "nodes": 412, "ipv6Nodes": 96 },
      "utp": true,
      "lsd": true,
      "interface": null,
      "interfaceUp": true
    }
  },
  "torrents": [ { "hash": "…", "name": "…", "…": "see below" } ],
  "removed": [],
  "order": ["…hash…", "…hash…"],
  "lists": {
    "categories": ["ISOs"],
    "tags": ["linux"],
    "categoryFolders": { "Films": "/media/films" },
    "saveDirs": ["/home/me/Downloads/EmpireBT", "/media/films"],
    "settings": [
      {
        "key": "network.upload_limit",
        "kind": "rate",
        "choices": [],
        "description": "Total upload speed in KiB/s; 0 is unlimited",
        "value": "300",
        "needsRestart": false,
        "overridden": false,
        "editable": true
      }
    ]
  },
  "notices": [
    { "id": 7, "level": "success", "text": "Added ubuntu.iso", "ageMs": 1200 }
  ]
}
```

- **Speeds** are bytes per second, and limits are bytes per second with
  `0` meaning unlimited.
- **`seeding.state`** is one of `idle`, `starting`, `listening` or
  `failed`. When it is `failed`, an `error` field says why.
- **`dht.state`** is one of `disabled`, `running` or `failed`.
- **`order`** lists every torrent's hash in the order the torrents were
  added.
- **Setting kinds** are `path`, `port`, `count`, `seconds`, `rate`, `bool`,
  `hosts`, `choice` (with `choices`), `interface`, `address` and `text`.
- **`notices`** are the dashboard's messages: `info`, `success` or `error`.

Each torrent:

```json
{
  "hash": "0123456789abcdef0123456789abcdef01234567",
  "name": "ubuntu-26.04-desktop-amd64.iso",
  "status": "downloading",
  "statusLabel": "Downloading",
  "progress": 0.4213,
  "size": 6114770944,
  "remaining": 3538944000,
  "down": 5242880.0,
  "up": 65536.0,
  "peers": 38,
  "seeders": 1200,
  "leechers": 85,
  "category": "ISOs",
  "tags": ["linux"],
  "error": null,
  "magnet": false,
  "savePath": "/home/me/Downloads/EmpireBT"
}
```

- **`status`** is one of `queued`, `metadata`, `checking`, `announcing`,
  `downloading`, `waiting`, `retrying`, `pausing`, `seeding`, `paused` or
  `error`. Show `statusLabel` to people.
- **`progress`** runs from 0 to 1. It is `null` until the torrent has been
  checked.
- **`size`** is `null` for a magnet link whose metadata hasn't arrived;
  `magnet` is then `true`.
- **`seeders` and `leechers`** are the swarm size reported by trackers,
  `null` when unknown.
- **`savePath`** is the folder its files are in.

## Live updates

`GET /api/events` is a stream of
[Server-Sent Events](https://html.spec.whatwg.org/multipage/server-sent-events.html)
named `update`, sent at most once a second and only when something changed:

```
event: update
data: {"full":false,"header":{…},"torrents":[…],"removed":[],"notices":[]}
```

The first event on each connection is full, like `/api/state`. Later events
carry only what changed:

- `header`, always.
- `torrents`: only the torrents that changed since the last event, whole.
- `removed`: hashes of torrents that are gone.
- `order`, only when torrents were added, removed or reordered.
- `lists`, only when categories, tags or settings changed.
- `notices`: only new ones.

To follow along, keep a map of torrents by hash:
1. On a `full` event, clear it.
2. Store every torrent in `torrents`.
3. Delete each hash in `removed`.
4. Replace your order with `order` when it is present.

After a reconnect the first event is full again, so nothing is missed.

At most 16 streams can be open at once. In a browser, `EventSource` sends
the session cookie. A script sends the `Authorization` header instead:

```bash
curl -N -H "Authorization: Bearer $TOKEN" http://127.0.0.1:8080/api/events
```

## Details

`GET /api/torrents/{hash}`:

```json
{
  "torrent": { "…": "as in the state" },
  "files": [ { "path": "album/01.flac", "size": 31457280 } ],
  "trackers": ["udp://tracker.example:6969/announce"],
  "pieces": 1458,
  "verifiedPieces": 614,
  "pieceLength": 4194304,
  "format": "v1",
  "private": false,
  "announce": {
    "trackersOk": 2,
    "trackersTotal": 3,
    "uploaded": 1073741824,
    "lastError": "tracker.example: timed out",
    "dhtAnnounced": true
  },
  "savePath": "/home/me/Downloads/EmpireBT"
}
```

- `files`, `pieces`, `pieceLength` and `format` (`v1`, `v2` or `hybrid`)
  are empty or `null` until a magnet link's metadata arrives.
- `announce` describes seeding announces. It is `null` while the torrent
  is not seeding.

## Peers

`GET /api/torrents/{hash}/peers`, for the peers connected right now:

```json
{
  "peers": [
    {
      "address": "203.0.113.7:51413",
      "ip": "203.0.113.7",
      "port": 51413,
      "country": "DE",
      "client": "qBittorrent 4.6.5",
      "outgoing": true,
      "utp": false,
      "encrypted": true,
      "progress": 0.25,
      "down": 1048576.0,
      "up": 0.0,
      "downloaded": 52428800,
      "uploaded": 0,
      "interested": false,
      "unchoked": false,
      "interesting": true,
      "chokingUs": false
    }
  ],
  "attribution": "IP geolocation by DB-IP (db-ip.com)"
}
```

- **`country`** is an ISO 3166 code. It is `null` until the country
  database has loaded, or when `peers.countries` is off. `ZZ` means
  unknown.
- **`client`** comes from the peer's extension handshake, or else is
  decoded from its peer ID. It is `null` when neither says.
- **`outgoing`** means we connected to the peer.
- **`progress`** is `null` until the peer says which pieces it has.
- **`down` and `up`** are bytes per second. **`downloaded` and
  `uploaded`** are bytes on this connection.
- **`interested` and `unchoked`** mean the peer wants data from us, and we
  are sending it.
- **`interesting` and `chokingUs`** mean we want data from the peer, and
  whether it refuses.

If you show countries, credit DB-IP with `attribution`; its free database
requires it.
