use std::{
    collections::{BTreeMap, HashMap},
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use tokio::{
    net::TcpListener,
    sync::{mpsc, oneshot, watch},
    task::{JoinHandle, JoinSet},
    time::{Instant, interval, sleep, timeout},
};

use crate::{
    forwarding::{ClientRoutes, Services},
    identity, links,
    management::{Action, Management},
    network, shutdown,
    signaling::{
        DescriptionKind, HANDSHAKE_TIMEOUT, Invite, Offer, POLL_INTERVAL, SignalClient, random_id,
    },
    transport::{Peer, Relay, Traffic},
};

const MAX_INVITES: usize = 16;
const MAX_PEERS: usize = 16;
const APPROVAL_LIMIT: Duration = Duration::from_secs(25);
const DENIED_ANSWER: &str = "unbind/peer-declined/v1";

#[derive(Clone, Copy, Default)]
pub struct InviteOptions {
    pub expires_in: Option<u64>,
    pub single_use: bool,
}

pub struct Share {
    pub server: String,
    pub target_names: Vec<String>,
    pub compatibility_target: Option<String>,
    pub services: Services,
    pub invite: InviteOptions,
    pub approve: bool,
    pub profile: Option<String>,
    pub one_file: bool,
    pub qr: bool,
}

pub enum Client {
    Listen(TcpListener),
    Stdio,
    Named(ClientRoutes),
    Once(ClientRoutes),
}

/// Lifecycle controls for a session managed by another Rust frontend.
pub struct Observer {
    pub ready: oneshot::Sender<Ready>,
    /// Set this to true or drop its sender to shut the session down.
    pub stop: watch::Receiver<bool>,
}

/// A session's local controls are available. A guest may still be connecting.
pub struct Ready {
    pub id: String,
    pub invite: Option<(String, String)>,
    pub listeners: Vec<String>,
}

impl Client {
    fn relay(&self) -> Relay<'_> {
        match self {
            Self::Listen(listener) => Relay::Listen(listener),
            Self::Stdio => Relay::Stdio,
            Self::Named(routes) | Self::Once(routes) => Relay::Connect(routes),
        }
    }

    fn advanced(&self) -> bool {
        matches!(self, Self::Named(_) | Self::Once(_))
    }

    fn one_shot(&self) -> bool {
        matches!(self, Self::Stdio | Self::Once(_))
    }

    fn addresses(&self) -> Vec<String> {
        match self {
            Self::Listen(listener) => listener
                .local_addr()
                .map(|addr| vec![addr.to_string()])
                .unwrap_or_default(),
            Self::Stdio => vec![],
            Self::Named(routes) | Self::Once(routes) => routes.addresses(),
        }
    }
}

#[derive(Default)]
struct PeerState {
    path: Option<String>,
    traffic: Option<Traffic>,
    fingerprint: Option<String>,
    decision: Option<oneshot::Sender<bool>>,
}

type PeerStates = Arc<Mutex<BTreeMap<String, PeerState>>>;

fn peer_status(peers: &PeerStates) -> Vec<Value> {
    peers.lock().expect("peer state mutex poisoned")
        .iter().map(|(peer, state)| {
            let (sent, received) = state.traffic.as_ref().map_or((0, 0), Traffic::snapshot);
            json!({
                "id": peer,
                "identity": state.fingerprint,
                "state": if state.decision.is_some() { "pending" } else if state.path.is_some() { "connected" } else { "connecting" },
                "path": state.path,
                "bytes_sent": sent,
                "bytes_received": received,
            })
        }).collect()
}

struct Room {
    stop: watch::Sender<bool>,
    consumed: Arc<AtomicBool>,
    peers: PeerStates,
    task: JoinHandle<()>,
    expires_at: Option<u64>,
    single_use: bool,
}

impl Room {
    async fn shutdown(self) {
        self.stop.send_replace(true);
        let _ = self.task.await;
    }
}

struct RoomWorker {
    invite: Invite,
    signal: SignalClient,
    host_key: String,
    share: Arc<Share>,
    stun: Arc<[String]>,
    network: network::Options,
    deadline: Option<Instant>,
    consumed: Arc<AtomicBool>,
    peers: PeerStates,
    done: mpsc::Sender<()>,
    cli: bool,
}

pub async fn share(share: Share, stun: Vec<String>, network: network::Options) -> Result<()> {
    run_share(share, stun, network, None).await
}

pub async fn share_observed(
    share: Share,
    stun: Vec<String>,
    network: network::Options,
    observer: Observer,
) -> Result<()> {
    run_share(share, stun, network, Some(observer)).await
}

async fn run_share(
    share: Share,
    stun: Vec<String>,
    network: network::Options,
    observer: Option<Observer>,
) -> Result<()> {
    let cli = observer.is_none();
    let (ready, stop) = observer
        .map(|observer| (observer.ready, observer.stop))
        .unzip();
    let stopped = requested_stop(stop);
    tokio::pin!(stopped);
    let share = Arc::new(share);
    let stun: Arc<[String]> = stun.into();
    let (done, mut completed) = mpsc::channel(1);
    let (mut control, (id, link, room)) = tokio::select! {
        biased;
        result = &mut stopped => return result,
        result = async {
            let control = Management::start().await?;
            let room = open_room(&share, &stun, &network, share.invite, &done, cli).await?;
            Ok::<_, anyhow::Error>((control, room))
        } => result?,
    };
    let mut rooms = BTreeMap::from([(id.clone(), room)]);
    if let Some(ready) = ready {
        let _ = ready.send(Ready {
            id: control.id.clone(),
            invite: Some((id.clone(), link.clone())),
            listeners: Vec::new(),
        });
    } else {
        eprintln!(
            "Sharing {}. Keep this command running.",
            share.target_names.join(", ")
        );
        eprintln!("Share {} · invite {id}", control.id);
        println!("{link}");
        if share.qr {
            links::print_qr(&link)?;
        }
    }
    let outcome = loop {
        tokio::select! {
            result = &mut stopped => break result,
            Some(()) = completed.recv() => break Ok(()),
            Some(command) = control.commands.recv() => {
                let (action, reply) = command.into_parts();
                let stop = matches!(&action, Action::Stop);
                let result = match action {
                    Action::Status => Ok(share_status(&control.id, &share, &rooms)),
                    Action::GetInitialInvite => rooms.get(&id)
                        .filter(|room| !room.task.is_finished() && !room.consumed.load(Ordering::Relaxed))
                        .context("The initial invite is closed")
                        .map(|_| json!({"id": id, "link": link})),
                    Action::NewInvite { expires_in, single_use } => {
                        rooms.retain(|_, room| !room.task.is_finished());
                        if rooms.len() >= MAX_INVITES {
                            Err(anyhow::anyhow!("This share already has {MAX_INVITES} active invites"))
                        } else {
                            let opts = InviteOptions { expires_in, single_use };
                            let opened = tokio::select! {
                                biased;
                                result = &mut stopped => break result,
                                result = open_room(&share, &stun, &network, opts, &done, cli) => result,
                            };
                            opened.map(|(id, link, room)| {
                                rooms.insert(id.clone(), room);
                                json!({"id": id, "link": link})
                            })
                        }
                    }
                    Action::Revoke { invite } => match rooms.remove(&invite) {
                        Some(room) => {
                            room.shutdown().await;
                            Ok(json!({"revoked": invite}))
                        }
                        None => Err(anyhow::anyhow!("No invite with that ID")),
                    },
                    Action::Approve { peer, remember } => approve(&rooms, &peer, true, remember),
                    Action::Deny { peer } => approve(&rooms, &peer, false, false),
                    Action::Stop => Ok(json!({"stopped": control.id})),
                };
                reply.send(result).await;
                if stop {
                    break Ok(());
                }
            }
        }
    };
    drop(control);
    let mut closes = JoinSet::new();
    for room in rooms.into_values() {
        closes.spawn(room.shutdown());
    }
    while closes.join_next().await.is_some() {}
    outcome
}

fn share_status(id: &str, share: &Share, rooms: &BTreeMap<String, Room>) -> Value {
    let invites: Vec<_> = rooms.iter().map(|(id, room)| {
        let state = if room.task.is_finished() {
            "closed"
        } else if room.consumed.load(Ordering::Relaxed) {
            "consumed"
        } else {
            "accepting"
        };
        json!({
            "id": id, "state": state, "single_use": room.single_use, "expires_at": room.expires_at,
            "peers": peer_status(&room.peers),
        })
    }).collect();
    json!({"id": id, "kind": "share", "profile": share.profile, "targets": share.target_names, "invites": invites })
}

fn approve(
    rooms: &BTreeMap<String, Room>,
    peer: &str,
    allow: bool,
    remember: bool,
) -> Result<Value> {
    for room in rooms.values() {
        let mut states = room.peers.lock().expect("peer state mutex poisoned");
        if let Some(state) = states.get_mut(peer)
            && state.decision.is_some()
        {
            if remember {
                let fingerprint = state
                    .fingerprint
                    .as_deref()
                    .context("This device did not use --identify")?;
                identity::remember(fingerprint)?;
            }
            state
                .decision
                .take()
                .expect("checked decision")
                .send(allow)
                .map_err(|_| anyhow::anyhow!("The peer is no longer waiting"))?;
            return Ok(json!({"peer": peer, "approved": allow, "remembered": remember}));
        }
    }
    bail!("No pending peer '{peer}'. Run unbind status to list pending peers.")
}

async fn open_room(
    share: &Arc<Share>,
    stun: &Arc<[String]>,
    network: &network::Options,
    options: InviteOptions,
    done: &mpsc::Sender<()>,
    cli: bool,
) -> Result<(String, String, Room)> {
    if let Some(seconds) = options.expires_in {
        ensure!(
            (1..=31_536_000).contains(&seconds),
            "Invite lifetime must be 1 to 31536000 seconds"
        );
    }
    let invite = Invite::new(&share.server)?;
    let signal = SignalClient::new(invite.origin.clone())?;
    let host_key = random_id()?;
    signal.register(&invite.room, &host_key).await?;
    let id = invite.room[..12].to_owned();
    let link = invite.link().to_string();
    let peers = PeerStates::default();
    let consumed = Arc::new(AtomicBool::new(false));
    let (stop, shutdown) = watch::channel(false);
    let lifetime = options.expires_in.map(Duration::from_secs);
    let expires_at = lifetime.and_then(|lifetime| {
        SystemTime::now()
            .checked_add(lifetime)?
            .duration_since(UNIX_EPOCH)
            .ok()
            .map(|time| time.as_secs())
    });
    let worker = RoomWorker {
        invite,
        signal,
        host_key,
        share: Arc::clone(share),
        stun: Arc::clone(stun),
        network: network.clone(),
        deadline: lifetime.map(|lifetime| Instant::now() + lifetime),
        consumed: Arc::clone(&consumed),
        peers: Arc::clone(&peers),
        done: done.clone(),
        cli,
    };
    let task = tokio::spawn(Arc::new(worker).run(shutdown, options.single_use));
    Ok((
        id,
        link,
        Room {
            stop,
            consumed,
            peers,
            task,
            expires_at,
            single_use: options.single_use,
        },
    ))
}

impl RoomWorker {
    async fn run(self: Arc<Self>, mut stop: watch::Receiver<bool>, single_use: bool) {
        let mut stopping = stop.clone();
        let mut tasks: JoinSet<String> = JoinSet::new();
        let mut seen = HashMap::new();
        let mut outage = false;
        let mut tick = interval(POLL_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let expires = wait_for_expiry(self.deadline);
        tokio::pin!(expires);
        let mut directory_closed = false;
        let (stop_peers, peer_stop) = watch::channel(false);
        loop {
            tokio::select! {
                biased;
                _ = wait_for_stop(&mut stop) => break,
                _ = &mut expires => break,
                Some(Ok(finished)) = tasks.join_next(), if !tasks.is_empty() => {
                    self.peers.lock().expect("peer state mutex poisoned").remove(&finished);
                }
                _ = tick.tick() => {
                    if self.consumed.load(Ordering::Relaxed) {
                        if !directory_closed {
                            directory_closed = self.signal.close(&self.invite.room, &self.host_key).await.is_ok();
                        }
                        if tasks.is_empty() {
                            break;
                        }
                        continue;
                    }
                    let poll = async {
                        match self.signal.host_poll(&self.invite.room, &self.host_key).await {
                            Ok(Some(offers)) => Ok(offers),
                            Ok(None) => self.signal.register(&self.invite.room, &self.host_key).await.map(|()| vec![]),
                            Err(error) => Err(error),
                        }
                    };
                    let offers = tokio::select! {
                        offers = poll => offers,
                        _ = wait_for_stop(&mut stopping) => break,
                        _ = wait_for_expiry(self.deadline) => break,
                    };
                    match offers {
                        Ok(offers) => {
                            if outage {
                                if self.cli {
                                    eprintln!("Rendezvous connection restored.");
                                }
                                outage = false;
                            }
                            if *stopping.borrow() || self.deadline.is_some_and(|deadline| deadline <= Instant::now()) {
                                break;
                            }
                            seen.retain(|_, time: &mut Instant| time.elapsed() < Duration::from_secs(100));
                            for offer in offers.into_iter().take(8) {
                                if tasks.len() >= MAX_PEERS || seen.contains_key(&offer.peer) {
                                    continue;
                                }
                                if (single_use || self.share.one_file) && !tasks.is_empty() {
                                    break;
                                }
                                seen.insert(offer.peer.clone(), Instant::now());
                                let worker = Arc::clone(&self);
                                let stop = peer_stop.clone();
                                tasks.spawn(async move {
                                    let id = offer.peer.clone();
                                    if let Err(error) = worker.serve(offer, single_use, stop).await
                                        && worker.cli
                                    {
                                        eprintln!("Peer session ended: {error:#}");
                                    }
                                    id
                                });
                            }
                        }
                        Err(error) if !outage => {
                            if self.cli {
                                eprintln!("Rendezvous temporarily unavailable: {error:#}");
                            }
                            outage = true;
                        }
                        Err(_) => {}
                    }
                }
            }
        }
        stop_peers.send_replace(true);
        let _ = timeout(Duration::from_secs(3), async {
            while tasks.join_next().await.is_some() {}
        })
        .await;
        tasks.shutdown().await;
        self.peers
            .lock()
            .expect("peer state mutex poisoned")
            .clear();
        if !directory_closed {
            let _ = self.signal.close(&self.invite.room, &self.host_key).await;
        }
    }

    async fn serve(
        &self,
        request: Offer,
        single_use: bool,
        mut stop: watch::Receiver<bool>,
    ) -> Result<()> {
        self.invite
            .verify(&request.peer, DescriptionKind::Offer, &request.offer)?;
        let (offer, fingerprint) =
            identity::verify(&self.invite.room, &request.peer, &request.offer.sdp)?;
        let advanced = Peer::supports_forwarding(&offer);
        let role = if advanced {
            Relay::Serve(&self.share.services)
        } else {
            let target = self
                .share
                .compatibility_target
                .as_deref()
                .context("This share needs a client with named forwarding or file support")?;
            Relay::Forward(target)
        };
        let approval =
            self.share.approve && !fingerprint.as_deref().is_some_and(identity::is_trusted);
        let (decision, receive) = oneshot::channel();
        self.peers
            .lock()
            .expect("peer state mutex poisoned")
            .insert(
                request.peer.clone(),
                PeerState {
                    fingerprint,
                    decision: approval.then_some(decision),
                    ..Default::default()
                },
            );
        if approval {
            if self.cli {
                eprintln!(
                    "Peer {} requests approval. Run unbind status to review it.",
                    request.peer
                );
            }
            let allowed = tokio::select! {
                result = timeout(APPROVAL_LIMIT, receive) => result.context("Host approval timed out")??,
                _ = wait_for_stop(&mut stop) => return Ok(()),
            };
            if !allowed {
                let answer = self.invite.sign(
                    &request.peer,
                    DescriptionKind::Answer,
                    DENIED_ANSWER.to_owned(),
                )?;
                self.signal
                    .answer(&self.invite.room, &self.host_key, &request.peer, answer)
                    .await?;
                bail!("Host declined the connection");
            }
        }

        let mut peer = tokio::select! {
            result = Peer::with_network(&self.stun, &self.network) => result?,
            _ = wait_for_stop(&mut stop) => return Ok(()),
        };
        let session = async {
            let answer = peer.answer(offer).await?;
            let answer = self
                .invite
                .sign(&request.peer, DescriptionKind::Answer, answer)?;
            self.signal
                .answer(&self.invite.room, &self.host_key, &request.peer, answer)
                .await?;
            let channel = peer.connect().await?;
            if single_use {
                self.consumed.store(true, Ordering::Relaxed);
            }
            let path = peer.status().await.map(|status| status.path);
            if let Some(state) = self
                .peers
                .lock()
                .expect("peer state mutex poisoned")
                .get_mut(&request.peer)
            {
                state.path = path;
                state.traffic = Some(peer.traffic());
            }
            if self.cli {
                eprintln!("Peer connected directly.");
            }
            peer.run(channel, role).await
        };
        let result = tokio::select! {
            result = session => result,
            _ = wait_for_stop(&mut stop) => Ok(()),
        };
        peer.close().await;
        if result.is_ok() && self.share.one_file && !*stop.borrow() {
            let _ = self.done.try_send(());
        }
        result
    }
}

async fn wait_for_stop(stop: &mut watch::Receiver<bool>) {
    let _ = stop.wait_for(|stop| *stop).await;
}

async fn requested_stop(stop: Option<watch::Receiver<bool>>) -> Result<()> {
    if let Some(mut stop) = stop {
        wait_for_stop(&mut stop).await;
        Ok(())
    } else {
        shutdown().await
    }
}

async fn wait_for_expiry(deadline: Option<Instant>) {
    if let Some(deadline) = deadline {
        tokio::time::sleep_until(deadline).await;
    } else {
        std::future::pending().await
    }
}

#[derive(Default)]
struct ClientState {
    connected: Option<(String, Traffic)>,
    attempts: u64,
}

pub async fn connect(
    link: &str,
    client: Client,
    stun: &[String],
    network: &network::Options,
    identify: bool,
    retry: bool,
) -> Result<()> {
    run_connect(link, client, stun, network, identify, retry, None).await
}

pub async fn connect_observed(
    link: &str,
    client: Client,
    stun: &[String],
    network: &network::Options,
    identify: bool,
    retry: bool,
    observer: Observer,
) -> Result<()> {
    run_connect(link, client, stun, network, identify, retry, Some(observer)).await
}

async fn run_connect(
    link: &str,
    client: Client,
    stun: &[String],
    network: &network::Options,
    identify: bool,
    retry: bool,
    observer: Option<Observer>,
) -> Result<()> {
    let cli = observer.is_none();
    let (ready, stop) = observer
        .map(|observer| (observer.ready, observer.stop))
        .unzip();
    let stopped = requested_stop(stop);
    tokio::pin!(stopped);
    let invite = Invite::from_link(link)?;
    let stun = if invite.origin.scheme() == "http" {
        &[]
    } else {
        stun
    };
    let signal = SignalClient::new(invite.origin.clone())?;
    let mut management = tokio::select! {
        biased;
        result = &mut stopped => return result,
        result = Management::start() => result?,
    };
    let state = Mutex::new(ClientState::default());
    let addresses = client.addresses();
    if let Some(ready) = ready {
        let _ = ready.send(Ready {
            id: management.id.clone(),
            invite: None,
            listeners: addresses.clone(),
        });
    }
    let control = async {
        while let Some(command) = management.commands.recv().await {
            let (action, reply) = command.into_parts();
            match action {
                Action::Status => {
                    let status = {
                        let state = state.lock().expect("client state mutex poisoned");
                        let (path, sent, received) =
                            state
                                .connected
                                .as_ref()
                                .map_or((None, 0, 0), |(path, traffic)| {
                                    let (sent, received) = traffic.snapshot();
                                    (Some(path), sent, received)
                                });
                        json!({
                            "kind": "connect",
                            "id": management.id,
                            "state": if path.is_some() { "connected" } else { "connecting" },
                            "attempts": state.attempts,
                            "listeners": addresses,
                            "path": path,
                            "bytes_sent": sent,
                            "bytes_received": received,
                        })
                    };
                    reply.send(Ok(status)).await;
                }
                Action::Stop => {
                    reply.send(Ok(json!({"stopped": management.id}))).await;
                    return Ok(());
                }
                _ => {
                    reply
                        .send(Err(anyhow::anyhow!(
                            "That action requires a sharing session"
                        )))
                        .await
                }
            }
        }
        Ok::<(), anyhow::Error>(())
    };
    let (request_stop, stop) = watch::channel(false);
    let sessions = connect_sessions(ClientConnection {
        invite: &invite,
        signal: &signal,
        client: &client,
        state: &state,
        stun,
        network,
        identify,
        retry,
        stop,
        cli,
    });
    tokio::pin!(sessions);
    let exit = tokio::select! {
        result = &mut sessions => return result,
        result = control => result,
        result = &mut stopped => result,
    };
    drop(management);
    request_stop.send_replace(true);
    sessions.await?;
    exit
}

struct ClientConnection<'a> {
    invite: &'a Invite,
    signal: &'a SignalClient,
    client: &'a Client,
    state: &'a Mutex<ClientState>,
    stun: &'a [String],
    network: &'a network::Options,
    identify: bool,
    retry: bool,
    stop: watch::Receiver<bool>,
    cli: bool,
}

async fn connect_sessions(connection: ClientConnection<'_>) -> Result<()> {
    let ClientConnection {
        invite,
        signal,
        client,
        state,
        stun,
        network,
        identify,
        retry,
        mut stop,
        cli,
    } = connection;
    let mut retry_delay = Duration::from_millis(500);
    let mut connected_before = false;
    loop {
        state.lock().expect("client state mutex poisoned").attempts += 1;
        let mut peer = tokio::select! {
            result = Peer::with_network(stun, network) => result?,
            _ = wait_for_stop(&mut stop) => return Ok(()),
        };
        let peer_id = random_id()?;
        let session = async {
            if cli {
                eprintln!("Connecting to the peer directly…");
            }
            let mut offer = peer.offer_for(client.advanced()).await?;
            if identify {
                offer = identity::sign(&invite.room, &peer_id, offer)?;
            }
            let offer = invite.sign(&peer_id, DescriptionKind::Offer, offer)?;
            signal.join(&invite.room, &peer_id, offer).await?;
            let answer = timeout(HANDSHAKE_TIMEOUT, async {
                loop {
                    if let Some(answer) = signal.join_poll(&invite.room, &peer_id).await? {
                        return Ok::<_, anyhow::Error>(answer);
                    }
                    sleep(POLL_INTERVAL).await;
                }
            }).await.context("The share host did not answer. Check that the host is running and the invite is intact.")??;
            invite.verify(&peer_id, DescriptionKind::Answer, &answer)?;
            ensure!(answer.sdp != DENIED_ANSWER, "Host declined the connection");
            ensure!(
                !client.advanced() || Peer::supports_forwarding(&answer.sdp),
                "The share host does not support named forwarding"
            );
            peer.accept(answer.sdp).await?;
            let channel = peer.connect().await?;
            let path = peer
                .status()
                .await
                .map(|status| status.path)
                .unwrap_or_else(|| "direct UDP".to_owned());
            state.lock().expect("client state mutex poisoned").connected =
                Some((path, peer.traffic()));
            retry_delay = Duration::from_millis(500);
            connected_before = true;
            if cli {
                if client.one_shot() {
                    eprintln!("Direct connection ready.");
                } else {
                    println!(
                        "Direct connection ready. Listening on {}",
                        client.addresses().join(", ")
                    );
                }
            }
            peer.run(channel, client.relay()).await
        };
        let result = tokio::select! {
            result = session => result,
            _ = wait_for_stop(&mut stop) => Ok(()),
        };
        peer.close().await;
        if *stop.borrow() || !retry || !connected_before || client.one_shot() {
            return result;
        }
        state.lock().expect("client state mutex poisoned").connected = None;
        if let Err(error) = result
            && cli
        {
            eprintln!("Direct connection interrupted: {error:#}. Reconnecting…");
        }
        tokio::select! {
            _ = sleep(retry_delay) => {}
            _ = wait_for_stop(&mut stop) => return Ok(()),
        }
        retry_delay = (retry_delay * 2).min(Duration::from_secs(15));
    }
}

pub async fn bind_legacy(listen: SocketAddr) -> Result<Client> {
    Ok(Client::Listen(
        TcpListener::bind(listen)
            .await
            .with_context(|| format!("Could not listen on {listen}"))?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpStream,
    };

    #[tokio::test]
    async fn observed_sessions_forward_and_clean_up_on_stop() -> Result<()> {
        let state_dir = tempfile::tempdir()?;
        crate::management::set_state_dir(state_dir.path().join("state"));
        let bind = "127.0.0.1:0".parse()?;
        let (server, rendezvous) = network::lan_rendezvous(bind).await?;
        let target = TcpListener::bind(bind).await?;
        let address = target.local_addr()?.to_string();
        let echo = tokio::spawn(async move {
            let (mut peer, _) = target.accept().await?;
            let mut bytes = [0; 6];
            peer.read_exact(&mut bytes).await?;
            peer.write_all(&bytes).await
        });
        let (host_ready, host_started) = oneshot::channel();
        let (stop_host, host_stop) = watch::channel(false);
        let host = tokio::spawn(share_observed(
            Share {
                server,
                target_names: vec![address.clone()],
                compatibility_target: Some(address.clone()),
                services: Services::new(vec![crate::forwarding::Service::Tcp {
                    name: "default".into(),
                    target: address,
                }])?,
                invite: InviteOptions::default(),
                approve: false,
                profile: None,
                one_file: false,
                qr: false,
            },
            Vec::new(),
            network::Options::default(),
            Observer {
                ready: host_ready,
                stop: host_stop,
            },
        ));
        let host_info = timeout(Duration::from_secs(5), host_started).await??;
        let (_, link) = host_info.invite.context("Host provided no invite")?;
        let (guest_ready, guest_started) = oneshot::channel();
        let (stop_guest, guest_stop) = watch::channel(false);
        let client = bind_legacy(bind).await?;
        let guest = tokio::spawn(async move {
            connect_observed(
                &link,
                client,
                &[],
                &network::Options::default(),
                false,
                true,
                Observer {
                    ready: guest_ready,
                    stop: guest_stop,
                },
            )
            .await
        });
        let guest_info = timeout(Duration::from_secs(5), guest_started).await??;
        let response = timeout(Duration::from_secs(15), async {
            let mut local = TcpStream::connect(guest_info.listeners[0].as_str()).await?;
            local.write_all(b"unbind").await?;
            let mut response = [0; 6];
            local.read_exact(&mut response).await?;
            Ok::<_, std::io::Error>(response)
        })
        .await??;
        assert_eq!(response, *b"unbind");
        echo.await??;
        let guest_record = crate::management::find(&guest_info.id)?;
        let status = crate::management::request(&guest_record, Action::Status).await?;
        assert_eq!(status["state"], "connected");

        stop_guest.send_replace(true);
        drop(stop_host);
        timeout(Duration::from_secs(10), guest).await???;
        timeout(Duration::from_secs(10), host).await???;
        assert!(crate::management::records()?.is_empty());
        rendezvous.abort();
        Ok(())
    }
}
