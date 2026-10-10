//! Configuration (spec §17): `kage.yaml` layered with environment variables
//! of the form `KAGE__SECTION__KEY=value`.
//!
//! Later layers win: built-in defaults, then the file, then the environment.
//!
//! # Secrets
//!
//! Secrets come only from the environment or from files mounted as Docker
//! secrets, never from `kage.yaml`:
//!
//! - `KAGE__DATABASE__URL=postgres://...` sets the value directly;
//! - `KAGE__DATABASE__URL_FILE=/run/secrets/database_url` names a file whose
//!   content is the value (one trailing newline is dropped).
//!
//! Setting both, or putting a secret in the file, is an error, and so is a
//! missing required secret. Secret values bypass the type guessing applied
//! to other environment values, so a token that looks like a number stays
//! the text it is. Every secret is a [`Secret`], which prints and serializes
//! as `[redacted]`.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use figment::Figment;
use figment::providers::{Format, Yaml};
use serde::{Deserialize, Serialize};

use crate::duration::HumanDuration;
use crate::secret::Secret;
use crate::ssrf::SsrfPolicy;
use crate::urlnorm::NormalizeOptions;

mod http;
mod personas;
mod proxies;

pub use http::{
    BrowserSelftestConfig, ChromiumEngineConfig, EngineConfig, EnginesConfig, HttpConfig,
    HttpSelftestConfig, MAX_REDIRECTS_LIMIT, MAX_SELFTEST_PAGES, MAX_SUBRESOURCE_CONCURRENCY,
    SelftestConfig, SelftestPageConfig, SelftestPageKind, SubresourceScope, SubresourcesConfig,
};
pub use personas::{
    ActiveHoursConfig, ClockTime, DelayRange, GapConfig, InvalidClockTime, LANGUAGE_PLACEHOLDER,
    MAX_WARMUP_SITES, MAX_WARMUP_VISITS, PersonasConfig, SessionsConfig, VisitRange, WarmupConfig,
    WarmupSite, warmup_url,
};
pub use proxies::{
    ProviderChoice, ProxiesConfig, ProxyHealthConfig, ReputationConfig, ResolvedProvider,
    StaticListError, StaticProxy, WEBSHARE_AUTO_REPLACE_SETTINGS,
    WEBSHARE_GENERAL_LIMIT_PER_MINUTE, WEBSHARE_LIST_LIMIT_PER_MINUTE, WebshareConfig,
};

/// Length of the master key in bytes; it is written as twice as many
/// hexadecimal characters.
pub const MASTER_KEY_BYTES: usize = 32;

/// Prefix of every configuration environment variable.
pub const ENV_PREFIX: &str = "KAGE__";

/// Separator between the section and the key in an environment variable.
const ENV_SEPARATOR: &str = "__";

/// Suffix that turns a secret's variable into "read the value from this file".
const FILE_SUFFIX: &str = "_FILE";

/// One secret the configuration knows about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SecretSpec {
    /// Its place in the configuration, for example `database.url`.
    pub key: &'static str,
    /// The environment variable that sets it.
    pub env: &'static str,
    /// Whether startup fails without it.
    pub required: bool,
}

/// Every secret (spec §17).
pub const SECRETS: [SecretSpec; 6] = [
    SecretSpec {
        key: "database.url",
        env: "KAGE__DATABASE__URL",
        required: true,
    },
    SecretSpec {
        key: "sidecar.token",
        env: "KAGE__SIDECAR__TOKEN",
        required: true,
    },
    SecretSpec {
        key: "secrets.master_key",
        env: "KAGE__SECRETS__MASTER_KEY",
        required: true,
    },
    // Required when `proxies.provider` is `webshare`; with `auto` its
    // presence selects the Webshare provider.
    SecretSpec {
        key: "webshare.api_key",
        env: "KAGE__WEBSHARE__API_KEY",
        required: false,
    },
    // Reputation lookups stay disabled without it (open item O3).
    SecretSpec {
        key: "reputation.abuseipdb_key",
        env: "KAGE__REPUTATION__ABUSEIPDB_KEY",
        required: false,
    },
    // A fixed proxy list with credentials, for tests and other vendors.
    // Required when `proxies.provider` is `static`.
    SecretSpec {
        key: "proxies.static_list",
        env: "KAGE__PROXIES__STATIC_LIST",
        required: false,
    },
];

/// Why the configuration could not be loaded. No variant ever carries a
/// secret value.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The configuration file could not be read.
    #[error("cannot read the configuration file {}: {source}", path.display())]
    Read {
        /// The file.
        path: PathBuf,
        /// The I/O error.
        source: std::io::Error,
    },
    /// A secret is set in the configuration file.
    #[error(
        "{key} is a secret and must not be set in {}; set {env} or {env}_FILE instead",
        file.display()
    )]
    SecretInFile {
        /// The configuration key.
        key: &'static str,
        /// The variable to use instead.
        env: &'static str,
        /// The offending file.
        file: PathBuf,
    },
    /// A secret is set both directly and through a file.
    #[error("{env} and {env}_FILE are both set; set only one")]
    SecretConflict {
        /// The secret's variable.
        env: &'static str,
    },
    /// The file named by a `_FILE` variable could not be read.
    #[error("{env}_FILE: cannot read {}: {source}", path.display())]
    SecretFile {
        /// The secret's variable.
        env: &'static str,
        /// The file it names.
        path: PathBuf,
        /// The I/O error.
        source: std::io::Error,
    },
    /// Required secrets are missing.
    #[error("missing required secret(s): {}", .0.join(", "))]
    MissingSecrets(Vec<&'static str>),
    /// An environment variable with the `KAGE__` prefix does not name a key.
    #[error(
        "environment variable {0} does not name a configuration key; expected KAGE__SECTION__KEY"
    )]
    InvalidEnvKey(String),
    /// The merged configuration has a wrong type, an unknown key or a value
    /// that is out of range.
    #[error("invalid configuration: {0}")]
    Invalid(String),
}

/// The HTTP server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Address `kage serve` listens on.
    #[serde(default = "default_bind")]
    pub bind: SocketAddr,
    /// Address `kage healthcheck` probes.
    #[serde(default = "default_healthcheck_addr")]
    pub healthcheck_addr: SocketAddr,
}

const fn default_bind() -> SocketAddr {
    SocketAddr::new(std::net::IpAddr::V4(Ipv4Addr::UNSPECIFIED), 8080)
}

const fn default_healthcheck_addr() -> SocketAddr {
    SocketAddr::new(std::net::IpAddr::V4(Ipv4Addr::LOCALHOST), 8080)
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: default_bind(),
            healthcheck_addr: default_healthcheck_addr(),
        }
    }
}

/// The Postgres connection.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseConfig {
    /// Connection URL. Secret: `KAGE__DATABASE__URL`.
    pub url: Secret,
    /// Largest number of pooled connections.
    #[serde(default = "default_max_connections")]
    pub max_connections: u32,
    /// How long to wait for a connection.
    #[serde(default = "default_connect_timeout")]
    pub connect_timeout: HumanDuration,
}

const fn default_max_connections() -> u32 {
    10
}

const fn default_connect_timeout() -> HumanDuration {
    HumanDuration::from_secs(10)
}

/// The browser sidecars (spec §10.1).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SidecarConfig {
    /// Shared secret sent as `X-Kage-Sidecar-Token`. Secret:
    /// `KAGE__SIDECAR__TOKEN`.
    pub token: Secret,
    /// Base URL of the Chromium sidecar on the compose network.
    #[serde(default = "default_chromium_url")]
    pub chromium_url: String,
    /// Base URL of the Firefox sidecar on the compose network.
    #[serde(default = "default_firefox_url")]
    pub firefox_url: String,
    /// How long a connection to a sidecar may take.
    #[serde(default = "default_sidecar_connect_timeout")]
    pub connect_timeout: HumanDuration,
    /// How long one sidecar call may take, the navigation it runs
    /// included. It must be longer than the longest attempt (spec §8.4:
    /// 300 s), or Kage gives up on a page the browser is still loading.
    #[serde(default = "default_sidecar_timeout")]
    pub timeout: HumanDuration,
    /// How often a call is tried again when the connection to the
    /// sidecar could not be made at all. A request that reached the
    /// sidecar is never repeated.
    #[serde(default = "default_sidecar_connect_retries")]
    pub connect_retries: u32,
}

/// Most connection retries a sidecar call may be given.
pub const MAX_SIDECAR_CONNECT_RETRIES: u32 = 10;

fn default_chromium_url() -> String {
    "http://browser-chromium:8090".to_owned()
}

fn default_firefox_url() -> String {
    "http://browser-firefox:8090".to_owned()
}

const fn default_sidecar_connect_timeout() -> HumanDuration {
    HumanDuration::from_secs(5)
}

const fn default_sidecar_timeout() -> HumanDuration {
    HumanDuration::from_secs(330)
}

const fn default_sidecar_connect_retries() -> u32 {
    3
}

/// Keys for data encrypted at rest.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretsConfig {
    /// Key that encrypts proxy credentials: 32 random bytes written as 64
    /// hexadecimal characters. Secret: `KAGE__SECRETS__MASTER_KEY`.
    pub master_key: Secret,
}

impl SecretsConfig {
    /// The master key as bytes, or `None` when the text is not exactly 64
    /// hexadecimal characters. A loaded [`Config`] always has a valid key.
    pub fn master_key_bytes(&self) -> Option<[u8; MASTER_KEY_BYTES]> {
        let text = self.master_key.expose().as_bytes();
        if text.len() != MASTER_KEY_BYTES * 2 {
            return None;
        }
        let mut key = [0_u8; MASTER_KEY_BYTES];
        let (pairs, _) = text.as_chunks::<2>();
        for (byte, pair) in key.iter_mut().zip(pairs) {
            let pair = std::str::from_utf8(pair).ok()?;
            // `from_str_radix` would also accept a sign.
            if !pair.bytes().all(|digit| digit.is_ascii_hexdigit()) {
                return None;
            }
            *byte = u8::from_str_radix(pair, 16).ok()?;
        }
        Some(key)
    }
}

/// The policy files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PoliciesConfig {
    /// Directory holding `domains.yaml`, `block-signatures.yaml` and
    /// `pacing-profiles.yaml`.
    #[serde(default = "default_policies_dir")]
    pub dir: PathBuf,
    /// Reload the files when they change on disk.
    #[serde(default = "default_true")]
    pub hot_reload: bool,
}

fn default_policies_dir() -> PathBuf {
    PathBuf::from("policies")
}

const fn default_true() -> bool {
    true
}

impl Default for PoliciesConfig {
    fn default() -> Self {
        Self {
            dir: default_policies_dir(),
            hot_reload: true,
        }
    }
}

/// The artifact store (spec §18).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactsConfig {
    /// Directory the blobs live in.
    #[serde(default = "default_artifacts_dir")]
    pub dir: PathBuf,
    /// Days an artifact is kept.
    #[serde(default = "default_retention_days")]
    pub retention_days: u32,
    /// Text bodies at least this large are gzip-compressed on disk.
    #[serde(default = "default_gzip_min_bytes")]
    pub gzip_min_bytes: u64,
    /// How often the retention job runs.
    #[serde(default = "default_sweep_interval")]
    pub sweep_interval: HumanDuration,
}

fn default_artifacts_dir() -> PathBuf {
    PathBuf::from("artifacts")
}

const fn default_retention_days() -> u32 {
    14
}

const fn default_gzip_min_bytes() -> u64 {
    1024
}

const fn default_sweep_interval() -> HumanDuration {
    HumanDuration::from_hours(1)
}

impl Default for ArtifactsConfig {
    fn default() -> Self {
        Self {
            dir: default_artifacts_dir(),
            retention_days: default_retention_days(),
            gzip_min_bytes: default_gzip_min_bytes(),
            sweep_interval: default_sweep_interval(),
        }
    }
}

impl ArtifactsConfig {
    /// How long an artifact is kept.
    pub fn retention(&self) -> Duration {
        Duration::from_secs(u64::from(self.retention_days) * 24 * 60 * 60)
    }
}

/// The effective configuration.
///
/// Later milestones add their own sections. `Debug` and `Serialize` are safe
/// to print: every secret shows as `[redacted]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The HTTP server.
    #[serde(default)]
    pub server: ServerConfig,
    /// The Postgres connection.
    pub database: DatabaseConfig,
    /// The browser sidecars.
    pub sidecar: SidecarConfig,
    /// Keys for data encrypted at rest.
    pub secrets: SecretsConfig,
    /// The Webshare account.
    #[serde(default)]
    pub webshare: WebshareConfig,
    /// IP reputation lookups.
    #[serde(default)]
    pub reputation: ReputationConfig,
    /// The proxy pool.
    #[serde(default)]
    pub proxies: ProxiesConfig,
    /// The http path.
    #[serde(default)]
    pub http: HttpConfig,
    /// Self-tests.
    #[serde(default)]
    pub selftest: SelftestConfig,
    /// The browser engines.
    #[serde(default)]
    pub engines: EnginesConfig,
    /// Personas.
    #[serde(default)]
    pub personas: PersonasConfig,
    /// Sessions.
    #[serde(default)]
    pub sessions: SessionsConfig,
    /// The policy files.
    #[serde(default)]
    pub policies: PoliciesConfig,
    /// The artifact store.
    #[serde(default)]
    pub artifacts: ArtifactsConfig,
    /// The SSRF guard.
    #[serde(default)]
    pub security: SsrfPolicy,
    /// URL normalization.
    #[serde(default)]
    pub urls: NormalizeOptions,
}

impl Config {
    /// Loads the configuration from an optional YAML file and the given
    /// environment variables. Pass [`process_env`] for the real environment.
    ///
    /// # Errors
    ///
    /// Fails when the file cannot be read, a secret is misplaced or missing,
    /// or the merged configuration is invalid.
    pub fn load(
        file: Option<&Path>,
        env: impl IntoIterator<Item = (String, String)>,
    ) -> Result<Self, ConfigError> {
        let mut figment = Figment::new();

        if let Some(path) = file {
            let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
                path: path.to_path_buf(),
                source,
            })?;
            let from_file = Figment::from(Yaml::string(&text));
            for secret in SECRETS {
                if from_file.find_value(secret.key).is_ok() {
                    return Err(ConfigError::SecretInFile {
                        key: secret.key,
                        env: secret.env,
                        file: path.to_path_buf(),
                    });
                }
            }
            figment = figment.merge(from_file);
        }

        let mut secret_values: Vec<(SecretSpec, Option<String>, Option<PathBuf>)> =
            SECRETS.iter().map(|spec| (*spec, None, None)).collect();

        for (name, value) in env {
            if !name.starts_with(ENV_PREFIX) {
                continue;
            }
            let slot = secret_values.iter_mut().find(|(spec, _, _)| {
                name == spec.env || name.strip_suffix(FILE_SUFFIX) == Some(spec.env)
            });
            if let Some((spec, direct, from_file)) = slot {
                if name == spec.env {
                    // An empty variable is how compose passes an optional
                    // secret that is not set, so it must not clash with the
                    // `_FILE` form.
                    *direct = Some(value).filter(|value| !value.is_empty());
                } else {
                    *from_file = Some(PathBuf::from(value));
                }
                continue;
            }
            let key = env_key(&name)?;
            let value: figment::value::Value = match value.parse() {
                Ok(value) => value,
                Err(infallible) => match infallible {},
            };
            figment = figment.merge((key, value));
        }

        let mut missing = Vec::new();
        for (spec, direct, from_file) in secret_values {
            let value = match (direct, from_file) {
                (Some(_), Some(_)) => return Err(ConfigError::SecretConflict { env: spec.env }),
                (Some(value), None) => Some(value),
                (None, Some(path)) => Some(read_secret_file(spec.env, &path)?),
                (None, None) => None,
            };
            // An empty secret is a mistake, not a value.
            match value.filter(|value| !value.is_empty()) {
                Some(value) => figment = figment.merge((spec.key, value)),
                None if spec.required => missing.push(spec.env),
                None => {}
            }
        }
        if !missing.is_empty() {
            return Err(ConfigError::MissingSecrets(missing));
        }

        let config: Self = figment
            .extract()
            .map_err(|error| ConfigError::Invalid(error.to_string()))?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        let invalid = |message: &str| Err(ConfigError::Invalid(message.to_owned()));
        let database_url = self.database.url.expose();
        if !(database_url.starts_with("postgres://") || database_url.starts_with("postgresql://")) {
            return invalid("database.url must start with postgres:// or postgresql://");
        }
        if self.database.max_connections == 0 {
            return invalid("database.max_connections must be at least 1");
        }
        if self.database.connect_timeout.get().is_zero() {
            return invalid("database.connect_timeout must be longer than zero");
        }
        if self.artifacts.retention_days == 0 {
            return invalid("artifacts.retention_days must be at least 1");
        }
        if self.artifacts.sweep_interval.get() < Duration::from_secs(1) {
            return invalid("artifacts.sweep_interval must be at least 1s");
        }
        if self.policies.dir.as_os_str().is_empty() {
            return invalid("policies.dir must not be empty");
        }
        if self.artifacts.dir.as_os_str().is_empty() {
            return invalid("artifacts.dir must not be empty");
        }
        for (key, value) in [
            ("sidecar.chromium_url", &self.sidecar.chromium_url),
            ("sidecar.firefox_url", &self.sidecar.firefox_url),
        ] {
            let plain_http = url::Url::parse(value).is_ok_and(|url| {
                url.scheme() == "http" && url.host_str().is_some() && url.path() == "/"
            });
            if !plain_http {
                return Err(ConfigError::Invalid(format!(
                    "{key} must be an http:// URL without a path: a sidecar listens on the \
                     compose network only"
                )));
            }
        }
        if self.sidecar.connect_timeout.get().is_zero() {
            return invalid("sidecar.connect_timeout must be longer than zero");
        }
        if self.sidecar.timeout.get() < Duration::from_secs(1) {
            return invalid("sidecar.timeout must be at least 1s");
        }
        if self.sidecar.connect_retries > MAX_SIDECAR_CONNECT_RETRIES {
            return Err(ConfigError::Invalid(format!(
                "sidecar.connect_retries must be at most {MAX_SIDECAR_CONNECT_RETRIES}"
            )));
        }
        if self.secrets.master_key_bytes().is_none() {
            return invalid(
                "secrets.master_key must be 32 random bytes written as 64 hexadecimal characters",
            );
        }
        proxies::validate(&self.webshare, &self.reputation, &self.proxies)
            .map_err(ConfigError::Invalid)?;
        http::validate(&self.http, &self.selftest, &self.engines).map_err(ConfigError::Invalid)?;
        personas::validate(
            &self.personas,
            &self.sessions,
            self.engines.chromium.allow_cross_os_claims,
        )
        .map_err(ConfigError::Invalid)?;
        match self.proxies.provider {
            ProviderChoice::Webshare if self.webshare.api_key.is_none() => {
                return Err(ConfigError::MissingSecrets(vec!["KAGE__WEBSHARE__API_KEY"]));
            }
            ProviderChoice::Static if self.proxies.static_list.is_none() => {
                return Err(ConfigError::MissingSecrets(vec![
                    "KAGE__PROXIES__STATIC_LIST",
                ]));
            }
            _ => {}
        }
        Ok(())
    }

    /// The proxy provider in force. `auto` resolves to Webshare when its API
    /// key is set, else to the static list when one is set, else to none.
    pub const fn proxy_provider(&self) -> ResolvedProvider {
        match self.proxies.provider {
            ProviderChoice::Webshare => ResolvedProvider::Webshare,
            ProviderChoice::Static => ResolvedProvider::Static,
            ProviderChoice::None => ResolvedProvider::None,
            ProviderChoice::Auto => {
                if self.webshare.api_key.is_some() {
                    ResolvedProvider::Webshare
                } else if self.proxies.static_list.is_some() {
                    ResolvedProvider::Static
                } else {
                    ResolvedProvider::None
                }
            }
        }
    }

    /// The effective configuration as JSON with every secret redacted. This
    /// is what `kage config check` prints.
    pub fn redacted(&self) -> serde_json::Value {
        // Serializing plain structs into a JSON value cannot fail.
        serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
    }
}

/// `KAGE__SECTION__KEY` to `section.key`.
fn env_key(name: &str) -> Result<String, ConfigError> {
    let invalid = || ConfigError::InvalidEnvKey(name.to_owned());
    let rest = name.strip_prefix(ENV_PREFIX).ok_or_else(invalid)?;
    let segments: Vec<String> = rest
        .split(ENV_SEPARATOR)
        .map(str::to_ascii_lowercase)
        .collect();
    if segments.len() < 2 || segments.iter().any(String::is_empty) {
        return Err(invalid());
    }
    Ok(segments.join("."))
}

fn read_secret_file(env: &'static str, path: &Path) -> Result<String, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|source| ConfigError::SecretFile {
        env,
        path: path.to_path_buf(),
        source,
    })?;
    let text = text.strip_suffix('\n').unwrap_or(&text);
    let text = text.strip_suffix('\r').unwrap_or(text);
    Ok(text.to_owned())
}

/// The process environment as text pairs. Variables whose name or value is
/// not valid Unicode cannot be configuration and are skipped.
pub fn process_env() -> Vec<(String, String)> {
    std::env::vars_os()
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use tempfile::TempDir;

    use super::{
        Config, ConfigError, ProviderChoice, ResolvedProvider, SECRETS, StaticProxy, process_env,
    };
    use crate::secret::REDACTED;
    use crate::urlnorm::TrailingSlash;

    const DB_URL: &str = "postgres://kage:db-pass-4f9a@db.internal:5432/kage";
    const TOKEN: &str = "sidecar-token-77c1";
    // 32 bytes as 64 hexadecimal characters.
    const MASTER: &str = "0b3e0b3e0b3e0b3e0b3e0b3e0b3e0b3e0b3e0b3e0b3e0b3e0b3e0b3e0b3e0b3e";

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect()
    }

    fn required() -> Vec<(String, String)> {
        env(&[
            ("KAGE__DATABASE__URL", DB_URL),
            ("KAGE__SIDECAR__TOKEN", TOKEN),
            ("KAGE__SECRETS__MASTER_KEY", MASTER),
        ])
    }

    fn with(extra: &[(&str, &str)]) -> Vec<(String, String)> {
        let mut all = required();
        all.extend(env(extra));
        all
    }

    fn write(dir: &TempDir, name: &str, text: &str) -> PathBuf {
        let path = dir.path().join(name);
        std::fs::write(&path, text).unwrap();
        path
    }

    fn invalid(result: Result<Config, ConfigError>) -> String {
        match result {
            Err(ConfigError::Invalid(message)) => message,
            other => panic!("expected an invalid configuration, got {other:?}"),
        }
    }

    #[test]
    fn the_sidecars_are_found_on_the_compose_network_by_default() {
        let sidecar = Config::load(None, required()).unwrap().sidecar;
        assert_eq!(sidecar.chromium_url, "http://browser-chromium:8090");
        assert_eq!(sidecar.firefox_url, "http://browser-firefox:8090");
        assert_eq!(sidecar.connect_timeout.get(), Duration::from_secs(5));
        // Longer than the longest attempt (spec §8.4: 300 s).
        assert_eq!(sidecar.timeout.get(), Duration::from_secs(330));
        assert_eq!(sidecar.connect_retries, 3);

        let changed = Config::load(
            None,
            with(&[
                ("KAGE__SIDECAR__CHROMIUM_URL", "http://10.0.0.7:9000"),
                ("KAGE__SIDECAR__TIMEOUT", "6m"),
                ("KAGE__SIDECAR__CONNECT_RETRIES", "0"),
                ("KAGE__ENGINES__CHROMIUM__ALLOW_CROSS_OS_CLAIMS", "true"),
                ("KAGE__ENGINES__CHROMIUM__BROWSER_MAJOR", "155"),
            ]),
        )
        .unwrap();
        assert_eq!(changed.sidecar.chromium_url, "http://10.0.0.7:9000");
        assert_eq!(changed.sidecar.timeout.get(), Duration::from_mins(6));
        assert_eq!(changed.sidecar.connect_retries, 0);
        assert!(changed.engines.chromium.allow_cross_os_claims);
        assert_eq!(changed.engines.chromium.browser_major, Some(155));
    }

    #[test]
    fn sidecar_settings_that_cannot_work_are_refused() {
        for (name, value, key) in [
            (
                "KAGE__SIDECAR__CHROMIUM_URL",
                "https://browser-chromium:8090",
                "sidecar.chromium_url",
            ),
            (
                "KAGE__SIDECAR__CHROMIUM_URL",
                "browser-chromium:8090",
                "sidecar.chromium_url",
            ),
            (
                "KAGE__SIDECAR__FIREFOX_URL",
                "http://browser-firefox:8090/v1",
                "sidecar.firefox_url",
            ),
            (
                "KAGE__SIDECAR__CONNECT_TIMEOUT",
                "0s",
                "sidecar.connect_timeout",
            ),
            ("KAGE__SIDECAR__TIMEOUT", "500ms", "sidecar.timeout"),
            (
                "KAGE__SIDECAR__CONNECT_RETRIES",
                "11",
                "sidecar.connect_retries",
            ),
        ] {
            let message = invalid(Config::load(None, with(&[(name, value)])));
            assert!(message.starts_with(key), "{name}={value}: {message}");
        }
    }

    #[test]
    fn defaults_apply_with_only_the_required_secrets() {
        let config = Config::load(None, required()).unwrap();

        assert_eq!(config.server.bind.to_string(), "0.0.0.0:8080");
        assert_eq!(config.server.healthcheck_addr.to_string(), "127.0.0.1:8080");
        assert_eq!(config.database.url.expose(), DB_URL);
        assert_eq!(config.database.max_connections, 10);
        assert_eq!(
            config.database.connect_timeout.get(),
            Duration::from_secs(10)
        );
        assert_eq!(config.sidecar.token.expose(), TOKEN);
        assert_eq!(config.secrets.master_key.expose(), MASTER);
        assert_eq!(
            config.secrets.master_key_bytes(),
            Some([0x0b, 0x3e].repeat(16).try_into().unwrap())
        );
        assert!(config.webshare.api_key.is_none());
        assert!(config.reputation.abuseipdb_key.is_none());
        assert!(config.proxies.static_list.is_none());
        assert_eq!(config.proxies.provider, ProviderChoice::Auto);
        assert_eq!(config.proxy_provider(), ResolvedProvider::None);
        assert_eq!(config.policies.dir, Path::new("policies"));
        assert!(config.policies.hot_reload);
        assert_eq!(config.artifacts.dir, Path::new("artifacts"));
        assert_eq!(config.artifacts.retention_days, 14);
        assert_eq!(config.artifacts.retention(), Duration::from_hours(14 * 24));
        assert_eq!(config.artifacts.gzip_min_bytes, 1024);
        assert_eq!(
            config.artifacts.sweep_interval.get(),
            Duration::from_hours(1)
        );
        assert!(!config.security.allow_ip_targets);
        assert_eq!(config.urls.trailing_slash, TrailingSlash::Keep);
    }

    #[test]
    fn the_file_overrides_defaults_and_the_environment_overrides_the_file() {
        let dir = TempDir::new().unwrap();
        let file = write(
            &dir,
            "kage.yaml",
            "
server:
  bind: 127.0.0.1:9000
database:
  max_connections: 4
artifacts:
  retention_days: 30
  dir: /var/lib/kage/artifacts
security:
  allow_ip_targets: true
urls:
  trailing_slash: strip
  tracking_params: [utm_*, sessionid]
",
        );

        let config = Config::load(
            Some(&file),
            with(&[
                ("KAGE__SERVER__BIND", "0.0.0.0:7000"),
                ("KAGE__ARTIFACTS__RETENTION_DAYS", "7"),
                ("KAGE__POLICIES__HOT_RELOAD", "false"),
                ("KAGE__DATABASE__CONNECT_TIMEOUT", "3s"),
            ]),
        )
        .unwrap();

        // Environment over file.
        assert_eq!(config.server.bind.to_string(), "0.0.0.0:7000");
        assert_eq!(config.artifacts.retention_days, 7);
        // File over defaults.
        assert_eq!(config.database.max_connections, 4);
        assert_eq!(config.artifacts.dir, Path::new("/var/lib/kage/artifacts"));
        assert!(config.security.allow_ip_targets);
        assert_eq!(config.urls.trailing_slash, TrailingSlash::Strip);
        assert_eq!(config.urls.tracking_params, ["utm_*", "sessionid"]);
        // Environment over defaults, with typed values.
        assert!(!config.policies.hot_reload);
        assert_eq!(
            config.database.connect_timeout.get(),
            Duration::from_secs(3)
        );
        // Untouched keys keep their defaults.
        assert_eq!(config.server.healthcheck_addr.to_string(), "127.0.0.1:8080");
    }

    #[test]
    fn unrelated_environment_variables_are_ignored() {
        let config = Config::load(
            None,
            with(&[
                ("PATH", "/usr/bin"),
                ("KAGE_POSTGRES_PASSWORD", "single-underscore"),
                ("KAGE_LIVE", "1"),
                ("NOT_KAGE__SERVER__BIND", "1.2.3.4:1"),
            ]),
        )
        .unwrap();

        assert_eq!(config.server.bind.to_string(), "0.0.0.0:8080");
    }

    #[test]
    fn optional_secrets_are_read_when_present() {
        let config = Config::load(
            None,
            with(&[
                ("KAGE__WEBSHARE__API_KEY", "ws-key-51aa"),
                ("KAGE__REPUTATION__ABUSEIPDB_KEY", "abuse-key-93bd"),
            ]),
        )
        .unwrap();

        assert_eq!(config.webshare.api_key.unwrap().expose(), "ws-key-51aa");
        assert_eq!(
            config.reputation.abuseipdb_key.unwrap().expose(),
            "abuse-key-93bd"
        );
    }

    #[test]
    fn secrets_keep_their_exact_text_whatever_they_look_like() {
        // Each of these would be read as a number, a boolean or a list by
        // the type guessing that other environment values go through.
        for token in [
            "123456",
            "1e5",
            "true",
            "0x1f",
            "[a,b]",
            "12e45678",
            "  padded  ",
            "null",
        ] {
            let config = Config::load(
                None,
                with(&[
                    ("KAGE__SIDECAR__TOKEN", token),
                    ("KAGE__WEBSHARE__API_KEY", token),
                ]),
            )
            .unwrap();
            assert_eq!(config.sidecar.token.expose(), token);
            assert_eq!(config.webshare.api_key.unwrap().expose(), token);
        }
        // A master key of 64 digits is text too, not a number.
        let digits = "12".repeat(32);
        let config = Config::load(
            None,
            with(&[("KAGE__SECRETS__MASTER_KEY", digits.as_str())]),
        )
        .unwrap();
        assert_eq!(config.secrets.master_key.expose(), digits);
        assert_eq!(config.secrets.master_key_bytes(), Some([0x12; 32]));
    }

    #[test]
    fn the_master_key_must_be_32_bytes_of_hex_and_is_never_echoed() {
        let bad_keys = [
            "master-key-0b3e".to_owned(),
            // 63 and 65 characters.
            "a".repeat(63),
            "a".repeat(65),
            // The right length with a character that is not hexadecimal.
            format!("{}g", "a".repeat(63)),
            format!("+{}", "a".repeat(63)),
            format!("{}é", "a".repeat(62)),
        ];
        for bad in &bad_keys {
            let message = invalid(Config::load(
                None,
                with(&[("KAGE__SECRETS__MASTER_KEY", bad.as_str())]),
            ));

            assert!(message.contains("secrets.master_key"), "{message}");
            assert!(message.contains("64 hexadecimal"), "{message}");
            assert!(!message.contains(bad.as_str()), "{message}");
        }

        let upper = "AB".repeat(32);
        let config =
            Config::load(None, with(&[("KAGE__SECRETS__MASTER_KEY", upper.as_str())])).unwrap();
        assert_eq!(config.secrets.master_key_bytes(), Some([0xab; 32]));
    }

    #[test]
    fn the_provider_follows_the_secrets_unless_it_is_named() {
        let provider = |extra: &[(&str, &str)]| {
            Config::load(None, with(extra)).map(|config| config.proxy_provider())
        };
        let key = ("KAGE__WEBSHARE__API_KEY", "ws-key-51aa");
        let list = (
            "KAGE__PROXIES__STATIC_LIST",
            "198.51.100.10:8080:user:pass-77aa",
        );

        // auto
        assert_eq!(provider(&[]).unwrap(), ResolvedProvider::None);
        assert_eq!(provider(&[key]).unwrap(), ResolvedProvider::Webshare);
        assert_eq!(provider(&[list]).unwrap(), ResolvedProvider::Static);
        assert_eq!(provider(&[key, list]).unwrap(), ResolvedProvider::Webshare);
        // named
        let choose = |choice: &'static str| ("KAGE__PROXIES__PROVIDER", choice);
        assert_eq!(
            provider(&[choose("static"), key, list]).unwrap(),
            ResolvedProvider::Static
        );
        assert_eq!(
            provider(&[choose("none"), key, list]).unwrap(),
            ResolvedProvider::None
        );
        assert_eq!(
            provider(&[choose("webshare"), key]).unwrap(),
            ResolvedProvider::Webshare
        );
        // A named provider without its secret is a missing secret.
        assert!(matches!(
            provider(&[choose("webshare"), list]),
            Err(ConfigError::MissingSecrets(names)) if names == ["KAGE__WEBSHARE__API_KEY"]
        ));
        assert!(matches!(
            provider(&[choose("static"), key]),
            Err(ConfigError::MissingSecrets(names)) if names == ["KAGE__PROXIES__STATIC_LIST"]
        ));
        assert!(invalid(Config::load(None, with(&[choose("rotating")]))).contains("provider"));
    }

    #[test]
    fn the_static_list_is_a_secret_from_the_environment_or_a_file() {
        let dir = TempDir::new().unwrap();
        let list = "198.51.100.10:8080:user-a:pass-77aa\n198.51.100.11:8080:user-a:pass-77aa:DE\n";
        let file = write(&dir, "proxies.txt", list);

        for extra in [
            [("KAGE__PROXIES__STATIC_LIST", list)],
            [("KAGE__PROXIES__STATIC_LIST_FILE", file.to_str().unwrap())],
        ] {
            let config = Config::load(None, with(&extra)).unwrap();

            let parsed =
                StaticProxy::parse_list(config.proxies.static_list.as_ref().unwrap().expose())
                    .unwrap();
            assert_eq!(parsed.len(), 2);
            assert_eq!(parsed[1].country.unwrap().as_str(), "DE");
            let printed = format!("{config:?}{}", config.redacted());
            assert!(!printed.contains("pass-77aa"), "{printed}");
            assert!(!printed.contains("user-a"), "{printed}");
            assert_eq!(config.redacted()["proxies"]["static_list"], REDACTED);
        }

        // A malformed list fails the load without being quoted.
        let message = invalid(Config::load(
            None,
            with(&[("KAGE__PROXIES__STATIC_LIST", "198.51.100.10:8080:pass-77aa")]),
        ));
        assert!(message.contains("static proxy list: line 1"), "{message}");
        assert!(!message.contains("pass-77aa"), "{message}");
    }

    #[test]
    fn nested_pool_keys_are_set_from_the_file_and_the_environment() {
        let dir = TempDir::new().unwrap();
        let file = write(
            &dir,
            "kage.yaml",
            "
webshare:
  plan_id: 4242
  requests_per_minute: 60
proxies:
  replacements_per_month: 2
  replace_after: 12h
  health:
    interval: 10m
    confirm_url: null
    geo_url: https://geo.example.net/json
reputation:
  bad_score: 90
",
        );

        let config = Config::load(
            Some(&file),
            with(&[
                ("KAGE__PROXIES__HEALTH__INTERVAL", "7m"),
                ("KAGE__PROXIES__HEALTH__MAX_CONCURRENT", "1"),
                ("KAGE__PROXIES__BACKOFF_CYCLES", "4"),
                ("KAGE__WEBSHARE__BASE_URL", "http://127.0.0.1:9/api/"),
            ]),
        )
        .unwrap();

        assert_eq!(config.webshare.plan_id, Some(4242));
        assert_eq!(config.webshare.requests_per_minute, 60);
        assert_eq!(config.webshare.base_url, "http://127.0.0.1:9/api/");
        assert_eq!(config.proxies.replacements_per_month, 2);
        assert_eq!(config.proxies.replace_after.get(), Duration::from_hours(12));
        assert_eq!(config.proxies.backoff_cycles, 4);
        // Environment over file, at the third level too.
        assert_eq!(config.proxies.health.interval.get(), Duration::from_mins(7));
        assert_eq!(config.proxies.health.max_concurrent, 1);
        assert_eq!(config.proxies.health.confirm_url, None);
        assert_eq!(
            config.proxies.health.geo_url.as_deref(),
            Some("https://geo.example.net/json")
        );
        assert_eq!(config.reputation.bad_score, 90);

        assert!(
            invalid(Config::load(
                None,
                with(&[("KAGE__PROXIES__HEALTH__INTERVALL", "7m")])
            ))
            .contains("intervall")
        );
        assert!(
            invalid(Config::load(
                None,
                with(&[("KAGE__WEBSHARE__REQUESTS_PER_MINUTE", "240")])
            ))
            .contains("below Webshare's limit of 240")
        );
    }

    #[test]
    fn secrets_can_come_from_files() {
        let dir = TempDir::new().unwrap();
        let url_file = write(&dir, "database_url", &format!("{DB_URL}\n"));
        let token_file = write(&dir, "sidecar_token", &format!("{TOKEN}\r\n"));
        let key_file = write(&dir, "master_key", MASTER);

        let config = Config::load(
            None,
            env(&[
                ("KAGE__DATABASE__URL_FILE", url_file.to_str().unwrap()),
                ("KAGE__SIDECAR__TOKEN_FILE", token_file.to_str().unwrap()),
                ("KAGE__SECRETS__MASTER_KEY_FILE", key_file.to_str().unwrap()),
            ]),
        )
        .unwrap();

        assert_eq!(config.database.url.expose(), DB_URL);
        assert_eq!(config.sidecar.token.expose(), TOKEN);
        assert_eq!(config.secrets.master_key.expose(), MASTER);
    }

    #[test]
    fn a_secret_set_twice_is_a_conflict() {
        let dir = TempDir::new().unwrap();
        let token_file = write(&dir, "sidecar_token", TOKEN);

        let error = Config::load(
            None,
            with(&[("KAGE__SIDECAR__TOKEN_FILE", token_file.to_str().unwrap())]),
        )
        .unwrap_err();

        assert!(matches!(
            error,
            ConfigError::SecretConflict {
                env: "KAGE__SIDECAR__TOKEN"
            }
        ));
        assert_eq!(
            error.to_string(),
            "KAGE__SIDECAR__TOKEN and KAGE__SIDECAR__TOKEN_FILE are both set; set only one"
        );
    }

    #[test]
    fn an_unreadable_secret_file_names_the_variable_and_the_path() {
        let error = Config::load(
            None,
            env(&[
                ("KAGE__DATABASE__URL_FILE", "/nonexistent/database_url"),
                ("KAGE__SIDECAR__TOKEN", TOKEN),
                ("KAGE__SECRETS__MASTER_KEY", MASTER),
            ]),
        )
        .unwrap_err();

        assert!(matches!(
            error,
            ConfigError::SecretFile {
                env: "KAGE__DATABASE__URL",
                ..
            }
        ));
        assert!(
            error
                .to_string()
                .starts_with("KAGE__DATABASE__URL_FILE: cannot read /nonexistent/database_url")
        );
    }

    #[test]
    fn missing_required_secrets_are_all_listed() {
        let error = Config::load(None, env(&[("KAGE__SIDECAR__TOKEN", TOKEN)])).unwrap_err();

        match &error {
            ConfigError::MissingSecrets(names) => {
                assert_eq!(names, &["KAGE__DATABASE__URL", "KAGE__SECRETS__MASTER_KEY"]);
            }
            other => panic!("expected missing secrets, got {other:?}"),
        }
        assert_eq!(
            error.to_string(),
            "missing required secret(s): KAGE__DATABASE__URL, KAGE__SECRETS__MASTER_KEY"
        );

        let all_missing = Config::load(None, Vec::new()).unwrap_err();
        assert!(matches!(all_missing, ConfigError::MissingSecrets(names) if names.len() == 3));
    }

    #[test]
    fn an_empty_secret_counts_as_missing() {
        let dir = TempDir::new().unwrap();
        let empty = write(&dir, "empty", "\n");

        let error = Config::load(
            None,
            env(&[
                ("KAGE__DATABASE__URL", DB_URL),
                ("KAGE__SIDECAR__TOKEN", ""),
                ("KAGE__SECRETS__MASTER_KEY_FILE", empty.to_str().unwrap()),
            ]),
        )
        .unwrap_err();

        assert!(matches!(
            error,
            ConfigError::MissingSecrets(names) if names == ["KAGE__SIDECAR__TOKEN", "KAGE__SECRETS__MASTER_KEY"]
        ));
    }

    #[test]
    fn an_empty_variable_does_not_clash_with_its_file() {
        let dir = TempDir::new().unwrap();
        let key_file = write(&dir, "webshare_key", "from-the-file\n");

        let config = Config::load(
            None,
            with(&[
                ("KAGE__WEBSHARE__API_KEY", ""),
                ("KAGE__WEBSHARE__API_KEY_FILE", key_file.to_str().unwrap()),
                ("KAGE__REPUTATION__ABUSEIPDB_KEY", ""),
            ]),
        )
        .unwrap();

        assert!(config.reputation.abuseipdb_key.is_none());
        assert_eq!(config.webshare.api_key.unwrap().expose(), "from-the-file");
    }

    #[test]
    fn every_secret_is_refused_in_the_configuration_file() {
        let dir = TempDir::new().unwrap();
        for secret in SECRETS {
            let (section, key) = secret.key.split_once('.').unwrap();
            let file = write(
                &dir,
                "kage.yaml",
                &format!("{section}:\n  {key}: leaked-in-file\n"),
            );

            let error = Config::load(Some(&file), required()).unwrap_err();

            assert!(
                matches!(&error, ConfigError::SecretInFile { key, .. } if *key == secret.key),
                "{}: {error:?}",
                secret.key
            );
            let message = error.to_string();
            assert!(message.contains(secret.env), "{message}");
            assert!(!message.contains("leaked-in-file"), "{message}");
        }
    }

    #[test]
    fn a_missing_configuration_file_is_a_read_error() {
        let error =
            Config::load(Some(Path::new("/nonexistent/kage.yaml")), required()).unwrap_err();

        assert!(matches!(error, ConfigError::Read { .. }));
        assert!(error.to_string().contains("/nonexistent/kage.yaml"));
    }

    #[test]
    fn unknown_keys_and_wrong_types_are_rejected() {
        let dir = TempDir::new().unwrap();

        let typo = write(&dir, "typo.yaml", "server:\n  bnid: 127.0.0.1:1\n");
        assert!(invalid(Config::load(Some(&typo), required())).contains("bnid"));

        let section = write(&dir, "section.yaml", "sever:\n  bind: 127.0.0.1:1\n");
        assert!(invalid(Config::load(Some(&section), required())).contains("sever"));

        assert!(invalid(Config::load(None, with(&[("KAGE__SERVER__BNID", "x")]))).contains("bnid"));
        assert!(
            invalid(Config::load(
                None,
                with(&[("KAGE__DATABASE__MAX_CONNECTIONS", "many")])
            ))
            .contains("max_connections")
        );
        assert!(
            invalid(Config::load(
                None,
                with(&[("KAGE__SERVER__BIND", "not-an-address")])
            ))
            .contains("bind")
        );
        assert!(
            invalid(Config::load(
                None,
                with(&[("KAGE__URLS__TRAILING_SLASH", "sometimes")])
            ))
            .contains("trailing_slash")
        );

        let broken = write(&dir, "broken.yaml", "server: [");
        assert!(matches!(
            Config::load(Some(&broken), required()),
            Err(ConfigError::Invalid(_))
        ));
    }

    #[test]
    fn malformed_environment_keys_are_rejected() {
        for name in [
            "KAGE__",
            "KAGE__SERVER",
            "KAGE__SERVER__",
            "KAGE____BIND",
            "KAGE__SERVER____BIND",
        ] {
            let error = Config::load(None, with(&[(name, "x")])).unwrap_err();
            assert!(
                matches!(&error, ConfigError::InvalidEnvKey(found) if found == name),
                "{name}: {error:?}"
            );
        }
    }

    #[test]
    fn out_of_range_values_are_rejected() {
        let cases = [
            ("KAGE__DATABASE__MAX_CONNECTIONS", "0", "max_connections"),
            ("KAGE__DATABASE__CONNECT_TIMEOUT", "0s", "connect_timeout"),
            ("KAGE__ARTIFACTS__RETENTION_DAYS", "0", "retention_days"),
            ("KAGE__ARTIFACTS__SWEEP_INTERVAL", "10ms", "sweep_interval"),
            ("KAGE__POLICIES__DIR", "\"\"", "policies.dir"),
            ("KAGE__ARTIFACTS__DIR", "\"\"", "artifacts.dir"),
        ];
        for (name, value, needle) in cases {
            let message = invalid(Config::load(None, with(&[(name, value)])));
            assert!(message.contains(needle), "{name}: {message}");
        }
    }

    #[test]
    fn the_database_url_must_be_a_postgres_url_and_is_never_echoed() {
        let message = invalid(Config::load(
            None,
            env(&[
                ("KAGE__DATABASE__URL", "mysql://root:hunter2@db/kage"),
                ("KAGE__SIDECAR__TOKEN", TOKEN),
                ("KAGE__SECRETS__MASTER_KEY", MASTER),
            ]),
        ));

        assert!(message.contains("postgres://"));
        assert!(!message.contains("hunter2"));

        let alternative = Config::load(
            None,
            env(&[
                ("KAGE__DATABASE__URL", "postgresql://kage@db/kage"),
                ("KAGE__SIDECAR__TOKEN", TOKEN),
                ("KAGE__SECRETS__MASTER_KEY", MASTER),
            ]),
        );
        assert!(alternative.is_ok());
    }

    #[test]
    fn the_effective_configuration_prints_with_every_secret_redacted() {
        let config = Config::load(
            None,
            with(&[
                ("KAGE__WEBSHARE__API_KEY", "ws-key-51aa"),
                ("KAGE__REPUTATION__ABUSEIPDB_KEY", "abuse-key-93bd"),
                (
                    "KAGE__PROXIES__STATIC_LIST",
                    "198.51.100.10:8080:user-c41d:pass-77aa",
                ),
            ]),
        )
        .unwrap();
        let secrets = [
            DB_URL,
            "db-pass-4f9a",
            TOKEN,
            MASTER,
            "ws-key-51aa",
            "abuse-key-93bd",
            "user-c41d",
            "pass-77aa",
        ];

        let redacted = config.redacted();
        let printed = serde_json::to_string_pretty(&redacted).unwrap();
        let debugged = format!("{config:?}");

        for secret in secrets {
            assert!(
                !printed.contains(secret),
                "serialized output leaks {secret}"
            );
            assert!(!debugged.contains(secret), "Debug output leaks {secret}");
        }
        for spec in SECRETS {
            let (section, key) = spec.key.split_once('.').unwrap();
            assert_eq!(redacted[section][key], REDACTED, "{}", spec.key);
        }
        // Everything that is not a secret is shown.
        assert_eq!(redacted["server"]["bind"], "0.0.0.0:8080");
        assert_eq!(redacted["database"]["max_connections"], 10);
        assert_eq!(redacted["database"]["connect_timeout"], "10s");
        assert_eq!(redacted["artifacts"]["retention_days"], 14);
        assert_eq!(redacted["policies"]["dir"], "policies");
        assert_eq!(redacted["security"]["allow_ip_targets"], false);
        assert_eq!(redacted["urls"]["trailing_slash"], "keep");
    }

    #[test]
    fn unset_optional_secrets_print_as_null() {
        let redacted = Config::load(None, required()).unwrap().redacted();

        assert!(redacted["webshare"]["api_key"].is_null());
        assert!(redacted["reputation"]["abuseipdb_key"].is_null());
        assert!(redacted["proxies"]["static_list"].is_null());
    }

    #[test]
    fn the_secret_table_matches_the_spec() {
        let names: Vec<&str> = SECRETS.iter().map(|spec| spec.env).collect();
        assert_eq!(
            names,
            [
                "KAGE__DATABASE__URL",
                "KAGE__SIDECAR__TOKEN",
                "KAGE__SECRETS__MASTER_KEY",
                "KAGE__WEBSHARE__API_KEY",
                "KAGE__REPUTATION__ABUSEIPDB_KEY",
                // Added in M2 for the static provider.
                "KAGE__PROXIES__STATIC_LIST",
            ]
        );
        for spec in SECRETS {
            let derived = format!("KAGE__{}", spec.key.replace('.', "__").to_ascii_uppercase());
            assert_eq!(spec.env, derived);
        }
    }

    /// `deploy/kage.example.yaml` documents every key at its default, so
    /// loading it must give exactly the built-in defaults.
    #[test]
    fn the_example_file_lists_every_key_at_its_default() {
        let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deploy/kage.example.yaml");

        let from_example = Config::load(Some(&example), required()).unwrap();
        let defaults = Config::load(None, required()).unwrap();

        assert_eq!(from_example.redacted(), defaults.redacted());
        // Every non-secret key is spelled out in the file.
        let text = std::fs::read_to_string(&example).unwrap();
        let documented: serde_json::Value = crate::yaml::from_str(&text).unwrap();
        let mut effective = defaults.redacted();
        for spec in SECRETS {
            let (section, key) = spec.key.split_once('.').unwrap();
            effective[section].as_object_mut().unwrap().remove(key);
        }
        effective
            .as_object_mut()
            .unwrap()
            .retain(|_, section| section.as_object().is_some_and(|keys| !keys.is_empty()));
        assert_eq!(documented, effective);
    }

    #[test]
    fn the_process_environment_is_readable() {
        // PATH exists in every environment the tests run in.
        assert!(process_env().iter().any(|(name, _)| name == "PATH"));
    }
}
