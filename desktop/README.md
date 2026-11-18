# Unbind Desktop

Unbind Desktop is a native [Rune](https://github.com/anomalyco/rune) app for macOS, Windows, and Linux. Both peers can share or connect from the app, and either can join a link created by the command-line client.

## Install

Get the archive for your operating system and processor from [unbind.dev/downloads](https://unbind.dev/downloads/#desktop-title).

- **macOS 13 or later**. Extract the zip, move `Unbind.app` to Applications, and open it. The bundle uses an ad-hoc signature. macOS may ask you to approve it in Privacy & Security. [Apple’s instructions](https://support.apple.com/en-us/102445)
- **Windows**. Extract the zip and open `Unbind.exe`. The portable executable does not require administrator access. Windows may show a publisher warning on first launch.
- **Linux**. Extract the archive and open `Unbind/unbind-desktop`. Use **Settings → Enable desktop links** to add Unbind to your app list and enable invite links. The archive also includes an optional per-user install script. The build targets Ubuntu 24.04 or a compatible glibc distribution and needs X11 or Wayland and Vulkan graphics. Linux file pickers use the desktop’s XDG portal. You can also enter file paths by hand.

## Share and connect

1. Open **Share** and enter the port or address of your service. Use **Advanced sharing** to add named TCP and UDP routes or enable LAN mode.
2. Select **Create invite**. Copy the link or show a QR code. Keep Unbind open while the share is active.
3. On the other computer, paste the invite into **Connect**, choose a local port, and connect. Open the service through that port. Choose matching route names for multi-port shares.

Run **Settings → Enable desktop links** to have the join page’s **Open in Unbind** button open the invite in the app. Review the invite and choose **Connect** before the app joins. On **Files**, use **Send** or **Receive** for a checksum-verified transfer.

**Sessions** shows active connections, invite controls, pending approvals, and traffic. **Settings** includes network diagnostics, custom rendezvous and STUN URLs, and manual UDP and PCP configuration. Saved share profiles and network settings are stored in the per-user Unbind state directory. Use the CLI for headless background profiles and SSH ProxyCommand integration.

The hosted rendezvous service introduces peers. Service and file traffic goes directly between them, and networks that block direct UDP can prevent a connection.

## Verify an archive

Releases include `unbind-desktop-SHA256SUMS` and its detached Ed25519 signature, `unbind-desktop-SHA256SUMS.sig`. After [verifying the checksum file’s signature](../release/README.md#desktop-archives), compare the SHA-256 of your downloaded archive with its corresponding entry. The [release archive](https://unbind.dev/downloads/releases/) retains versioned downloads.

## Build from source

```sh
cargo +1.99.1 run --locked --no-default-features --features desktop --bin unbind-desktop
```

Use Rust 1.99.1 to match the pinned Rune revision. Windows builds use the MSVC target. Linux builds require the system Wayland and xkbcommon development packages. Debug builds can render a headless snapshot using `-- --scene share --snap share.png`.

[Third-party notices](THIRD-PARTY-NOTICES.txt)
