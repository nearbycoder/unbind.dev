#[path = "shared/rendezvous.rs"]
mod rendezvous;

use std::{
    env,
    net::{Ipv4Addr, SocketAddr},
    path::PathBuf,
};

use axum::{
    Router,
    extract::{Path, Request},
    http::{HeaderValue, Method, StatusCode, header, uri::Authority},
    middleware::{self, Next},
    response::{IntoResponse, Redirect, Response},
    routing::{get, get_service},
};
use tokio::net::TcpListener;
use tower_http::services::{ServeDir, ServeFile};

const PUBLIC_ORIGIN: &str = "https://unbind.dev";
const LEGACY_PUBLIC_HOST: &str = "unbind-web-production.up.railway.app";

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

async fn release_download(Path((version, asset)): Path<(String, String)>) -> Response {
    const RELEASES: &str = "https://github.com/nearbycoder/unbind.dev/releases";
    let version_path = if version == "latest" {
        "latest/download".to_owned()
    } else {
        let mut parts = version.split('.');
        if parts.clone().count() != 3
            || !parts.all(|part| !part.is_empty() && part.bytes().all(|c| c.is_ascii_digit()))
        {
            return StatusCode::NOT_FOUND.into_response();
        }
        format!("download/v{version}")
    };
    if asset.len() > 100
        || !asset
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-' | b'_'))
        || !(asset.starts_with("unbind-")
            || matches!(
                asset.as_str(),
                "SHA256SUMS"
                    | "release.json"
                    | "release.json.sig"
                    | "unbind.rb"
                    | "unbind.json"
                    | "nearbycoder.Unbind.yaml"
            ))
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    Redirect::temporary(&format!("{RELEASES}/{version_path}/{asset}")).into_response()
}

fn app(site: PathBuf, downloads: PathBuf, api: Router) -> Router {
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
        .route(
            "/downloads/releases/",
            get(|| async {
                Redirect::temporary("https://github.com/nearbycoder/unbind.dev/releases")
            }),
        )
        .route(
            "/downloads/releases/{version}/{asset}",
            get(release_download),
        )
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
    let (api, cleanup) = rendezvous::router();
    tokio::select! {
        result = axum::serve(listener, app(site, downloads, api)) => result?,
        _ = cleanup => {}
    }
    Ok(())
}
