use std::collections::BTreeSet;
use std::fs::File;
use std::io::Read;
use std::net::SocketAddr;
use std::path::Path;

use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    pub bind: SocketAddr,
    pub s2: S2Config,
    pub tokens: Vec<TokenRecord>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct S2Config {
    pub access_token: String,
    pub account_endpoint: String,
    pub basin_endpoint: String,
    pub basin: String,
    pub stream_prefix: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenRecord {
    pub bearer: String,
    pub actor_id: String,
    pub workspaces: BTreeSet<String>,
}

pub fn load_config(path: &Path) -> Result<ServerConfig, String> {
    let mut file = open_private_config(path)?;
    let mut contents = String::new();
    file.read_to_string(&mut contents)
        .map_err(|error| format!("cannot read config: {error}"))?;
    let config: ServerConfig =
        serde_json::from_str(&contents).map_err(|error| format!("invalid config JSON: {error}"))?;
    config.validate()?;
    Ok(config)
}

impl ServerConfig {
    fn validate(&self) -> Result<(), String> {
        if self.s2.access_token.is_empty() {
            return Err("s2.access_token must not be empty".to_string());
        }
        for (name, value) in [
            ("s2.account_endpoint", &self.s2.account_endpoint),
            ("s2.basin_endpoint", &self.s2.basin_endpoint),
            ("s2.basin", &self.s2.basin),
            ("s2.stream_prefix", &self.s2.stream_prefix),
        ] {
            if value.trim().is_empty() {
                return Err(format!("{name} must not be empty"));
            }
        }
        if self.tokens.is_empty() {
            return Err("tokens must contain at least one record".to_string());
        }

        let mut bearers = BTreeSet::new();
        let mut actors = BTreeSet::new();
        for token in &self.tokens {
            if token.bearer.is_empty()
                || token
                    .bearer
                    .bytes()
                    .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
            {
                return Err("each bearer must be a non-empty token without whitespace".to_string());
            }
            if !bearers.insert(token.bearer.as_str()) {
                return Err("bearer values must be unique".to_string());
            }
            validate_identifier("actor_id", &token.actor_id)?;
            if !actors.insert(token.actor_id.as_str()) {
                return Err("actor_id values must be distinct".to_string());
            }
            if token.workspaces.is_empty() {
                return Err(format!(
                    "actor {} must belong to at least one workspace",
                    token.actor_id
                ));
            }
            for workspace in &token.workspaces {
                validate_identifier("workspace", workspace)?;
                format!("{}{}", self.s2.stream_prefix, workspace)
                    .parse::<s2_sdk::types::StreamName>()
                    .map_err(|_| {
                        format!(
                            "s2.stream_prefix and workspace {workspace} form an invalid S2 stream name"
                        )
                    })?;
            }
        }
        Ok(())
    }
}

fn validate_identifier(label: &str, value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(format!(
            "{label} must be 1..=128 ASCII letters, digits, '.', '_' or '-'"
        ));
    }
    Ok(())
}

fn open_private_config(path: &Path) -> Result<File, String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let path_metadata = std::fs::symlink_metadata(path)
            .map_err(|error| format!("cannot inspect config: {error}"))?;
        if !path_metadata.file_type().is_file() {
            return Err("config must be a regular file, not a symlink".to_string());
        }
        if path_metadata.permissions().mode() & 0o7777 != 0o600 {
            return Err("config permissions must be exactly 0600".to_string());
        }
        let file = File::open(path).map_err(|error| format!("cannot open config: {error}"))?;
        let opened = file
            .metadata()
            .map_err(|error| format!("cannot inspect opened config: {error}"))?;
        if opened.dev() != path_metadata.dev() || opened.ino() != path_metadata.ino() {
            return Err("config changed while it was being opened".to_string());
        }
        if opened.permissions().mode() & 0o7777 != 0o600 {
            return Err("opened config permissions must be exactly 0600".to_string());
        }
        Ok(file)
    }

    #[cfg(not(unix))]
    {
        let metadata = std::fs::symlink_metadata(path)
            .map_err(|error| format!("cannot inspect config: {error}"))?;
        if !metadata.file_type().is_file() {
            return Err("config must be a regular file".to_string());
        }
        File::open(path).map_err(|error| format!("cannot open config: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_json(tokens: &str) -> String {
        format!(
            r#"{{
                "bind":"127.0.0.1:7800",
                "s2":{{
                    "access_token":"s2-secret",
                    "account_endpoint":"http://127.0.0.1:8080",
                    "basin_endpoint":"http://127.0.0.1:8080",
                    "basin":"tsk",
                    "stream_prefix":"workspace-"
                }},
                "tokens":{tokens}
            }}"#
        )
    }

    #[test]
    fn actor_ids_must_be_distinct() {
        let config: ServerConfig = serde_json::from_str(&valid_json(
            r#"[
                {"bearer":"one","actor_id":"alice","workspaces":["demo"]},
                {"bearer":"two","actor_id":"alice","workspaces":["demo"]}
            ]"#,
        ))
        .expect("shape");
        assert!(config.validate().unwrap_err().contains("distinct"));
    }

    #[cfg(unix)]
    #[test]
    fn config_requires_exact_0600_permissions() {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        use std::sync::atomic::{AtomicU64, Ordering};

        static SEQ: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "tsk-sync-config-{}-{}.json",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true).mode(0o600);
        let mut file = options.open(&path).expect("create config");
        use std::io::Write;
        file.write_all(
            valid_json(r#"[{"bearer":"one","actor_id":"alice","workspaces":["demo"]}]"#).as_bytes(),
        )
        .expect("write");
        drop(file);

        load_config(&path).expect("0600 accepted");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).expect("chmod");
        let error = match load_config(&path) {
            Ok(_) => panic!("0640 config must be rejected"),
            Err(error) => error,
        };
        assert!(error.contains("0600"));
        let _ = std::fs::remove_file(path);
    }
}
