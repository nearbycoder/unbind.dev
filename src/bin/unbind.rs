#[path = "client/signaling.rs"]
mod signaling;
#[path = "client/transport.rs"]
mod transport;

use std::{
    collections::HashMap,
    net::{Ipv4Addr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use signaling::{
    DEFAULT_SERVER, DescriptionKind, HANDSHAKE_TIMEOUT, Invite, Offer, POLL_INTERVAL, SignalClient,
    random_id,
};
use tokio::{
    net::TcpListener,
    task::JoinSet,
    time::{interval, sleep, timeout},
};
use transport::{DEFAULT_STUN, Peer, Relay, parse_stun};

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[arg(long, global = true, env = "UNBIND_STUN", default_value = DEFAULT_STUN, hide_default_value = true)]
    stun: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Share a TCP service and print its invite link.
    Share {
        /// Local PORT or a TCP target as HOST:PORT.
        #[arg(value_parser = parse_target)]
        target: String,

        /// Rendezvous server URL.
        #[arg(long, env = "UNBIND_SERVER", default_value = DEFAULT_SERVER)]
        server: String,
    },

    /// Open a local TCP port connected to a shared service.
    Connect {
        /// Invite link printed by the other peer.
        link: String,

        /// Local PORT or IP:PORT. A bare port binds to 127.0.0.1.
        #[arg(short, long, value_parser = parse_listen, default_value = "8080")]
        listen: SocketAddr,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let stun = parse_stun(&cli.stun)?;
    match cli.command {
        Command::Share { target, server } => share(target, &server, stun).await,
        Command::Connect { link, listen } => connect(&link, listen, &stun).await,
    }
}

async fn share(target: String, server: &str, stun: Vec<String>) -> Result<()> {
    let invite = Arc::new(Invite::new(server)?);
    let signals = SignalClient::new(invite.origin.clone())?;
    let host_key = random_id()?;
    signals.register(&invite.room, &host_key).await?;
    eprintln!("Sharing {target}. Keep this command running.");
    println!("{}", invite.link());

    let mut peers = JoinSet::new();
    let mut seen = HashMap::new();
    let mut poll = interval(POLL_INTERVAL);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut interruption_reported = false;
    let stop = shutdown();
    tokio::pin!(stop);

    loop {
        tokio::select! {
            result = &mut stop => { result?; break; }
            _ = peers.join_next(), if !peers.is_empty() => {}
            _ = poll.tick() => {
                let offers = match signals.host_poll(&invite.room, &host_key).await {
                    Ok(Some(offers)) => Ok(offers),
                    Ok(None) => signals.register(&invite.room, &host_key).await.map(|()| vec![]),
                    Err(error) => Err(error),
                };
                match offers {
                    Ok(offers) => {
                        if interruption_reported {
                            eprintln!("Rendezvous connection restored.");
                            interruption_reported = false;
                        }
                        seen.retain(|_, last_seen: &mut Instant| last_seen.elapsed() < Duration::from_secs(100));
                        for offer in offers.into_iter().take(8) {
                            if peers.len() >= 16 || seen.contains_key(&offer.peer) {
                                continue;
                            }
                            seen.insert(offer.peer.clone(), Instant::now());
                            let invite = Arc::clone(&invite);
                            let signals = signals.clone();
                            let target = target.clone();
                            let stun = stun.clone();
                            let host_key = host_key.clone();
                            peers.spawn(async move {
                                if let Err(error) = serve_peer(&invite, &signals, &host_key, &target, &stun, offer).await {
                                    eprintln!("Peer session ended: {error:#}");
                                }
                            });
                        }
                    }
                    Err(error) if !interruption_reported => {
                        eprintln!("Rendezvous temporarily unavailable: {error:#}");
                        interruption_reported = true;
                    }
                    Err(_) => {}
                }
            }
        }
    }
    peers.shutdown().await;
    let _ = signals.close(&invite.room, &host_key).await;
    Ok(())
}

async fn serve_peer(
    invite: &Invite,
    signals: &SignalClient,
    host_key: &str,
    target: &str,
    stun: &[String],
    request: Offer,
) -> Result<()> {
    invite.verify(&request.peer, DescriptionKind::Offer, &request.offer)?;
    let mut peer = Peer::new(stun).await?;
    let result = async {
        let answer = peer.answer(request.offer.sdp).await?;
        let answer = invite.sign(&request.peer, DescriptionKind::Answer, answer)?;
        signals
            .answer(&invite.room, host_key, &request.peer, answer)
            .await?;
        let channel = peer.connect().await?;
        eprintln!("Peer connected directly.");
        peer.run(channel, Relay::Forward(target)).await
    }
    .await;
    peer.close().await;
    result
}

async fn connect(link: &str, listen: SocketAddr, stun: &[String]) -> Result<()> {
    let invite = Invite::from_link(link)?;
    let signals = SignalClient::new(invite.origin.clone())?;
    let listener = TcpListener::bind(listen)
        .await
        .with_context(|| format!("Could not listen on {listen}"))?;
    let address = listener.local_addr()?;
    let peer_id = random_id()?;
    let mut peer = Peer::new(stun).await?;

    let session = async {
        eprintln!("Connecting to the peer directly…");
        let offer = invite.sign(&peer_id, DescriptionKind::Offer, peer.offer().await?)?;
        signals.join(&invite.room, &peer_id, offer).await?;
        let answer = timeout(HANDSHAKE_TIMEOUT, async {
            loop {
                if let Some(answer) = signals.join_poll(&invite.room, &peer_id).await? {
                    return Ok::<_, anyhow::Error>(answer);
                }
                sleep(POLL_INTERVAL).await;
            }
        })
        .await
        .context("The share host did not answer. Check that the host is running and the invite is intact.")??;
        invite.verify(&peer_id, DescriptionKind::Answer, &answer)?;
        peer.accept(answer.sdp).await?;
        let channel = peer.connect().await?;
        println!("Direct connection ready. Listening on {address}");
        peer.run(channel, Relay::Listen(&listener)).await
    };
    let result = tokio::select! {
        result = session => result,
        result = shutdown() => result,
    };
    peer.close().await;
    result
}

fn parse_target(input: &str) -> Result<String> {
    if let Ok(port) = input.parse::<u16>() {
        ensure!(port != 0, "The target port must be between 1 and 65535");
        return Ok(SocketAddr::from((Ipv4Addr::LOCALHOST, port)).to_string());
    }

    let (host, port) = input
        .rsplit_once(':')
        .context("Expected PORT or HOST:PORT")?;
    let port: u16 = port.parse().context("Invalid TCP target port")?;
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

fn parse_listen(input: &str) -> Result<SocketAddr> {
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
