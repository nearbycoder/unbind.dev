use std::{
    collections::{HashMap, HashSet},
    future::{pending, poll_fn},
    io::{self, Read},
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::{Poll, ready},
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};
use tokio::{
    fs::File,
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream, copy_bidirectional},
    net::{TcpListener, TcpStream, UdpSocket, lookup_host},
    sync::{mpsc, watch},
    task::JoinSet,
    time::{Instant, sleep_until, timeout},
};
use tokio_util::compat::{Compat, FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt};

const MAX_SERVICES: usize = 16;
const MAX_STREAMS: usize = 64;
const MAX_NAME: usize = 64;
const MAX_DATAGRAM: usize = 65_507;
const MAX_UDP_ASSOCIATIONS: usize = 16;
const UDP_QUEUE: usize = 4;
const UDP_IDLE: Duration = Duration::from_secs(60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const OK: u8 = 0;
const NO_SERVICE: u8 = 1;
const UNAVAILABLE: u8 = 2;

#[derive(Clone)]
pub enum Service {
    Tcp { name: String, target: String },
    Udp { name: String, target: String },
    File { path: PathBuf },
}

#[derive(Clone)]
pub struct Services(Vec<Service>);

pub enum Binding {
    Tcp { name: String, listen: SocketAddr },
    Udp { name: String, listen: SocketAddr },
    Stdio { name: String },
    Receive { destination: PathBuf },
}

pub struct ClientRoutes(Vec<Bound>);

enum Bound {
    Tcp {
        name: String,
        listener: Arc<TcpListener>,
    },
    Udp {
        name: String,
        socket: Arc<UdpSocket>,
    },
    Stdio(String),
    Receive(PathBuf),
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum Protocol {
    Tcp = 1,
    Udp = 2,
    File = 3,
}

struct Request {
    name: String,
    local: LocalEndpoint,
}

enum LocalEndpoint {
    Tcp(TcpStream),
    Udp {
        socket: Arc<UdpSocket>,
        client: SocketAddr,
        datagrams: mpsc::Receiver<Vec<u8>>,
    },
    Stdio,
    Receive(PathBuf),
}

impl Services {
    pub fn new(entries: Vec<Service>) -> Result<Self> {
        ensure!(
            !entries.is_empty() && entries.len() <= MAX_SERVICES,
            "A share requires 1 to {MAX_SERVICES} services"
        );
        let mut names = HashSet::new();
        for entry in &entries {
            let (protocol, name) = entry.route();
            validate_name(protocol, name)?;
            ensure!(
                names.insert((protocol, name)),
                "Duplicate {protocol:?} service {name:?}"
            );
        }
        Ok(Self(entries))
    }

    fn find(&self, protocol: Protocol, name: &str) -> Option<&Service> {
        self.0
            .iter()
            .find(|entry| entry.route() == (protocol, name))
    }
}

impl Service {
    fn route(&self) -> (Protocol, &str) {
        match self {
            Self::Tcp { name, .. } => (Protocol::Tcp, name),
            Self::Udp { name, .. } => (Protocol::Udp, name),
            Self::File { .. } => (Protocol::File, ""),
        }
    }
}

impl ClientRoutes {
    pub async fn bind(bindings: Vec<Binding>) -> Result<Self> {
        ensure!(
            !bindings.is_empty() && bindings.len() <= MAX_SERVICES,
            "Specify 1 to {MAX_SERVICES} local routes"
        );
        ensure!(
            bindings.len() == 1
                || !bindings.iter().any(|binding| matches!(
                    binding,
                    Binding::Stdio { .. } | Binding::Receive { .. }
                )),
            "Stdio and file receive require an exclusive connection"
        );
        let mut routes = Vec::with_capacity(bindings.len());
        for binding in bindings {
            routes.push(match binding {
                Binding::Tcp { name, listen } => {
                    validate_name(Protocol::Tcp, &name)?;
                    let listener = TcpListener::bind(listen)
                        .await
                        .with_context(|| format!("Could not listen on TCP {listen}"))?;
                    Bound::Tcp {
                        name,
                        listener: Arc::new(listener),
                    }
                }
                Binding::Udp { name, listen } => {
                    validate_name(Protocol::Udp, &name)?;
                    let socket = UdpSocket::bind(listen)
                        .await
                        .with_context(|| format!("Could not listen on UDP {listen}"))?;
                    Bound::Udp {
                        name,
                        socket: Arc::new(socket),
                    }
                }
                Binding::Stdio { name } => {
                    validate_name(Protocol::Tcp, &name)?;
                    Bound::Stdio(name)
                }
                Binding::Receive { destination } => {
                    ensure!(
                        !tokio::fs::try_exists(&destination).await?,
                        "Destination already exists: {}",
                        destination.display()
                    );
                    Bound::Receive(destination)
                }
            });
        }
        Ok(Self(routes))
    }

    pub fn addresses(&self) -> Vec<String> {
        self.0
            .iter()
            .filter_map(|route| match route {
                Bound::Tcp { name, listener } => listener
                    .local_addr()
                    .ok()
                    .map(|addr| format!("{name} TCP {addr}")),
                Bound::Udp { name, socket } => socket
                    .local_addr()
                    .ok()
                    .map(|addr| format!("{name} UDP {addr}")),
                _ => None,
            })
            .collect()
    }

    fn one_shot(&self) -> bool {
        matches!(self.0.as_slice(), [Bound::Stdio(_) | Bound::Receive(_)])
    }
}

fn validate_name(protocol: Protocol, name: &str) -> Result<()> {
    if protocol == Protocol::File {
        ensure!(name.is_empty(), "Invalid file route");
        return Ok(());
    }
    ensure!(
        !name.is_empty()
            && name.len() <= MAX_NAME
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
        "Service names must contain 1 to {MAX_NAME} letters, numbers, hyphens, or underscores"
    );
    Ok(())
}

impl LocalEndpoint {
    fn protocol(&self) -> Protocol {
        match self {
            Self::Tcp(_) | Self::Stdio => Protocol::Tcp,
            Self::Udp { .. } => Protocol::Udp,
            Self::Receive(_) => Protocol::File,
        }
    }
}

pub(super) async fn serve(
    socket: DuplexStream,
    services: &Services,
    file_complete: Arc<AtomicBool>,
) -> Result<()> {
    let mut mux = yamux::Connection::new(
        socket.compat(),
        yamux::Config::default(),
        yamux::Mode::Server,
    );
    let services = Arc::new(services.clone());
    let mut streams = JoinSet::new();
    let mut finish = None;
    loop {
        tokio::select! {
            inbound = poll_fn(|cx| mux.poll_next_inbound(cx)) => {
                let stream = inbound.context("The forwarding session closed")??;
                if streams.len() < MAX_STREAMS && finish.is_none() {
                    let services = Arc::clone(&services);
                    let file_complete = Arc::clone(&file_complete);
                    streams.spawn(serve_request(stream.compat(), services, file_complete));
                }
            }
            result = streams.join_next(), if !streams.is_empty() => {
                match result.expect("a stream is pending")? {
                    Ok(true) => {
                        // Keep driving the mux until the receiver sees our receipt and closes.
                        finish = Some(Instant::now() + REQUEST_TIMEOUT);
                    }
                    Ok(false) => {}
                    Err(error) => eprintln!("Forwarding stream closed: {error:#}"),
                }
            }
            _ = async {
                if let Some(deadline) = finish {
                    sleep_until(deadline).await;
                } else {
                    pending().await
                }
            } => return Ok(()),
        }
    }
}

pub(super) async fn connect(
    socket: DuplexStream,
    routes: &ClientRoutes,
    file_complete: Arc<AtomicBool>,
) -> Result<()> {
    let (request_tx, mut request_rx) = mpsc::channel(MAX_STREAMS);
    let mut listeners = JoinSet::new();
    for route in &routes.0 {
        let tx = request_tx.clone();
        match route {
            Bound::Tcp { name, listener } => {
                let (name, listener) = (name.clone(), Arc::clone(listener));
                listeners.spawn(tcp_listener(name, listener, tx));
            }
            Bound::Udp { name, socket } => {
                let (name, socket) = (name.clone(), Arc::clone(socket));
                listeners.spawn(udp_listener(name, socket, tx));
            }
            Bound::Stdio(name) => {
                tx.send(Request {
                    name: name.clone(),
                    local: LocalEndpoint::Stdio,
                })
                .await
                .ok()
                .context("The forwarding session closed")?;
            }
            Bound::Receive(path) => {
                tx.send(Request {
                    name: String::new(),
                    local: LocalEndpoint::Receive(path.clone()),
                })
                .await
                .ok()
                .context("The forwarding session closed")?;
            }
        }
    }

    let mut mux = yamux::Connection::new(
        socket.compat(),
        yamux::Config::default(),
        yamux::Mode::Client,
    );
    let mut streams = JoinSet::new();
    let mut next = None;
    loop {
        tokio::select! {
            request = request_rx.recv(), if next.is_none() && streams.len() < MAX_STREAMS => {
                next = request;
            }
            result = listeners.join_next(), if !listeners.is_empty() => {
                result.expect("a listener is active")??;
                bail!("A forwarding listener stopped");
            }
            result = streams.join_next(), if !streams.is_empty() => {
                let result = result.expect("a stream is pending")?;
                if routes.one_shot() {
                    return result;
                }
                if let Err(error) = result {
                    eprintln!("Forwarding stream closed: {error:#}");
                }
            }
            event = poll_fn(|cx| {
                if next.is_some()
                    && let Poll::Ready(stream) = mux.poll_new_outbound(cx)
                {
                    return Poll::Ready(stream.map(Some));
                }
                ready!(mux.poll_next_inbound(cx)).map_or_else(
                    || Poll::Ready(Err(yamux::ConnectionError::Closed)),
                    |result| Poll::Ready(result.map(|_| None)),
                )
            }) => {
                if let Some(stream) = event? {
                    let request = next.take().context("Missing forwarding request")?;
                    streams.spawn(connect_request(stream.compat(), request, Arc::clone(&file_complete)));
                }
            }
        }
    }
}

async fn tcp_listener(
    name: String,
    listener: Arc<TcpListener>,
    requests: mpsc::Sender<Request>,
) -> Result<()> {
    loop {
        let (socket, _) = listener.accept().await?;
        requests
            .send(Request {
                name: name.clone(),
                local: LocalEndpoint::Tcp(socket),
            })
            .await
            .ok()
            .context("The forwarding session closed")?;
    }
}

async fn udp_listener(
    name: String,
    socket: Arc<UdpSocket>,
    requests: mpsc::Sender<Request>,
) -> Result<()> {
    let mut associations: HashMap<SocketAddr, mpsc::Sender<Vec<u8>>> = HashMap::new();
    let mut buffer = vec![0; u16::MAX as usize];
    loop {
        let (size, client) = socket.recv_from(&mut buffer).await?;
        if size > MAX_DATAGRAM {
            continue;
        }
        associations.retain(|_, association| !association.is_closed());
        if let Some(association) = associations.get(&client) {
            let _ = association.try_send(buffer[..size].to_vec());
        } else if associations.len() < MAX_UDP_ASSOCIATIONS {
            let (datagram_tx, datagrams) = mpsc::channel(UDP_QUEUE);
            let request = Request {
                name: name.clone(),
                local: LocalEndpoint::Udp {
                    socket: Arc::clone(&socket),
                    client,
                    datagrams,
                },
            };
            if requests.try_send(request).is_ok() {
                let _ = datagram_tx.try_send(buffer[..size].to_vec());
                associations.insert(client, datagram_tx);
            }
        }
    }
}

async fn connect_request(
    mut stream: Compat<yamux::Stream>,
    request: Request,
    file_complete: Arc<AtomicBool>,
) -> Result<()> {
    let protocol = request.local.protocol();
    stream.write_u8(protocol as u8).await?;
    stream.write_u8(request.name.len() as u8).await?;
    stream.write_all(request.name.as_bytes()).await?;
    stream.flush().await?;
    match timeout(REQUEST_TIMEOUT, stream.read_u8())
        .await
        .context("The host did not open the requested service")??
    {
        OK => {}
        NO_SERVICE => bail!(
            "The host does not share {protocol:?} service {:?}",
            request.name
        ),
        UNAVAILABLE => bail!("The shared {protocol:?} target is unavailable"),
        _ => bail!("The host sent an invalid service response"),
    }

    match request.local {
        LocalEndpoint::Tcp(mut local) => {
            let _ = local.set_nodelay(true);
            copy_bidirectional(&mut local, &mut stream).await?;
            Ok(())
        }
        LocalEndpoint::Udp {
            socket,
            client,
            datagrams,
        } => udp_client(stream, socket, client, datagrams).await,
        LocalEndpoint::Stdio => copy_stdio(stream).await,
        LocalEndpoint::Receive(path) => receive_file(&mut stream, &path, &file_complete).await,
    }
}

async fn serve_request(
    mut stream: Compat<yamux::Stream>,
    services: Arc<Services>,
    file_complete: Arc<AtomicBool>,
) -> Result<bool> {
    let (protocol, name) = timeout(REQUEST_TIMEOUT, read_route(&mut stream))
        .await
        .context("Timed out reading the requested service")??;
    let Some(service) = services.find(protocol, &name) else {
        reject(&mut stream, NO_SERVICE).await?;
        return Ok(false);
    };
    match service {
        Service::Tcp { target, .. } => {
            match timeout(REQUEST_TIMEOUT, TcpStream::connect(target)).await {
                Ok(Ok(mut local)) => {
                    let _ = local.set_nodelay(true);
                    stream.write_u8(OK).await?;
                    stream.flush().await?;
                    copy_bidirectional(&mut local, &mut stream).await?;
                }
                _ => {
                    reject(&mut stream, UNAVAILABLE).await?;
                    bail!("Cannot open TCP target {target}");
                }
            }
        }
        Service::Udp { target, .. } => {
            let resolved = timeout(REQUEST_TIMEOUT, async {
                let address = lookup_host(target)
                    .await?
                    .next()
                    .context("No addresses found")?;
                let bind = match address {
                    SocketAddr::V4(_) => SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)),
                    SocketAddr::V6(_) => SocketAddr::from((Ipv6Addr::UNSPECIFIED, 0)),
                };
                let socket = UdpSocket::bind(bind).await?;
                socket.connect(address).await?;
                Ok::<_, anyhow::Error>(socket)
            })
            .await;
            let Ok(Ok(socket)) = resolved else {
                reject(&mut stream, UNAVAILABLE).await?;
                bail!("Cannot open UDP target {target}");
            };
            stream.write_u8(OK).await?;
            stream.flush().await?;
            udp_target(stream, socket).await?;
        }
        Service::File { path } => {
            let file = File::open(path).await;
            let Ok(file) = file else {
                reject(&mut stream, UNAVAILABLE).await?;
                bail!("Cannot open shared file {}", path.display());
            };
            let metadata = file.metadata().await?;
            if !metadata.is_file() {
                reject(&mut stream, UNAVAILABLE).await?;
                bail!("Shared path is not a regular file: {}", path.display());
            }
            stream.write_u8(OK).await?;
            send_file(&mut stream, file, metadata.len(), &file_complete).await?;
            return Ok(true);
        }
    }
    Ok(false)
}

async fn read_route(stream: &mut (impl AsyncRead + Unpin)) -> Result<(Protocol, String)> {
    let protocol = match stream.read_u8().await? {
        1 => Protocol::Tcp,
        2 => Protocol::Udp,
        3 => Protocol::File,
        _ => bail!("Unknown forwarding protocol"),
    };
    let size = stream.read_u8().await? as usize;
    ensure!(size <= MAX_NAME, "Service name is too long");
    let mut name = vec![0; size];
    stream.read_exact(&mut name).await?;
    let name = String::from_utf8(name)?;
    validate_name(protocol, &name)?;
    Ok((protocol, name))
}

async fn reject(stream: &mut (impl AsyncWrite + Unpin), error: u8) -> Result<()> {
    stream.write_u8(error).await?;
    stream.shutdown().await?;
    Ok(())
}

async fn write_datagram(stream: &mut (impl AsyncWrite + Unpin), data: &[u8]) -> Result<()> {
    ensure!(data.len() <= MAX_DATAGRAM, "UDP datagram is too large");
    stream.write_u16(data.len() as u16).await?;
    stream.write_all(data).await?;
    stream.flush().await?;
    Ok(())
}

async fn read_datagram(stream: &mut (impl AsyncRead + Unpin), buffer: &mut [u8]) -> Result<usize> {
    let size = stream.read_u16().await? as usize;
    ensure!(size <= buffer.len(), "Received an oversized UDP datagram");
    stream.read_exact(&mut buffer[..size]).await?;
    Ok(size)
}

async fn udp_client(
    stream: Compat<yamux::Stream>,
    socket: Arc<UdpSocket>,
    client: SocketAddr,
    mut datagrams: mpsc::Receiver<Vec<u8>>,
) -> Result<()> {
    let (mut input, mut output) = tokio::io::split(stream);
    let (activity, clock) = watch::channel(Instant::now());
    let incoming = async {
        let mut buffer = vec![0; MAX_DATAGRAM];
        loop {
            let size = read_datagram(&mut input, &mut buffer).await?;
            socket.send_to(&buffer[..size], client).await?;
            activity.send_replace(Instant::now());
        }
    };
    let outgoing = async {
        while let Some(data) = datagrams.recv().await {
            write_datagram(&mut output, &data).await?;
            activity.send_replace(Instant::now());
        }
        Ok(())
    };
    tokio::select! {
        result = incoming => result,
        result = outgoing => result,
        _ = udp_expiry(clock) => Ok(()),
    }
}

async fn udp_target(stream: Compat<yamux::Stream>, socket: UdpSocket) -> Result<()> {
    let (mut input, mut output) = tokio::io::split(stream);
    let (activity, clock) = watch::channel(Instant::now());
    let incoming = async {
        let mut buffer = vec![0; MAX_DATAGRAM];
        loop {
            let size = read_datagram(&mut input, &mut buffer).await?;
            socket.send(&buffer[..size]).await?;
            activity.send_replace(Instant::now());
        }
    };
    let outgoing = async {
        let mut buffer = vec![0; u16::MAX as usize];
        loop {
            let size = socket.recv(&mut buffer).await?;
            if size <= MAX_DATAGRAM {
                write_datagram(&mut output, &buffer[..size]).await?;
                activity.send_replace(Instant::now());
            }
        }
    };
    tokio::select! {
        result = incoming => result,
        result = outgoing => result,
        _ = udp_expiry(clock) => Ok(()),
    }
}

async fn udp_expiry(mut clock: watch::Receiver<Instant>) {
    loop {
        let deadline = *clock.borrow_and_update() + UDP_IDLE;
        tokio::select! {
            _ = sleep_until(deadline) => return,
            result = clock.changed() => {
                if result.is_err() {
                    return;
                }
            }
        }
    }
}

pub(super) async fn legacy_stdio(socket: DuplexStream) -> Result<()> {
    let mut mux = yamux::Connection::new(
        socket.compat(),
        yamux::Config::default(),
        yamux::Mode::Client,
    );
    let stream = poll_fn(|cx| mux.poll_new_outbound(cx)).await?;
    let drive = poll_fn(|cx| {
        loop {
            match ready!(mux.poll_next_inbound(cx)) {
                Some(Ok(_)) => {}
                Some(Err(error)) => return Poll::Ready(Err(error.into())),
                None => return Poll::Ready(Ok(())),
            }
        }
    });
    tokio::select! {
        result = copy_stdio(stream.compat()) => result,
        result = drive => result,
    }
}

async fn copy_stdio(stream: Compat<yamux::Stream>) -> Result<()> {
    let (mut input, mut output) = tokio::io::split(stream);
    let (tx, mut stdin) = mpsc::channel(4);
    // A detached synchronous reader lets the command exit when the peer closes
    // stdout even if the caller still has its input pipe open.
    std::thread::Builder::new()
        .name("unbind-stdin".to_owned())
        .spawn(move || {
            let mut input = io::stdin().lock();
            loop {
                let mut buffer = vec![0; 16 * 1024];
                let result = input.read(&mut buffer).map(|size| {
                    buffer.truncate(size);
                    buffer
                });
                if result.as_ref().is_ok_and(Vec::is_empty) {
                    return;
                }
                let error = result.is_err();
                if tx.blocking_send(result).is_err() || error {
                    return;
                }
            }
        })?;

    let upstream = async {
        while let Some(data) = stdin.recv().await {
            output.write_all(&data?).await?;
            output.flush().await?;
        }
        output.shutdown().await?;
        Ok::<_, io::Error>(())
    };
    let downstream = async {
        let mut stdout = tokio::io::stdout();
        tokio::io::copy(&mut input, &mut stdout).await?;
        stdout.flush().await?;
        Ok::<_, io::Error>(())
    };
    tokio::pin!(downstream);
    tokio::select! {
        result = &mut downstream => result?,
        result = upstream => {
            result?;
            downstream.await?;
        }
    }
    Ok(())
}

async fn send_file(
    stream: &mut (impl AsyncRead + AsyncWrite + Unpin),
    mut file: File,
    size: u64,
    file_complete: &AtomicBool,
) -> Result<()> {
    stream.write_u64(size).await?;
    let digest = file_payload(&mut file, stream, size, "Sent").await?;
    stream.write_all(&digest).await?;
    stream.flush().await?;
    let receipt = timeout(REQUEST_TIMEOUT, stream.read_u8())
        .await
        .context("Timed out waiting for the file verification receipt")??;
    ensure!(receipt == OK, "The receiver rejected the file");
    file_complete.store(true, Ordering::Release);
    eprintln!(
        "Peer verified {size} bytes (SHA-256 {}).",
        hex_digest(&digest)
    );
    stream.write_u8(OK).await?;
    stream.shutdown().await?;
    Ok(())
}

async fn receive_file(
    stream: &mut (impl AsyncRead + AsyncWrite + Unpin),
    destination: &Path,
    file_complete: &AtomicBool,
) -> Result<()> {
    let size = timeout(REQUEST_TIMEOUT, stream.read_u64())
        .await
        .context("Timed out reading file size")??;
    let parent = destination
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let (file, path) = tempfile::Builder::new()
        .prefix(".unbind-")
        .tempfile_in(parent)
        .with_context(|| format!("Could not stage a file in {}", parent.display()))?
        .into_parts();
    let mut file = File::from_std(file);
    let actual = file_payload(stream, &mut file, size, "Received").await?;
    let mut expected = [0; 32];
    stream.read_exact(&mut expected).await?;
    ensure!(
        actual == expected,
        "The received file did not match its SHA-256 checksum"
    );
    file.flush().await?;
    file.sync_all().await?;
    drop(file);
    path.persist_noclobber(destination)
        .with_context(|| format!("Could not save {}", destination.display()))?;
    file_complete.store(true, Ordering::Release);
    eprintln!(
        "Saved {} ({size} bytes, SHA-256 {}).",
        destination.display(),
        hex_digest(&actual)
    );
    stream.write_u8(OK).await?;
    stream.flush().await?;
    let acknowledged = timeout(REQUEST_TIMEOUT, stream.read_u8())
        .await
        .context("Timed out waiting for the peer to confirm the file receipt")??;
    ensure!(
        acknowledged == OK,
        "The peer did not confirm the file receipt"
    );
    Ok(())
}

async fn file_payload(
    input: &mut (impl AsyncRead + Unpin),
    output: &mut (impl AsyncWrite + Unpin),
    size: u64,
    action: &str,
) -> Result<[u8; 32]> {
    let mut hash = Sha256::new();
    let mut buffer = vec![0; 64 * 1024];
    let mut remaining = size;
    let mut update = Instant::now() + Duration::from_secs(1);
    while remaining > 0 {
        let count = remaining.min(buffer.len() as u64) as usize;
        input.read_exact(&mut buffer[..count]).await?;
        output.write_all(&buffer[..count]).await?;
        hash.update(&buffer[..count]);
        remaining -= count as u64;
        if Instant::now() >= update {
            eprintln!("{action} {} / {size} bytes.", size - remaining);
            update = Instant::now() + Duration::from_secs(1);
        }
    }
    Ok(hash.finalize().into())
}

fn hex_digest(digest: &[u8; 32]) -> String {
    use std::fmt::Write;

    let mut result = String::with_capacity(64);
    for byte in digest {
        write!(result, "{byte:02x}").expect("writing to a string cannot fail");
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{Peer, Relay};
    use webrtc::data::data_channel::DataChannel;

    type Connected = (Peer, Arc<DataChannel>);

    async fn direct_pair() -> Result<(Connected, Connected)> {
        let network = crate::network::Options::default();
        let mut host = Peer::with_network(&[], &network).await?;
        let mut guest = Peer::with_network(&[], &network).await?;
        let offer = guest.offer_for(true).await?;
        assert!(Peer::supports_forwarding(&offer));
        let answer = host.answer(offer).await?;
        assert!(Peer::supports_forwarding(&answer));
        guest.accept(answer).await?;
        let (host_channel, guest_channel) = tokio::try_join!(host.connect(), guest.connect())?;
        Ok(((host, host_channel), (guest, guest_channel)))
    }

    #[tokio::test]
    async fn named_tcp_and_udp_share_one_direct_session() -> Result<()> {
        timeout(Duration::from_secs(25), async {
            let tcp = TcpListener::bind("127.0.0.1:0").await?;
            let udp = UdpSocket::bind("127.0.0.1:0").await?;
            let services = Services::new(vec![
                Service::Tcp {
                    name: "web".into(),
                    target: tcp.local_addr()?.to_string(),
                },
                Service::Udp {
                    name: "dns".into(),
                    target: udp.local_addr()?.to_string(),
                },
            ])?;
            let local = "127.0.0.1:0".parse()?;
            let routes = ClientRoutes::bind(vec![
                Binding::Tcp {
                    name: "web".into(),
                    listen: local,
                },
                Binding::Udp {
                    name: "dns".into(),
                    listen: local,
                },
            ])
            .await?;
            let addresses = routes.addresses();
            let tcp_addr: SocketAddr = addresses[0].split_whitespace().last().unwrap().parse()?;
            let udp_addr: SocketAddr = addresses[1].split_whitespace().last().unwrap().parse()?;

            let ((host, host_channel), (guest, guest_channel)) = direct_pair().await?;
            let host =
                tokio::spawn(async move { host.run(host_channel, Relay::Serve(&services)).await });
            let guest =
                tokio::spawn(
                    async move { guest.run(guest_channel, Relay::Connect(&routes)).await },
                );
            let tcp_echo = async {
                let (mut socket, _) = tcp.accept().await?;
                let mut data = Vec::new();
                socket.read_to_end(&mut data).await?;
                socket.write_all(&data).await?;
                socket.shutdown().await
            };
            let tcp_request = async {
                let payload = vec![1; 96 * 1024];
                let mut socket = TcpStream::connect(tcp_addr).await?;
                socket.write_all(&payload).await?;
                socket.shutdown().await?;
                let mut echoed = Vec::new();
                socket.read_to_end(&mut echoed).await?;
                assert_eq!(echoed, payload);
                Ok::<_, io::Error>(())
            };
            let udp_echo = async {
                let mut data = vec![0; MAX_DATAGRAM];
                for _ in 0..2 {
                    let (size, client) = udp.recv_from(&mut data).await?;
                    udp.send_to(&data[..size], client).await?;
                }
                Ok::<_, io::Error>(())
            };
            let udp_request = async {
                let mut buffer = vec![0; MAX_DATAGRAM];
                let first = UdpSocket::bind(local).await?;
                let second = UdpSocket::bind(local).await?;
                first.send_to(&vec![2; 20_000], udp_addr).await?;
                second.send_to(&[], udp_addr).await?;
                let (count, _) = first.recv_from(&mut buffer).await?;
                assert_eq!(&buffer[..count], &vec![2; 20_000]);
                let (count, _) = second.recv_from(&mut buffer).await?;
                assert_eq!(count, 0);
                Ok::<_, io::Error>(())
            };
            tokio::try_join!(tcp_echo, tcp_request, udp_echo, udp_request)?;
            host.abort();
            guest.abort();
            Ok(())
        })
        .await?
    }

    #[tokio::test]
    async fn direct_file_transfer_is_verified_and_acknowledged() -> Result<()> {
        timeout(Duration::from_secs(25), async {
            let dir = tempfile::tempdir()?;
            let from = dir.path().join("from");
            let to = dir.path().join("to");
            let bytes = vec![7; 512 * 1024 + 9];
            tokio::fs::write(&from, &bytes).await?;
            let services = Services::new(vec![Service::File { path: from }])?;
            let routes = ClientRoutes::bind(vec![Binding::Receive {
                destination: to.clone(),
            }])
            .await?;
            let ((host, host_channel), (guest, guest_channel)) = direct_pair().await?;
            let send = host.run(host_channel, Relay::Serve(&services));
            let receive = async {
                let result = guest.run(guest_channel, Relay::Connect(&routes)).await;
                guest.close().await;
                result
            };
            tokio::try_join!(send, receive)?;
            assert_eq!(tokio::fs::read(&to).await?, bytes);
            assert!(
                ClientRoutes::bind(vec![Binding::Receive { destination: to }])
                    .await
                    .is_err()
            );
            Ok(())
        })
        .await?
    }

    #[tokio::test]
    async fn invalid_file_digest_keeps_the_destination_absent() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("to");
        let (mut receive, mut send) = tokio::io::duplex(256);
        let finished = AtomicBool::new(false);
        send.write_u64(4).await?;
        send.write_all(b"data").await?;
        send.write_all(&[1; 32]).await?;
        let error = receive_file(&mut receive, &path, &finished)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("SHA-256 checksum"));
        assert!(!finished.load(Ordering::Acquire));
        assert_eq!(std::fs::read_dir(dir.path())?.count(), 0);
        Ok(())
    }
}
