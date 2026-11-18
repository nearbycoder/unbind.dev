#[path = "../shared/rendezvous.rs"]
mod rendezvous;

use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use axum::{Router, response::Html, routing::get};
use clap::Args;
use serde::Serialize;
use tokio::{
    net::{TcpListener, UdpSocket, lookup_host},
    sync::OnceCell,
    task::{JoinHandle, JoinSet},
    time::{sleep, timeout},
};
use webrtc::{
    api::setting_engine::SettingEngine,
    ice::{
        network_type::NetworkType,
        udp_mux::{UDPMux, UDPMuxDefault, UDPMuxParams},
        udp_network::UDPNetwork,
    },
};

#[derive(Args, Clone, Default)]
pub struct Options {
    /// Bind ICE to a fixed UDP socket shared by the peers in this process.
    #[arg(long, global = true, value_name = "IP:PORT")]
    udp_bind: Option<SocketAddr>,

    /// Advertise an explicit UDP address forwarded to the local socket.
    #[arg(long, global = true, value_name = "IP:PORT")]
    public_endpoint: Option<SocketAddr>,

    /// Request a UDP mapping from this local PCP router. The default port is 5351.
    #[arg(long, global = true, value_name = "ROUTER", value_parser = parse_router)]
    map_port: Option<SocketAddr>,

    #[arg(skip)]
    socket: Arc<OnceCell<Arc<DirectSocket>>>,
}

#[derive(Default)]
pub struct Resources(Option<Arc<DirectSocket>>);

struct DirectSocket {
    mux: Arc<UDPMuxDefault>,
    local: SocketAddr,
    advertised: Option<SocketAddr>,
    _lease: Option<PcpLease>,
}

impl Drop for DirectSocket {
    fn drop(&mut self) {
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let mux = Arc::clone(&self.mux);
            runtime.spawn(async move {
                let _ = mux.close().await;
            });
        }
    }
}

impl Resources {
    pub fn advertise(&self, sdp: &mut String) {
        let Some(endpoint) = self.0.as_ref().and_then(|socket| socket.advertised) else {
            return;
        };
        // A peer can check this explicitly forwarded endpoint with the normal ICE credentials.
        let candidate = format!(
            "a=candidate:9000 1 udp 1694498815 {} {} typ host\r\n",
            endpoint.ip(),
            endpoint.port()
        );
        if let Some(end) = sdp.find("a=end-of-candidates") {
            sdp.insert_str(end, &candidate);
        } else {
            if !sdp.ends_with('\n') {
                sdp.push_str("\r\n");
            }
            sdp.push_str(&candidate);
        }
    }
}

pub async fn configure(settings: &mut SettingEngine, options: &Options) -> Result<Resources> {
    if options.udp_bind.is_none() && options.public_endpoint.is_none() && options.map_port.is_none()
    {
        return Ok(Resources::default());
    }

    let socket = options
        .socket
        .get_or_try_init(|| async {
            ensure!(
                options.public_endpoint.is_none() || options.map_port.is_none(),
                "Choose either --public-endpoint or --map-port"
            );
            let family = options.public_endpoint.or(options.map_port);
            let bind = options.udp_bind.unwrap_or_else(|| {
                let ip = if family.is_some_and(|address| address.is_ipv6()) {
                    IpAddr::V6(Ipv6Addr::UNSPECIFIED)
                } else {
                    IpAddr::V4(Ipv4Addr::UNSPECIFIED)
                };
                SocketAddr::new(
                    ip,
                    options.public_endpoint.map_or(0, |address| address.port()),
                )
            });
            if let Some(endpoint) = options.public_endpoint {
                ensure!(
                    !endpoint.ip().is_unspecified()
                        && !endpoint.ip().is_multicast()
                        && endpoint.port() > 0,
                    "--public-endpoint needs a unicast address and nonzero port"
                );
            }
            if let Some(endpoint) = family {
                ensure!(
                    bind.is_ipv4() == endpoint.is_ipv4(),
                    "UDP bind and advertised address must use the same IP family"
                );
            }
            let udp = UdpSocket::bind(bind)
                .await
                .with_context(|| format!("Could not bind ICE UDP at {bind}"))?;
            let local = udp.local_addr()?;
            let lease = match options.map_port {
                Some(router) => Some(PcpLease::new(router, local).await?),
                None => None,
            };
            let advertised = options
                .public_endpoint
                .or_else(|| lease.as_ref().map(|lease| lease.mapping.external));
            let mux = UDPMuxDefault::new(UDPMuxParams::new(udp));
            Ok::<_, anyhow::Error>(Arc::new(DirectSocket {
                mux,
                local,
                advertised,
                _lease: lease,
            }))
        })
        .await?;
    let local = socket.local;
    settings.set_network_types(vec![if local.is_ipv4() {
        NetworkType::Udp4
    } else {
        NetworkType::Udp6
    }]);
    if !local.ip().is_unspecified() {
        settings.set_ip_filter(Box::new(move |ip| ip == local.ip()));
    }
    settings.set_udp_network(UDPNetwork::Muxed(socket.mux.clone()));
    Ok(Resources(Some(Arc::clone(socket))))
}

fn parse_router(input: &str) -> Result<SocketAddr> {
    let router = match input.parse::<IpAddr>() {
        Ok(ip) => SocketAddr::new(ip, 5351),
        Err(_) => input.parse().context("Expected a PCP router IP address")?,
    };
    ensure!(
        is_lan_ip(router.ip()) && router.port() > 0,
        "Choose a local PCP router and nonzero port"
    );
    Ok(router)
}

pub fn is_local_host(host: &str) -> bool {
    host == "localhost"
        || host
            .trim_matches(['[', ']'])
            .parse::<IpAddr>()
            .is_ok_and(is_lan_ip)
}

fn is_lan_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => ip.is_loopback() || ip.is_private() || ip.is_link_local(),
        IpAddr::V6(ip) => ip.is_loopback() || ip.is_unique_local() || ip.is_unicast_link_local(),
    }
}

fn lan_address() -> Result<IpAddr> {
    if let Ok(socket) = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
        && socket.connect((Ipv4Addr::new(192, 0, 2, 1), 9)).is_ok()
        && let Ok(address) = socket.local_addr()
        && is_lan_ip(address.ip())
        && !address.ip().is_loopback()
    {
        return Ok(address.ip());
    }
    if_addrs::get_if_addrs()?
        .into_iter()
        .map(|interface| interface.ip())
        .filter(|ip| is_lan_ip(*ip) && !ip.is_loopback())
        .filter(|ip| !matches!(ip, IpAddr::V6(ip) if ip.is_unicast_link_local()))
        .min_by_key(|ip| ip.is_ipv6())
        .context("No LAN address found. Set --lan-bind to a local IP and port.")
}

pub async fn lan_rendezvous(bind: SocketAddr) -> Result<(String, JoinHandle<()>)> {
    let ip = if bind.ip().is_unspecified() {
        lan_address()?
    } else {
        bind.ip()
    };
    ensure!(
        is_lan_ip(ip),
        "LAN rendezvous requires a private or loopback address"
    );
    let listener = TcpListener::bind(SocketAddr::new(ip, bind.port()))
        .await
        .with_context(|| format!("Could not start LAN rendezvous on {ip}"))?;
    let origin = format!("http://{}", listener.local_addr()?);
    let (api, sweep) = rendezvous::router();
    let app = Router::new()
        .nest("/v1", api)
        .route("/healthz", get(|| async { "ok\n" }))
        .route(
            "/join",
            get(|| async { Html(include_str!("../../../site/join.html")) }),
        )
        .route(
            "/app.js",
            get(|| async {
                (
                    [("content-type", "application/javascript")],
                    include_str!("../../../site/app.js"),
                )
            }),
        )
        .route(
            "/style.css",
            get(|| async {
                (
                    [("content-type", "text/css")],
                    include_str!("../../../site/style.css"),
                )
            }),
        )
        .route(
            "/icon.svg",
            get(|| async {
                (
                    [("content-type", "image/svg+xml")],
                    include_str!("../../../site/icon.svg"),
                )
            }),
        );
    let task = tokio::spawn(async move {
        tokio::select! {
            result = axum::serve(listener, app) => { if let Err(error) = result { eprintln!("LAN rendezvous stopped: {error}"); } }
            _ = sweep => {}
        }
    });
    Ok((origin, task))
}

const PCP_LIFETIME: u32 = 120;

#[derive(Clone, Copy)]
struct PcpMapping {
    router: SocketAddr,
    client: IpAddr,
    internal_port: u16,
    nonce: [u8; 12],
    external: SocketAddr,
}

struct PcpLease {
    mapping: PcpMapping,
    renew: JoinHandle<()>,
}

impl PcpLease {
    async fn new(router: SocketAddr, internal: SocketAddr) -> Result<Self> {
        let socket = UdpSocket::bind(SocketAddr::new(internal.ip(), 0)).await?;
        socket.connect(router).await?;
        let mut nonce = [0; 12];
        getrandom::fill(&mut nonce)
            .map_err(|error| anyhow::anyhow!("System randomness: {error}"))?;
        let mut mapping = PcpMapping {
            router,
            client: socket.local_addr()?.ip(),
            internal_port: internal.port(),
            nonce,
            external: SocketAddr::new(internal.ip(), internal.port()),
        };
        let (external, lifetime) = mapping
            .exchange(&socket)
            .await
            .with_context(|| format!("Could not request a PCP mapping from {router}"))?;
        mapping.external = external;
        eprintln!("PCP mapped UDP {} to {external}", internal.port());
        let renew = tokio::spawn(async move {
            let mut wait = (lifetime / 2).max(1);
            loop {
                sleep(Duration::from_secs(wait.into())).await;
                match mapping.exchange(&socket).await {
                    Ok((address, seconds)) if address == mapping.external => {
                        wait = (seconds / 2).max(1)
                    }
                    Ok((address, _)) => {
                        eprintln!(
                            "PCP mapping changed to {address}. Restart the share to advertise it."
                        );
                        break;
                    }
                    Err(error) => {
                        eprintln!("PCP renewal failed: {error:#}");
                        wait = 10;
                    }
                }
            }
        });
        Ok(Self { mapping, renew })
    }
}

impl Drop for PcpLease {
    fn drop(&mut self) {
        self.renew.abort();
        if let Ok(socket) = std::net::UdpSocket::bind(SocketAddr::new(self.mapping.client, 0)) {
            let _ = socket.send_to(&self.mapping.request(0), self.mapping.router);
        }
    }
}

impl PcpMapping {
    fn request(self, lifetime: u32) -> [u8; 60] {
        let mut packet = [0; 60];
        packet[0] = 2;
        packet[1] = 1;
        packet[4..8].copy_from_slice(&lifetime.to_be_bytes());
        packet[8..24].copy_from_slice(&pcp_ip(self.client));
        packet[24..36].copy_from_slice(&self.nonce);
        packet[36] = 17;
        packet[40..42].copy_from_slice(&self.internal_port.to_be_bytes());
        packet[42..44].copy_from_slice(&self.external.port().to_be_bytes());
        if !self.external.ip().is_unspecified() {
            packet[44..60].copy_from_slice(&pcp_ip(self.external.ip()));
        }
        packet
    }

    async fn exchange(self, socket: &UdpSocket) -> Result<(SocketAddr, u32)> {
        let mut response = [0; 1100];
        for wait in [350, 700, 1400] {
            socket.send(&self.request(PCP_LIFETIME)).await?;
            let Ok(size) = timeout(Duration::from_millis(wait), socket.recv(&mut response)).await
            else {
                continue;
            };
            let size = size?;
            ensure!(
                size >= 24 && response[0] == 2 && response[1] == 0x81,
                "Invalid PCP MAP response"
            );
            ensure!(
                response[3] == 0,
                "Router rejected PCP MAP with result {}",
                response[3]
            );
            ensure!(
                size >= 60
                    && response[24..36] == self.nonce
                    && response[36] == 17
                    && response[40..42] == self.internal_port.to_be_bytes(),
                "PCP response does not match the UDP mapping request"
            );
            let lifetime = u32::from_be_bytes(response[4..8].try_into()?);
            let port = u16::from_be_bytes(response[42..44].try_into()?);
            let address = Ipv6Addr::from(<[u8; 16]>::try_from(&response[44..60])?);
            let ip = address
                .to_ipv4_mapped()
                .map(IpAddr::V4)
                .unwrap_or(IpAddr::V6(address));
            ensure!(
                lifetime > 0 && port > 0 && !ip.is_unspecified() && !ip.is_multicast(),
                "Router returned an unusable UDP mapping"
            );
            ensure!(
                ip.is_ipv4() == self.client.is_ipv4(),
                "Router returned a PCP mapping for a different IP family"
            );
            return Ok((SocketAddr::new(ip, port), lifetime));
        }
        bail!("PCP router did not reply")
    }
}

fn pcp_ip(ip: IpAddr) -> [u8; 16] {
    match ip {
        IpAddr::V4(ip) => ip.to_ipv6_mapped().octets(),
        IpAddr::V6(ip) => ip.octets(),
    }
}

#[derive(Serialize)]
struct Probe {
    target: String,
    result: String,
}

pub async fn doctor(server: &str, stun: &[String], json: bool) -> Result<()> {
    let origin = crate::signaling::Invite::new(server)?.origin;
    let mut checks = JoinSet::new();
    checks.spawn(async move {
        let health = origin.join("healthz").expect("valid health path");
        let result = async {
            let response = reqwest::Client::builder()
                .timeout(Duration::from_secs(4))
                .redirect(reqwest::redirect::Policy::none())
                .build()?
                .get(health)
                .send()
                .await?
                .error_for_status()?;
            ensure!(
                response.text().await?.trim() == "ok",
                "unexpected health response"
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        Probe {
            target: format!("Rendezvous {origin}"),
            result: result.map_or_else(|e| e.to_string(), |()| "reachable".into()),
        }
    });
    for ipv6 in [false, true] {
        let servers = stun.to_owned();
        checks.spawn(async move { probe_stun(servers, ipv6).await });
    }
    let mut reports = Vec::new();
    while let Some(report) = checks.join_next().await {
        reports.push(report?);
    }
    reports.sort_by(|lhs, rhs| lhs.target.cmp(&rhs.target));
    if json {
        println!("{}", serde_json::to_string_pretty(&reports)?);
    } else {
        for report in reports {
            println!("{}: {}", report.target, report.result);
        }
        println!("Run share and connect across the two actual networks to test a direct path.");
    }
    Ok(())
}

async fn probe_stun(servers: Vec<String>, ipv6: bool) -> Probe {
    let target = if ipv6 {
        "STUN over IPv6"
    } else {
        "STUN over IPv4"
    }
    .to_owned();
    let result = async {
        ensure!(!servers.is_empty(), "disabled");
        let bind = if ipv6 { "[::]:0" } else { "0.0.0.0:0" };
        let socket = UdpSocket::bind(bind).await?;
        let mut addresses = Vec::new();
        let mut observations = Vec::new();
        for server in servers {
            let url = webrtc::ice::url::Url::parse_url(&server)?;
            let resolved =
                timeout(Duration::from_secs(2), lookup_host((&*url.host, url.port))).await;
            let Some(address) = resolved
                .ok()
                .and_then(Result::ok)
                .and_then(|mut addresses| addresses.find(|a| a.is_ipv6() == ipv6))
            else {
                observations.push(format!(
                    "{}: no reachable DNS address for this family",
                    url.host
                ));
                continue;
            };
            match stun_binding(&socket, address).await {
                Ok(mapped) => {
                    addresses.push(mapped);
                    observations.push(format!("{}: {mapped}", url.host));
                }
                Err(error) => observations.push(format!("{}: {error}", url.host)),
            }
        }
        if addresses.len() > 1 {
            let assessment = if addresses.windows(2).all(|pair| pair[0] == pair[1]) {
                "mapping matched across the tested servers"
            } else {
                "mapping changed by destination"
            };
            observations.push(assessment.into());
        }
        Ok::<_, anyhow::Error>(observations.join("; "))
    }
    .await;
    Probe {
        target,
        result: result.unwrap_or_else(|e| e.to_string()),
    }
}

async fn stun_binding(socket: &UdpSocket, server: SocketAddr) -> Result<SocketAddr> {
    let mut request = [0; 20];
    request[1] = 1;
    request[4..8].copy_from_slice(&[0x21, 0x12, 0xa4, 0x42]);
    getrandom::fill(&mut request[8..])
        .map_err(|error| anyhow::anyhow!("System randomness: {error}"))?;
    socket.send_to(&request, server).await?;
    let mut response = [0; 1024];
    timeout(Duration::from_secs(2), async {
        loop {
            let (len, source) = socket.recv_from(&mut response).await?;
            if source == server && len >= 20 && response[4..20] == request[4..20] {
                return stun_mapped(&response[..len]);
            }
        }
    })
    .await
    .context("UDP binding request timed out")?
}

fn stun_mapped(message: &[u8]) -> Result<SocketAddr> {
    ensure!(
        message.len() >= 20 && message[..2] == [1, 1],
        "STUN binding rejected"
    );
    let length = usize::from(u16::from_be_bytes(message[2..4].try_into()?));
    let mut attributes = message
        .get(20..20 + length)
        .context("Truncated STUN response")?;
    while attributes.len() >= 4 {
        let kind = u16::from_be_bytes(attributes[..2].try_into()?);
        let length = usize::from(u16::from_be_bytes(attributes[2..4].try_into()?));
        let value = attributes
            .get(4..4 + length)
            .context("Truncated STUN attribute")?;
        if kind == 0x20 && value.len() >= 4 {
            let port = u16::from_be_bytes(value[2..4].try_into()?) ^ 0x2112;
            let masked = match (value[1], value.len()) {
                (1, 8) | (2, 20) => &value[4..],
                _ => bail!("Invalid STUN address family"),
            };
            let mut ip = [0; 16];
            for (i, byte) in masked.iter().enumerate() {
                ip[i] = byte ^ message[4 + i];
            }
            let address = if masked.len() == 4 {
                IpAddr::V4(Ipv4Addr::from(<[u8; 4]>::try_from(&ip[..4])?))
            } else {
                IpAddr::V6(Ipv6Addr::from(ip))
            };
            return Ok(SocketAddr::new(address, port));
        }
        attributes = attributes
            .get(4 + length.next_multiple_of(4)..)
            .context("Truncated STUN padding")?;
    }
    bail!("STUN response had no mapped address")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_xor_mapped_ipv4() {
        let result = stun_mapped(&[
            1, 1, 0, 12, 0x21, 0x12, 0xa4, 0x42, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x20, 0, 8,
            0, 1, 0x32, 0x9a, 0xea, 0x12, 0xd5, 0x45,
        ])
        .unwrap();
        assert_eq!(result, "203.0.113.7:5000".parse().unwrap());
    }

    #[tokio::test]
    async fn pcp_maps_and_releases_a_port_with_its_nonce() {
        let router = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = router.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let mut request = [0; 60];
            let (size, peer) = router.recv_from(&mut request).await.unwrap();
            assert_eq!(size, 60);
            assert_eq!(&request[4..8], &PCP_LIFETIME.to_be_bytes());
            let mut response = request;
            response[1] = 0x81;
            response[42..44].copy_from_slice(&4567u16.to_be_bytes());
            response[44..60].copy_from_slice(&pcp_ip("203.0.113.7".parse().unwrap()));
            router.send_to(&response, peer).await.unwrap();
            let mut delete = [0; 60];
            router.recv_from(&mut delete).await.unwrap();
            assert_eq!(&delete[4..8], &[0; 4]);
            assert_eq!(&delete[24..36], &request[24..36]);
            assert_eq!(&delete[42..44], &4567u16.to_be_bytes());
        });
        let lease = PcpLease::new(addr, "127.0.0.1:3456".parse().unwrap())
            .await
            .unwrap();
        assert_eq!(lease.mapping.external, "203.0.113.7:4567".parse().unwrap());
        drop(lease);
        timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
    }
}
