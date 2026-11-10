# Security policy

## Supported versions

Only the latest release gets security fixes. A Mac updates itself while
Latch runs. A Windows PC updates itself while its Latch window is open, or
when a Mac on the same tailnet pushes the release to it; a PC that only
runs the background host stays on its version until one of those happens.
A Linux host shows a notice and is updated by hand.

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
  refused.** A tagged host (a relay VPS) has no account of its own: on a
  personal tailnet it answers the tailnet's owner, and on an organisation's
  tailnet only the logins listed in `allowed_logins` in its `host.toml`.
- **Programs running as the same OS user are trusted; other users on the
  machine are not.** The service takes control requests from its own
  machine only with a private per-user token kept in Latch's data folder.
  Without it a local process can read the status and nothing else: it
  cannot start a stream, read the service log, change the display or put
  the machine to sleep.
- **The clipboard** is shared only with the machine that is streaming, or
  was in the last five seconds, and can be switched off.
- **Updates.** A Windows host checks every executable it is sent against
  the SHA-256 GitHub publishes for that release, whoever sent it, and
  refuses one that is not newer. A Mac checks its download the same way,
  and the new app's code signature. Releases carry `SHA256SUMS` and build
  provenance attestations (`gh attestation verify <file> -R
  MrBeldum/latch`). Mac and Windows builds are not yet signed with a paid
  developer certificate ([docs/ROADMAP.md](docs/ROADMAP.md#signing-and-notarization)),
  so the trust anchor for an update is GitHub and this repository.
- **Windows setup** elevates once and runs only a copy in
  `%ProgramFiles%\Latch\Setup` whose SHA-256 matches the running file.
- **Controlling administrator windows on Windows is optional and off by
  default.** Turned on, an elevated helper injects the viewer's input, and
  it answers only the installed `latch-host.exe` in the same session. That
  narrows, but does not remove, a route by which another program running
  as the same user could send input to an elevated window; leave it off
  if that matters on your PC.

In scope: anything that lets someone outside that model see a screen,
inject input, read the clipboard, run code, push an update, or learn
secrets; and anything that lets a local unprivileged process raise its
privileges through Latch. Out of scope: attacks that need a device already
signed in to the victim's Tailscale account, and denial of service by a
peer the machine already trusts.
