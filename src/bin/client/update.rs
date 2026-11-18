use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use ed25519_dalek::{Signature, VerifyingKey};
use reqwest::{Client, Url};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tempfile::{Builder, NamedTempFile};
use tokio::io::AsyncWriteExt;

const RELEASES: &str = "https://unbind.dev/downloads/releases/";
const PUBLIC_KEY: &str = include_str!("../../../release/public-key.hex");
const MANIFEST_LIMIT: usize = 16 * 1024;
const BINARY_LIMIT: u64 = 128 * 1024 * 1024;
const BACKUP_MAGIC: &[u8; 17] = b"UNBIND-BACKUP-v1\n";
const DIGEST_LENGTH: usize = 64;

#[derive(Deserialize)]
struct Manifest {
    schema: u8,
    version: String,
    assets: BTreeMap<String, Asset>,
}

#[derive(Deserialize)]
struct Asset {
    sha256: String,
    size: u64,
}

struct Releases {
    client: Client,
    base: Url,
    key: VerifyingKey,
}

impl Releases {
    fn new() -> Result<Self> {
        Ok(Self {
            client: Client::builder()
                .https_only(true)
                .connect_timeout(Duration::from_secs(12))
                .timeout(Duration::from_secs(120))
                .build()?,
            base: Url::parse(RELEASES)?,
            key: public_key(PUBLIC_KEY)?,
        })
    }

    async fn fetch(&self, version: &str, file: &str) -> Result<reqwest::Response> {
        let url = self.base.join(&format!("{version}/{file}"))?;
        self.client
            .get(url)
            .send()
            .await
            .context("Could not reach the Unbind release service")?
            .error_for_status()
            .with_context(|| format!("Could not fetch {file} for release {version}"))
    }

    async fn bytes(&self, version: &str, file: &str, limit: usize) -> Result<Vec<u8>> {
        let mut response = self.fetch(version, file).await?;
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            ensure!(chunk.len() <= limit - bytes.len(), "{file} is too large");
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }

    async fn manifest(&self, requested: Option<&str>) -> Result<Manifest> {
        let version = match requested {
            Some(version) => version.strip_prefix('v').unwrap_or(version).to_owned(),
            None => {
                let bytes = self.bytes("latest", "release.json", MANIFEST_LIMIT).await?;
                serde_json::from_slice::<Manifest>(&bytes)
                    .context("Invalid latest release metadata")?
                    .version
            }
        };
        parse_version(&version)?;
        let bytes = self.bytes(&version, "release.json", MANIFEST_LIMIT).await?;
        let signature = self.bytes(&version, "release.json.sig", 64).await?;
        verify_manifest(&self.key, &version, &bytes, &signature)
    }

    async fn download(
        &self,
        version: &str,
        name: &str,
        asset: &Asset,
        dir: &Path,
    ) -> Result<NamedTempFile> {
        let mut response = self.fetch(version, name).await?;
        let temp = Builder::new().prefix(".unbind-new-").tempfile_in(dir)?;
        let mut file = tokio::fs::File::from_std(temp.reopen()?);
        let mut digest = Sha256::new();
        let mut size = 0;
        while let Some(chunk) = response.chunk().await? {
            size += chunk.len() as u64;
            ensure!(
                size <= asset.size,
                "The release binary is larger than its signed size"
            );
            digest.update(&chunk);
            file.write_all(&chunk).await?;
        }
        ensure!(
            size == asset.size,
            "The release binary does not match its signed size"
        );
        ensure!(
            format!("{:x}", digest.finalize()) == asset.sha256,
            "The release binary does not match its signed SHA-256 checksum"
        );
        file.sync_all().await?;
        Ok(temp)
    }
}

pub async fn run(check: bool, version: Option<&str>) -> Result<()> {
    let releases = Releases::new()?;
    let manifest = releases.manifest(version).await?;
    let name = native_asset()?;
    let asset = manifest
        .assets
        .get(name)
        .context("The signed release lacks a binary for this platform")?;
    let installed =
        std::env::current_exe().context("Could not locate the running Unbind executable")?;
    let current = parse_version(env!("CARGO_PKG_VERSION"))?;
    let target = parse_version(&manifest.version)?;
    if version.is_none() && target < current {
        println!(
            "Unbind {} is newer than the published release {}.",
            env!("CARGO_PKG_VERSION"),
            manifest.version
        );
        return Ok(());
    }

    if check {
        let installed_hash = hash_file(&installed)?;
        if installed_hash == asset.sha256 {
            println!("Unbind {} matches the signed release.", manifest.version);
        } else {
            println!("Signed Unbind {} is available.", manifest.version);
        }
        return Ok(());
    }

    let _lock = update_lock(&installed)?;
    let installed_hash = hash_file(&installed)?;
    recover_backup(&installed, &installed_hash)?;
    if installed_hash == asset.sha256 {
        println!(
            "Unbind {} already matches the signed release.",
            manifest.version
        );
        return Ok(());
    }

    let directory = installed
        .parent()
        .context("Unbind installation has no parent directory")?;
    let stage = releases
        .download(&manifest.version, name, asset, directory)
        .await?;
    replace(&installed, &installed_hash, stage.path(), &asset.sha256)?;
    println!(
        "Installed signed Unbind {}. Run unbind rollback to restore the previous executable.",
        manifest.version
    );
    Ok(())
}

pub async fn rollback() -> Result<()> {
    tokio::task::spawn_blocking(|| {
        let installed =
            std::env::current_exe().context("Could not locate the running Unbind executable")?;
        let _lock = update_lock(&installed)?;
        let installed_hash = hash_file(&installed)?;
        recover_backup(&installed, &installed_hash)?;
        let backup = companion(&installed, ".previous");
        let mut file = File::open(&backup).context("No previous Unbind executable is saved")?;
        let (previous_hash, replacement_hash) = read_backup_header(&mut file)?;
        ensure!(
            replacement_hash == installed_hash,
            "This backup belongs to a different installed executable"
        );

        let directory = installed
            .parent()
            .context("Unbind installation has no parent directory")?;
        let mut stage = Builder::new()
            .prefix(".unbind-restore-")
            .tempfile_in(directory)?;
        let size = io::copy(&mut file.take(BINARY_LIMIT + 1), &mut stage)?;
        ensure!(size <= BINARY_LIMIT, "The backup binary is too large");
        stage.as_file().sync_all()?;
        ensure!(
            hash_file(stage.path())? == previous_hash,
            "The previous executable failed its SHA-256 check"
        );
        replace(&installed, &installed_hash, stage.path(), &previous_hash)?;
        println!("Restored the previous Unbind executable.");
        Ok(())
    })
    .await?
}

fn native_asset() -> Result<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Ok("unbind-linux-amd64"),
        ("linux", "aarch64") => Ok("unbind-linux-arm64"),
        ("macos", "x86_64") => Ok("unbind-darwin-amd64"),
        ("macos", "aarch64") => Ok("unbind-darwin-arm64"),
        ("windows", "x86_64") => Ok("unbind-windows-amd64.exe"),
        ("windows", "aarch64") => Ok("unbind-windows-arm64.exe"),
        (os, arch) => bail!("Unbind releases do not include {os}/{arch}"),
    }
}

fn parse_version(version: &str) -> Result<[u64; 3]> {
    let mut parts = version.split('.');
    let mut parsed = [0; 3];
    for number in &mut parsed {
        let part = parts
            .next()
            .context("Expected a stable release version like 0.2.0")?;
        ensure!(
            !part.is_empty()
                && part.bytes().all(|byte| byte.is_ascii_digit())
                && (part.len() == 1 || !part.starts_with('0')),
            "Expected a stable release version like 0.2.0"
        );
        *number = part.parse().context("Invalid release version")?;
    }
    ensure!(
        parts.next().is_none(),
        "Expected a stable release version like 0.2.0"
    );
    Ok(parsed)
}

fn valid_digest(value: &str) -> bool {
    value.len() == DIGEST_LENGTH
        && value
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

fn public_key(hex: &str) -> Result<VerifyingKey> {
    let hex = hex.trim();
    ensure!(
        valid_digest(hex),
        "Invalid embedded Unbind release public key"
    );
    let mut bytes = [0; 32];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16)?;
    }
    VerifyingKey::from_bytes(&bytes).context("Invalid embedded Unbind release public key")
}

fn verify_manifest(
    key: &VerifyingKey,
    requested: &str,
    bytes: &[u8],
    signature: &[u8],
) -> Result<Manifest> {
    let signature = Signature::from_slice(signature).context("Invalid release signature length")?;
    key.verify_strict(bytes, &signature)
        .context("The release manifest failed Ed25519 verification")?;
    let manifest: Manifest =
        serde_json::from_slice(bytes).context("Invalid signed release metadata")?;
    ensure!(
        manifest.schema == 1,
        "This signing manifest requires a newer Unbind"
    );
    ensure!(
        manifest.version == requested,
        "The signed release does not match the requested version"
    );
    for asset in manifest.assets.values() {
        ensure!(
            valid_digest(&asset.sha256) && (1..=BINARY_LIMIT).contains(&asset.size),
            "Invalid signed asset metadata"
        );
    }
    Ok(manifest)
}

fn hash_file(path: &Path) -> Result<String> {
    let mut digest = Sha256::new();
    let mut file =
        File::open(path).with_context(|| format!("Could not read {}", path.display()))?;
    let length = io::copy(&mut Read::take(&mut file, BINARY_LIMIT + 1), &mut digest)?;
    ensure!(length <= BINARY_LIMIT, "The executable is too large");
    Ok(format!("{:x}", digest.finalize()))
}

fn companion(installed: &Path, suffix: &str) -> PathBuf {
    let mut name = OsString::from(".");
    name.push(installed.file_name().unwrap_or_default());
    name.push(suffix);
    installed.with_file_name(name)
}

fn update_lock(installed: &Path) -> Result<File> {
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(companion(installed, ".update.lock"))
        .context("Cannot write in the install directory. Use your package manager for managed installations")?;
    lock.try_lock()
        .context("Another Unbind update is already running")?;
    Ok(lock)
}

fn read_backup_header(file: &mut File) -> Result<(String, String)> {
    let mut magic = [0u8; BACKUP_MAGIC.len()];
    let mut previous = [0u8; DIGEST_LENGTH];
    let mut replacement = [0u8; DIGEST_LENGTH];
    file.read_exact(&mut magic)?;
    file.read_exact(&mut previous)?;
    file.read_exact(&mut replacement)?;
    let previous = String::from_utf8(previous.to_vec())?;
    let replacement = String::from_utf8(replacement.to_vec())?;
    ensure!(
        &magic == BACKUP_MAGIC && valid_digest(&previous) && valid_digest(&replacement),
        "Invalid Unbind rollback backup"
    );
    Ok((previous, replacement))
}

fn recover_backup(installed: &Path, installed_hash: &str) -> Result<()> {
    let pending = companion(installed, ".pending");
    let mut file = match File::open(&pending) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let (previous, replacement) = read_backup_header(&mut file)?;
    drop(file);
    if replacement == installed_hash {
        fs::rename(pending, companion(installed, ".previous"))?;
    } else if previous == installed_hash {
        fs::remove_file(pending)?;
    } else {
        bail!(
            "An unfinished Unbind update could not be recovered from {}",
            pending.display()
        );
    }
    Ok(())
}

fn replace(
    installed: &Path,
    installed_hash: &str,
    replacement: &Path,
    replacement_hash: &str,
) -> Result<()> {
    let directory = installed
        .parent()
        .context("Unbind installation has no parent directory")?;
    let mut backup = Builder::new()
        .prefix(".unbind-backup-")
        .tempfile_in(directory)?;
    backup.write_all(BACKUP_MAGIC)?;
    backup.write_all(installed_hash.as_bytes())?;
    backup.write_all(replacement_hash.as_bytes())?;
    let mut original = File::open(installed)?;
    let size = io::copy(
        &mut Read::take(&mut original, BINARY_LIMIT + 1),
        &mut backup,
    )?;
    ensure!(
        size <= BINARY_LIMIT,
        "The installed binary is too large to back up"
    );
    drop(original);
    backup.flush()?;
    backup.seek(SeekFrom::Start(
        (BACKUP_MAGIC.len() + DIGEST_LENGTH * 2) as u64,
    ))?;
    let mut digest = Sha256::new();
    io::copy(&mut backup, &mut digest)?;
    ensure!(
        format!("{:x}", digest.finalize()) == installed_hash,
        "The executable changed during the update"
    );
    backup.as_file().sync_all()?;
    backup
        .persist(companion(installed, ".pending"))
        .context("Could not save the previous executable")?;
    fs::set_permissions(replacement, fs::metadata(installed)?.permissions())?;
    self_replace::self_replace(replacement)
        .context("Could not replace the running Unbind executable")?;
    recover_backup(installed, replacement_hash)
        .context("Unbind was replaced, but its previous executable needs recovery")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    #[test]
    fn verifies_signature_and_requested_version() -> Result<()> {
        let key = SigningKey::from_bytes(&[8; 32]);
        let bytes = br#"{"schema":1,"version":"1.2.3","assets":{"unbind-linux-amd64":{"sha256":"abababababababababababababababababababababababababababababababab","size":42}}}"#;
        let signature = key.sign(bytes).to_bytes();
        let manifest = verify_manifest(&key.verifying_key(), "1.2.3", bytes, &signature)?;
        assert_eq!(manifest.assets["unbind-linux-amd64"].size, 42);
        assert!(verify_manifest(&key.verifying_key(), "1.2.4", bytes, &signature).is_err());

        let mut altered = bytes.to_vec();
        altered[0] ^= 1;
        assert!(verify_manifest(&key.verifying_key(), "1.2.3", &altered, &signature).is_err());
        assert!(
            verify_manifest(
                &SigningKey::from_bytes(&[7; 32]).verifying_key(),
                "1.2.3",
                bytes,
                &signature
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn accepts_only_canonical_stable_versions() {
        assert_eq!(parse_version("0.12.3").unwrap(), [0, 12, 3]);
        for input in [
            "latest",
            "1.2",
            "1.2.3.4",
            "01.2.3",
            "1.+2.3",
            "1.2.3-rc1",
            "../1.2.3",
        ] {
            assert!(parse_version(input).is_err(), "{input} unexpectedly parsed");
        }
    }

    #[test]
    fn recovers_backup_from_interrupted_update() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let installed = directory.path().join("unbind");
        let previous = "a".repeat(DIGEST_LENGTH);
        let replacement = "b".repeat(DIGEST_LENGTH);
        let pending = companion(&installed, ".pending");
        let mut backup = BACKUP_MAGIC.to_vec();
        backup.extend_from_slice(previous.as_bytes());
        backup.extend_from_slice(replacement.as_bytes());
        backup.extend_from_slice(b"previous binary");
        fs::write(&pending, &backup)?;
        recover_backup(&installed, &previous)?;
        assert!(!pending.exists());
        assert!(!companion(&installed, ".previous").exists());

        fs::write(&pending, &backup)?;
        assert!(recover_backup(&installed, &"c".repeat(DIGEST_LENGTH)).is_err());
        recover_backup(&installed, &replacement)?;
        assert!(!pending.exists());
        assert_eq!(fs::read(companion(&installed, ".previous"))?, backup);
        Ok(())
    }
}
