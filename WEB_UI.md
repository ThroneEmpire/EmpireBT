# EmpireBT web UI

A browser interface to the same engine the terminal dashboard drives,
served from inside EmpireBT. The code is in `crates/web`; the browser files
(`crates/web/assets`) are built into the binary.

When adding torrents you can choose their category, tags and folder.
Categories can have a folder of their own: give one when creating the
category, or with ✎ next to it. New torrents in that category are then
saved there unless you pick another folder. The web UI saves only in the
download folder, a category's folder, one listed in
`storage.allowed_dirs`, or a folder inside one of those. The web UI can't save anywhere else, so a browser
session can't write outside those folders.

It shows every torrent with live progress and speeds, filters by status,
category, and tag, sorts by any column, and has detail tabs for the
overview, files, peers, and trackers. It works on phones: torrents become
cards, filters a row of chips, and details a full-screen sheet.
Under Settings → Appearance you choose the theme (system, dark, or light)
and an accent color (plain, a preset, or any color); the choice is saved in
that browser only.

The **Peers** tab lists each connected peer with its country flag, address,
client, how it is connected (`in`/`out`, `uTP`/`TCP`, `enc` when
encrypted; hover for what it is doing), its progress, speeds, and the
bytes downloaded from and uploaded to it. It opens sorted by upload speed,
so the peers you are sending to come first, shown in green; click a column
(or, on a phone, use the menu above the list) to sort by anything else. Countries come from the free
[DB-IP Lite](https://db-ip.com) country database (CC BY 4.0), which
EmpireBT downloads in the background (a few MB, through the chosen
network interface) and refreshes monthly, as qBittorrent does. Turn it off
with `peers.countries = false`.

Live updates carry only what changed, so the page stays quick with
thousands of torrents.

## Running it

```bash
empirebt serve          # no dashboard: just the web UI, until Ctrl-C
empirebt ui --web       # the dashboard and the web UI together
```

Both print (or show) the address, a **one-time login link** valid for ten
minutes, and on the very first start a generated password. Change the
password with `empirebt web password`; make a token for scripts with
`empirebt web token <name>` (sent as `Authorization: Bearer <token>`) and
revoke them all with `empirebt web revoke-tokens`.

By default the web UI listens on `127.0.0.1:8080`, this computer only. To
reach it from other devices set `web.address` (for example `0.0.0.0`):
EmpireBT then serves **HTTPS only**, with your certificate
(`web.tls_cert`, `web.tls_key`) or a generated one whose SHA-256
fingerprint it prints so you can check it in the browser. Any host name
reaches it, unless you list names in `web.allowed_hosts`.

What protects it:

- Signing in is always required, even on localhost. Passwords are stored
  as Argon2id hashes, sessions and API tokens as SHA-256 hashes, in a file
  only you can read. Five wrong passwords from an address mean a
  five-minute wait.
- Session cookies are `HttpOnly` and `SameSite=Strict` (and `Secure` over
  HTTPS). Changes need a custom header and a matching `Origin`, so other
  websites cannot act through your browser. A site pointing its own name
  at you (DNS rebinding) gets no further than the sign-in page, since your
  session belongs to the name you signed in on. To refuse other names
  entirely, list yours in `web.allowed_hosts`.
- A strict Content Security Policy (no inline scripts), no framing, and
  data inserted as text only.
- A browser session cannot reach the file system: torrents are added by
  link, URL, or upload, never by a path on the server, and the settings for
  storage, the network interface, and the web UI itself can only be changed
  outside the web UI.
- Request size limits, timeouts for slow clients, and connection caps.

## Settings

| Setting | Default | Meaning |
| --- | --- | --- |
| `web.address` | `127.0.0.1` | Address to listen on; anything but loopback means HTTPS only |
| `web.port` | `8080` | Port |
| `web.https` | `auto` | `auto` serves HTTPS on any address but loopback; `off` serves plain HTTP, only for use behind a reverse proxy that adds HTTPS (see [DOCKER.md](DOCKER.md#behind-a-reverse-proxy)) |
| `web.tls_cert`, `web.tls_key` | empty | Your PEM certificate and key; empty makes one when needed |
| `web.allowed_hosts` | empty | Optional: only these host names (comma-separated), besides `localhost` and IP addresses, reach the web UI; empty allows any, as qBittorrent's default does |

None of these can be changed from the web UI itself. Change them with
`empirebt config set`, in the dashboard (`c`), or in `config.toml`, then
restart.

## Files

In the data directory: `web-auth` (password hash and API token hashes,
owner-only), and when a certificate was generated, `web-cert.pem` and
`web-key.pem` (owner-only). `ip-country.csv.gz` is the country database,
shared with the dashboard.

## API

Everything the page does goes through a JSON API that scripts can use with
an API token; see [WEB_API.md](WEB_API.md).
