mod authz;
mod default_allow;
mod namespace;
mod node;
mod pod;
mod server;

pub use self::default_allow::DefaultAllow;
use self::{
    default_allow::DefaultAllows,
    namespace::{Namespace, NamespaceIndex},
    node::NodeIndex,
    server::SrvIndex,
};
use crate::{k8s, lookup, ClientAuthz};
use anyhow::{Context, Error};
use std::sync::Arc;
use tokio::{sync::watch, time};
use tracing::{debug, instrument, warn};

pub struct Index {
    /// Holds per-namespace pod/server/authorization indexes.
    namespaces: NamespaceIndex,

    /// Cached Node IPs.
    nodes: NodeIndex,

    default_allows: DefaultAllows,
}

/// Selects servers for an authorization.
#[derive(Clone, Debug, PartialEq, Eq)]
enum ServerSelector {
    Name(k8s::polixy::server::Name),
    Selector(Arc<k8s::labels::Selector>),
}

// === impl Index ===

impl Index {
    pub(crate) fn new(
        cluster_nets: Vec<ipnet::IpNet>,
        default_allow: DefaultAllow,
        detect_timeout: time::Duration,
    ) -> Self {
        Self {
            namespaces: NamespaceIndex::new(default_allow),
            nodes: NodeIndex::default(),
            default_allows: DefaultAllows::new(cluster_nets, detect_timeout),
        }
    }

    /// Drives indexing for all resource types.
    ///
    /// This is all driven on a single task, so it's not necessary for any of the indexing logic to
    /// worry about concurrent access for the internal indexing structures.  All updates are
    /// published to the shared `lookups` map after indexing occurs; but the indexing task is solely
    /// responsible for mutating it. The associated `Handle` is used for reads against this.
    #[instrument(skip(self, resources, ready_tx), fields(result))]
    pub(crate) async fn index(
        mut self,
        resources: k8s::ResourceWatches,
        ready_tx: watch::Sender<bool>,
        mut lookups: lookup::Index,
    ) -> Error {
        let k8s::ResourceWatches {
            mut namespaces,
            mut nodes,
            mut pods,
            mut servers,
            mut authorizations,
        } = resources;

        let mut ready = false;
        loop {
            let res = tokio::select! {
                // Track the kubelet IPs for all nodes.
                up = nodes.recv() => match up {
                    k8s::Event::Applied(node) => self.nodes.apply(node).context("applying a node"),
                    k8s::Event::Deleted(node) => self.nodes.delete(node).context("deleting a node"),
                    k8s::Event::Restarted(nodes) => self.nodes.reset(nodes).context("resetting nodes"),
                },

                // Track namespace-level annotations
                up = namespaces.recv() => match up {
                    k8s::Event::Applied(ns) => self.namespaces.apply(ns).context("applying a namespace"),
                    k8s::Event::Deleted(ns) => self.namespaces.delete(ns).context("deleting a namespace"),
                    k8s::Event::Restarted(nss) => self.namespaces.reset(nss).context("resetting namespaces"),
                },

                up = pods.recv() => match up {
                    k8s::Event::Applied(pod) => self.apply_pod(pod, &mut lookups).context("applying a pod"),
                    k8s::Event::Deleted(pod) => self.delete_pod(pod, &mut lookups).context("deleting a pod"),
                    k8s::Event::Restarted(pods) => self.reset_pods(pods, &mut lookups).context("resetting pods"),
                },

                up = servers.recv() => match up {
                    k8s::Event::Applied(srv) => {
                        self.apply_server(srv);
                        Ok(())
                    }
                    k8s::Event::Deleted(srv) => self.delete_server(srv).context("deleting a server"),
                    k8s::Event::Restarted(srvs) => self.reset_servers(srvs).context("resetting servers"),
                },

                up = authorizations.recv() => match up {
                    k8s::Event::Applied(authz) => self.apply_authz(authz).context("applying an authorization"),
                    k8s::Event::Deleted(authz) => {
                        self.delete_authz(authz);
                        Ok(())
                    }
                    k8s::Event::Restarted(authzs) => self.reset_authzs(authzs).context("resetting authorizations"),
                },
            };

            if let Err(error) = res {
                warn!(?error);
            }

            // Notify the readiness watch if readiness changes.
            let ready_now = nodes.ready()
                && namespaces.ready()
                && pods.ready()
                && servers.ready()
                && authorizations.ready();
            if ready != ready_now {
                let _ = ready_tx.send(ready_now);
                ready = ready_now;
                debug!(%ready);
            }
        }
    }
}
