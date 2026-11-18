use std::{
    fs::{self, OpenOptions},
    io::Write,
    net::{Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::OnceLock,
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use subtle::ConstantTimeEq;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{mpsc, oneshot},
    task::{JoinHandle, JoinSet},
    time::timeout,
};

use crate::signaling::random_id;

static STATE_OVERRIDE: OnceLock<PathBuf> = OnceLock::new();

const REQUEST_LIMIT: u64 = 4096;
const RESPONSE_LIMIT: u64 = 64 * 1024;
const CONTROL_TIMEOUT: Duration = Duration::from_secs(12);

#[derive(Deserialize, Serialize)]
pub enum Action {
    Status,
    GetInitialInvite,
    NewInvite {
        expires_in: Option<u64>,
        single_use: bool,
    },
    Revoke {
        invite: String,
    },
    Approve {
        peer: String,
        remember: bool,
    },
    Deny {
        peer: String,
    },
    Stop,
}

#[derive(Deserialize, Serialize)]
struct Request {
    token: String,
    action: Action,
}

#[derive(Deserialize, Serialize)]
pub struct Record {
    pub id: String,
    pub pid: u32,
    address: SocketAddr,
    token: String,
}

type Reply = std::result::Result<Value, String>;

pub struct Command {
    pub action: Action,
    responder: Responder,
}

pub struct Responder {
    reply: oneshot::Sender<Reply>,
    delivered: oneshot::Receiver<()>,
}

impl Responder {
    pub async fn send(self, result: Result<Value>) {
        let _ = self
            .reply
            .send(result.map_err(|error| format!("{error:#}")));
        let _ = self.delivered.await;
    }
}

impl Command {
    pub fn into_parts(self) -> (Action, Responder) {
        (self.action, self.responder)
    }
}

pub struct Management {
    pub id: String,
    pub commands: mpsc::Receiver<Command>,
    server: JoinHandle<()>,
    record: PathBuf,
}

impl Drop for Management {
    fn drop(&mut self) {
        self.server.abort();
        let _ = fs::remove_file(&self.record);
    }
}

pub fn set_state_dir(path: PathBuf) {
    let _ = STATE_OVERRIDE.set(path);
}

pub fn state_dir() -> Result<PathBuf> {
    let path = if let Some(path) = STATE_OVERRIDE.get() {
        path.clone()
    } else if let Some(path) = std::env::var_os("UNBIND_STATE_DIR") {
        PathBuf::from(path)
    } else if cfg!(windows) {
        PathBuf::from(std::env::var_os("LOCALAPPDATA").context("LOCALAPPDATA is unavailable")?)
            .join("unbind")
    } else if let Some(path) = std::env::var_os("XDG_STATE_HOME") {
        PathBuf::from(path).join("unbind")
    } else if cfg!(target_os = "macos") {
        PathBuf::from(std::env::var_os("HOME").context("HOME is unavailable")?)
            .join("Library/Application Support/Unbind")
    } else {
        PathBuf::from(std::env::var_os("HOME").context("HOME is unavailable")?)
            .join(".local/state/unbind")
    };
    private_dir(&path)?;
    fs::canonicalize(path).context("Resolve Unbind state directory")
}

pub fn private_dir(path: &Path) -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)?;
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "Unbind needs a private directory at {}",
        path.display()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            metadata.permissions().mode() & 0o077 == 0,
            "Set the permissions on {} to 0700",
            path.display()
        );
    }
    Ok(())
}

pub fn create_private_file(path: &Path) -> std::io::Result<fs::File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

fn record_dir() -> Result<PathBuf> {
    let path = state_dir()?.join("sessions");
    private_dir(&path)?;
    Ok(path)
}

impl Management {
    pub async fn start() -> Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let id = random_id()?[..12].to_owned();
        let record = Record {
            id: id.clone(),
            pid: std::process::id(),
            address: listener.local_addr()?,
            token: random_id()?,
        };
        let path = record_dir()?.join(format!("{id}.json"));
        let bytes = serde_json::to_vec(&record)?;
        let mut file = create_private_file(&path)?;
        if let Err(error) = file.write_all(&bytes) {
            let _ = fs::remove_file(&path);
            return Err(error.into());
        }
        let (send, commands) = mpsc::channel(32);
        let server = tokio::spawn(control_server(listener, record.token, send));
        Ok(Self {
            id,
            commands,
            server,
            record: path,
        })
    }
}

async fn control_server(listener: TcpListener, token: String, send: mpsc::Sender<Command>) {
    let mut clients = JoinSet::new();
    loop {
        tokio::select! {
            incoming = listener.accept(), if clients.len() < 32 => {
                let Ok((stream, _)) = incoming else { return; };
                let token = token.clone();
                let send = send.clone();
                clients.spawn(async move {
                    let _ = timeout(CONTROL_TIMEOUT, serve_request(stream, &token, send)).await;
                });
            }
            _ = clients.join_next(), if !clients.is_empty() => {}
        }
    }
}

async fn serve_request(
    mut stream: TcpStream,
    token: &str,
    send: mpsc::Sender<Command>,
) -> Result<()> {
    let mut bytes = Vec::new();
    (&mut stream)
        .take(REQUEST_LIMIT + 1)
        .read_to_end(&mut bytes)
        .await?;
    ensure!(bytes.len() as u64 <= REQUEST_LIMIT, "Oversized request");
    let request: Request = serde_json::from_slice(&bytes)?;
    ensure!(
        bool::from(request.token.as_bytes().ct_eq(token.as_bytes())),
        "Invalid control credential"
    );
    let (reply, receive) = oneshot::channel();
    let (written, delivered) = oneshot::channel();
    send.send(Command {
        action: request.action,
        responder: Responder { reply, delivered },
    })
    .await?;
    let result = receive.await?;
    stream.write_all(&serde_json::to_vec(&result)?).await?;
    stream.shutdown().await?;
    let _ = written.send(());
    Ok(())
}

pub fn records() -> Result<Vec<Record>> {
    let mut records = Vec::new();
    for entry in fs::read_dir(record_dir()?)?.take(256) {
        let entry = entry?;
        if !entry.file_type()?.is_file() || entry.metadata()?.len() > REQUEST_LIMIT {
            continue;
        }
        if let Ok(bytes) = fs::read(entry.path())
            && let Ok(record) = serde_json::from_slice::<Record>(&bytes)
            && record.address.ip() == Ipv4Addr::LOCALHOST
            && entry.file_name() == format!("{}.json", record.id).as_str()
        {
            records.push(record);
        }
    }
    records.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(records)
}

pub async fn request(record: &Record, action: Action) -> Result<Value> {
    let request = Request {
        token: record.token.clone(),
        action,
    };
    let response = timeout(CONTROL_TIMEOUT, async {
        let mut stream = TcpStream::connect(record.address).await?;
        stream.write_all(&serde_json::to_vec(&request)?).await?;
        stream.shutdown().await?;
        let mut bytes = Vec::new();
        stream
            .take(RESPONSE_LIMIT + 1)
            .read_to_end(&mut bytes)
            .await?;
        ensure!(
            bytes.len() as u64 <= RESPONSE_LIMIT,
            "Oversized control response"
        );
        Ok::<_, anyhow::Error>(serde_json::from_slice::<Reply>(&bytes)?)
    })
    .await
    .context("Timed out waiting for Unbind")??;
    response.map_err(anyhow::Error::msg)
}

pub async fn active() -> Result<Vec<(Record, Value)>> {
    let mut tasks = JoinSet::new();
    for record in records()? {
        tasks.spawn(async move {
            request(&record, Action::Status)
                .await
                .ok()
                .map(|value| (record, value))
        });
    }
    let mut active = Vec::new();
    while let Some(result) = tasks.join_next().await {
        if let Ok(Some(record)) = result {
            active.push(record);
        }
    }
    active.sort_by(|a, b| a.0.id.cmp(&b.0.id));
    Ok(active)
}

pub fn find(id: &str) -> Result<Record> {
    for record in records()? {
        if record.id == id {
            return Ok(record);
        }
    }
    bail!("No Unbind session '{id}'. Run unbind status to list sessions.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn control_requires_the_token() -> Result<()> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let mut record = Record {
            id: "test".into(),
            pid: std::process::id(),
            address: listener.local_addr()?,
            token: "expected".into(),
        };
        let (send, mut commands) = mpsc::channel(1);
        let server = tokio::spawn(control_server(listener, record.token.clone(), send));
        let service = tokio::spawn(async move {
            let command = commands.recv().await.unwrap();
            assert!(matches!(command.action, Action::Status));
            command
                .into_parts()
                .1
                .send(Ok(serde_json::json!({"id":"test"})))
                .await;
        });

        let token = std::mem::replace(&mut record.token, "invalid".into());
        assert!(request(&record, Action::Stop).await.is_err());
        record.token = token;
        assert_eq!(request(&record, Action::Status).await?["id"], "test");
        service.await?;
        server.abort();
        Ok(())
    }
}
