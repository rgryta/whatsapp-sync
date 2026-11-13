//! Typed configuration for the MCP process.
//!
//! The MCP process reads the archive and nothing else, so it has no WhatsApp setting at
//! all: no credential is merely unused here, it is absent. That is the enforcement the
//! missing `whatsapp-client` dependency gives at the manifest level, restated where the
//! secrets live. `WHATSAPP_MCP_ALLOWED_HOSTS` has to be set — see
//! [`DEFAULT_ALLOWED_HOSTS`].

use std::env;
use std::fmt;

/// A value that must not reach a log line. `Debug` prints the redaction, so a
/// `{settings:?}` anywhere cannot leak the DSN's password — the same guarantee
/// `pydantic.SecretStr` gives the Python side.
#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(**********)")
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{0} is required")]
    Missing(&'static str),
    #[error("{name} is not a number: {value:?}")]
    NotANumber { name: &'static str, value: String },
    #[error("port must be between 1 and 65535")]
    PortOutOfRange,
}

/// rmcp's own default, repeated here so the allowlist is never empty.
///
/// An empty list means "allow every `Host`" to rmcp, so the parse below falls back to
/// this rather than to nothing: a compose file that forgets
/// `WHATSAPP_MCP_ALLOWED_HOSTS` should fail closed and be obvious, not fail open and
/// be invisible. The consequence is that there is deliberately no way to spell "allow
/// all" through the environment.
pub const DEFAULT_ALLOWED_HOSTS: [&str; 3] = ["localhost", "127.0.0.1", "::1"];

#[derive(Debug, Clone)]
pub struct McpSettings {
    pub dsn: Secret,
    pub host: String,
    pub port: u16,
    pub log_level: String,
    /// `Host` headers the MCP endpoint will answer, as rmcp's DNS-rebinding guard.
    ///
    /// An entry without a port matches that hostname on any port; an entry with one
    /// requires an exact match. MCPHub addresses this server by container name
    /// (`http://whatsapp-mcp:8000/mcp`), so `whatsapp-mcp` is the production value —
    /// written without a port on purpose, because `WHATSAPP_PORT` is already
    /// configurable and pinning the port in two places invites the two to disagree.
    pub allowed_hosts: Vec<String>,
}

/// Where the server listens, read on its own so `--health-check` can find the port
/// without a DSN it has no use for.
pub fn address_from_env() -> Result<(String, u16), ConfigError> {
    address_from_source(|name| env::var(name).ok())
}

fn address_from_source(
    get: impl Fn(&'static str) -> Option<String>,
) -> Result<(String, u16), ConfigError> {
    let port = match get("WHATSAPP_PORT") {
        Some(raw) => {
            let parsed: u32 = raw.trim().parse().map_err(|_| ConfigError::NotANumber {
                name: "WHATSAPP_PORT",
                value: raw.clone(),
            })?;
            if !(1..=65535).contains(&parsed) {
                return Err(ConfigError::PortOutOfRange);
            }
            parsed as u16
        }
        None => 8000,
    };
    Ok((
        get("WHATSAPP_HOST").unwrap_or_else(|| "0.0.0.0".to_owned()),
        port,
    ))
}

/// `WHATSAPP_MCP_ALLOWED_HOSTS` as a comma-separated list, falling back to
/// [`DEFAULT_ALLOWED_HOSTS`].
///
/// Blank entries are dropped, and a value that is entirely blank or absent yields the
/// default rather than an empty list — see [`DEFAULT_ALLOWED_HOSTS`] for why that
/// matters.
pub fn parse_allowed_hosts(raw: &str) -> Vec<String> {
    let configured: Vec<String> = raw
        .split(',')
        .map(str::trim)
        .filter(|host| !host.is_empty())
        .map(str::to_owned)
        .collect();
    if configured.is_empty() {
        return DEFAULT_ALLOWED_HOSTS
            .iter()
            .map(|host| (*host).to_owned())
            .collect();
    }
    configured
}

impl McpSettings {
    /// Read `WHATSAPP_*` from the environment, with the Python defaults.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_source(|name| env::var(name).ok())
    }

    /// The same read, against any source of variables.
    ///
    /// `from_env` is one line over this so the variable *names* are testable: `set_var`
    /// is process-global and unsafe in edition 2024, so a test that set them would race
    /// every other test in the binary.
    pub fn from_source(get: impl Fn(&'static str) -> Option<String>) -> Result<Self, ConfigError> {
        let (host, port) = address_from_source(&get)?;
        Ok(Self {
            dsn: Secret(get("WHATSAPP_DSN").ok_or(ConfigError::Missing("WHATSAPP_DSN"))?),
            host,
            port,
            log_level: get("WHATSAPP_LOG_LEVEL").unwrap_or_else(|| "INFO".to_owned()),
            allowed_hosts: parse_allowed_hosts(
                &get("WHATSAPP_MCP_ALLOWED_HOSTS").unwrap_or_default(),
            ),
        })
    }

    pub fn for_dsn(dsn: impl Into<String>) -> Self {
        Self {
            dsn: Secret(dsn.into()),
            host: "0.0.0.0".to_owned(),
            port: 8000,
            log_level: "INFO".to_owned(),
            allowed_hosts: DEFAULT_ALLOWED_HOSTS
                .iter()
                .map(|host| (*host).to_owned())
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(vars: &[(&str, &str)]) -> McpSettings {
        let source: Vec<(String, String)> = vars
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect();
        McpSettings::from_source(|name| {
            source
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        })
        .expect("settings")
    }

    #[test]
    fn the_dsn_never_appears_in_a_debug_line() {
        // `test_credentials_never_render`: a stray log line or a panic must not print
        // the password.
        let rendered = format!(
            "{:?}",
            settings(&[(
                "WHATSAPP_DSN",
                "postgresql://whatsapp_mcp:hunter2@db/whatsapp"
            )])
        );
        assert!(!rendered.contains("hunter2"), "{rendered}");
    }

    #[test]
    fn the_mcp_requires_a_dsn() {
        // `test_the_mcp_also_requires_a_dsn`: it reads the archive, so starting without
        // one would only fail later.
        let error = McpSettings::from_source(|_| None).unwrap_err();
        assert!(error.to_string().contains("WHATSAPP_DSN"), "{error}");
    }

    #[test]
    fn the_defaults_are_the_ones_the_compose_file_relies_on() {
        let settings = settings(&[("WHATSAPP_DSN", "postgresql://u:p@localhost/whatsapp")]);
        assert_eq!(settings.port, 8000);
        assert_eq!(settings.host, "0.0.0.0");
        assert_eq!(settings.log_level, "INFO");
    }

    #[test]
    fn the_names_the_compose_file_sets_are_the_names_read() {
        // The point of `from_source`: a renamed variable fails here rather than in
        // production, where it would quietly read as a default.
        let settings = settings(&[
            ("WHATSAPP_DSN", "postgresql://u:p@localhost/whatsapp"),
            ("WHATSAPP_HOST", "127.0.0.1"),
            ("WHATSAPP_PORT", "9001"),
            ("WHATSAPP_LOG_LEVEL", "DEBUG"),
        ]);
        assert_eq!(settings.host, "127.0.0.1");
        assert_eq!(settings.port, 9001);
        assert_eq!(settings.log_level, "DEBUG");
    }

    #[test]
    fn an_unreadable_or_out_of_range_port_is_refused() {
        let dsn = ("WHATSAPP_DSN", "postgresql://u:p@localhost/whatsapp");
        for (raw, expected) in [("soon", "not a number"), ("70000", "between 1 and 65535")] {
            let source = [dsn, ("WHATSAPP_PORT", raw)];
            let error = McpSettings::from_source(|name| {
                source
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| (*value).to_owned())
            })
            .unwrap_err();
            assert!(error.to_string().contains(expected), "{raw}: {error}");
        }
    }

    #[test]
    fn the_allowlist_is_comma_separated_and_trimmed() {
        assert_eq!(
            parse_allowed_hosts("whatsapp-mcp, whatsapp-mcp:8000 ,localhost"),
            ["whatsapp-mcp", "whatsapp-mcp:8000", "localhost"]
        );
    }

    #[test]
    fn an_unset_or_blank_allowlist_falls_back_to_loopback_not_to_nothing() {
        // rmcp reads an empty list as "allow every Host". A compose file that forgets
        // the variable must fail closed and be obvious, not fail open and be invisible.
        for raw in ["", "   ", ",", " , , "] {
            assert_eq!(
                parse_allowed_hosts(raw),
                DEFAULT_ALLOWED_HOSTS,
                "{raw:?} must not disable the guard"
            );
        }
    }

    #[test]
    fn a_single_host_replaces_the_default_rather_than_extending_it() {
        // Production sets exactly `whatsapp-mcp`; loopback is not silently kept
        // alongside it, so what the log line prints is the whole truth.
        assert_eq!(parse_allowed_hosts("whatsapp-mcp"), ["whatsapp-mcp"]);
    }
}
