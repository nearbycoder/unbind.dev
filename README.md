<img src="assets/icon.svg" width="64" height="64" alt="">

# Unbind

Unbind shares a TCP service through a direct, encrypted connection between two computers. Install the Rust CLI on both peers. The host shares one port and sends an invite link. The other peer opens a local TCP listener connected to that service.

## Install

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
