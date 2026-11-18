use std::{future::Future, net::SocketAddr, path::PathBuf, pin::pin, time::Duration};

use anyhow::{Context, Result};
use rune::AppHandle;
use tokio::{
    runtime::Runtime,
    sync::{mpsc, oneshot, watch},
    task::{JoinHandle, JoinSet},
    time::{interval, timeout},
};
use unbind::{
    forwarding::{Binding, ClientRoutes},
    management::{self, Action},
    network,
    session::{self, Client, Observer, Ready, Share},
};

use super::state::{Page, Session, State};

pub struct Network {
    pub server: String,
    pub stun: Vec<String>,
    pub options: network::Options,
}

pub enum Listener {
    Tcp(SocketAddr),
    Named(Vec<Binding>),
    File(PathBuf),
}

impl Listener {
    async fn bind(self) -> Result<Client> {
        match self {
            Self::Tcp(address) => session::bind_legacy(address).await,
            Self::Named(bindings) => Ok(Client::Named(ClientRoutes::bind(bindings).await?)),
            Self::File(destination) => Ok(Client::Once(
                ClientRoutes::bind(vec![Binding::Receive { destination }]).await?,
            )),
        }
    }
}

pub enum Request {
    Share {
        share: Share,
        lan: bool,
        network: Network,
    },
    Connect {
        link: String,
        listener: Listener,
        identify: bool,
        retry: bool,
        network: Network,
    },
    Manage {
        session: String,
        action: Action,
    },
    Diagnose(Network),
    EnableLinks,
    Refresh,
}

pub struct Engine {
    runtime: Runtime,
    shutdown: watch::Sender<bool>,
    worker: JoinHandle<()>,
}

impl Engine {
    pub fn start(ui: AppHandle<State>) -> Result<(Self, mpsc::UnboundedSender<Request>)> {
        let runtime = Runtime::new().context("Start the networking runtime")?;
        let (sender, requests) = mpsc::unbounded_channel();
        let (shutdown, stop) = watch::channel(false);
        let worker = runtime.spawn(serve(ui, requests, stop));
        Ok((
            Self {
                runtime,
                shutdown,
                worker,
            },
            sender,
        ))
    }

    pub fn finish(self) {
        self.shutdown.send_replace(true);
        let _ = self
            .runtime
            .block_on(async { timeout(Duration::from_secs(5), self.worker).await });
        self.runtime.shutdown_timeout(Duration::from_millis(250));
    }
}

async fn serve(
    ui: AppHandle<State>,
    mut requests: mpsc::UnboundedReceiver<Request>,
    mut stop: watch::Receiver<bool>,
) {
    let mut tasks = JoinSet::new();
    let status_ui = ui.clone();
    let poll = tokio::spawn(async move {
        let mut tick = interval(Duration::from_millis(1000));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            if let Ok(sessions) = snapshots().await {
                update_sessions(&status_ui, sessions);
            }
        }
    });
    loop {
        tokio::select! {
            _ = stop.changed() => break,
            Some(_) = tasks.join_next(), if !tasks.is_empty() => {},
            request = requests.recv() => match request {
                Some(request) => {
                    let ui = ui.clone();
                    let stop = stop.clone();
                    tasks.spawn(handle(request, ui, stop));
                },
                None => break,
            },
        }
    }
    poll.abort();
    while tasks.join_next().await.is_some() {}
}

async fn handle(request: Request, ui: AppHandle<State>, stop: watch::Receiver<bool>) {
    match request {
        Request::Share {
            share,
            lan,
            network,
        } => {
            run_session(&ui, stop, None, |observer| {
                start_share(share, lan, network, observer)
            })
            .await;
        }
        Request::Connect {
            link,
            listener,
            identify,
            retry,
            network,
        } => {
            let destination = match &listener {
                Listener::File(path) => Some(path.clone()),
                _ => None,
            };
            run_session(&ui, stop, destination, |observer| async move {
                let client = listener.bind().await?;
                session::connect_observed(
                    &link,
                    client,
                    &network.stun,
                    &network.options,
                    identify,
                    retry,
                    observer,
                )
                .await
            })
            .await;
        }
        Request::Manage { session, action } => {
            let invite = matches!(action, Action::NewInvite { .. });
            let request = async {
                let record = management::find(&session)?;
                management::request(&record, action).await
            };
            match request.await {
                Ok(result) if invite => {
                    if let Some(link) = result["link"].as_str() {
                        let link = link.to_owned();
                        ui.update(move |s, _| s.show_invite(link));
                    }
                }
                Ok(_) => {
                    ui.update(|s, _| s.set_notice("Session updated."));
                }
                Err(error) => report_error(&ui, error),
            }
        }
        Request::Refresh => match snapshots().await {
            Ok(sessions) => update_sessions(&ui, sessions),
            Err(error) => report_error(&ui, error),
        },
        Request::Diagnose(network) => {
            let result = network::diagnose(&network.server, &network.stun).await;
            ui.update(move |s, _| {
                s.busy = false;
                match result {
                    Ok(rows) => {
                        s.diagnostics = rows
                            .into_iter()
                            .map(|row| format!("{} — {}", row.target, row.result))
                            .collect();
                        s.set_notice("Network checks finished.");
                    }
                    Err(error) => s.set_error(format!("{error:#}")),
                }
            });
        }
        Request::EnableLinks => {
            let result = tokio::task::spawn_blocking(super::platform::enable_links).await;
            ui.update(move |s, _| {
                s.busy = false;
                match result {
                    Ok(Ok(message)) => s.set_notice(message),
                    Ok(Err(error)) => s.set_error(format!("{error:#}")),
                    Err(error) => s.set_error(error.to_string()),
                }
            });
        }
    }
}

async fn snapshots() -> Result<Vec<Session>> {
    management::active()
        .await?
        .into_iter()
        .map(|(_, value)| serde_json::from_value(value).context("Read active peer status"))
        .collect()
}

fn update_sessions(ui: &AppHandle<State>, sessions: Vec<Session>) {
    ui.update(move |s, tx| {
        if sessions == s.sessions {
            tx.live_only();
        } else {
            s.set_sessions(sessions);
        }
    });
}

fn report_error(ui: &AppHandle<State>, error: anyhow::Error) {
    let message = format!("{error:#}");
    ui.update(move |s, _| s.set_error(message));
}

struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn start_share(
    mut share: Share,
    lan: bool,
    network: Network,
    observer: Observer,
) -> Result<()> {
    let mut stun = network.stun;
    let _rendezvous = if lan {
        let bind = SocketAddr::from(([0, 0, 0, 0], 0));
        let (origin, task) = network::lan_rendezvous(bind).await?;
        share.server = origin;
        stun.clear();
        Some(AbortOnDrop(task))
    } else {
        None
    };
    session::share_observed(share, stun, network.options, observer).await
}

async fn run_session<F>(
    ui: &AppHandle<State>,
    stop: watch::Receiver<bool>,
    destination: Option<PathBuf>,
    start: impl FnOnce(Observer) -> F,
) where
    F: Future<Output = Result<()>>,
{
    let (ready, mut started) = oneshot::channel();
    let observer = Observer { ready, stop };
    let mut task = pin!(start(observer));
    let result = tokio::select! {
        biased;
        ready = &mut started => {
            match ready {
                Ok(ready) => ui.update(move |s, _| ready_session(s, ready)),
                Err(_) => ui.update(|s, _| s.busy = false),
            }
            task.await
        }
        result = &mut task => {
            if let Ok(ready) = started.try_recv() {
                ui.update(move |s, _| ready_session(s, ready));
            } else {
                ui.update(|s, _| s.busy = false);
            }
            result
        }
    };
    match result {
        Ok(()) => {
            if let Some(path) = destination.filter(|path| path.is_file()) {
                ui.update(move |s, _| {
                    s.set_notice(format!("Saved {}.", path.display()));
                });
            }
        }
        Err(error) => report_error(ui, error),
    }
}

fn ready_session(state: &mut State, ready: Ready) {
    state.busy = false;
    if let Some((_, link)) = ready.invite {
        state.show_invite(link);
    } else if ready.listeners.is_empty() {
        state.set_notice("Waiting for the file sender…");
    } else {
        state.page = Page::Sessions;
        state.set_notice(format!(
            "Local listener ready at {}.",
            ready.listeners.join(", ")
        ));
    }
}
