# Operational changes, UTC

Times identify completion or the recorded event. Private command outputs and
backup manifests remain on the owner's SSD.

| UTC on 2026-11-15 | Change or verification |
| --- | --- |
| 04:13:20 | Confirm AWS account and begin read-only inventory. Confirm the operator /32 matches the SSH firewall. |
| 04:14:43 | Create mode-0700 backup/scratch directories on the Mac, with mode-0600 backup files. |
| 04:16:02–04:18 | Create implementation branch and build the repository probe locally. |
| 04:16:29 | SSM creates the EC2 state archive, source checksum inventory and package/unit exports. A temporary loopback-only backup HTTP service supports an authenticated SSM port forward. Download the session-manager plugin into private scratch. |
| 04:17:37 | Verify all 572 regular files, archive checksum and readable tar listing against EC2 source. Save AWS resource descriptions and user-data privately. |
| 04:18–04:20 | Run native baseline probes and short-path diagnostics. Cancel the stalled Windows echo attempt after it exceeded the intended short window. |
| 04:20–04:22 | Add signed Tailscale APT repository and install 1.104.7 on Lightsail. Stop its unregistered daemon. Native service stays available. |
| 04:22:58 | Open Lightsail UDP 443. |
| 04:23:00 | Open Lightsail UDP 41641. Existing TCP rules remain present. |
| 04:23:16–04:23:54 | CPU-sampled, read-only Windows SFTP baseline through EC2. |
| 04:24:12 | Stop EC2 tailscaled and archive quiescent state. A trailing checksum command fails after archive creation. |
| 04:24:40–04:24:41 | Restore identity on Lightsail and start kernel-mode tailscaled. Backend reaches Running. Initial early preference command returns NoState. |
| 04:25:16–04:25:18 | Apply explicit Lightsail relay endpoint, remove its temporary identity-transfer tar, and disable EC2 tailscaled to prevent duplicate boot startup. |
| 04:25:18–04:25:59 | Verify all eight stopped identity files. Apply persistent sysctls, GRO forwarding, memory/descriptor drop-ins and live limits. Mask unused modem, multipath, removable-media and firmware services. |
| 04:26–04:27 | Run matched native after probes. The first 50-Mbps attempt fails before Ready. |
| 04:28:15–04:28:50 | CPU-sampled read-only Windows SFTP after test through Lightsail. |
| 04:29:09–04:29:17 | Record both clients' status and pings via the new peer-relay endpoint. Read effective packet filters privately. |
| 04:30–04:31 | Retry native 50-Mbps probe. It times out. |
| 04:31:52–04:32:13 | Run RAM-only PowerShell socket upload, with temporary Tailscale-bound listener. Receive 8,650,752 bytes and close the listener. |
| 04:33–04:35 | Capture safe final host inventory, provider metrics and CPU summaries. Read bundle/EC2 pricing, Free Tier credit balance, Cloudflare DNS and AWS IPv6 documentation. |
| 2026-11-15T04:47:53.445538+00:00 | Reapply the final host profile twice, verify systemd units and TLS/ALPN, finalize docs and private backup manifest, stop temporary backup transport and remove its EC2 staging directory. |
| ~06:15 | Orchestrator terminates the old EC2 instance and deletes its security group after backup verification. Lightsail is the only relay. |
