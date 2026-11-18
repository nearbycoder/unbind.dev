use std::{
    collections::HashMap,
    env,
    net::{Ipv4Addr, SocketAddr},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Request, State},
    http::{HeaderValue, Method, StatusCode, header, uri::Authority},
    middleware::{self, Next},
    response::{IntoResponse, Redirect, Response},
    routing::{get, get_service, post},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;
use tokio::net::TcpListener;
use tower_http::services::{ServeDir, ServeFile};

const PUBLIC_ORIGIN: &str = "https://unbind.dev";
const LEGACY_PUBLIC_HOST: &str = "unbind-web-production.up.railway.app";
const ROOM_TTL: Duration = Duration::from_secs(60);
const EXCHANGE_TTL: Duration = Duration::from_secs(90);
const EXPIRY_SWEEP_INTERVAL: Duration = Duration::from_secs(15);
const MAX_ROOMS: usize = 256;
const MAX_PENDING_PER_ROOM: usize = 8;
const MAX_EXCHANGES_PER_ROOM: usize = 16;
const MAX_EXCHANGES: usize = 256;
const MAX_SDP_BYTES: usize = 32 * 1024;
const MAX_REQUEST_BYTES: usize = 40 * 1024;

type Rooms = Arc<Mutex<HashMap<Key, Room>>>;
type ApiError = (StatusCode, &'static str);
type ApiResult<T> = Result<T, ApiError>;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(try_from = "String", into = "String")]
struct Key([u8; 32]);

impl TryFrom<String> for Key {
    type Error = &'static str;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        let mut bytes = [0; 32];
        if value.len() != 43 || URL_SAFE_NO_PAD.decode_slice(value, &mut bytes) != Ok(bytes.len()) {
            return Err("expected a 32-byte base64url value without padding");
        }
        Ok(Self(bytes))
    }
}

impl From<Key> for String {
    fn from(value: Key) -> Self {
        URL_SAFE_NO_PAD.encode(value.0)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HostRequest {
    room: Key,
    host_key: Key,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HostAnswer {
    room: Key,
    host_key: Key,
    peer: Key,
    answer: Signal,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JoinRequest {
    room: Key,
    peer: Key,
    offer: Signal,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JoinPoll {
    room: Key,
    peer: Key,
}

#[derive(Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Signal {
    sdp: String,
    mac: Key,
}

impl Signal {
    fn validate(&self) -> ApiResult<()> {
        if self.sdp.is_empty() || self.sdp.len() > MAX_SDP_BYTES {
            return Err((StatusCode::BAD_REQUEST, "SDP must contain 1 to 32768 bytes"));
        }
        Ok(())
    }
}

#[derive(Serialize)]
struct PendingOffer {
    peer: Key,
    offer: Signal,
}

struct Room {
    host_key: Key,
    last_seen: Instant,
    exchanges: HashMap<Key, Exchange>,
}

impl Room {
    fn authenticate_host(&self, key: Key) -> ApiResult<()> {
        if bool::from(self.host_key.0.ct_eq(&key.0)) {
            return Ok(());
        }
        Err((StatusCode::FORBIDDEN, "invalid host key"))
    }
}

struct Exchange {
    expires: Instant,
    offer: Signal,
    answer: Option<Signal>,
}

fn prune(rooms: &mut HashMap<Key, Room>, now: Instant) {
    rooms.retain(|_, room| {
        if now.duration_since(room.last_seen) >= ROOM_TTL {
            return false;
        }
        room.exchanges.retain(|_, exchange| exchange.expires > now);
        true
    });
}

const ROOM_MISSING: ApiError = (StatusCode::NOT_FOUND, "room is offline or has expired");
const PEER_MISSING: ApiError = (
    StatusCode::NOT_FOUND,
    "peer offer has expired or is missing",
);
const AT_CAPACITY: ApiError = (
    StatusCode::SERVICE_UNAVAILABLE,
    "rendezvous capacity reached",
);

async fn register(
    State(rooms): State<Rooms>,
    Json(request): Json<HostRequest>,
) -> ApiResult<StatusCode> {
    let mut rooms = rooms.lock().expect("room registry mutex poisoned");
    let now = Instant::now();
    prune(&mut rooms, now);
    if let Some(room) = rooms.get_mut(&request.room) {
        if room.authenticate_host(request.host_key).is_err() {
            return Err((StatusCode::CONFLICT, "room has a different host key"));
        }
        room.last_seen = now;
        return Ok(StatusCode::OK);
    }
    if rooms.len() >= MAX_ROOMS {
        return Err(AT_CAPACITY);
    }
    rooms.insert(
        request.room,
        Room {
            host_key: request.host_key,
            last_seen: now,
            exchanges: HashMap::new(),
        },
    );
    Ok(StatusCode::OK)
}

async fn host_poll(
    State(rooms): State<Rooms>,
    Json(request): Json<HostRequest>,
) -> ApiResult<Json<Vec<PendingOffer>>> {
    let mut rooms = rooms.lock().expect("room registry mutex poisoned");
    let now = Instant::now();
    prune(&mut rooms, now);
    let room = rooms.get_mut(&request.room).ok_or(ROOM_MISSING)?;
    room.authenticate_host(request.host_key)?;
    room.last_seen = now;
    let offers = room
        .exchanges
        .iter()
        .filter(|(_, exchange)| exchange.answer.is_none())
        .map(|(&peer, exchange)| PendingOffer {
            peer,
            offer: exchange.offer.clone(),
        })
        .collect();
    Ok(Json(offers))
}

async fn host_answer(
    State(rooms): State<Rooms>,
    Json(request): Json<HostAnswer>,
) -> ApiResult<StatusCode> {
    request.answer.validate()?;
    let mut rooms = rooms.lock().expect("room registry mutex poisoned");
    let now = Instant::now();
    prune(&mut rooms, now);
    let room = rooms.get_mut(&request.room).ok_or(ROOM_MISSING)?;
    room.authenticate_host(request.host_key)?;
    let exchange = room.exchanges.get_mut(&request.peer).ok_or(PEER_MISSING)?;
    if let Some(answer) = &exchange.answer {
        if *answer == request.answer {
            return Ok(StatusCode::OK);
        }
        return Err((StatusCode::CONFLICT, "peer already has a different answer"));
    }
    exchange.answer = Some(request.answer);
    exchange.expires = now + EXCHANGE_TTL;
    Ok(StatusCode::OK)
}

async fn join(
    State(rooms): State<Rooms>,
    Json(request): Json<JoinRequest>,
) -> ApiResult<StatusCode> {
    request.offer.validate()?;
    let mut rooms = rooms.lock().expect("room registry mutex poisoned");
    let now = Instant::now();
    prune(&mut rooms, now);
    let total_exchanges = rooms
        .values()
        .map(|room| room.exchanges.len())
        .sum::<usize>();
    let room = rooms.get_mut(&request.room).ok_or(ROOM_MISSING)?;
    if let Some(exchange) = room.exchanges.get(&request.peer) {
        if exchange.offer == request.offer {
            return Ok(StatusCode::OK);
        }
        return Err((StatusCode::CONFLICT, "peer already has a different offer"));
    }
    let pending = room
        .exchanges
        .values()
        .filter(|exchange| exchange.answer.is_none())
        .count();
    if pending >= MAX_PENDING_PER_ROOM
        || room.exchanges.len() >= MAX_EXCHANGES_PER_ROOM
        || total_exchanges >= MAX_EXCHANGES
    {
        return Err(AT_CAPACITY);
    }
    room.exchanges.insert(
        request.peer,
        Exchange {
            expires: now + EXCHANGE_TTL,
            offer: request.offer,
            answer: None,
        },
    );
    Ok(StatusCode::OK)
}

async fn join_poll(
    State(rooms): State<Rooms>,
    Json(request): Json<JoinPoll>,
) -> ApiResult<Json<Option<Signal>>> {
    let mut rooms = rooms.lock().expect("room registry mutex poisoned");
    prune(&mut rooms, Instant::now());
    let room = rooms.get(&request.room).ok_or(ROOM_MISSING)?;
    let exchange = room.exchanges.get(&request.peer).ok_or(PEER_MISSING)?;
    Ok(Json(exchange.answer.clone()))
}

async fn close_room(
    State(rooms): State<Rooms>,
    Json(request): Json<HostRequest>,
) -> ApiResult<StatusCode> {
    let mut rooms = rooms.lock().expect("room registry mutex poisoned");
    prune(&mut rooms, Instant::now());
    rooms
        .get(&request.room)
        .ok_or(ROOM_MISSING)?
        .authenticate_host(request.host_key)?;
    rooms.remove(&request.room);
    Ok(StatusCode::OK)
}

async fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

async fn as_plain_text(mut response: Response) -> Response {
    if response.status().is_success() {
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain; charset=utf-8"),
        );
    }
    response
}

async fn redirect_legacy_site(request: Request, next: Next) -> Response {
    let path = request.uri().path();
    if !matches!(*request.method(), Method::GET | Method::HEAD)
        || matches!(path, "/healthz" | "/v1")
        || path.starts_with("/v1/")
    {
        return next.run(request).await;
    }

    let is_legacy = |host: &str| host.eq_ignore_ascii_case(LEGACY_PUBLIC_HOST);
    let legacy_host = request.uri().host().is_some_and(is_legacy)
        || request
            .headers()
            .get(header::HOST)
            .and_then(|value| Authority::try_from(value.as_bytes()).ok())
            .is_some_and(|authority| is_legacy(authority.host()));
    if legacy_host {
        let path_and_query = request
            .uri()
            .path_and_query()
            .map_or("/", |part| part.as_str());
        return Redirect::permanent(&format!("{PUBLIC_ORIGIN}{path_and_query}")).into_response();
    }

    next.run(request).await
}

fn app(site: PathBuf, downloads: PathBuf, rooms: Rooms) -> Router {
    let api = Router::new()
        .route("/register", post(register))
        .route("/host/poll", post(host_poll))
        .route("/host/answer", post(host_answer))
        .route("/join", post(join))
        .route("/join/poll", post(join_poll))
        .route("/close", post(close_room))
        .fallback(|| async { StatusCode::NOT_FOUND })
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BYTES))
        .layer(middleware::map_response(no_store))
        .with_state(rooms);

    let downloads_page = ServeFile::new(site.join("downloads.html"));
    let download_routes = Router::new()
        .route_service("/", downloads_page.clone())
        .route(
            "/SHA256SUMS",
            get_service(ServeFile::new(downloads.join("SHA256SUMS")))
                .layer(middleware::map_response(as_plain_text)),
        )
        .route_service(
            "/{*file}",
            ServeDir::new(downloads).append_index_html_on_directories(false),
        );

    Router::new()
        .nest("/v1", api)
        .route("/healthz", get(|| async { "ok\n" }))
        .route_service("/join", ServeFile::new(site.join("join.html")))
        .route_service("/join/", ServeFile::new(site.join("join.html")))
        .route_service("/downloads", downloads_page)
        .nest("/downloads/", download_routes)
        .route(
            "/install.sh",
            get_service(ServeFile::new(site.join("install.sh")))
                .layer(middleware::map_response(as_plain_text)),
        )
        .route(
            "/install.ps1",
            get_service(ServeFile::new(site.join("install.ps1")))
                .layer(middleware::map_response(as_plain_text)),
        )
        .fallback_service(ServeDir::new(site))
        .layer(middleware::from_fn(redirect_legacy_site))
}

async fn expire_rooms(rooms: Rooms) {
    let mut interval = tokio::time::interval(EXPIRY_SWEEP_INTERVAL);
    loop {
        interval.tick().await;
        prune(
            &mut rooms.lock().expect("room registry mutex poisoned"),
            Instant::now(),
        );
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let port = match env::var("PORT") {
        Ok(port) => port.parse()?,
        Err(env::VarError::NotPresent) => 8080,
        Err(error) => return Err(error.into()),
    };
    let site = env::var_os("UNBIND_SITE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("site"));
    let downloads = env::var_os("UNBIND_DOWNLOAD_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| site.join("downloads"));
    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, port))).await?;
    eprintln!(
        "Unbind site and rendezvous listening on {}",
        listener.local_addr()?
    );
    let rooms = Rooms::default();
    tokio::spawn(expire_rooms(rooms.clone()));
    axum::serve(listener, app(site, downloads, rooms)).await?;
    Ok(())
}
