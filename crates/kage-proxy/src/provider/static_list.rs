//! A fixed proxy list as a provider: for tests and for vendors without an
//! API (spec §7.1).

use std::collections::BTreeMap;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use kage_core::config::{ProxiesConfig, StaticListError, StaticProxy};
use kage_core::domain::{Proxy, ProxyProviderKind};

use super::{
    AccountInfo, ProviderError, ProviderProxy, ProxyActivity, ProxyProvider, ReplaceSpec,
    ReplaceState, ReplaceTicket, ReplacedProxy, Result,
};

/// Serves a fixed list of proxies. It cannot replace a proxy: a static
/// proxy that fails stays in backoff until an operator edits the list.
#[derive(Debug, Clone)]
pub struct StaticListProvider {
    proxies: Vec<StaticProxy>,
}

impl StaticListProvider {
    /// A provider over the given proxies.
    pub const fn new(proxies: Vec<StaticProxy>) -> Self {
        Self { proxies }
    }

    /// A provider over the list in `proxies.static_list`; an empty list
    /// when the secret is not set.
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
        Ok(Self::new(proxies))
    }

    /// How many proxies the list holds.
    pub const fn len(&self) -> usize {
        self.proxies.len()
    }

    /// Whether the list is empty.
    pub const fn is_empty(&self) -> bool {
        self.proxies.is_empty()
    }

    const fn unsupported(operation: &'static str) -> ProviderError {
        ProviderError::Unsupported {
            provider: ProxyProviderKind::Static,
            operation,
        }
    }
}

#[async_trait]
impl ProxyProvider for StaticListProvider {
    fn kind(&self) -> ProxyProviderKind {
        ProxyProviderKind::Static
    }

    async fn list(&self) -> Result<Vec<ProviderProxy>> {
        Ok(self
            .proxies
            .iter()
            .map(|proxy| ProviderProxy {
                provider_ref: proxy.id.clone(),
                host: proxy.host.clone(),
                port: proxy.port,
                username: proxy.username.clone(),
                password: proxy.password.clone(),
                country: proxy.country,
                city: proxy.city.clone(),
                asn: proxy.asn.clone(),
                valid: None,
            })
            .collect())
    }

    async fn replace(&self, _proxy: &Proxy, _want: ReplaceSpec) -> Result<ReplaceTicket> {
        Err(Self::unsupported("replacement"))
    }

    async fn replace_status(&self, _ticket: &ReplaceTicket) -> Result<ReplaceState> {
        Err(Self::unsupported("replacement"))
    }

    async fn replaced(&self, _since: DateTime<Utc>) -> Result<Vec<ReplacedProxy>> {
        Ok(Vec::new())
    }

    async fn account(&self) -> Result<AccountInfo> {
        let mut countries = BTreeMap::new();
        for proxy in &self.proxies {
            if let Some(country) = proxy.country {
                *countries.entry(country.as_str().to_owned()).or_insert(0) += 1;
            }
        }
        Ok(AccountInfo {
            proxy_type: Some("static".to_owned()),
            proxy_count: Some(u32::try_from(self.proxies.len()).unwrap_or(u32::MAX)),
            countries,
            // A static list has no replacements to hand out.
            replacements_total: Some(0),
            replacements_used: Some(0),
            replacements_available: Some(0),
            ..AccountInfo::default()
        })
    }

    async fn stats(&self, _since: DateTime<Utc>) -> Result<Vec<ProxyActivity>> {
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use kage_core::config::{ProxiesConfig, StaticProxy};
    use kage_core::domain::ProxyProviderKind;
    use kage_core::secret::Secret;

    use super::StaticListProvider;
    use crate::provider::{ProviderError, ProxyProvider, ReplaceSpec, ReplaceTicket};
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

    #[tokio::test]
    async fn the_account_counts_proxies_and_offers_no_replacements() {
        let account = provider().account().await.unwrap();

        assert_eq!(account.proxy_count, Some(4));
        assert_eq!(account.countries["DE"], 2);
        assert_eq!(account.countries["AT"], 1);
        assert_eq!(account.countries.len(), 2);
        assert_eq!(account.replacements_available, Some(0));
        assert_eq!(account.auto_replace, None);
        assert_eq!(account.plan_id, None);
    }

    #[tokio::test]
    async fn a_static_list_cannot_replace_and_has_no_history() {
        let provider = provider();
        let unsupported = ProviderError::Unsupported {
            provider: ProxyProviderKind::Static,
            operation: "replacement",
        };

        assert_eq!(
            provider
                .replace(&proxy("198.51.100.10:8080"), ReplaceSpec::default())
                .await
                .unwrap_err(),
            unsupported
        );
        assert_eq!(
            provider
                .replace_status(&ReplaceTicket("1".to_owned()))
                .await
                .unwrap_err(),
            unsupported
        );
        assert_eq!(provider.replaced(Utc::now()).await.unwrap(), []);
        assert_eq!(provider.stats(Utc::now()).await.unwrap(), []);
    }

    #[tokio::test]
    async fn the_provider_is_built_from_configuration() {
        let mut config = ProxiesConfig::default();
        let empty = StaticListProvider::from_config(&config).unwrap();
        assert!(empty.is_empty());
        assert_eq!(empty.account().await.unwrap().proxy_count, Some(0));

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
