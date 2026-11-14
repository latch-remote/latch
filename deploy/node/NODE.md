# Latch node (Docker)

A container that **shares a desktop** on your Tailscale network. Any Latch
app — Mac, Windows, or another node — lists it and can Connect.

Use this for a VPS, or run several copies on one host so each container is
its own machine (its own Tailscale identity, its own desktop).

It is not the packet relay. The relay kit stays in `deploy/relay/`.

## What runs inside

- Xvfb + XFCE (a virtual 1920×1080 desktop)
- Latch streaming engine (pure-Rust, in-process inside `latch-host`)
- Latch's control service on TCP 47850 and streaming on UDP 47850
- Optional Tailscale (`TS_AUTHKEY`), with kernel networking: the compose
  file gives the container `/dev/net/tun` and `NET_ADMIN`. Latch Host
  authorizes each peer from its own tailnet address; userspace Tailscale
  uses a different socket path: the service queries `whois` on loopback
  connections when TUN is not true rather than trusting all of them as local.
  This deployment kit nevertheless requires kernel networking and refuses
  to start Tailscale without the device.

The desktop and host run as root inside the container, with `NET_ADMIN`
and persistent volumes. A streamed terminal has those privileges. Treat
this as a trusted personal desktop, not a multi-user sandbox. Gamepads
need explicit `/dev/uinput` access; Compose does not provision it.

## One node

Install Docker Engine and its Compose plugin on a Linux host with
`/dev/net/tun`. Download `latch-<version>-source.tar.gz` and `SHA256SUMS`
from the public release, verify the archive's SHA-256 and extract it.
The source archive includes this container kit and its build inputs.
[Public releases](https://github.com/latch-remote/latch-releases/releases/latest)

Developers with private repository access can also clone it.

```bash
git clone https://github.com/latch-remote/latch.git
cd latch
```

From the Latch repository:

```bash
cp deploy/node/env.example deploy/node/.env
chmod 600 deploy/node/.env
# edit .env: paste a Tailscale auth key, pick TS_HOSTNAME
docker compose -f deploy/node/docker-compose.yml up -d --build
```

If your tailnet requires device approval, approve the node at
[the admin console](https://login.tailscale.com/admin/machines). Open Latch on your Mac or Windows PC: the hostname appears
under **Machines**. Click **Connect**. Use the same Tailscale account for
an untagged node. A tagged node has no account: on a personal tailnet it
answers the tailnet's owner, and on an organisation's tailnet only the
logins listed in `allowed_logins` in `host.toml`; with none listed it
answers nobody.
Keep `TS_AUTHKEY` in the protected `.env`: this kit starts its daemon only
when that variable is nonempty, even when the identity volume exists.

## Several containers on one VPS

Each compose project needs its own name and hostname:

```bash
LATCH_CONTAINER=latch-node-a TS_HOSTNAME=vps-a \
  docker compose -p node-a -f deploy/node/docker-compose.yml up -d --build

LATCH_CONTAINER=latch-node-b TS_HOSTNAME=vps-b \
  docker compose -p node-b -f deploy/node/docker-compose.yml up -d --build
```

Give each a distinct `TS_AUTHKEY` (or the same reusable key; Tailscale
issues a new node per `up`). Do not put `tag:relay` on these nodes.

## Same host as the peer relay

The relay container uses host networking and `tag:relay`. A node container
should **not** use host networking: its own network namespace and its own
tailscaled give it a second identity, so it shows up as a machine instead
of being hidden as a relay.

## Build notes

The image builds one Rust `latch-host` with `--locked` from `Cargo.lock`,
installs Ubuntu 24.04 desktop/runtime packages and copies the stable
Tailscale client and daemon. There is no external streaming engine.
To update Latch, pull the desired source revision and run the build command
again. There is no in-app Linux binary updater. Native Linux GPU encoding
remains unverified on hardware.

`.dockerignore` keeps every `.env` out of the build context: the build
stage copies the whole repository, and a layer keeps what it copied, so an
auth key there would travel with any image built `--target build` and
pushed or saved.
