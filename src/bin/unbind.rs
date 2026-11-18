#[path = "client/background.rs"]
mod background;
#[path = "client/forwarding.rs"]
mod forwarding;
#[path = "client/identity.rs"]
mod identity;
#[path = "client/links.rs"]
mod links;
#[path = "client/management.rs"]
mod management;
#[path = "client/network.rs"]
mod network;
#[path = "client/profiles.rs"]
mod profiles;
#[path = "client/session.rs"]
mod session;
#[path = "client/signaling.rs"]
mod signaling;
#[path = "client/transport.rs"]
mod transport;
#[path = "client/update.rs"]
mod update;

use std::{
    net::{Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use clap::{Args, Parser, Subcommand};
use forwarding::{Binding, ClientRoutes, Service, Services};
use management::Action;
use serde_json::Value;
use session::{Client, InviteOptions, Share};
use signaling::DEFAULT_SERVER;
use transport::{DEFAULT_STUN, parse_stun};

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[arg(long, global = true, env = "UNBIND_STUN", default_value = DEFAULT_STUN, hide_default_value = true)]
    stun: String,
    /// Configuration file for saved share profiles.
    #[arg(long, global = true, default_value = "unbind.toml")]
    config: PathBuf,
    #[arg(long, global = true, hide = true)]
    state_dir: Option<PathBuf>,
    #[command(flatten)]
    network: network::Options,
    #[command(subcommand)]
    command: Command,
}

#[derive(Args, Default)]
struct HostFlags {
    /// Rendezvous server URL.
    #[arg(long, env = "UNBIND_SERVER")]
    server: Option<String>,
    /// Use local signaling and direct candidates on the LAN.
    #[arg(long)]
    lan: bool,
    /// LAN rendezvous address. The default selects a private address automatically.
    #[arg(long, value_name = "IP:PORT")]
    lan_bind: Option<SocketAddr>,
    /// Seconds before the invite and its connections expire.
    #[arg(long, value_name = "SECONDS", value_parser = clap::value_parser!(u64).range(1..=31_536_000))]
    expires_in: Option<u64>,
    /// Admit only the first peer to connect successfully.
    #[arg(long)]
    single_use: bool,
    /// Require a host to approve peers, with an exception for remembered devices.
    #[arg(long)]
    approve: bool,
    /// Print a QR code containing the invite.
    #[arg(long)]
    qr: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Share local services and print an invite.
    Share {
        /// Local PORT, HOST:PORT, or a profile from the configuration file.
        target: Option<String>,
        /// Additional named TCP target.
        #[arg(long, value_name = "NAME=TARGET")]
        tcp: Vec<String>,
        /// Named UDP target.
        #[arg(long, value_name = "NAME=TARGET")]
        udp: Vec<String>,
        #[command(flatten)]
        host: HostFlags,
    },
    /// Open local ports or attach standard input and output to a shared service.
    Connect {
        /// Invite link printed by the other peer.
        link: String,
        /// Local TCP PORT or IP:PORT. Default 127.0.0.1:8080.
        #[arg(short, long, value_parser = parse_listen, conflicts_with = "stdio")]
        listen: Option<SocketAddr>,
        /// Named TCP listener.
        #[arg(long, value_name = "NAME=LISTEN", conflicts_with = "stdio")]
        tcp: Vec<String>,
        /// Named UDP listener.
        #[arg(long, value_name = "NAME=LISTEN", conflicts_with = "stdio")]
        udp: Vec<String>,
        /// Use stdin and stdout for a TCP service. Use --stdio=NAME for a named service.
        #[arg(long, num_args = 0..=1, default_missing_value = "", require_equals = true)]
        stdio: Option<String>,
        /// Prove your persistent device identity to the invite host.
        #[arg(long)]
        identify: bool,
        /// Exit if a connected tunnel is interrupted.
        #[arg(long)]
        no_reconnect: bool,
    },
    /// Offer a file directly to the first peer that accepts the invite.
    Send {
        file: PathBuf,
        #[command(flatten)]
        host: HostFlags,
    },
    /// Receive a file into a new destination and check its checksum.
    Receive {
        link: String,
        destination: PathBuf,
        #[arg(long)]
        identify: bool,
    },
    /// Inspect active shares, connections, and peer approvals.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Create a separate guest invite for a running share.
    Invite {
        share: String,
        #[arg(long, value_name = "SECONDS", value_parser = clap::value_parser!(u64).range(1..=31_536_000))]
        expires_in: Option<u64>,
        #[arg(long)]
        single_use: bool,
        #[arg(long)]
        qr: bool,
    },
    /// Revoke an invite and disconnect its peers.
    Revoke { share: String, invite: String },
    /// Admit a pending peer.
    Approve {
        share: String,
        peer: String,
        /// Trust this verified device for future invites.
        #[arg(long)]
        remember: bool,
    },
    /// Decline a pending peer.
    Deny { share: String, peer: String },
    /// Print your persistent public device fingerprint.
    Identity,
    /// Run a saved share in the background.
    Start {
        profile: String,
        /// Also start this profile at user login.
        #[arg(long)]
        at_login: bool,
    },
    /// Stop a session or all background sessions for a profile.
    Stop {
        id: String,
        /// Remove the profile's login registration.
        #[arg(long)]
        at_login: bool,
    },
    /// Test rendezvous, STUN, and local connectivity.
    Doctor {
        #[arg(long, env = "UNBIND_SERVER", default_value = DEFAULT_SERVER)]
        server: String,
        #[arg(long)]
        json: bool,
    },
    /// Register Unbind as the handler for unbind:// links on this computer.
    RegisterLink,
    /// Confirm and connect to an invite opened by the desktop link handler.
    OpenLink {
        link: String,
        #[arg(short, long, value_parser = parse_listen, default_value = "8080")]
        listen: SocketAddr,
    },
    /// Check for a release or install a signed update.
    Update {
        #[arg(long)]
        check: bool,
        #[arg(long, value_name = "VERSION")]
        version: Option<String>,
    },
    /// Restore the previously installed executable.
    Rollback,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Some(path) = cli.state_dir {
        management::set_state_dir(path);
    }
    match cli.command {
        Command::Share {
            target,
            tcp,
            udp,
            host,
        } => run_share(target, tcp, udp, host, &cli.config, &cli.stun, cli.network).await,
        Command::Connect {
            link,
            listen,
            tcp,
            udp,
            stdio,
            identify,
            no_reconnect,
        } => {
            let client = bind_client(listen, tcp, udp, stdio).await?;
            session::connect(
                &link,
                client,
                &parse_stun(&cli.stun)?,
                &cli.network,
                identify,
                !no_reconnect,
            )
            .await
        }
        Command::Send { file, host } => {
            let file = std::fs::canonicalize(file).context("Find the file to send")?;
            ensure!(file.is_file(), "Choose a single regular file to send");
            let label = file
                .file_name()
                .context("Missing file name")?
                .to_string_lossy()
                .into_owned();
            let options = InviteOptions {
                expires_in: host.expires_in,
                single_use: true,
            };
            let share = Share {
                server: host.server.unwrap_or_else(|| DEFAULT_SERVER.to_owned()),
                target_names: vec![label],
                compatibility_target: None,
                services: Services::new(vec![Service::File { path: file }])?,
                invite: options,
                approve: host.approve,
                profile: None,
                one_file: true,
                qr: host.qr,
            };
            run_host(share, host.lan, host.lan_bind, &cli.stun, cli.network).await
        }
        Command::Receive {
            link,
            destination,
            identify,
        } => {
            let client =
                Client::Once(ClientRoutes::bind(vec![Binding::Receive { destination }]).await?);
            session::connect(
                &link,
                client,
                &parse_stun(&cli.stun)?,
                &cli.network,
                identify,
                false,
            )
            .await
        }
        Command::Status { json } => print_status(json).await,
        Command::Invite {
            share,
            expires_in,
            single_use,
            qr,
        } => {
            let result = local_action(
                &share,
                Action::NewInvite {
                    expires_in,
                    single_use,
                },
            )
            .await?;
            let link = result["link"].as_str().context("Missing invite link")?;
            println!("{link}");
            eprintln!("Invite {}", display_value(&result["id"]));
            if qr {
                links::print_qr(link)?;
            }
            Ok(())
        }
        Command::Revoke { share, invite } => {
            local_action(&share, Action::Revoke { invite }).await?;
            println!("Invite revoked.");
            Ok(())
        }
        Command::Approve {
            share,
            peer,
            remember,
        } => {
            local_action(&share, Action::Approve { peer, remember }).await?;
            println!("Peer approved.");
            Ok(())
        }
        Command::Deny { share, peer } => {
            local_action(&share, Action::Deny { peer }).await?;
            println!("Peer declined.");
            Ok(())
        }
        Command::Identity => {
            println!("{}", identity::device_id()?);
            Ok(())
        }
        Command::Start { profile, at_login } => {
            background::start(&cli.config, &profile, at_login).await
        }
        Command::Stop { id, at_login } => background::stop(&id, at_login).await,
        Command::Doctor { server, json } => {
            network::doctor(&server, &parse_stun(&cli.stun)?, json).await
        }
        Command::RegisterLink => links::register(),
        Command::OpenLink { link, listen } => {
            let link = links::confirm(&link)?;
            session::connect(
                &link,
                session::bind_legacy(listen).await?,
                &parse_stun(&cli.stun)?,
                &cli.network,
                false,
                true,
            )
            .await
        }
        Command::Update { check, version } => update::run(check, version.as_deref()).await,
        Command::Rollback => update::rollback().await,
    }
}

async fn local_action(id: &str, action: Action) -> Result<Value> {
    let record = management::find(id)?;
    management::request(&record, action).await
}

async fn print_status(json: bool) -> Result<()> {
    let active = management::active().await?;
    let snapshots: Vec<_> = active.into_iter().map(|(_, status)| status).collect();
    if json {
        println!("{}", serde_json::to_string_pretty(&snapshots)?);
        return Ok(());
    }
    if snapshots.is_empty() {
        println!("No active Unbind sessions.");
    }
    for state in snapshots {
        let id = display_value(&state["id"]);
        if state["kind"] == "share" {
            let targets = state["targets"]
                .as_array()
                .into_iter()
                .flatten()
                .map(display_value)
                .collect::<Vec<_>>();
            println!("{id}  share  {}", targets.join(", "));
            for invite in state["invites"].as_array().into_iter().flatten() {
                println!(
                    "  invite {}  {}",
                    display_value(&invite["id"]),
                    display_value(&invite["state"])
                );
                for peer in invite["peers"].as_array().into_iter().flatten() {
                    println!(
                        "    {}  {}  {}  sent {} B  received {} B",
                        display_value(&peer["id"]),
                        display_value(&peer["state"]),
                        display_value(&peer["path"]),
                        display_value(&peer["bytes_sent"]),
                        display_value(&peer["bytes_received"])
                    );
                    if let Some(identity) = peer["identity"].as_str() {
                        println!("      device {identity}");
                    }
                }
            }
        } else {
            let listeners = state["listeners"]
                .as_array()
                .into_iter()
                .flatten()
                .map(display_value)
                .collect::<Vec<_>>();
            println!(
                "{id}  connect  {}  {}",
                display_value(&state["state"]),
                listeners.join(", ")
            );
            println!(
                "  {}  sent {} B  received {} B",
                display_value(&state["path"]),
                display_value(&state["bytes_sent"]),
                display_value(&state["bytes_received"])
            );
        }
    }
    Ok(())
}

fn display_value(value: &Value) -> String {
    if let Some(text) = value.as_str() {
        text.to_owned()
    } else if value.is_null() {
        String::new()
    } else {
        value.to_string()
    }
}

async fn run_share(
    target: Option<String>,
    tcp: Vec<String>,
    udp: Vec<String>,
    host: HostFlags,
    config: &Path,
    stun: &str,
    network: network::Options,
) -> Result<()> {
    let mut profile_name = None;
    let mut profile = profiles::Profile::default();
    let target = match target {
        Some(target) if profiles::valid_name(&target) => {
            profile = profiles::load(config, &target)?;
            profile_name = Some(target);
            profile.target.take()
        }
        target => target,
    };

    let mut entries = Vec::new();
    let mut names = Vec::new();
    let compatibility_target = target.as_deref().map(parse_target).transpose()?;
    if let Some(target) = &compatibility_target {
        names.push(target.clone());
        entries.push(Service::Tcp {
            name: "default".to_owned(),
            target: target.clone(),
        });
    }
    for route in tcp {
        let (name, target) = split_route(&route)?;
        profile.tcp.insert(name.to_owned(), target.to_owned());
    }
    for route in udp {
        let (name, target) = split_route(&route)?;
        profile.udp.insert(name.to_owned(), target.to_owned());
    }
    for (name, target) in profile.tcp {
        let target = parse_target(&target)?;
        names.push(format!("{name} TCP {target}"));
        entries.push(Service::Tcp { name, target });
    }
    for (name, target) in profile.udp {
        let target = parse_target(&target)?;
        names.push(format!("{name} UDP {target}"));
        entries.push(Service::Udp { name, target });
    }

    let share = Share {
        server: host
            .server
            .or(profile.server)
            .unwrap_or_else(|| DEFAULT_SERVER.to_owned()),
        target_names: names,
        compatibility_target,
        services: Services::new(entries)?,
        invite: InviteOptions {
            expires_in: host.expires_in.or(profile.expires_in),
            single_use: host.single_use || profile.single_use,
        },
        approve: host.approve || profile.approve,
        profile: profile_name,
        one_file: false,
        qr: host.qr || profile.qr,
    };
    run_host(share, host.lan || profile.lan, host.lan_bind, stun, network).await
}

async fn run_host(
    mut share: Share,
    lan: bool,
    lan_bind: Option<SocketAddr>,
    stun: &str,
    network: network::Options,
) -> Result<()> {
    if lan || lan_bind.is_some() {
        let bind = lan_bind.unwrap_or_else(|| SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)));
        let (server, task) = network::lan_rendezvous(bind).await?;
        share.server = server;
        let outcome = session::share(share, vec![], network).await;
        task.abort();
        outcome
    } else {
        session::share(share, parse_stun(stun)?, network).await
    }
}

async fn bind_client(
    listen: Option<SocketAddr>,
    tcp: Vec<String>,
    udp: Vec<String>,
    stdio: Option<String>,
) -> Result<Client> {
    if let Some(name) = stdio {
        return if name.is_empty() {
            Ok(Client::Stdio)
        } else {
            Ok(Client::Once(
                ClientRoutes::bind(vec![Binding::Stdio { name }]).await?,
            ))
        };
    }
    if tcp.is_empty() && udp.is_empty() {
        return session::bind_legacy(
            listen.unwrap_or_else(|| SocketAddr::from((Ipv4Addr::LOCALHOST, 8080))),
        )
        .await;
    }

    let mut bindings = Vec::new();
    if let Some(listen) = listen {
        bindings.push(Binding::Tcp {
            name: "default".to_owned(),
            listen,
        });
    }
    for route in tcp {
        let (name, listen) = split_route(&route)?;
        bindings.push(Binding::Tcp {
            name: name.to_owned(),
            listen: parse_listen(listen)?,
        });
    }
    for route in udp {
        let (name, listen) = split_route(&route)?;
        bindings.push(Binding::Udp {
            name: name.to_owned(),
            listen: parse_listen(listen)?,
        });
    }
    Ok(Client::Named(ClientRoutes::bind(bindings).await?))
}

fn split_route(input: &str) -> Result<(&str, &str)> {
    let (name, address) = input
        .split_once('=')
        .context("Expected a named route as NAME=ADDRESS")?;
    ensure!(
        !name.is_empty() && !address.is_empty(),
        "Expected a named route as NAME=ADDRESS"
    );
    Ok((name, address))
}

fn parse_target(input: &str) -> Result<String> {
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
