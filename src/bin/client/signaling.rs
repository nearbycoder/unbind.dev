use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use reqwest::{StatusCode, Url};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

pub const DEFAULT_SERVER: &str = "https://unbind.dev";
pub const POLL_INTERVAL: Duration = Duration::from_millis(900);
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(35);
const MAX_SDP_LEN: usize = 32 * 1024;

pub struct Invite {
    pub origin: Url,
    pub room: String,
    secret: [u8; 32],
}

#[derive(Deserialize, Serialize)]
pub struct Description {
    pub sdp: String,
    mac: String,
}

#[derive(Deserialize)]
pub struct Offer {
    pub peer: String,
    pub offer: Description,
}

#[derive(Clone)]
pub struct SignalClient {
    origin: Url,
    http: reqwest::Client,
}

#[derive(Clone, Copy)]
pub enum DescriptionKind {
    Offer,
    Answer,
}

impl Invite {
    pub fn new(server: &str) -> Result<Self> {
        let origin = Url::parse(server).context("Invalid rendezvous URL")?;
        validate_origin(&origin)?;
        ensure!(
            origin.path() == "/" && origin.fragment().is_none(),
            "Use a rendezvous origin without a path or fragment"
        );
        Ok(Self {
            origin,
            room: random_id()?,
            secret: random_bytes()?,
        })
    }

    pub fn from_link(link: &str) -> Result<Self> {
        let mut origin = Url::parse(link).context("Invalid Unbind link")?;
        validate_origin(&origin)?;
        ensure!(origin.path() == "/join", "Expected an Unbind /join link");
        let Some(fragment) = origin.fragment() else {
            bail!("The link is missing its invite token");
        };
        let mut parts = fragment.split('.');
        let (Some("v1"), Some(room), Some(secret), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            bail!("Invalid Unbind invite token");
        };
        decode_id(room)?;
        let room = room.to_owned();
        let secret = decode_id(secret)?;
        origin.set_path("/");
        origin.set_fragment(None);
        Ok(Self {
            origin,
            room,
            secret,
        })
    }

    pub fn link(&self) -> Url {
        let mut link = self.origin.clone();
        link.set_path("/join");
        link.set_fragment(Some(&format!(
            "v1.{}.{}",
            self.room,
            URL_SAFE_NO_PAD.encode(self.secret)
        )));
        link
    }

    fn authenticator(&self, peer: &str, kind: DescriptionKind, sdp: &str) -> Result<Hmac<Sha256>> {
        let room = decode_id(&self.room)?;
        let peer = decode_id(peer)?;
        ensure!(
            !sdp.is_empty() && sdp.len() <= MAX_SDP_LEN,
            "Invalid connection description size"
        );
        let mut mac =
            Hmac::<Sha256>::new_from_slice(&self.secret).expect("HMAC accepts 32-byte keys");
        mac.update(b"unbind/v1/sdp");
        mac.update(&[match kind {
            DescriptionKind::Offer => 0,
            DescriptionKind::Answer => 1,
        }]);
        mac.update(&room);
        mac.update(&peer);
        mac.update(sdp.as_bytes());
        Ok(mac)
    }

    pub fn sign(&self, peer: &str, kind: DescriptionKind, sdp: String) -> Result<Description> {
        let mac = self.authenticator(peer, kind, &sdp)?.finalize();
        Ok(Description {
            sdp,
            mac: URL_SAFE_NO_PAD.encode(mac.into_bytes()),
        })
    }

    pub fn verify(&self, peer: &str, kind: DescriptionKind, desc: &Description) -> Result<()> {
        self.authenticator(peer, kind, &desc.sdp)?
            .verify_slice(&decode_id(&desc.mac)?)
            .context("The peer could not prove it has the invite")
    }
}

fn validate_origin(url: &Url) -> Result<()> {
    let loopback = url.host_str() == Some("localhost")
        || url
            .host_str()
            .and_then(|host| {
                host.trim_matches(['[', ']'])
                    .parse::<std::net::IpAddr>()
                    .ok()
            })
            .is_some_and(|ip| ip.is_loopback());
    ensure!(
        url.host().is_some()
            && (url.scheme() == "https" || url.scheme() == "http" && loopback)
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none(),
        "Use an HTTPS rendezvous URL or local loopback HTTP"
    );
    Ok(())
}

fn random_bytes() -> Result<[u8; 32]> {
    let mut random = [0; 32];
    getrandom::fill(&mut random).map_err(|error| anyhow::anyhow!("System randomness: {error}"))?;
    Ok(random)
}

pub fn random_id() -> Result<String> {
    Ok(URL_SAFE_NO_PAD.encode(random_bytes()?))
}

fn decode_id(value: &str) -> Result<[u8; 32]> {
    ensure!(value.len() == 43, "Invalid invite or peer identifier");
    URL_SAFE_NO_PAD
        .decode(value)
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .context("Invalid invite or peer identifier")
}

impl SignalClient {
    pub fn new(origin: Url) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(6))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self { origin, http })
    }

    async fn post(&self, path: &str, body: serde_json::Value) -> Result<reqwest::Response> {
        let response = self
            .http
            .post(self.origin.join(path)?)
            .json(&body)
            .send()
            .await
            .context("Could not reach the rendezvous service")?;

        match response.status() {
            StatusCode::OK | StatusCode::NOT_FOUND => Ok(response),
            StatusCode::CONFLICT => bail!("Rendezvous request conflicted with an existing session"),
            StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE => {
                bail!("The rendezvous service is busy. Try again shortly.")
            }
            status => bail!("The rendezvous service returned HTTP {status}"),
        }
    }

    pub async fn register(&self, room: &str, key: &str) -> Result<()> {
        let response = self
            .post(
                "v1/register",
                serde_json::json!({"room": room, "host_key": key}),
            )
            .await?;
        ensure!(
            response.status() == StatusCode::OK,
            "Rendezvous API unavailable"
        );
        Ok(())
    }

    pub async fn host_poll(&self, room: &str, key: &str) -> Result<Option<Vec<Offer>>> {
        let response = self
            .post(
                "v1/host/poll",
                serde_json::json!({"room": room, "host_key": key}),
            )
            .await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        Ok(Some(
            response
                .json()
                .await
                .context("Invalid offers from rendezvous")?,
        ))
    }

    pub async fn answer(
        &self,
        room: &str,
        key: &str,
        peer: &str,
        answer: Description,
    ) -> Result<()> {
        let response = self
            .post(
                "v1/host/answer",
                serde_json::json!({
                    "room": room, "host_key": key, "peer": peer, "answer": answer
                }),
            )
            .await?;
        ensure!(
            response.status() == StatusCode::OK,
            "The join request expired"
        );
        Ok(())
    }

    pub async fn join(&self, room: &str, peer: &str, offer: Description) -> Result<()> {
        let response = self
            .post(
                "v1/join",
                serde_json::json!({"room": room, "peer": peer, "offer": offer}),
            )
            .await?;
        ensure!(
            response.status() == StatusCode::OK,
            "The share is no longer available"
        );
        Ok(())
    }

    pub async fn join_poll(&self, room: &str, peer: &str) -> Result<Option<Description>> {
        let response = self
            .post(
                "v1/join/poll",
                serde_json::json!({"room": room, "peer": peer}),
            )
            .await?;
        ensure!(
            response.status() == StatusCode::OK,
            "The share is no longer available"
        );
        response
            .json()
            .await
            .context("Invalid answer from rendezvous")
    }

    pub async fn close(&self, room: &str, key: &str) -> Result<()> {
        self.post(
            "v1/close",
            serde_json::json!({"room": room, "host_key": key}),
        )
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invite_round_trips_and_authenticates_descriptions() -> Result<()> {
        let invite = Invite::new(DEFAULT_SERVER)?;
        let joined = Invite::from_link(invite.link().as_str())?;
        let peer = random_id()?;
        let mut signed = invite.sign(&peer, DescriptionKind::Offer, "sdp".into())?;
        joined.verify(&peer, DescriptionKind::Offer, &signed)?;
        assert!(
            joined
                .verify(&peer, DescriptionKind::Answer, &signed)
                .is_err()
        );
        assert!(
            joined
                .verify(&random_id()?, DescriptionKind::Offer, &signed)
                .is_err()
        );
        signed.sdp.push('!');
        assert!(
            joined
                .verify(&peer, DescriptionKind::Offer, &signed)
                .is_err()
        );
        assert!(joined.origin.fragment().is_none());
        assert!(Invite::new("http://example.com").is_err());
        assert!(Invite::new("http://[::1]:8080").is_ok());
        Ok(())
    }
}
