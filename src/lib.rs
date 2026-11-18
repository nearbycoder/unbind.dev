//! Shared client engine for Unbind frontends.

#![cfg(feature = "client")]

#[cfg(feature = "cli")]
#[path = "bin/client/background.rs"]
pub mod background;
#[path = "bin/client/forwarding.rs"]
pub mod forwarding;
#[path = "bin/client/identity.rs"]
pub mod identity;
#[path = "bin/client/links.rs"]
pub mod links;
#[path = "bin/client/management.rs"]
pub mod management;
#[path = "bin/client/network.rs"]
pub mod network;
#[path = "bin/client/profiles.rs"]
pub mod profiles;
#[path = "bin/client/session.rs"]
pub mod session;
#[path = "bin/client/signaling.rs"]
pub mod signaling;
#[path = "bin/client/transport.rs"]
pub mod transport;
#[cfg(feature = "cli")]
#[path = "bin/client/update.rs"]
pub mod update;

use std::net::{Ipv4Addr, SocketAddr};

use anyhow::{Context, Result, ensure};

pub fn split_route(input: &str) -> Result<(&str, &str)> {
    let (name, address) = input
        .split_once('=')
        .context("Expected a named route as NAME=ADDRESS")?;
    ensure!(
        !name.is_empty() && !address.is_empty(),
        "Expected a named route as NAME=ADDRESS"
    );
    Ok((name, address))
}

pub fn parse_target(input: &str) -> Result<String> {
    if let Ok(port) = input.parse::<u16>() {
        ensure!(port != 0, "The target port must be between 1 and 65535");
        return Ok(SocketAddr::from((Ipv4Addr::LOCALHOST, port)).to_string());
    }
    let (host, port) = input
        .rsplit_once(':')
        .context("Expected PORT or HOST:PORT")?;
    let port: u16 = port.parse().context("Invalid target port")?;
    ensure!(
        port != 0
            && !host.is_empty()
            && !host.contains(['/', '\\', '@', '#', '?'])
            && !host.chars().any(char::is_whitespace)
            && (!host.contains([':', '[', ']']) || input.parse::<SocketAddr>().is_ok()),
        "Expected PORT or HOST:PORT. Use brackets around an IPv6 address."
    );
    Ok(input.to_owned())
}

pub fn parse_listen(input: &str) -> Result<SocketAddr> {
    if let Ok(port) = input.parse::<u16>() {
        return Ok(SocketAddr::from((Ipv4Addr::LOCALHOST, port)));
    }
    input.parse().context("Expected PORT or IP:PORT")
}

async fn shutdown() -> Result<()> {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result?,
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await?;
    Ok(())
}
