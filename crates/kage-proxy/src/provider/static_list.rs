//! The operator's declared list as a provider (spec §7.1, ADR 0013): for
//! proxies of any vendor, without an API.
//!
//! It lists and does nothing else: no replacement, no log of replaced
//! proxies, no account, no stats, no echo of its own
//! ([`Capabilities::LIST_ONLY`]). A static proxy that keeps failing stays
//! in backoff until the operator edits the list.
//!
//! Every entry declares where its exit is (`country`, and optionally
//! `locale` and `timezone`): its persona is made from that, and a geo
//! lookup only verifies it. An entry without a country gets no persona and
//! serves as a plain exit.
//!
//! A list given as a file (`KAGE__PROXIES__STATIC_LIST_FILE`) is read
//! again at every listing, so an edit is picked up at the next sync; a
//! version that does not parse is [`ProviderError::InvalidList`] and the
//! pool keeps what it has. A list given in the variable needs a restart.

use std::path::PathBuf;

use async_trait::async_trait;
use kage_core::config::{ProxiesConfig, StaticListError, StaticProxy};
use kage_core::domain::ProxyProviderKind;

use super::{Capabilities, ProviderError, ProviderProxy, ProxyProvider, Result};

/// Serves the declared list. See the module documentation.
#[derive(Debug, Clone)]
pub struct StaticListProvider {
    proxies: Vec<StaticProxy>,
    file: Option<PathBuf>,
}

impl StaticListProvider {
    /// A provider over the given proxies.
    pub const fn new(proxies: Vec<StaticProxy>) -> Self {
        Self {
            proxies,
            file: None,
        }
    }

    /// A provider over the list in `proxies.static_list`, read again from
    /// `proxies.static_list_file` at every listing when it came from a
    /// file; an empty list when the secret is not set.
    ///
    /// # Errors
    ///
    /// Fails when the list is malformed. A configuration that loaded has a
    /// well-formed list.
    pub fn from_config(config: &ProxiesConfig) -> std::result::Result<Self, StaticListError> {
        let proxies = match &config.static_list {
            Some(list) => StaticProxy::parse_list(list.expose())?,
            None => Vec::new(),
        };
        Ok(Self {
            proxies,
            file: config.static_list_file.clone(),
        })
    }

    /// How many proxies the list held when the provider was built.
    pub const fn len(&self) -> usize {
        self.proxies.len()
    }

    /// Whether the list was empty when the provider was built.
    pub const fn is_empty(&self) -> bool {
        self.proxies.is_empty()
    }

    /// The list in force: the file's as it reads now, else the one the
    /// provider was built with.
    async fn current(&self) -> Result<Vec<StaticProxy>> {
        let Some(path) = &self.file else {
            return Ok(self.proxies.clone());
        };
        // The error names the file's state, never its content.
        let text = tokio::fs::read_to_string(path).await.map_err(|error| {
            ProviderError::InvalidList(format!("the file cannot be read ({})", error.kind()))
        })?;
        StaticProxy::parse_list(&text)
            .map_err(|error| ProviderError::InvalidList(error.to_string()))
    }
}

#[async_trait]
impl ProxyProvider for StaticListProvider {
    fn kind(&self) -> ProxyProviderKind {
        ProxyProviderKind::Static
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::LIST_ONLY
    }

    async fn list(&self) -> Result<Vec<ProviderProxy>> {
        Ok(self
            .current()
            .await?
            .into_iter()
            .map(|proxy| ProviderProxy {
                provider_ref: proxy.id,
                host: proxy.host,
                port: proxy.port,
                username: proxy.username,
                password: proxy.password,
                country: proxy.country,
                locale: proxy.locale,
                timezone: proxy.timezone,
                city: proxy.city,
                asn: proxy.asn,
                valid: None,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use kage_core::config::{ProxiesConfig, StaticProxy};
    use kage_core::domain::ProxyProviderKind;
    use kage_core::secret::Secret;

    use super::StaticListProvider;
    use crate::provider::{
        Capabilities, Operation, ProviderError, ProxyProvider, ReplaceSpec, ReplaceTicket,
    };
    use crate::test_support::proxy;

    const LIST: &str = "198.51.100.10:8080:user-a:pass-a:DE\n\
                        198.51.100.11:8080:user-a:pass-a:DE\n\
                        198.51.100.12:8081:user-b:pass-b:AT\n\
                        198.51.100.13:8081:user-b:pass-b\n";

    fn provider() -> StaticListProvider {
        StaticListProvider::new(StaticProxy::parse_list(LIST).unwrap())
    }

    #[tokio::test]
    async fn the_list_is_served_as_it_was_configured() {
        let provider = provider();
        assert_eq!(provider.kind(), ProxyProviderKind::Static);
        assert_eq!(provider.len(), 4);
        assert!(!provider.is_empty());

        let listed = provider.list().await.unwrap();

        assert_eq!(listed.len(), 4);
        assert_eq!(listed[0].provider_ref, "198.51.100.10:8080");
        assert_eq!(listed[0].host, "198.51.100.10");
        assert_eq!(listed[0].port, 8080);
        assert_eq!(listed[0].username.expose(), "user-a");
        assert_eq!(listed[0].password.expose(), "pass-a");
        assert_eq!(listed[0].country.unwrap().as_str(), "DE");
        assert_eq!(listed[0].valid, None);
        assert_eq!(listed[3].country, None);
        // Listing again gives the same proxies: a static list never changes.
        let again = provider.list().await.unwrap();
        assert_eq!(
            again
                .iter()
                .map(|proxy| &proxy.provider_ref)
                .collect::<Vec<_>>(),
            listed
                .iter()
                .map(|proxy| &proxy.provider_ref)
                .collect::<Vec<_>>()
        );
    }

    /// A declared list lists and does nothing else: every optional
    /// operation answers that it is unsupported, as its capabilities say.
    #[tokio::test]
    async fn a_static_list_only_lists() {
        let provider = provider();
        assert_eq!(provider.capabilities(), Capabilities::LIST_ONLY);
        let unsupported =
            |operation| ProviderError::unsupported(ProxyProviderKind::Static, operation);

        assert_eq!(
            provider
                .replace(&proxy("198.51.100.10:8080"), ReplaceSpec::default())
                .await
                .unwrap_err(),
            unsupported(Operation::Replacement)
        );
        assert_eq!(
            provider
                .replace_status(&ReplaceTicket("1".to_owned()))
                .await
                .unwrap_err(),
            unsupported(Operation::Replacement)
        );
        assert_eq!(
            provider.replaced(Utc::now()).await.unwrap_err(),
            unsupported(Operation::ReplacedLog)
        );
        assert_eq!(
            provider.account().await.unwrap_err(),
            unsupported(Operation::Account)
        );
        assert_eq!(
            provider.stats(Utc::now()).await.unwrap_err(),
            unsupported(Operation::Stats)
        );
        assert_eq!(
            unsupported(Operation::ReplacedLog).to_string(),
            "the static provider does not support replaced_log"
        );
    }

    #[tokio::test]
    async fn the_provider_is_built_from_configuration() {
        let mut config = ProxiesConfig::default();
        let empty = StaticListProvider::from_config(&config).unwrap();
        assert!(empty.is_empty());
        assert_eq!(empty.list().await.unwrap().len(), 0);

        config.static_list = Some(Secret::new(LIST));
        assert_eq!(StaticListProvider::from_config(&config).unwrap().len(), 4);

        config.static_list = Some(Secret::new("not-a-proxy"));
        let error = StaticListProvider::from_config(&config).unwrap_err();
        assert!(error.to_string().contains("line 1"), "{error}");
    }

    #[test]
    fn nothing_prints_the_credentials() {
        let printed = format!("{:?}", provider());

        assert!(!printed.contains("pass-a"), "{printed}");
        assert!(!printed.contains("user-a"), "{printed}");
    }
}
