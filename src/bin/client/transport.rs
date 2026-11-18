use std::{
    future::poll_fn,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    task::{Poll, ready},
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use bytes::Bytes;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, DuplexStream, ReadHalf, WriteHalf, copy_bidirectional},
    net::{TcpListener, TcpStream},
    sync::oneshot,
    task::JoinSet,
    time::{sleep, timeout},
};
use tokio_util::compat::{FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt};
use webrtc::{
    api::{APIBuilder, setting_engine::SettingEngine},
    data::data_channel::DataChannel,
    data_channel::data_channel_init::RTCDataChannelInit,
    ice::{mdns::MulticastDnsMode, network_type::NetworkType},
    ice_transport::ice_server::RTCIceServer,
    peer_connection::{
        RTCPeerConnection, configuration::RTCConfiguration,
        peer_connection_state::RTCPeerConnectionState,
        sdp::session_description::RTCSessionDescription,
    },
};

use crate::{
    forwarding::{self, ClientRoutes, Services},
    network,
};

pub const DEFAULT_STUN: &str = "stun:stun.l.google.com:19302,stun:stun.cloudflare.com:3478";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(18);
const MAX_STREAMS: usize = 64;
const MESSAGE_SIZE: usize = 16 * 1024;
const SEND_BUFFER_LIMIT: usize = 512 * 1024;
const FORWARDING_ATTRIBUTE: &str = "a=x-unbind-forwarding:1";

pub enum Relay<'a> {
    Listen(&'a TcpListener),
    Forward(&'a str),
    Stdio,
    Serve(&'a Services),
    Connect(&'a ClientRoutes),
}

#[derive(Clone, Default)]
pub struct Traffic(Arc<TrafficCounters>);

#[derive(Default)]
struct TrafficCounters {
    sent: AtomicU64,
    received: AtomicU64,
}

impl Traffic {
    pub fn snapshot(&self) -> (u64, u64) {
        (
            self.0.sent.load(Ordering::Relaxed),
            self.0.received.load(Ordering::Relaxed),
        )
    }
}

pub struct DirectStatus {
    pub path: String,
}

pub struct Peer {
    rtc: Arc<RTCPeerConnection>,
    channel: Option<oneshot::Receiver<Arc<DataChannel>>>,
    traffic: Traffic,
    _network: network::Resources,
}

impl Peer {
    pub async fn with_network(stun: &[String], options: &network::Options) -> Result<Self> {
        let mut settings = SettingEngine::default();
        settings.detach_data_channels();
        settings.set_network_types(vec![NetworkType::Udp4, NetworkType::Udp6]);
        settings.set_ice_multicast_dns_mode(MulticastDnsMode::Disabled);
        settings.set_include_loopback_candidate(true);
        let network = network::configure(&mut settings, options).await?;
        let api = APIBuilder::new().with_setting_engine(settings).build();
        let ice_servers = if stun.is_empty() {
            vec![]
        } else {
            vec![RTCIceServer {
                urls: stun.to_vec(),
                ..Default::default()
            }]
        };
        let rtc = Arc::new(
            api.new_peer_connection(RTCConfiguration {
                ice_servers,
                ..Default::default()
            })
            .await?,
        );
        let channel = rtc
            .create_data_channel(
                "unbind-tcp-v1",
                Some(RTCDataChannelInit {
                    negotiated: Some(0),
                    ..Default::default()
                }),
            )
            .await?;
        let (send_channel, receive_channel) = oneshot::channel();
        let weak_channel = Arc::downgrade(&channel);
        channel.on_open(Box::new(move || {
            Box::pin(async move {
                if let Some(channel) = weak_channel.upgrade()
                    && let Ok(detached) = channel.detach().await
                {
                    let _ = send_channel.send(detached);
                }
            })
        }));
        Ok(Self {
            rtc,
            channel: Some(receive_channel),
            traffic: Traffic::default(),
            _network: network,
        })
    }

    pub async fn offer_for(&self, advanced: bool) -> Result<String> {
        self.gather(self.rtc.create_offer(None).await?, advanced)
            .await
    }

    pub fn supports_forwarding(sdp: &str) -> bool {
        sdp.lines().any(|line| line == FORWARDING_ATTRIBUTE)
    }

    pub async fn answer(&self, offer: String) -> Result<String> {
        validate_sdp(&offer)?;
        let advanced = Self::supports_forwarding(&offer);
        self.rtc
            .set_remote_description(RTCSessionDescription::offer(offer)?)
            .await?;
        self.gather(self.rtc.create_answer(None).await?, advanced)
            .await
    }

    pub async fn accept(&self, answer: String) -> Result<()> {
        validate_sdp(&answer)?;
        self.rtc
            .set_remote_description(RTCSessionDescription::answer(answer)?)
            .await?;
        Ok(())
    }

    async fn gather(&self, desc: RTCSessionDescription, advanced: bool) -> Result<String> {
        let mut complete = self.rtc.gathering_complete_promise().await;
        self.rtc.set_local_description(desc).await?;
        // Snapshot available candidates if a STUN server is unreachable.
        let _ = timeout(Duration::from_secs(6), complete.recv()).await;
        let mut sdp = self
            .rtc
            .local_description()
            .await
            .context("Missing local ICE description")?
            .sdp;
        self._network.advertise(&mut sdp);
        if advanced {
            let media = sdp
                .find("\nm=")
                .context("Missing local media description")?
                + 1;
            sdp.insert_str(media, &format!("{FORWARDING_ATTRIBUTE}\r\n"));
        }
        validate_sdp(&sdp)?;
        ensure!(
            sdp.lines().any(|line| line.starts_with("a=candidate:")),
            "No UDP address is available for a direct connection"
        );
        Ok(sdp)
    }

    pub async fn connect(&mut self) -> Result<Arc<DataChannel>> {
        let receive = self
            .channel
            .take()
            .context("The direct connection was already used")?;
        let channel = timeout(CONNECT_TIMEOUT, receive)
            .await
            .context("Could not establish a direct UDP connection. The peers may be behind restrictive NAT or a firewall.")?
            .context("The encrypted peer connection closed during setup")?;
        self.rtc
            .sctp()
            .transport()
            .ice_transport()
            .get_selected_candidate_pair()
            .await
            .context("No direct ICE path was selected")?;
        Ok(channel)
    }

    pub async fn run(&self, channel: Arc<DataChannel>, role: Relay<'_>) -> Result<()> {
        let (out, mut closed) = tokio::sync::watch::channel(false);
        self.rtc
            .on_peer_connection_state_change(Box::new(move |state| {
                if matches!(
                    state,
                    RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed
                ) {
                    out.send_replace(true);
                }
                Box::pin(async {})
            }));
        if matches!(
            self.rtc.connection_state(),
            RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed
        ) {
            bail!("The direct peer connection closed");
        }

        let (mux_socket, wire_socket) = tokio::io::duplex(64 * 1024);
        let (wire_read, wire_write) = tokio::io::split(wire_socket);
        let file_complete = Arc::new(AtomicBool::new(false));
        let result = tokio::select! {
            result = read_channel(&channel, wire_write, &self.traffic) => result,
            result = write_channel(&channel, wire_read, &self.traffic) => result,
            result = run_relay(mux_socket, role, Arc::clone(&file_complete)) => result,
            _ = closed.wait_for(|closed| *closed) => Err(anyhow::anyhow!("The direct peer connection closed")),
        };
        if file_complete.load(Ordering::Acquire) {
            Ok(())
        } else {
            result
        }
    }

    pub fn traffic(&self) -> Traffic {
        self.traffic.clone()
    }

    pub async fn status(&self) -> Option<DirectStatus> {
        let pair = self
            .rtc
            .sctp()
            .transport()
            .ice_transport()
            .get_selected_candidate_pair()
            .await?;
        Some(DirectStatus {
            path: pair.to_string(),
        })
    }

    pub async fn close(&self) {
        let _ = timeout(Duration::from_secs(2), self.rtc.close()).await;
    }
}

fn validate_sdp(sdp: &str) -> Result<()> {
    ensure!(
        !sdp.lines().any(|line| {
            line.starts_with("a=candidate:")
                && line.split_ascii_whitespace().nth(7) == Some("relay")
        }),
        "Unbind accepts direct ICE candidates only"
    );
    Ok(())
}

pub fn parse_stun(value: &str) -> Result<Vec<String>> {
    if value == "none" {
        return Ok(vec![]);
    }
    value
        .split(',')
        .map(|url| {
            let url = url.trim();
            ensure!(
                url.starts_with("stun:"),
                "UNBIND_STUN accepts stun: URLs or none"
            );
            webrtc::ice::url::Url::parse_url(url).context("Invalid STUN address")?;
            Ok(url.to_owned())
        })
        .collect()
}

async fn read_channel(
    channel: &DataChannel,
    mut output: WriteHalf<DuplexStream>,
    traffic: &Traffic,
) -> Result<()> {
    let mut buffer = vec![0; MESSAGE_SIZE];
    loop {
        let size = channel.read(&mut buffer).await?;
        if size == 0 {
            bail!("The direct peer closed the connection");
        }
        traffic.0.received.fetch_add(size as u64, Ordering::Relaxed);
        output.write_all(&buffer[..size]).await?;
    }
}

async fn write_channel(
    channel: &DataChannel,
    mut input: ReadHalf<DuplexStream>,
    traffic: &Traffic,
) -> Result<()> {
    let mut buffer = vec![0; MESSAGE_SIZE];
    loop {
        let size = input.read(&mut buffer).await?;
        if size == 0 {
            return Ok(());
        }
        while channel.buffered_amount() >= SEND_BUFFER_LIMIT {
            sleep(Duration::from_millis(10)).await;
        }
        let sent = channel
            .write(&Bytes::copy_from_slice(&buffer[..size]))
            .await?;
        ensure!(sent == size, "The direct peer accepted a partial message");
        traffic.0.sent.fetch_add(sent as u64, Ordering::Relaxed);
    }
}

enum MuxEvent {
    Incoming(yamux::Stream),
    Outgoing(yamux::Stream),
}

enum LegacyRelay<'a> {
    Listen(&'a TcpListener),
    Forward(&'a str),
}

async fn run_relay(
    socket: DuplexStream,
    role: Relay<'_>,
    file_complete: Arc<AtomicBool>,
) -> Result<()> {
    match role {
        Relay::Listen(listener) => multiplex(socket, LegacyRelay::Listen(listener)).await,
        Relay::Forward(target) => multiplex(socket, LegacyRelay::Forward(target)).await,
        Relay::Stdio => forwarding::legacy_stdio(socket).await,
        Relay::Serve(services) => forwarding::serve(socket, services, file_complete).await,
        Relay::Connect(routes) => forwarding::connect(socket, routes, file_complete).await,
    }
}

async fn multiplex(socket: DuplexStream, role: LegacyRelay<'_>) -> Result<()> {
    let mode = match role {
        LegacyRelay::Listen(_) => yamux::Mode::Client,
        LegacyRelay::Forward(_) => yamux::Mode::Server,
    };
    let config = yamux::Config::default();
    let mut connection = yamux::Connection::new(socket.compat(), config, mode);
    let mut tasks = JoinSet::new();
    let mut pending = None;

    loop {
        let ready_for_tcp = pending.is_none() && tasks.len() < MAX_STREAMS;
        tokio::select! {
            stream = accept_local(&role), if ready_for_tcp => {
                pending = Some(stream?);
            }
            _ = tasks.join_next(), if !tasks.is_empty() => {}
            event = poll_fn(|cx| {
                if pending.is_some()
                    && let Poll::Ready(stream) = connection.poll_new_outbound(cx)
                {
                    return Poll::Ready(stream.map(MuxEvent::Outgoing));
                }
                Poll::Ready(match ready!(connection.poll_next_inbound(cx)) {
                    Some(stream) => stream.map(MuxEvent::Incoming),
                    None => Err(yamux::ConnectionError::Closed),
                })
            }) => {
                match (event?, &role) {
                    (MuxEvent::Incoming(stream), LegacyRelay::Forward(target))
                        if tasks.len() < MAX_STREAMS =>
                    {
                        let target = target.to_string();
                        tasks.spawn(async move {
                            let local = timeout(Duration::from_secs(10), TcpStream::connect(&target)).await;
                            match local {
                                Ok(Ok(local)) => copy_stream(local, stream).await,
                                Ok(Err(error)) => eprintln!("Cannot connect to {target}: {error}"),
                                Err(_) => eprintln!("Timed out connecting to {target}"),
                            }
                        });
                    }
                    (MuxEvent::Outgoing(stream), LegacyRelay::Listen(_)) => {
                        let local = pending.take().context("Missing local TCP connection")?;
                        tasks.spawn(copy_stream(local, stream));
                    }
                    _ => {}
                }
            }
        }
    }
}

async fn accept_local(role: &LegacyRelay<'_>) -> Result<TcpStream> {
    if let LegacyRelay::Listen(listener) = role {
        let (socket, _) = listener.accept().await?;
        Ok(socket)
    } else {
        std::future::pending().await
    }
}

async fn copy_stream(mut local: TcpStream, stream: yamux::Stream) {
    let _ = local.set_nodelay(true);
    if let Err(error) = copy_bidirectional(&mut local, &mut stream.compat()).await {
        eprintln!("TCP stream closed: {error}");
    }
}
