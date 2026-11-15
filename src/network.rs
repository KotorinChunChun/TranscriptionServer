use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::{
    collections::HashSet,
    net::{IpAddr, Ipv4Addr},
};
#[derive(Clone, Serialize)]
pub struct Interface {
    pub name: String,
    pub ip: Ipv4Addr,
    pub network: String,
}
#[derive(Clone)]
pub struct Network {
    base: u32,
    mask: u32,
    pub cidr: String,
}
impl Network {
    pub fn parse(value: &str) -> Result<Self> {
        let (ip, bits) = value
            .split_once('/')
            .context("IPv4/CIDRで指定してください")?;
        let ip: Ipv4Addr = ip.parse()?;
        let bits: u32 = bits.parse()?;
        if bits > 32 {
            bail!("ネットマスクが不正です")
        };
        let mask = u32::MAX.checked_shl(32 - bits).unwrap_or(0);
        let base = u32::from(ip) & mask;
        let valid = [(0x0a000000u32, 8), (0xac100000u32, 12), (0xc0a80000u32, 16)]
            .iter()
            .any(|(b, n)| bits >= *n && (base >> (32 - n)) == (*b >> (32 - n)));
        if !valid {
            bail!("信頼LANはプライベートIPv4のサブネットに限定してください")
        };
        Ok(Self {
            base,
            mask,
            cidr: format!("{}/{bits}", Ipv4Addr::from(base)),
        })
    }
    pub fn contains(&self, ip: IpAddr) -> bool {
        match ip {
            IpAddr::V4(v) => u32::from(v) & self.mask == self.base,
            _ => false,
        }
    }
}
pub fn interfaces() -> Result<Vec<Interface>> {
    let mut result = vec![];
    for row in if_addrs::get_if_addrs()? {
        if let if_addrs::IfAddr::V4(v) = row.addr {
            let bits = u32::from(v.netmask).count_ones();
            if let Ok(net) = Network::parse(&format!("{}/{bits}", v.ip)) {
                result.push(Interface {
                    name: row.name,
                    ip: v.ip,
                    network: net.cidr,
                })
            }
        }
    }
    result.sort_by_key(|r| r.ip);
    result.dedup_by_key(|r| r.ip);
    Ok(result)
}
pub struct Policy {
    pub https: bool,
    pub interfaces: Vec<Interface>,
    pub networks: Vec<Network>,
    hosts: HashSet<String>,
    origins: HashSet<String>,
    tls_hosts: Option<HashSet<String>>,
    pub lan: bool,
    pub bearer: bool,
    pub port: u16,
}
impl Policy {
    pub fn new(cfg: &crate::config::Config) -> Result<Self> {
        let interfaces = interfaces()?;
        let values = if cfg.allow_network.is_empty() {
            interfaces
                .iter()
                .map(|r| r.network.clone())
                .collect::<HashSet<_>>()
                .into_iter()
                .collect()
        } else {
            cfg.allow_network.clone()
        };
        let networks = values
            .iter()
            .map(|s| Network::parse(s))
            .collect::<Result<Vec<_>>>()?;
        if cfg.lan && (interfaces.is_empty() || networks.is_empty()) {
            bail!("LANアドレスがありません。ネットワークを確認するか--localを指定してください")
        }
        let mut hosts = HashSet::from(["127.0.0.1".into(), "localhost".into(), "[::1]".into()]);
        hosts.extend(interfaces.iter().map(|r| r.ip.to_string()));
        if let Ok(host) = std::env::var("COMPUTERNAME").or_else(|_| std::env::var("HOSTNAME")) {
            hosts.insert(host.to_lowercase());
        }
        let mut origins = HashSet::new();
        for value in &cfg.allow_origin {
            let url = url::Url::parse(value)?;
            if !matches!(url.scheme(), "http" | "https")
                || url.origin().ascii_serialization() != *value
                || !url.username().is_empty()
                || url.password().is_some()
            {
                bail!("allow-originは正確なhttp(s) originで指定してください")
            };
            origins.insert(value.clone());
        }
        let tls_hosts = cfg.tls_cert.as_ref().map(|path| {
            hosts
                .iter()
                .filter(|host| crate::tls::covers_host(path, host))
                .cloned()
                .collect()
        });
        Ok(Self {
            https: cfg.tls_cert.is_some(),
            interfaces,
            networks,
            hosts,
            origins,
            tls_hosts,
            lan: cfg.lan,
            bearer: cfg.server,
            port: cfg.port,
        })
    }
    pub fn peer(&self, ip: IpAddr) -> bool {
        if self.lan {
            ip.is_loopback() || self.networks.iter().any(|n| n.contains(ip))
        } else if self.bearer {
            true
        } else {
            ip.is_loopback()
        }
    }
    pub fn host(&self, host: &str) -> bool {
        if self.bearer {
            return true;
        }
        let Ok(authority) = host.parse::<axum::http::uri::Authority>() else {
            return false;
        };
        authority.port_u16() == Some(self.port)
            && self.hosts.contains(&authority.host().to_lowercase())
    }
    pub fn origin(&self, origin: Option<&str>) -> bool {
        let Some(origin) = origin else { return true };
        if self.origins.contains(origin) {
            return true;
        }
        let Ok(url) = url::Url::parse(origin) else {
            return false;
        };
        if url.origin().ascii_serialization() != origin
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return false;
        }
        if self.bearer {
            return false;
        }
        let authority = url::Position::BeforeHost..url::Position::AfterPort;
        self.host(&url[authority])
    }
    pub fn urls(&self, host: &str) -> Vec<String> {
        let scheme = if self.https { "https" } else { "http" };
        if matches!(host, "0.0.0.0" | "::") {
            self.interfaces
                .iter()
                .filter(|r| {
                    self.tls_hosts
                        .as_ref()
                        .is_none_or(|hosts| hosts.contains(&r.ip.to_string()))
                })
                .map(|r| format!("{scheme}://{}:{}/", r.ip, self.port))
                .collect()
        } else if !matches!(host, "127.0.0.1" | "localhost" | "::1") {
            if self
                .tls_hosts
                .as_ref()
                .is_none_or(|hosts| hosts.contains(host))
            {
                vec![format!("{scheme}://{host}:{}/", self.port)]
            } else {
                vec![]
            }
        } else {
            vec![]
        }
    }
}
pub fn equal(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes().zip(b.bytes()).fold(0u8, |v, (a, b)| v | (a ^ b)) == 0
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn subnet_and_host_limits() {
        assert!(Network::parse("0.0.0.0/0").is_err());
        let n = Network::parse("192.168.21.12/22").unwrap();
        assert!(n.contains("192.168.23.1".parse().unwrap()));
        assert!(!n.contains("192.168.24.1".parse().unwrap()));
        assert!(!equal("key", "kez"));
        assert!(equal("key", "key"));
    }
}
