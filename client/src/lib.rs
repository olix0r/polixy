#![deny(warnings, rust_2018_idioms)]
#![forbid(unsafe_code)]

pub mod http_api;
mod watch_ports;

pub use self::watch_ports::{watch_ports, PortWatch};
use anyhow::{anyhow, bail, Context, Error, Result};
use futures::prelude::*;
use ipnet::IpNet;
use linkerd2_proxy_api::inbound::{
    self as proto, inbound_server_discovery_client::InboundServerDiscoveryClient,
};
use std::{
    collections::{HashMap, HashSet},
    convert::TryInto,
    net::IpAddr,
};
use tokio::time;
use tracing::{instrument, trace};

#[derive(Clone, Debug)]
pub struct Client {
    client: InboundServerDiscoveryClient<tonic::transport::Channel>,
}

#[derive(Clone, Debug)]
pub struct Inbound {
    pub authorizations: Vec<Authz>,
    pub labels: HashMap<String, String>,
    pub protocol: Protocol,
}

#[derive(Copy, Clone, Debug)]
pub enum Protocol {
    Detect { timeout: time::Duration },
    Http1,
    Http2,
    Grpc,
    Opaque,
    Tls,
}

#[derive(Clone, Debug)]
pub struct Authz {
    networks: Vec<Network>,
    authn: Authn,
    labels: HashMap<String, String>,
}

#[derive(Clone, Debug, Default)]
pub struct Network {
    net: IpNet,
    except: Vec<IpNet>,
}

#[derive(Clone, Debug)]
pub enum Authn {
    Unauthenticated,
    TlsUnauthenticated,
    TlsAuthenticated {
        identities: HashSet<String>,
        suffixes: Vec<Suffix>,
    },
}

#[derive(Clone, Debug)]
pub struct Suffix {
    ends_with: String,
}

// === impl Client ===

impl Client {
    pub async fn connect<D>(dst: D) -> Result<Self>
    where
        D: std::convert::TryInto<tonic::transport::Endpoint>,
        D::Error: Into<Box<dyn std::error::Error + Send + Sync + 'static>>,
    {
        let client = InboundServerDiscoveryClient::connect(dst).await?;
        Ok(Client { client })
    }

    #[instrument(skip(self))]
    pub async fn get_port(&mut self, workload: String, port: u16) -> Result<Inbound> {
        let req = tonic::Request::new(proto::PortSpec {
            workload,
            port: port.into(),
        });

        let proto = self.client.get_port(req).await?.into_inner();
        trace!(?proto);
        proto.try_into()
    }

    #[instrument(skip(self))]
    pub async fn watch_port(
        &mut self,
        workload: String,
        port: u16,
    ) -> Result<impl Stream<Item = Result<Inbound>>> {
        let req = tonic::Request::new(proto::PortSpec {
            workload,
            port: port.into(),
        });

        let rsp = self.client.watch_port(req).await?;

        let updates = rsp.into_inner().map_err(Into::into).and_then(|proto| {
            trace!(?proto);
            future::ready(proto.try_into())
        });

        Ok(updates)
    }
}

// === impl Inbound ===

impl Inbound {
    #[instrument(skip(self))]
    pub fn check_non_tls(&self, client_ip: IpAddr) -> Option<&HashMap<String, String>> {
        trace!(authorizations = %self.authorizations.len());
        for Authz {
            networks,
            authn,
            labels,
        } in self.authorizations.iter()
        {
            trace!(?authn);
            trace!(?networks);
            trace!(?labels);
            if matches!(authn, Authn::Unauthenticated)
                && networks.iter().any(|net| net.contains(&client_ip))
            {
                trace!("Match found");
                return Some(labels);
            }
        }

        trace!("No match found");
        None
    }

    #[instrument(skip(self))]
    pub fn check_tls(
        &self,
        client_ip: IpAddr,
        id: Option<&str>,
    ) -> Option<&HashMap<String, String>> {
        trace!(authorizations = %self.authorizations.len());
        for Authz {
            networks,
            authn,
            labels,
        } in self.authorizations.iter()
        {
            trace!(?networks);
            if networks.iter().any(|net| net.contains(&client_ip)) {
                trace!("Matches network");
                trace!(?authn);
                match authn {
                    Authn::Unauthenticated | Authn::TlsUnauthenticated => {
                        trace!("Match found");
                        trace!(?labels);
                        return Some(labels);
                    }
                    Authn::TlsAuthenticated {
                        identities,
                        suffixes,
                    } => {
                        if let Some(id) = id {
                            trace!(identities = %identities.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(","));
                            if identities.contains(id)
                                || suffixes.iter().any(|sfx| sfx.contains(id))
                            {
                                trace!("Match found");
                                trace!(?labels);
                                return Some(labels);
                            }
                        }
                    }
                }
            }
        }

        trace!("No match found");
        None
    }
}

impl std::convert::TryFrom<proto::Server> for Inbound {
    type Error = Error;

    fn try_from(proto: proto::Server) -> Result<Self> {
        let protocol = match proto.protocol {
            Some(proto::ProxyProtocol { kind: Some(k) }) => match k {
                proto::proxy_protocol::Kind::Detect(proto::proxy_protocol::Detect { timeout }) => {
                    Protocol::Detect {
                        timeout: match timeout {
                            Some(t) => t
                                .try_into()
                                .map_err(|t| anyhow!("negative detect timeout: {:?}", t))?,
                            None => bail!("protocol missing detect timeout"),
                        },
                    }
                }
                proto::proxy_protocol::Kind::Http1(_) => Protocol::Http1,
                proto::proxy_protocol::Kind::Http2(_) => Protocol::Http2,
                proto::proxy_protocol::Kind::Grpc(_) => Protocol::Grpc,
                proto::proxy_protocol::Kind::Opaque(_) => Protocol::Opaque,
                proto::proxy_protocol::Kind::Tls(_) => Protocol::Tls,
            },
            _ => bail!("proxy protocol missing"),
        };

        let authorizations = proto
            .authorizations
            .into_iter()
            .map(
                |proto::Authz {
                     labels,
                     authentication,
                     networks,
                 }| {
                    if networks.is_empty() {
                        bail!("networks missing");
                    }
                    let networks = networks
                        .into_iter()
                        .map(|proto::Network { net, except }| {
                            let net = net
                                .ok_or_else(|| anyhow!("network missing"))?
                                .try_into()
                                .context("invalid network")?;
                            let except = except
                                .into_iter()
                                .map(|net| net.try_into().context("invalid network"))
                                .collect::<Result<Vec<IpNet>>>()?;
                            Ok(Network { net, except })
                        })
                        .collect::<Result<Vec<_>>>()?;

                    let authn = match authentication.and_then(|proto::Authn { permit }| permit) {
                        Some(proto::authn::Permit::Unauthenticated(_)) => Authn::Unauthenticated,
                        Some(proto::authn::Permit::MeshTls(proto::authn::PermitMeshTls {
                            clients,
                        })) => match clients {
                            Some(proto::authn::permit_mesh_tls::Clients::Unauthenticated(_)) => {
                                Authn::TlsUnauthenticated
                            }
                            Some(proto::authn::permit_mesh_tls::Clients::Identities(
                                proto::authn::permit_mesh_tls::PermitClientIdentities {
                                    identities,
                                    suffixes,
                                },
                            )) => Authn::TlsAuthenticated {
                                identities: identities
                                    .into_iter()
                                    .map(|proto::Identity { name }| name)
                                    .collect(),
                                suffixes: suffixes
                                    .into_iter()
                                    .map(|proto::IdentitySuffix { parts }| Suffix::from(parts))
                                    .collect(),
                            },
                            None => bail!("no clients permitted"),
                        },
                        authn => bail!("no authentication provided: {:?}", authn),
                    };

                    Ok(Authz {
                        networks,
                        authn,
                        labels,
                    })
                },
            )
            .collect::<Result<Vec<_>>>()?;

        Ok(Inbound {
            labels: proto.labels,
            authorizations,
            protocol,
        })
    }
}

// === impl Network ===

impl Network {
    pub fn contains(&self, addr: &IpAddr) -> bool {
        self.net.contains(addr) && !self.except.iter().any(|net| net.contains(addr))
    }
}

// === impl Suffix ===

impl From<Vec<String>> for Suffix {
    fn from(parts: Vec<String>) -> Self {
        let ends_with = if parts.is_empty() {
            "".to_string()
        } else {
            format!(".{}", parts.join("."))
        };
        Suffix { ends_with }
    }
}

impl Suffix {
    pub fn contains(&self, name: &str) -> bool {
        name.ends_with(&self.ends_with)
    }
}

#[cfg(test)]
mod network_tests {
    use super::Network;
    use ipnet::{IpNet, Ipv4Net, Ipv6Net};
    use quickcheck::{quickcheck, TestResult};
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    quickcheck! {
        fn contains_v4(addr: Ipv4Addr, exclude: Option<Ipv4Addr>) -> TestResult {
            let net = Network {
                net: Ipv4Net::default().into(),
                except: exclude.into_iter().map(|a| IpNet::from(IpAddr::V4(a))).collect(),
            };

            if let Some(e) = exclude {
                if net.contains(&e.into()) {
                    return TestResult::failed();
                }
                if addr == e {
                    return TestResult::passed();
                }
            }
            TestResult::from_bool(net.contains(&addr.into()))
        }

        fn contains_v6(addr: Ipv6Addr, exclude: Option<Ipv6Addr>) -> TestResult {
            let net = Network {
                net: Ipv6Net::default().into(),
                except: exclude.into_iter().map(|a| IpNet::from(IpAddr::V6(a))).collect(),
            };

            if let Some(e) = exclude {
                if net.contains(&e.into()) {
                    return TestResult::failed();
                }
                if addr == e {
                    return TestResult::passed();
                }
            }
            TestResult::from_bool(net.contains(&addr.into()))
        }
    }
}
