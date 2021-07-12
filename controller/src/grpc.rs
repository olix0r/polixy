use crate::{lookup, KubeletIps, ServerRxRx};
use futures::prelude::*;
use linkerd2_proxy_api::inbound::{
    self as proto,
    inbound_server_discovery_server::{InboundServerDiscovery, InboundServerDiscoveryServer},
};
use polixy_controller_core::{
    ClientAuthentication, ClientAuthorization, ClientIdentityMatch, ClientNetwork, InboundServer,
    ProxyProtocol,
};
use tracing::trace;

#[derive(Clone, Debug)]
pub struct Server {
    lookup: lookup::Reader,
    drain: drain::Watch,
}

impl Server {
    pub fn new(lookup: lookup::Reader, drain: drain::Watch) -> Self {
        Self { lookup, drain }
    }

    pub async fn serve(
        self,
        addr: std::net::SocketAddr,
        shutdown: impl std::future::Future<Output = ()>,
    ) -> Result<(), tonic::transport::Error> {
        tonic::transport::Server::builder()
            .add_service(InboundServerDiscoveryServer::new(self))
            .serve_with_shutdown(addr, shutdown)
            .await
    }

    fn lookup(&self, workload: String, port: u32) -> Result<lookup::PodPort, tonic::Status> {
        // Parse a workload name in the form namespace:name.
        let (ns, name) = match workload.split_once(':') {
            None => {
                return Err(tonic::Status::invalid_argument(format!(
                    "Invalid workload: {}",
                    workload
                )));
            }
            Some((ns, pod)) if ns.is_empty() || pod.is_empty() => {
                return Err(tonic::Status::invalid_argument(format!(
                    "Invalid workload: {}",
                    workload
                )));
            }
            Some((ns, pod)) => (ns, pod),
        };

        // Ensure that the port is in the valid range.
        let port = {
            if port == 0 || port > std::u16::MAX as u32 {
                return Err(tonic::Status::invalid_argument(format!(
                    "Invalid port: {}",
                    port
                )));
            }
            port as u16
        };

        // Lookup the configuration for an inbound port. If the pod hasn't (yet)
        // been indexed, return a Not Found error.
        self.lookup.lookup(&ns, &name, port).ok_or_else(|| {
            tonic::Status::not_found(format!("unknown pod ns={} name={} port={}", ns, name, port))
        })
    }
}

#[async_trait::async_trait]
impl InboundServerDiscovery for Server {
    async fn get_port(
        &self,
        req: tonic::Request<proto::PortSpec>,
    ) -> Result<tonic::Response<proto::Server>, tonic::Status> {
        let proto::PortSpec { workload, port } = req.into_inner();
        let lookup::PodPort { kubelet_ips, rx } = self.lookup(workload, port)?;

        let kubelet = kubelet_authz(kubelet_ips);

        let server = to_server(&kubelet, rx.borrow().borrow().clone());
        Ok(tonic::Response::new(server))
    }

    type WatchPortStream = BoxWatchStream;

    async fn watch_port(
        &self,
        req: tonic::Request<proto::PortSpec>,
    ) -> Result<tonic::Response<BoxWatchStream>, tonic::Status> {
        let proto::PortSpec { workload, port } = req.into_inner();
        let lookup::PodPort { kubelet_ips, rx } = self.lookup(workload, port)?;

        Ok(tonic::Response::new(response_stream(
            kubelet_ips,
            self.drain.clone(),
            rx,
        )))
    }
}

type BoxWatchStream =
    std::pin::Pin<Box<dyn Stream<Item = Result<proto::Server, tonic::Status>> + Send + Sync>>;

fn response_stream(
    kubelet_ips: KubeletIps,
    drain: drain::Watch,
    mut port_rx: ServerRxRx,
) -> BoxWatchStream {
    let kubelet = kubelet_authz(kubelet_ips);

    Box::pin(async_stream::try_stream! {
        tokio::pin! {
            let shutdown = drain.signaled();
        }

        let mut server_rx = port_rx.borrow().clone();
        let mut prior = None;
        loop {
            let server = server_rx.borrow().clone();

            // Deduplicate identical updates (i.e., especially after the controller reconnects to
            // the k8s API).
            if prior.as_ref() != Some(&server) {
                prior = Some(server.clone());
                yield to_server(&kubelet, server);
            }

            tokio::select! {
                // When the port is updated with a new server, update the server watch.
                res = port_rx.changed() => {
                    // If the port watch closed, end the stream.
                    if res.is_err() {
                        return;
                    }
                    // Otherwise, update the server watch.
                    server_rx = port_rx.borrow().clone();
                }

                // Wait for the current server watch to update.
                res = server_rx.changed() => {
                    // If the server was deleted (the server watch closes), get an updated server
                    // watch.
                    if res.is_err() {
                        server_rx = port_rx.borrow().clone();
                    }
                }

                // If the server starts shutting down, close the stream so that it doesn't hold the
                // server open.
                _ = (&mut shutdown) => {
                    return;
                }
            }
        }
    })
}

fn to_server(kubelet_authz: &proto::Authz, srv: InboundServer) -> proto::Server {
    // Convert the protocol object into a protobuf response.
    let protocol = proto::ProxyProtocol {
        kind: match srv.protocol {
            ProxyProtocol::Detect { timeout } => Some(proto::proxy_protocol::Kind::Detect(
                proto::proxy_protocol::Detect {
                    timeout: Some(timeout.into()),
                },
            )),
            ProxyProtocol::Http1 => Some(proto::proxy_protocol::Kind::Http1(
                proto::proxy_protocol::Http1::default(),
            )),
            ProxyProtocol::Http2 => Some(proto::proxy_protocol::Kind::Http2(
                proto::proxy_protocol::Http2::default(),
            )),
            ProxyProtocol::Grpc => Some(proto::proxy_protocol::Kind::Grpc(
                proto::proxy_protocol::Grpc::default(),
            )),
            ProxyProtocol::Opaque => Some(proto::proxy_protocol::Kind::Opaque(
                proto::proxy_protocol::Opaque {},
            )),
            ProxyProtocol::Tls => Some(proto::proxy_protocol::Kind::Tls(
                proto::proxy_protocol::Tls {},
            )),
        },
    };
    trace!(?protocol);

    let server_authzs = srv.authorizations.into_iter().map(|(n, c)| to_authz(n, c));
    trace!(?kubelet_authz);
    trace!(?server_authzs);

    proto::Server {
        protocol: Some(protocol),
        authorizations: Some(kubelet_authz.clone())
            .into_iter()
            .chain(server_authzs)
            .collect(),
        ..Default::default()
    }
}

fn kubelet_authz(ips: KubeletIps) -> proto::Authz {
    // Traffic is always permitted from the pod's Kubelet IPs.
    proto::Authz {
        networks: ips
            .to_nets()
            .into_iter()
            .map(|net| proto::Network {
                net: Some(net.into()),
                except: vec![],
            })
            .collect(),
        authentication: Some(proto::Authn {
            permit: Some(proto::authn::Permit::Unauthenticated(
                proto::authn::PermitUnauthenticated {},
            )),
        }),
        labels: Some(("authn".to_string(), "false".to_string()))
            .into_iter()
            .chain(Some(("name".to_string(), "_kubelet".to_string())))
            .collect(),
    }
}

fn to_authz(
    name: impl ToString,
    ClientAuthorization {
        networks,
        authentication,
    }: ClientAuthorization,
) -> proto::Authz {
    let networks = if networks.is_empty() {
        // TODO use cluster networks (from config).
        vec![
            proto::Network {
                net: Some(ipnet::IpNet::V4(Default::default()).into()),
                except: vec![],
            },
            proto::Network {
                net: Some(ipnet::IpNet::V6(Default::default()).into()),
                except: vec![],
            },
        ]
    } else {
        networks
            .iter()
            .map(|ClientNetwork { net, except }| proto::Network {
                net: Some((*net).into()),
                except: except.iter().cloned().map(Into::into).collect(),
            })
            .collect()
    };

    match authentication {
        ClientAuthentication::Unauthenticated => {
            let labels = Some(("authn".to_string(), "false".to_string()))
                .into_iter()
                .chain(Some(("tls".to_string(), "false".to_string())))
                .chain(Some(("name".to_string(), name.to_string())))
                .collect();

            proto::Authz {
                labels,
                networks,
                authentication: Some(proto::Authn {
                    permit: Some(proto::authn::Permit::Unauthenticated(
                        proto::authn::PermitUnauthenticated {},
                    )),
                }),
            }
        }

        ClientAuthentication::TlsUnauthenticated => {
            let labels = Some(("authn".to_string(), "false".to_string()))
                .into_iter()
                .chain(Some(("tls".to_string(), "true".to_string())))
                .chain(Some(("name".to_string(), name.to_string())))
                .collect();

            // todo
            proto::Authz {
                labels,
                networks,
                authentication: Some(proto::Authn {
                    permit: Some(proto::authn::Permit::MeshTls(proto::authn::PermitMeshTls {
                        clients: Some(proto::authn::permit_mesh_tls::Clients::Unauthenticated(
                            proto::authn::PermitUnauthenticated {},
                        )),
                    })),
                }),
            }
        }

        // Authenticated connections must have TLS and apply to all
        // networks.
        ClientAuthentication::TlsAuthenticated(identities) => {
            let labels = Some(("authn".to_string(), "true".to_string()))
                .into_iter()
                .chain(Some(("tls".to_string(), "true".to_string())))
                .chain(Some(("name".to_string(), name.to_string())))
                .collect();

            let authn = {
                let suffixes = identities
                    .iter()
                    .filter_map(|i| match i {
                        ClientIdentityMatch::Suffix(s) => {
                            Some(proto::IdentitySuffix { parts: s.to_vec() })
                        }
                        _ => None,
                    })
                    .collect();

                let identities = identities
                    .iter()
                    .filter_map(|i| match i {
                        ClientIdentityMatch::Name(n) => Some(proto::Identity {
                            name: n.to_string(),
                        }),
                        _ => None,
                    })
                    .collect();

                proto::Authn {
                    permit: Some(proto::authn::Permit::MeshTls(proto::authn::PermitMeshTls {
                        clients: Some(proto::authn::permit_mesh_tls::Clients::Identities(
                            proto::authn::permit_mesh_tls::PermitClientIdentities {
                                identities,
                                suffixes,
                            },
                        )),
                    })),
                }
            };

            proto::Authz {
                labels,
                networks,
                authentication: Some(authn),
            }
        }
    }
}
