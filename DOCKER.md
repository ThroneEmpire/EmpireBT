# EmpireBT in Docker

The image runs `empirebt serve`, EmpireBT with its web UI and no terminal
dashboard. It is built in two stages:
- **Build:** the official Rust image builds the binary.
- **Run:** the binary goes into
  [distroless](https://github.com/GoogleContainerTools/distroless)
  `cc-debian12`, a minimal image with glibc, CA certificates and a non-root
  user, and no shell or package manager.

The image is about 33 MB.

## Quick start

From the project folder:

```bash
mkdir -p data downloads
docker compose up -d
docker compose logs empirebt
```

The logs show a one-time sign-in link and, on the first start, the
password:

```
Web UI: https://localhost:8080
Sign in with this one-time link (valid for 10 minutes):
  https://localhost:8080/#login=…
First start: the web UI password is xyfAe3zzVYQart9Rqw8v
HTTPS certificate fingerprint (SHA-256):
  20:BD:46:…
```

Open the link. The web UI uses HTTPS with a certificate EmpireBT made for
itself, so your browser warns you once. You can check that the fingerprint
it shows matches the one in the logs, then continue.

From another device, use the computer's IP address or its name, for
example `https://192.168.1.20:8080` or `https://nas:8080`.

**Create `data` and `downloads` before the first start.** If Docker creates
them, they belong to root and EmpireBT can't write to them; the log then
says `Permission denied`.

## What's where

| In the container | In the compose file | Holds |
| --- | --- | --- |
| `/data` | `./data` | `config.toml`, the torrent list, resume data, DHT nodes, the country database, the web UI password hash and certificate |
| `/downloads` | `./downloads` | Downloaded files |

The container runs as the user given by `PUID` and `PGID` (default
`1000:1000`), so these files belong to you. If your ids differ (`id -u`,
`id -g`), put them in a `.env` file next to `docker-compose.yml`:

```
PUID=1001
PGID=1001
```

## Ports

| Port | For |
| --- | --- |
| `8080/tcp` | The web UI |
| `6881/tcp`, `6881/udp` | Peers: TCP, uTP and the DHT |

Keep the peer port the same on both sides (`6881:6881`): EmpireBT tells
other peers the port it listens on inside the container. To use another
port, change both sides and set `EMPIREBT_NETWORK_PORT` to match. Forward
that port, TCP and UDP, on your router so peers can connect to you.

## Settings

Every setting can be set with an environment variable named
`EMPIREBT_<SECTION>_<NAME>`: for example, `network.upload_limit` is
`EMPIREBT_NETWORK_UPLOAD_LIMIT`. The full list is in the
[README](README.md#settings) and [WEB_UI.md](WEB_UI.md#settings).

Environment variables apply on every start and override `config.toml`
without changing it. The web UI shows them as overridden. Settings you
don't set in the environment can still be changed in the web UI, and are
saved in `data/config.toml`. A misspelled variable gets a warning in the
logs, and a bad value stops the container with an error naming the
variable.

The image already sets:

| Variable | Value | Why |
| --- | --- | --- |
| `EMPIREBT_DATA_DIR` | `/data` | Where EmpireBT keeps its files |
| `EMPIREBT_STORAGE_DOWNLOAD_DIR` | `/downloads` | Where downloads go |
| `EMPIREBT_WEB_ADDRESS` | `0.0.0.0` | Reachable from outside the container |

Commonly set:

| Variable | Example | Meaning |
| --- | --- | --- |
| `EMPIREBT_WEB_ALLOWED_HOSTS` | `torrents.example.com` | Optional: only these names (and IP addresses, `localhost`) reach the web UI; empty allows any |
| `EMPIREBT_WEB_HTTPS` | `auto` or `off` | `off` only behind a reverse proxy (see below) |
| `EMPIREBT_WEB_PASSWORD` | | Sets the web UI password on every start, instead of the generated one |
| `EMPIREBT_WEB_PASSWORD_FILE` | `/run/secrets/empirebt_password` | The same, read from a file (a Docker secret) |
| `EMPIREBT_NETWORK_UPLOAD_LIMIT` | `1000` | Upload speed limit, KiB/s (`0` is unlimited) |
| `EMPIREBT_NETWORK_DOWNLOAD_LIMIT` | `0` | Download speed limit, KiB/s |
| `EMPIREBT_NETWORK_MAX_CONNECTIONS` | `500` | Peer connections across all torrents |
| `EMPIREBT_NETWORK_INTERFACE` | `tun0` | Use only this network interface (see [VPN](#vpn)) |
| `EMPIREBT_NETWORK_ANONYMOUS` | `true` | Don't tell peers and trackers this is EmpireBT (see [Privacy](README.md#privacy)) |
| `EMPIREBT_STORAGE_ALLOWED_DIRS` | `/media/films,/media/shows` | More folders the web UI may save torrents in; mount them as volumes too |

You can also run commands in the container, for example to change the
password or make an API token:

```bash
docker compose exec empirebt empirebt web password
docker compose exec empirebt empirebt web token my-script
```

Restart the container afterwards (`docker compose restart`) for the change
to take effect.

## Behind a reverse proxy

If Caddy, Traefik or nginx already gives your services HTTPS, set
`EMPIREBT_WEB_HTTPS: "off"`. EmpireBT then serves plain HTTP and leaves
HTTPS to the proxy. Signing in is still required.

- **Don't publish port 8080 to the internet with HTTPS off.** Passwords
  would cross the network in plain text. Only the proxy should reach it.
- **Any domain works;** there is nothing to list. Proxies that pass the
  original `Host` (Caddy and Traefik do by default) and ones that send
  `X-Forwarded-Host` instead both work.
- **Disable buffering for live updates,** which use Server-Sent Events. In
  nginx that's `proxy_buffering off;` for `/api/events`.

Caddy example:

```
torrents.example.com {
    reverse_proxy empirebt:8080
}
```

Or under a path of an existing site, with the path stripped before it
reaches EmpireBT (the web UI asks for everything by relative paths):

```
example.com {
    redir /torrents /torrents/
    handle_path /torrents/* {
        reverse_proxy empirebt:8080
    }
}
```

## VPN

`docker-compose.vpn.yml` runs EmpireBT through
[gluetun](https://github.com/qdm12/gluetun), a VPN client container that
supports most providers. EmpireBT shares gluetun's network, and
`EMPIREBT_NETWORK_INTERFACE: "tun0"` binds every connection to the VPN
interface. If the VPN drops, EmpireBT connects nothing at all, rather than
falling back to your normal connection.

1. Fill in your provider's details in the `gluetun` section. The example
   is set up for ProtonVPN; the
   [gluetun wiki](https://github.com/qdm12/gluetun-wiki) lists every
   provider.
2. `mkdir -p data downloads forwarded && docker compose -f docker-compose.vpn.yml up -d`

The web UI's port is published on the `gluetun` service, because EmpireBT
uses its network.

### Port forwarding

Without a forwarded port, peers can't connect to you. You can still
download, and upload to peers you connect to, but you'll find fewer peers
and seed less. Providers that forward a port fix that; gluetun supports
ProtonVPN, Private Internet Access, Perfect Privacy and PrivateVPN.

The VPN provider picks the port, and ProtonVPN picks a new one every time
the VPN reconnects. The example handles that in three steps:

1. **gluetun asks for a port** (`VPN_PORT_FORWARDING: "on"`), opens it in
   its firewall, and runs `VPN_PORT_FORWARDING_UP_COMMAND`. That command
   writes the port into `forwarded/port`, a folder both containers share.
2. **EmpireBT reads that file** (`EMPIREBT_NETWORK_PORT_FILE`) at start
   and every 5 seconds after.
3. **When the port changes, EmpireBT moves to it without restarting.** It
   moves its TCP listeners, its uTP and DHT socket, and its local
   discovery, then announces the new port to every tracker and the DHT at
   once. Connections on the old port end, but a VPN reconnect has already
   ended them. Downloads continue where they were.

Until the file exists, EmpireBT listens on 6881, which nobody outside can
reach. To check that it worked, see the "Now taking peers on port …"
message in the web UI, or the port in its header.

For a provider without native support in gluetun, forward the port in your
provider's account. Then set `FIREWALL_VPN_INPUT_PORTS` in gluetun and
`EMPIREBT_NETWORK_PORT` in EmpireBT to that port.

The port file works outside Docker too: `network.port_file` names any file
holding a port number. A file that is missing, empty, or holds `0` leaves
the port as it is. Anything else that isn't a port is reported once.

## Local peer discovery

Finding peers on your own network (`network.lsd`) uses multicast, which
doesn't cross Docker's default network. To use it, give the container the
host's network: replace `ports:` with `network_mode: host`.

## Importing from qBittorrent

An import changes EmpireBT's session, so EmpireBT must be stopped while it
runs. Run it as a one-off container that uses the same volumes:

```bash
docker compose stop empirebt
docker compose run --rm empirebt import qbittorrent \
  --url http://qbittorrent:8080 --username admin --map /downloads=/downloads
docker compose start empirebt
```

`--url` is qBittorrent's address as this container sees it: a service
name on the same Docker network, or `http://host.docker.internal:8080`.
`--map` rewrites qBittorrent's paths to the ones this container has. See
the [README](README.md#moving-from-qbittorrent) for what comes over.

## Stopping and updating

`docker compose stop` gives EmpireBT time (`stop_grace_period: 30s`) to
tell trackers it is leaving and to save the session. Downloads resume and
finished torrents seed again on the next start.

To update after pulling new code:

```bash
docker compose build && docker compose up -d
```

## Building for other machines

The image builds for the architecture it is built on. For a Raspberry Pi
or an ARM NAS, build there, or cross-build with buildx:

```bash
docker buildx build --platform linux/arm64 -t empirebt:arm64 --load .
```

## Logs

`docker compose logs -f empirebt` shows what EmpireBT is doing: startup,
torrents added, finished, or failing, trackers that go down, the VPN
interface going down or up, port changes, and web sign-ins (failed ones
with the address they came from). For more detail, set
`EMPIREBT_LOG_LEVEL: "debug"`, or just one area, such as
`"info,tracker=debug"`. Passwords and private trackers' passkeys are never
logged, and peers' addresses only at `debug`.

## Health check

The image checks itself every 30 seconds with `empirebt health`, which
succeeds when the web UI accepts connections. `docker ps` shows `healthy`
or `unhealthy`.
