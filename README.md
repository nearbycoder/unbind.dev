<img src="assets/icon.svg" width="64" height="64" alt="">

# Unbind

Unbind shares TCP and UDP services and sends files through a direct, encrypted connection between two computers. Both peers can use the native desktop app or the Rust CLI. The host creates an invite and the guest connects through a local port.

## Desktop app

Download Unbind for macOS, Windows, or Linux. Open **Share** to create a link or **Connect** to use one. The app also handles file transfers, approvals, saved profiles, and network diagnostics. [Download the app](https://unbind.dev/downloads/#desktop-title) · [Desktop setup](desktop/README.md)

## Install the CLI

On Linux and macOS:

```sh
curl -fsSL https://unbind.dev/install.sh | sh
```

On Windows, run this in PowerShell:

```powershell
irm https://unbind.dev/install.ps1 | iex
```

The installers check the binary against its published SHA-256 checksum before installing it. You can also [download a binary manually](https://unbind.dev/downloads/). If your shell cannot find `unbind` after the Linux or macOS install, add `$HOME/.local/bin` to your `PATH`.

### Verify the installed binary

Compare the installed executable with the current published checksum. The check reads the executable without running or replacing it. An older build can have a legitimately different checksum.

On Linux and macOS:

```sh
curl -fsSL https://unbind.dev/install.sh | sh -s -- --verify
```

On Windows, run this in PowerShell:

```powershell
& ([scriptblock]::Create((irm 'https://unbind.dev/install.ps1'))) -Verify
```

If you used a custom `UNBIND_BIN_DIR` when installing, set it to the same directory when verifying.

## Share and connect

On the computer hosting a service on port 3000:

```sh
unbind share 3000
```

Keep that command running. Send the printed link to the other computer and run:

```sh
unbind connect '<link>' --listen 8080
```

For a web service, open `http://127.0.0.1:8080` on the connecting computer. For SSH, databases, and other TCP services, point your client at the local listener. To share a target on a different address, use `unbind share HOST:PORT`. A bare target port refers to `127.0.0.1` on the sharing computer.

Anyone who has the complete invite link can access the shared service while the host is running. Stop the sharing command to revoke the invite and close its connections.

### Connection and access controls

- Run `unbind doctor` to check HTTPS rendezvous and STUN over IPv4 and IPv6. Run `unbind status` to inspect current shares, peers, traffic, and the selected ICE path. Both commands support `--json`.
- Use `unbind share 3000 --expires-in 600 --single-use` for an invite that expires after ten minutes and admits one peer.
- Run `unbind invite SHARE_ID` to issue a separate link. Use `unbind revoke SHARE_ID INVITE_ID` to disconnect that guest and close that link. Find the IDs in `unbind status`.
- Use `unbind share 3000 --approve` to approve peers individually. The guest can opt in to an Ed25519 device identity with `unbind connect '<link>' --identify`. Use `unbind approve SHARE_ID PEER_ID --remember` to approve a verified device now and recognize it in future approved shares.
- Connecting listeners keep their local address while Unbind retries an interrupted tunnel. Applications reopen any TCP connections lost during the interruption. Use `--no-reconnect` to exit on interruption.
- Pass `--qr` when creating an invite to display it as a QR code. Run `unbind register-link` once to enable **Open in Unbind** on the join page. Confirm the invite in the terminal when the desktop opens it.

### Multiple TCP and UDP ports

Each name selects a target configured by the host.

```sh
unbind share --tcp web=3000 --tcp api=8081 --udp dns=5353
unbind connect '<link>' --tcp web=8000 --tcp api=8001 --udp dns=8053
```

To save these settings, create `unbind.toml`.

```toml
[shares.dev]
expires_in = 3600

[shares.dev.tcp]
web = "3000"
api = "8081"

[shares.dev.udp]
dns = "5353"
```

Use `unbind share dev` in the foreground, or `unbind start dev` for a background process. On systems with a user service manager, `unbind start dev --at-login` also registers it at login. Use `unbind stop dev --at-login` to stop the share and remove that registration. Pass `--config PATH` to select a different configuration file.

### SSH and file transfer

Share SSH with `unbind share 22`. On the connecting machine, point SSH at Unbind's standard input and output mode.

```sh
ssh -o "ProxyCommand=unbind connect '<link>' --stdio" user@host
```

Use `unbind send ./report.pdf` to create a one-recipient file invite. On the other machine, run `unbind receive '<link>' ./report.pdf`. The receiver checks the transmitted SHA-256 before creating the destination file.

### Local networks and manual UDP settings

- Use `unbind share 3000 --lan` to host rendezvous on a private LAN address. Copy its link to another computer on that network. Set `--lan-bind IP:PORT` to choose the address.
- Use `--udp-bind 0.0.0.0:40000 --public-endpoint PUBLIC_IP:40000` to advertise an existing UDP port forward. The internal and external ports can differ.
- Use `--udp-bind 0.0.0.0:40000 --map-port ROUTER_IP` to request a temporary UDP mapping from a local router that supports PCP. Unbind renews the mapping while it is in use and requests removal when it exits.

### Signed updates and package managers

Run `unbind update --check` to inspect the latest signed release. Run `unbind update` to install it or `unbind update --version 0.2.0` to select a specific version. `unbind rollback` restores the executable saved before the last update.

Published versions, signatures, and package manifests are listed in the [release archive](https://unbind.dev/downloads/releases/). The [downloads page](https://unbind.dev/downloads/) also provides the current builds and installer commands.

See [package manager installation](packages/README.md) for the Homebrew tap, Scoop bucket, and WinGet manifest.

## How it connects

The rendezvous service exchanges short-lived WebRTC offers and answers over HTTPS. The invite keeps the authentication secret in its URL fragment, and the clients authenticate the signaling messages before establishing the encrypted data channel. STUN helps peers discover their UDP addresses. The forwarded TCP traffic uses that data channel directly and can continue after rendezvous becomes unavailable.

Some firewalls and NAT configurations prevent a working direct UDP path. Unbind reports a connection failure in those cases. `UNBIND_STUN` accepts a comma-separated list of STUN URLs or `none` for a local test without a STUN service.

The share-link workflow is inspired by [Tailcat by Tailscale](https://github.com/tailscale/tailcat). Unbind is an independent Rust implementation using WebRTC and yamux.

## Build and run locally

```sh
cargo build --locked --all-features
PORT=8080 UNBIND_DOWNLOAD_DIR=dist ./target/debug/unbind-web
```

Open two other terminals and run the following commands with an existing service on port 3000:

```sh
./target/debug/unbind --stun none share 3000 --server http://127.0.0.1:8080
./target/debug/unbind --stun none connect '<link>' --listen 8081
```

Each push to `main` triggers the GitHub Actions deployment. It builds the six platform binaries, uploads the source and downloads to Railway, and verifies the published checksums at `unbind.dev`.

The Railway service runs the Dockerfile and hosts the website, installer downloads, and in-memory rendezvous API. Use a single replica so both peers reach the same rendezvous state, and configure the health check at `/healthz`. For a manual upload, `railway up --no-gitignore` includes locally generated binaries from `dist/`.

To assemble the download binaries from source, use [the release script](scripts/build-downloads.sh) with Zig, `cargo-zigbuild`, and the Rust targets listed in the script. Cross-building the macOS binaries may require a macOS SDK supplied through `SDKROOT`.
