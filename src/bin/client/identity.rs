use std::{
    fs,
    io::{ErrorKind, Write},
};

use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use sha2::{Digest, Sha256};

use crate::{
    management::{create_private_file, private_dir, state_dir},
    signaling::random_id,
};

const ATTRIBUTE: &str = "a=x-unbind-device:";
const ENTROPY_FILE: &str = "device.ed25519";

fn entropy() -> Result<[u8; 32]> {
    let path = state_dir()?.join(ENTROPY_FILE);
    if let Ok(existing) = fs::read(&path) {
        return existing
            .try_into()
            .ok()
            .context("Invalid Unbind device key");
    }

    let mut key = [0; 32];
    getrandom::fill(&mut key).map_err(|error| anyhow::anyhow!("System randomness: {error}"))?;
    let temporary = path.with_file_name(format!(".key-{}", random_id()?));
    let result = (|| -> Result<[u8; 32]> {
        create_private_file(&temporary)?.write_all(&key)?;
        match fs::hard_link(&temporary, &path) {
            Ok(()) => Ok(key),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => fs::read(&path)?
                .try_into()
                .ok()
                .context("Invalid Unbind device key"),
            Err(error) => Err(error).context("Save Unbind device key"),
        }
    })();
    let _ = fs::remove_file(&temporary);
    result
}

fn message(room: &str, peer: &str, sdp: &str) -> Vec<u8> {
    let mut bytes = b"unbind/device/v1\0".to_vec();
    bytes.extend(room.as_bytes());
    bytes.push(0);
    bytes.extend(peer.as_bytes());
    bytes.push(0);
    bytes.extend(sdp.as_bytes());
    bytes
}

pub fn device_id() -> Result<String> {
    Ok(fingerprint(
        &SigningKey::from_bytes(&entropy()?).verifying_key(),
    ))
}

pub fn sign(room: &str, peer: &str, sdp: String) -> Result<String> {
    Ok(sign_with(
        &SigningKey::from_bytes(&entropy()?),
        room,
        peer,
        sdp,
    ))
}

fn sign_with(key: &SigningKey, room: &str, peer: &str, mut sdp: String) -> String {
    if !sdp.ends_with("\r\n") {
        sdp.push_str("\r\n");
    }
    let signature = key.sign(&message(room, peer, &sdp));
    format!(
        "{sdp}{ATTRIBUTE}{}.{}\r\n",
        URL_SAFE_NO_PAD.encode(key.verifying_key().to_bytes()),
        URL_SAFE_NO_PAD.encode(signature.to_bytes()),
    )
}

fn fingerprint(key: &VerifyingKey) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(key.as_bytes()))
}

pub fn verify(room: &str, peer: &str, offer: &str) -> Result<(String, Option<String>)> {
    let Some(attribute) = offer.lines().find(|line| line.starts_with(ATTRIBUTE)) else {
        return Ok((offer.to_owned(), None));
    };
    let suffix = format!("{attribute}\r\n");
    let original = offer
        .strip_suffix(&suffix)
        .context("Invalid device proof placement")?;
    ensure!(
        !original.lines().any(|line| line.starts_with(ATTRIBUTE)),
        "Multiple device proofs in the offer"
    );
    let (public, signature) = attribute[ATTRIBUTE.len()..]
        .split_once('.')
        .context("Invalid device proof")?;
    let decoded = URL_SAFE_NO_PAD.decode(public)?;
    let public =
        VerifyingKey::from_bytes(&decoded.try_into().ok().context("Invalid device key size")?)?;
    let signature = Signature::from_slice(&URL_SAFE_NO_PAD.decode(signature)?)?;
    public
        .verify_strict(&message(room, peer, original), &signature)
        .context("Invalid device signature")?;
    Ok((original.to_owned(), Some(fingerprint(&public))))
}

pub fn is_trusted(fingerprint: &str) -> bool {
    trusted_path(fingerprint).is_ok_and(|path| path.is_file())
}

pub fn remember(fingerprint: &str) -> Result<()> {
    let path = trusted_path(fingerprint)?;
    match create_private_file(&path) {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == ErrorKind::AlreadyExists && path.is_file() => Ok(()),
        Err(error) => Err(error).context("Remember device"),
    }
}

fn trusted_path(fingerprint: &str) -> Result<std::path::PathBuf> {
    if fingerprint.len() != 43
        || !fingerprint
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        bail!("Invalid device fingerprint");
    }
    let directory = state_dir()?.join("trusted-devices");
    private_dir(&directory)?;
    Ok(directory.join(fingerprint))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_proof_binds_invite_attempt_and_offer() -> Result<()> {
        let key = SigningKey::from_bytes(&[39; 32]);
        let sdp = "a=ice-ufrag:example\r\n";
        let signed = sign_with(&key, "room", "peer", sdp.into());
        assert_eq!(
            verify("room", "peer", &signed)?,
            (sdp.into(), Some(fingerprint(&key.verifying_key()))),
        );
        assert!(verify("room", "other-peer", &signed).is_err());
        assert!(verify("other-room", "peer", &signed).is_err());
        assert!(verify("room", "peer", &signed.replace("example", "changed")).is_err());
        Ok(())
    }
}
