//! IP reputation lookups (spec §7.4): a provider trait, and AbuseIPDB as
//! the first implementation. Without an API key there is no provider and no
//! lookup is ever sent (open item O3).
//!
//! AbuseIPDB's `check` endpoint was verified against
//! <https://docs.abuseipdb.com/> on 2026-10-08:
//! `GET https://api.abuseipdb.com/api/v2/check?ipAddress=…&maxAgeInDays=…`
//! with the headers `Key` and `Accept: application/json`. The free plan
//! allows 1,000 checks a day; twenty proxies looked up once a day use
//! twenty.
//!
//! Lookups go out directly, not through a proxy: they are about the proxy's
//! address, they do not come from it.

use std::fmt;
use std::net::IpAddr;
use std::time::Duration;

use async_trait::async_trait;
use kage_core::config::ReputationConfig;
use serde::Deserialize;
use url::Url;
use wreq::header::{ACCEPT, HeaderValue, RETRY_AFTER};

/// What the lookup client calls itself.
const USER_AGENT: &str = concat!("kage/", env!("CARGO_PKG_VERSION"));

/// Timeout of one lookup.
const TIMEOUT: Duration = Duration::from_secs(20);

/// What a reputation provider says about an address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reputation {
    /// Abuse score, 0 (clean) to 100.
    pub score: u8,
    /// How the provider classifies the address, for example
    /// `Fixed Line ISP` or `Data Center/Web Hosting/Transit`.
    pub usage_type: Option<String>,
    /// Number of abuse reports in the look-back window.
    pub reports: Option<u32>,
}

/// Why a reputation lookup failed. No variant carries the API key.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReputationError {
    /// The client could not be built from its configuration.
    #[error("the reputation client cannot be set up: {0}")]
    Setup(String),
    /// The request did not complete.
    #[error("the reputation provider could not be reached: {0}")]
    Transport(String),
    /// The provider refused the API key.
    #[error("the reputation provider refused the API key (HTTP {0})")]
    Unauthorized(u16),
    /// The daily allowance is used up.
    #[error("the reputation provider is rate limiting")]
    RateLimited {
        /// How long the provider asked to stay away, when it said.
        retry_after: Option<Duration>,
    },
    /// The provider answered with an error status.
    #[error("the reputation provider answered HTTP {0}")]
    Status(u16),
    /// The answer is not what the provider's documentation describes.
    #[error("the reputation provider's answer could not be read: {0}")]
    Decode(String),
}

/// Looks up the reputation of an address.
#[async_trait]
pub trait ReputationProvider: Send + Sync + fmt::Debug {
    /// The provider's name, for logs.
    fn name(&self) -> &'static str;

    /// Looks one address up.
    async fn lookup(&self, ip: IpAddr) -> Result<Reputation, ReputationError>;
}

/// AbuseIPDB's `check` endpoint.
pub struct AbuseIpDb {
    client: wreq::Client,
    base: Url,
    key: HeaderValue,
    max_age_days: u16,
}

impl fmt::Debug for AbuseIpDb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AbuseIpDb")
            .field("base", &self.base.as_str())
            .field("max_age_days", &self.max_age_days)
            .finish_non_exhaustive()
    }
}

impl AbuseIpDb {
    /// The client the configuration describes, or `None` when no API key is
    /// set: reputation lookups are disabled then.
    ///
    /// # Errors
    ///
    /// Fails when the key cannot be sent in a header or the HTTP client
    /// cannot be built.
    pub fn from_config(config: &ReputationConfig) -> Result<Option<Self>, ReputationError> {
        let Some(key) = &config.abuseipdb_key else {
            return Ok(None);
        };
        let mut key = HeaderValue::from_str(key.expose()).map_err(|_| {
            ReputationError::Setup(
                "the AbuseIPDB key contains characters that cannot be sent in a header".to_owned(),
            )
        })?;
        key.set_sensitive(true);
        let base = Url::parse(&config.base_url)
            .map_err(|_| ReputationError::Setup("reputation.base_url is not a URL".to_owned()))?;
        let client = wreq::Client::builder()
            .no_proxy()
            .user_agent(USER_AGENT)
            .redirect(wreq::redirect::Policy::none())
            .timeout(TIMEOUT)
            .connect_timeout(TIMEOUT)
            .build()
            .map_err(|error| ReputationError::Setup(error.to_string()))?;
        Ok(Some(Self {
            client,
            base,
            key,
            max_age_days: config.max_age_days,
        }))
    }
}

#[derive(Deserialize)]
struct CheckResponse {
    data: CheckData,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CheckData {
    abuse_confidence_score: i64,
    #[serde(default)]
    usage_type: Option<String>,
    #[serde(default)]
    total_reports: Option<u32>,
}

#[async_trait]
impl ReputationProvider for AbuseIpDb {
    fn name(&self) -> &'static str {
        "abuseipdb"
    }

    async fn lookup(&self, ip: IpAddr) -> Result<Reputation, ReputationError> {
        let mut url = self
            .base
            .join("check")
            .map_err(|_| ReputationError::Setup("cannot build the lookup URL".to_owned()))?;
        url.query_pairs_mut()
            .append_pair("ipAddress", &ip.to_string())
            .append_pair("maxAgeInDays", &self.max_age_days.to_string());

        let response = self
            .client
            .get(url.as_str())
            .header("Key", self.key.clone())
            .header(ACCEPT, "application/json")
            .send()
            .await
            .map_err(|error| ReputationError::Transport(error.to_string()))?;
        let status = response.status();
        match status.as_u16() {
            200..=299 => {}
            401 | 403 => return Err(ReputationError::Unauthorized(status.as_u16())),
            429 => {
                let retry_after = response
                    .headers()
                    .get(RETRY_AFTER)
                    .and_then(|value| value.to_str().ok())
                    .and_then(|text| text.trim().parse().ok())
                    .map(Duration::from_secs);
                return Err(ReputationError::RateLimited { retry_after });
            }
            other => return Err(ReputationError::Status(other)),
        }
        let body = response
            .bytes()
            .await
            .map_err(|error| ReputationError::Transport(error.to_string()))?;
        let parsed: CheckResponse = serde_json::from_slice(&body)
            .map_err(|error| ReputationError::Decode(error.to_string()))?;
        let score = u8::try_from(parsed.data.abuse_confidence_score.clamp(0, 100))
            .map_err(|_| ReputationError::Decode("the score is not a percentage".to_owned()))?;
        Ok(Reputation {
            score,
            usage_type: parsed
                .data
                .usage_type
                .map(|usage| usage.trim().to_owned())
                .filter(|usage| !usage.is_empty()),
            reports: parsed.data.total_reports,
        })
    }
}

#[cfg(test)]
mod tests {
    use kage_core::config::ReputationConfig;
    use kage_core::secret::Secret;

    use super::{AbuseIpDb, ReputationError, ReputationProvider};

    #[test]
    fn without_a_key_there_is_no_provider() {
        let config = ReputationConfig::default();

        assert!(AbuseIpDb::from_config(&config).unwrap().is_none());
    }

    #[test]
    fn with_a_key_the_provider_exists_and_never_prints_it() {
        let config = ReputationConfig {
            abuseipdb_key: Some(Secret::new("abuse-key-93bd")),
            ..ReputationConfig::default()
        };

        let provider = AbuseIpDb::from_config(&config).unwrap().unwrap();

        assert_eq!(provider.name(), "abuseipdb");
        let printed = format!("{provider:?}");
        assert!(printed.contains("api.abuseipdb.com"), "{printed}");
        assert!(!printed.contains("abuse-key-93bd"), "{printed}");
    }

    #[test]
    fn a_key_or_a_base_that_cannot_be_used_is_a_setup_error() {
        let broken_key = ReputationConfig {
            abuseipdb_key: Some(Secret::new("line\nbreak")),
            ..ReputationConfig::default()
        };
        let error = AbuseIpDb::from_config(&broken_key).unwrap_err();
        assert!(matches!(&error, ReputationError::Setup(_)), "{error}");
        assert!(!error.to_string().contains("line"), "{error}");

        let broken_base = ReputationConfig {
            abuseipdb_key: Some(Secret::new("abuse-key-93bd")),
            base_url: "not a url".to_owned(),
            ..ReputationConfig::default()
        };
        assert!(matches!(
            AbuseIpDb::from_config(&broken_base),
            Err(ReputationError::Setup(_))
        ));
    }
}
