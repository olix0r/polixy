use super::{Index, NsIndex, Server, ServerMeta};
use crate::{
    k8s::{self, polixy},
    InboundServerConfig, ProxyProtocol,
};
use anyhow::{anyhow, bail, Result};
use std::collections::{hash_map::Entry as HashEntry, HashMap, HashSet};
use tokio::{sync::watch, time};
use tracing::{debug, instrument, trace};

impl Index {
    /// Builds a `Server`, linking it against authorizations and pod ports.
    #[instrument(
        skip(self, srv),
        fields(
            ns = ?srv.metadata.namespace,
            name = ?srv.metadata.name,
        )
    )]
    pub(super) fn apply_server(&mut self, srv: polixy::Server) {
        let ns_name = k8s::NsName::from_srv(&srv);
        let NsIndex {
            ref pods,
            authzs: ref ns_authzs,
            ref mut servers,
            default_mode: _,
        } = self.namespaces.get_or_default(ns_name);

        let srv_name = polixy::server::Name::from_server(&srv);
        let port = srv.spec.port;
        let protocol = mk_protocol(srv.spec.proxy_protocol.as_ref());

        match servers.index.entry(srv_name) {
            HashEntry::Vacant(entry) => {
                let labels = k8s::Labels::from(srv.metadata.labels);
                let authzs = ns_authzs.collect_by_server(entry.key(), &labels);
                let meta = ServerMeta {
                    labels,
                    port,
                    pod_selector: srv.spec.pod_selector.into(),
                    protocol: protocol.clone(),
                };
                debug!(authzs = ?authzs.keys());
                let (tx, rx) = watch::channel(InboundServerConfig {
                    protocol,
                    authorizations: authzs
                        .iter()
                        .map(|(n, a)| (Some(n.clone()), a.clone()))
                        .collect(),
                });
                entry.insert(Server {
                    meta,
                    rx,
                    tx,
                    authorizations: authzs,
                });
            }

            HashEntry::Occupied(mut entry) => {
                // If something about the server changed, we need to update the config to reflect
                // the change.
                let labels_changed =
                    Some(entry.get().meta.labels.as_ref()) != srv.metadata.labels.as_ref();
                let protocol_changed = entry.get().meta.protocol == protocol;
                if labels_changed || protocol_changed {
                    // NB: Only a single task applies server updates, so it's
                    // okay to borrow a version, modify, and send it.  We don't
                    // need a lock because serialization is guaranteed.
                    let mut config = entry.get().rx.borrow().clone();

                    if labels_changed {
                        let labels = k8s::Labels::from(srv.metadata.labels);
                        let authzs = ns_authzs.collect_by_server(entry.key(), &labels);
                        debug!(authzs = ?authzs.keys());
                        config.authorizations = authzs
                            .iter()
                            .map(|(n, a)| (Some(n.clone()), a.clone()))
                            .collect();
                        entry.get_mut().meta.labels = labels;
                        entry.get_mut().authorizations = authzs;
                    }

                    if protocol_changed {
                        config.protocol = protocol.clone();
                        entry.get_mut().meta.protocol = protocol;
                    }

                    debug!("Updating");
                    entry
                        .get()
                        .tx
                        .send(config)
                        .expect("server update must succeed");
                }

                // If the pod/port selector didn't change, we don't need to
                // refresh the index.
                if *entry.get().meta.pod_selector == srv.spec.pod_selector
                    && entry.get().meta.port == port
                {
                    return;
                }

                entry.get_mut().meta.pod_selector = srv.spec.pod_selector.into();
                entry.get_mut().meta.port = port;
            }
        }

        // If we've updated the server->pod selection, then we need to re-index
        // all pods and servers.
        for pod in pods.index.values() {
            Self::link_pod_servers(servers, &pod.labels, &pod.servers);
        }
    }

    #[instrument(
        skip(self, srv),
        fields(
            ns = ?srv.metadata.namespace,
            name = ?srv.metadata.name,
        )
    )]
    pub(super) fn delete_server(&mut self, srv: polixy::Server) -> Result<()> {
        let ns_name = k8s::NsName::from_srv(&srv);
        let srv_name = polixy::server::Name::from_server(&srv);
        self.rm_server(ns_name, srv_name)
    }

    fn rm_server(&mut self, ns_name: k8s::NsName, srv_name: polixy::server::Name) -> Result<()> {
        let ns =
            self.namespaces.index.get_mut(&ns_name).ok_or_else(|| {
                anyhow!("removing server from non-existent namespace {}", ns_name)
            })?;

        if ns.servers.index.remove(&srv_name).is_none() {
            bail!("removing non-existent server {}", srv_name);
        }

        // Reset the server config for all pods that were using this server.
        for (pod_name, pod) in ns.pods.index.iter() {
            for (port_num, port) in pod.servers.by_port.iter() {
                let mut sn = port.server_name.lock();
                if sn.as_ref() == Some(&srv_name) {
                    debug!(pod = %pod_name, port = %port_num, "Removing server from pod");
                    *sn = None;
                    let rx = self.default_mode_rxs.get(ns.default_mode);
                    port.tx
                        .send(rx)
                        .expect("pod config receiver must still be held");
                } else {
                    trace!(pod = %pod_name, port = %port_num, server = ?sn, "Server does not match");
                }
            }
        }

        debug!("Removed server");
        Ok(())
    }

    #[instrument(skip(self, srvs))]
    pub(super) fn reset_servers(&mut self, srvs: Vec<polixy::Server>) -> Result<()> {
        let mut prior_servers = self
            .namespaces
            .index
            .iter()
            .map(|(n, ns)| {
                let servers = ns.servers.index.keys().cloned().collect::<HashSet<_>>();
                (n.clone(), servers)
            })
            .collect::<HashMap<_, _>>();

        let mut result = Ok(());
        for srv in srvs.into_iter() {
            let ns_name = k8s::NsName::from_srv(&srv);
            if let Some(ns) = prior_servers.get_mut(&ns_name) {
                let srv_name = polixy::server::Name::from_server(&srv);
                ns.remove(&srv_name);
            }

            self.apply_server(srv);
        }

        for (ns_name, ns_servers) in prior_servers.into_iter() {
            for srv_name in ns_servers.into_iter() {
                if let Err(e) = self.rm_server(ns_name.clone(), srv_name) {
                    result = Err(e);
                }
            }
        }

        result
    }
}

fn mk_protocol(p: Option<&polixy::server::ProxyProtocol>) -> ProxyProtocol {
    match p {
        Some(polixy::server::ProxyProtocol::Unknown) | None => ProxyProtocol::Detect {
            timeout: time::Duration::from_secs(5),
        },
        Some(polixy::server::ProxyProtocol::Http1) => ProxyProtocol::Http1,
        Some(polixy::server::ProxyProtocol::Http2) => ProxyProtocol::Http2,
        Some(polixy::server::ProxyProtocol::Grpc) => ProxyProtocol::Grpc,
        Some(polixy::server::ProxyProtocol::Opaque) => ProxyProtocol::Opaque,
        Some(polixy::server::ProxyProtocol::Tls) => ProxyProtocol::Tls,
    }
}