# Security policy

## Supported versions

Only the latest release gets security fixes. Latch updates itself, so a
machine that is switched on and reachable is normally on it.

| Version | Supported |
|---------|-----------|
| latest 5.x release | yes |
| anything older | no; update first |

## Reporting a vulnerability

Please do not open a public issue for a security problem.

Report it privately through GitHub:
**[Report a vulnerability](https://github.com/MrBeldum/latch/security/advisories/new)**
(the Security tab of this repository, then *Report a vulnerability*).

Include what you can of:

- the Latch version (`latch-host --version`, or the Settings tab) and the
  operating system of each machine involved;
- what an attacker needs to start with (a machine on the same tailnet, a
  node shared in from another tailnet, an unprivileged local account, a
  position on the LAN, nothing at all);
- what they gain, and the steps or a proof of concept.

You should get an answer within a week. A confirmed problem is fixed in a
new release, the advisory is published with it, and you are credited
unless you ask not to be.

## What Latch trusts

Knowing the model makes a report more useful.

- **Tailscale is the identity check and the encryption.** The control
  service (TCP 47850) and the stream (UDP 47850) answer only loopback and
  Tailscale addresses that `tailscale whois` attributes to the account the
  machine itself is signed in to. There is no Latch account, password or
  pairing. Anyone who can sign a device in to your Tailscale account can
  see and control your shared machines; that is by design.
- **Nodes shared in from another tailnet, and tagged devices, are
  refused.** A tagged host (a relay VPS) answers people of its own tailnet
  only.
- **A local process can read status but cannot** start a stream, push an
  update, or read the clipboard.
- **Updates** are checked against the digest GitHub publishes for the
  release asset, and on a Mac against the app's own code signature. Mac and
  Windows builds are not yet signed with a paid developer certificate
  ([docs/ROADMAP.md](docs/ROADMAP.md)).
- **Windows setup** elevates once and runs only a copy in
  `%ProgramFiles%\Latch\Setup` whose SHA-256 matches the running file.

In scope: anything that lets someone outside that model see a screen,
inject input, read the clipboard, run code, push an update, or learn
secrets; and anything that lets a local unprivileged process raise its
privileges through Latch. Out of scope: attacks that need a device already
signed in to the victim's Tailscale account, and denial of service by a
peer the machine already trusts.
