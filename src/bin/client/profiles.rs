use std::{collections::BTreeMap, fs, path::Path};

use anyhow::{Context, Result, ensure};
use serde::Deserialize;

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Profile {
    pub target: Option<String>,
    pub tcp: BTreeMap<String, String>,
    pub udp: BTreeMap<String, String>,
    pub server: Option<String>,
    pub expires_in: Option<u64>,
    pub single_use: bool,
    pub approve: bool,
    pub lan: bool,
    pub qr: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Configuration {
    shares: BTreeMap<String, Profile>,
}

pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 40
        && !name.bytes().all(|byte| byte.is_ascii_digit())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

pub fn load(path: &Path, name: &str) -> Result<Profile> {
    ensure!(valid_name(name), "Invalid profile name");
    let text = fs::read_to_string(path).with_context(|| format!("Read {}", path.display()))?;
    let mut configuration: Configuration =
        toml::from_str(&text).with_context(|| format!("Parse {}", path.display()))?;
    configuration
        .shares
        .remove(name)
        .with_context(|| format!("No share profile '{name}' in {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiles_parse_named_ports_and_access_policy() -> Result<()> {
        let config: Configuration = toml::from_str(
            r#"
[shares.dev]
target = "3000"
expires_in = 600
single_use = true

[shares.dev.udp]
dns = "127.0.0.1:5353"
"#,
        )?;
        assert_eq!(config.shares["dev"].target.as_deref(), Some("3000"));
        assert_eq!(config.shares["dev"].expires_in, Some(600));
        assert!(config.shares["dev"].single_use);
        assert_eq!(config.shares["dev"].udp["dns"], "127.0.0.1:5353");
        Ok(())
    }
}
