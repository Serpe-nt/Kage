//! The host's own public address, which the leak check compares every exit
//! address against (spec §7.4).
//!
//! It is read with one direct request, without a proxy, at start and then
//! hourly. The default endpoint is the proxy provider's own echo: the
//! provider sees the host's address on every API call and every proxy
//! connection anyway, so the lookup tells nobody anything new. This is the
//! only request besides the provider API that leaves the host directly, and
//! it goes to a configured echo, never to a target.

use std::net::IpAddr;
use std::sync::{PoisonError, RwLock};
use std::time::Duration;

use chrono::{DateTime, Utc};

use super::probe::parse_ip;

/// What the lookup client calls itself.
const USER_AGENT: &str = concat!("kage/", env!("CARGO_PKG_VERSION"));

/// Longest answer an echo may send.
const MAX_BODY_BYTES: usize = 4 * 1024;

/// Why the host address could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HostIpError {
    /// The HTTP client could not be built.
    #[error("cannot build the HTTP client: {0}")]
    Setup(String),
    /// The echo could not be reached.
    #[error("the echo could not be reached: {0}")]
    Transport(String),
    /// The echo answered with an error status.
    #[error("the echo answered HTTP {0}")]
    Status(u16),
    /// The echo's answer holds no address.
    #[error("the echo's answer holds no address")]
    NoAddress,
}

/// The host's public address as last read.
pub struct HostIp {
    client: wreq::Client,
    url: String,
    current: RwLock<Option<(IpAddr, DateTime<Utc>)>>,
}

impl std::fmt::Debug for HostIp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostIp")
            .field("url", &self.url)
            .field("learned_at", &self.learned_at())
            .finish_non_exhaustive()
    }
}

impl HostIp {
    /// A lookup against `url`. Nothing is read yet.
    ///
    /// # Errors
    ///
    /// Fails when the HTTP client cannot be built.
    pub fn new(url: impl Into<String>, timeout: Duration) -> Result<Self, HostIpError> {
        let client = wreq::Client::builder()
            // Directly, and never through a proxy named in the environment:
            // the point is to see the host's own address.
            .no_proxy()
            .user_agent(USER_AGENT)
            .redirect(wreq::redirect::Policy::none())
            .timeout(timeout)
            .connect_timeout(timeout)
            .build()
            .map_err(|error| HostIpError::Setup(error.to_string()))?;
        Ok(Self {
            client,
            url: url.into(),
            current: RwLock::new(None),
        })
    }

    /// A lookup with a fixed answer, for tests and for hosts whose address
    /// the operator knows.
    ///
    /// # Errors
    ///
    /// Fails when the HTTP client cannot be built.
    pub fn fixed(ip: IpAddr, at: DateTime<Utc>) -> Result<Self, HostIpError> {
        let host = Self::new("http://127.0.0.1:9/", Duration::from_secs(1))?;
        *host.current.write().unwrap_or_else(PoisonError::into_inner) = Some((ip, at));
        Ok(host)
    }

    /// The address as last read, if it ever was.
    pub fn current(&self) -> Option<IpAddr> {
        self.read().map(|(ip, _)| ip)
    }

    /// When the address was last read.
    pub fn learned_at(&self) -> Option<DateTime<Utc>> {
        self.read().map(|(_, at)| at)
    }

    fn read(&self) -> Option<(IpAddr, DateTime<Utc>)> {
        *self.current.read().unwrap_or_else(PoisonError::into_inner)
    }

    /// Reads the address again. On failure the address read before stays in
    /// force: a stale address still catches a leak, no address catches none.
    ///
    /// # Errors
    ///
    /// Fails when the echo cannot be read; [`HostIp::current`] is unchanged
    /// then.
    pub async fn refresh(&self, now: DateTime<Utc>) -> Result<IpAddr, HostIpError> {
        let response = self
            .client
            .get(self.url.as_str())
            .send()
            .await
            .map_err(|error| HostIpError::Transport(error.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            return Err(HostIpError::Status(status.as_u16()));
        }
        let body = response
            .bytes()
            .await
            .map_err(|error| HostIpError::Transport(error.to_string()))?;
        if body.len() > MAX_BODY_BYTES {
            return Err(HostIpError::NoAddress);
        }
        let ip = parse_ip(&String::from_utf8_lossy(&body)).ok_or(HostIpError::NoAddress)?;
        *self.current.write().unwrap_or_else(PoisonError::into_inner) = Some((ip, now));
        Ok(ip)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{HostIp, HostIpError};
    use crate::test_support::noon;

    #[test]
    fn nothing_is_known_before_the_first_read() {
        let host = HostIp::new("https://ipv4.webshare.io/", Duration::from_secs(5)).unwrap();

        assert_eq!(host.current(), None);
        assert_eq!(host.learned_at(), None);
    }

    #[test]
    fn a_fixed_address_is_known_at_once() {
        let host = HostIp::fixed("192.0.2.77".parse().unwrap(), noon()).unwrap();

        assert_eq!(host.current(), Some("192.0.2.77".parse().unwrap()));
        assert_eq!(host.learned_at(), Some(noon()));
    }

    #[tokio::test]
    async fn a_failed_read_keeps_the_address_read_before() {
        // Port 9 (discard) on loopback: nothing answers HTTP there.
        let host = HostIp::fixed("192.0.2.77".parse().unwrap(), noon()).unwrap();

        let error = host
            .refresh(noon() + chrono::Duration::hours(1))
            .await
            .unwrap_err();

        assert!(matches!(error, HostIpError::Transport(_)), "{error}");
        assert_eq!(host.current(), Some("192.0.2.77".parse().unwrap()));
        assert_eq!(host.learned_at(), Some(noon()));
    }
}
