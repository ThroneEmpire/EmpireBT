# qBittorrent API

EmpireBT also answers qBittorrent's Web API, so programs made for
qBittorrent work with it: front ends such as
[qui](https://github.com/autobrr/qui), and scripts or tools that add and
watch torrents. It is served under `/api/v2/` by `empirebt serve` or
`empirebt ui --web`, at the same address as the web UI.

EmpireBT reports itself as qBittorrent 5.0 (Web API `2.11.4`). It answers
the methods it can, and the rest return 404, as qBittorrent does for
methods it doesn't know.

## Connecting qui

In qui, add an instance:

- **Host:** the web UI's address, such as `http://127.0.0.1:8080`. If qui
  runs in another container, use an address it can reach. Beyond this
  computer the web UI is HTTPS. With EmpireBT's own self-signed
  certificate, turn on qui's *TLS skip verify*. Behind a reverse proxy,
  use the proxy's address.
- **Either** username and password: any username, and the web UI's
  password.
- **Or** an API key: an EmpireBT API token from
  `empirebt web token qui`. The running server reads tokens at start, so
  restart EmpireBT after making one.

qui keeps up through `sync/maindata`, which sends only what changed since
its last request. A few thousand idle torrents cost almost nothing per
refresh.

## What works

| Area | Methods |
|---|---|
| Signing in | `auth/login`, `auth/logout`; the web UI's password, or `Authorization: Bearer <API token>` |
| Application | `app/version`, `app/webapiVersion`, `app/buildInfo`, `app/preferences`, `app/setPreferences`, `app/defaultSavePath` |
| Transfer | `transfer/info`, `transfer/downloadLimit`, `transfer/uploadLimit`, `transfer/setDownloadLimit`, `transfer/setUploadLimit`, `transfer/speedLimitsMode` (always 0) |
| Sync | `sync/maindata` (incremental by `rid`), `sync/torrentPeers` |
| Torrents | `torrents/info` (with `filter`, `category`, `tag`, `hashes`, `sort`, `reverse`, `limit`, `offset`, `includeTrackers`), `properties`, `files`, `trackers`, `webseeds` |
| Changing torrents | `add` (links, URLs, `.torrent` uploads; `savepath`, `category`, `tags`, `stopped`/`paused`, `contentLayout`), `delete` (with `deleteFiles`), `stop`/`pause`, `start`/`resume`, `recheck`, `reannounce`, `rename`, `renameFile`, `renameFolder` |
| Categories and tags | `categories`, `createCategory`, `editCategory`, `removeCategories`, `setCategory`, `tags`, `createTags`, `deleteTags`, `addTags`, `removeTags`, `setTags` |

`app/setPreferences` changes the settings EmpireBT has: `listen_port`, `dht`,
`lsd`, `anonymous_mode`, `encryption`, `bittorrent_protocol`, `max_connec`,
`max_connec_per_torrent`, `dl_limit`, `up_limit`, and
`add_stopped_enabled`. It skips the rest, as qBittorrent skips unknown
keys.

## What doesn't, yet

These answer 404, and qui shows an error if you try them:

- file priorities, sequential download, force start, queue positions,
  automatic torrent management;
- per-torrent speed limits and share limits; alternative speed limits;
- moving a torrent (`setLocation`);
- adding, editing, or removing trackers; adding or banning peers;
- RSS, search, the torrent creator, exporting `.torrent` files, logs,
  piece states.

EmpireBT doesn't record some facts yet, so these read as zero:

- when a torrent was added or completed;
- totals across runs: downloaded, uploaded, ratio, seeding time;
- connected seeds counted separately from peers (`num_seeds` is 0, and
  `num_leechs` counts every connection).

Upload totals for seeds come from this run.

## Security

The web UI's rules apply here too:

- The same password and API tokens.
- The same lockout after failed sign-ins. qBittorrent's 403 says the
  address is banned.
- Torrents come only as links, URLs, or uploads, never server paths.
- Save folders only where the web UI may save.
- Storage, interface, and web settings stay out of reach.

The session cookie is qBittorrent's `SID` (HttpOnly, SameSite=Strict).
Cross-site protection works as in qBittorrent: a request carrying the
cookie that sends `Origin` or `Referer` must name this server. Programs
send neither, so they are unaffected. API tokens need no such check.
