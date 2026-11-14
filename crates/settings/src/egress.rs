//! `sandbox.egress_proxy` as a settings file writes it — an address, or an
//! object with an optional SOCKS address and `no_proxy` — read into the
//! engine's [`EgressProxy`]. A value that does not read is a value the merge
//! leaves out, so a mistyped proxy leaves `network_mode: proxy` with nothing
//! to reach, which cuts the network.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use base::interface::exec::EgressProxy;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Wire", into = "Wire")]
pub struct EgressProxySetting(pub EgressProxy);

impl schemars::JsonSchema for EgressProxySetting {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "EgressProxy".into()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        Wire::json_schema(generator)
    }
}

/// `"127.0.0.1:3128"` is `{"http": "127.0.0.1:3128"}`.
#[derive(Serialize, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
enum Wire {
    /// The HTTP proxy's `ip:port` or `localhost:port`.
    Address(String),
    Object(EgressProxyObject),
}

#[derive(Serialize, Deserialize, schemars::JsonSchema)]
struct EgressProxyObject {
    /// An HTTP proxy that takes `CONNECT`, as `ip:port` or `localhost:port`.
    http: String,
    /// A SOCKS5 proxy for `ALL_PROXY`, written the same way; its port must
    /// differ from `http`'s.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    socks: Option<String>,
    /// `NO_PROXY` for the command. Empty — the default — is set as empty, so
    /// even `localhost` goes through the proxy; the sandbox lets nothing past
    /// it either way.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    no_proxy: Vec<String>,
}

impl TryFrom<Wire> for EgressProxySetting {
    type Error = String;

    fn try_from(wire: Wire) -> Result<Self, String> {
        let full = match wire {
            Wire::Address(http) => EgressProxyObject {
                http,
                socks: None,
                no_proxy: Vec::new(),
            },
            Wire::Object(full) => full,
        };
        let http = proxy_address(&full.http)?;
        let socks = full.socks.as_deref().map(proxy_address).transpose()?;
        if socks.is_some_and(|s| s.port() == http.port()) {
            return Err(format!(
                "egress_proxy: socks and http share port {}",
                http.port()
            ));
        }
        if let Some(bad) = full
            .no_proxy
            .iter()
            .find(|n| n.is_empty() || n.contains(',') || n.contains(char::is_whitespace))
        {
            return Err(format!("egress_proxy.no_proxy: {bad:?} is not one host"));
        }
        Ok(Self(EgressProxy {
            http,
            socks,
            no_proxy: full.no_proxy,
        }))
    }
}

impl From<EgressProxySetting> for Wire {
    fn from(EgressProxySetting(proxy): EgressProxySetting) -> Self {
        Wire::Object(EgressProxyObject {
            http: proxy.http.to_string(),
            socks: proxy.socks.map(|s| s.to_string()),
            no_proxy: proxy.no_proxy,
        })
    }
}

/// `ip:port`, or `localhost:port` for `127.0.0.1`. No other name: resolving
/// one at load would pin whatever it resolved to then.
fn proxy_address(text: &str) -> Result<SocketAddr, String> {
    let text = text.trim();
    let addr = match text.strip_prefix("localhost:") {
        Some(port) => port
            .parse::<u16>()
            .map(|port| SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port))
            .map_err(|e| format!("egress_proxy: {text:?}: {e}"))?,
        None => text
            .parse::<SocketAddr>()
            .map_err(|_| format!("egress_proxy: {text:?} is not `ip:port` or `localhost:port`"))?,
    };
    if addr.port() == 0 || addr.ip().is_unspecified() {
        return Err(format!("egress_proxy: {text:?} names no one proxy"));
    }
    Ok(addr)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn read(value: serde_json::Value) -> Result<EgressProxy, String> {
        serde_json::from_value::<EgressProxySetting>(value)
            .map(|s| s.0)
            .map_err(|e| e.to_string())
    }

    #[test]
    fn an_address_is_the_http_proxy_and_localhost_is_the_loopback() {
        let short = read(json!("127.0.0.1:3128")).unwrap();
        assert_eq!(short, read(json!({"http": "127.0.0.1:3128"})).unwrap());
        assert_eq!(short, read(json!("localhost:3128")).unwrap());
        assert_eq!(short.http, "127.0.0.1:3128".parse().unwrap());
        assert!(short.socks.is_none() && short.no_proxy.is_empty());
    }

    #[test]
    fn the_object_form_carries_socks_and_no_proxy() {
        let full = read(json!({
            "http": "10.0.0.5:3128",
            "socks": "[::1]:1080",
            "no_proxy": ["internal.example"],
        }))
        .unwrap();
        assert_eq!(full.socks, Some("[::1]:1080".parse().unwrap()));
        assert_eq!(full.no_proxy, ["internal.example"]);
        let again = serde_json::to_value(EgressProxySetting(full.clone())).unwrap();
        assert_eq!(read(again).unwrap(), full);
    }

    #[test]
    fn what_names_no_one_proxy_does_not_read() {
        for bad in [
            json!("proxy.example:3128"),
            json!("127.0.0.1"),
            json!("127.0.0.1:0"),
            json!("0.0.0.0:3128"),
            json!("localhost:http"),
            json!({"socks": "127.0.0.1:1080"}),
            json!({"http": "127.0.0.1:3128", "socks": "127.0.0.2:3128"}),
            json!({"http": "127.0.0.1:3128", "no_proxy": ["a,b"]}),
        ] {
            assert!(read(bad.clone()).is_err(), "{bad} was read");
        }
    }
}
