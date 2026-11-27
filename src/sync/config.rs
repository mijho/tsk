//! The server config file: where it listens and where its database lives. Strict: every key
//! is known and required, so a typo refuses instead of silently using a default.

use std::fmt;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use toml_edit::DocumentMut;

/// The `tsk-sync` config.
///
/// ```toml
/// [server]
/// listen = "127.0.0.1:7878"
/// database = "/var/lib/tsk-sync/tsk-sync.db"
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub listen: SocketAddr,
    pub database: PathBuf,
}

#[derive(Debug)]
pub enum ConfigError {
    Read(PathBuf, std::io::Error),
    Toml(Box<toml_edit::TomlError>),
    /// A key, table, or value the config does not allow, with what is wrong.
    Invalid(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(path, error) => write!(f, "cannot read {}: {error}", path.display()),
            Self::Toml(error) => write!(f, "config is not valid TOML: {error}"),
            Self::Invalid(reason) => write!(f, "invalid config: {reason}"),
        }
    }
}

impl std::error::Error for ConfigError {}

impl Config {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text =
            std::fs::read_to_string(path).map_err(|error| ConfigError::Read(path.into(), error))?;
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let invalid = |reason: String| ConfigError::Invalid(reason);
        let document: DocumentMut = text
            .parse()
            .map_err(|error| ConfigError::Toml(Box::new(error)))?;
        if let Some((key, _)) = document.iter().find(|(key, _)| *key != "server") {
            return Err(invalid(format!("unknown table or key `{key}`")));
        }
        let server = document
            .get("server")
            .and_then(|item| item.as_table_like())
            .ok_or_else(|| invalid("missing table [server]".into()))?;
        if let Some((key, _)) = server
            .iter()
            .find(|(key, _)| !matches!(*key, "listen" | "database"))
        {
            return Err(invalid(format!("unknown key `server.{key}`")));
        }
        let text_value = |key: &str| {
            server
                .get(key)
                .ok_or_else(|| invalid(format!("missing key `server.{key}`")))?
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| invalid(format!("`server.{key}` must be a string")))
        };
        let listen = text_value("listen")?;
        let listen = listen
            .parse()
            .map_err(|_| invalid(format!("`server.listen` is not host:port: {listen}")))?;
        let database = PathBuf::from(text_value("database")?);
        if !database.is_absolute() {
            return Err(invalid("`server.database` must be an absolute path".into()));
        }
        Ok(Self { listen, database })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_complete_config_parses() {
        let config = Config::parse(
            "[server]\nlisten = \"127.0.0.1:7878\"\ndatabase = \"/var/lib/tsk-sync/db\"\n",
        )
        .expect("config");
        assert_eq!(config.listen, "127.0.0.1:7878".parse().unwrap());
        assert_eq!(config.database, PathBuf::from("/var/lib/tsk-sync/db"));
    }

    #[test]
    fn unknown_missing_and_malformed_keys_refuse() {
        let refused = |text: &str| Config::parse(text).expect_err(text).to_string();
        let base = "listen = \"127.0.0.1:1\"\ndatabase = \"/db\"\n";
        assert!(refused(&format!("[server]\n{base}token = \"x\"\n")).contains("server.token"));
        assert!(refused(&format!("[server]\n{base}[extra]\n")).contains("`extra`"));
        assert!(refused("[server]\nlisten = \"127.0.0.1:1\"\n").contains("server.database"));
        assert!(
            refused("[server]\nlisten = \"nowhere\"\ndatabase = \"/db\"\n").contains("host:port")
        );
        assert!(
            refused("[server]\nlisten = \"127.0.0.1:1\"\ndatabase = \"db\"\n").contains("absolute")
        );
        assert!(refused("[server]\nlisten = 7878\ndatabase = \"/db\"\n").contains("string"));
        assert!(refused("").contains("[server]"));
    }
}
