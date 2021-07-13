use crate::KubeletIps;
use anyhow::{anyhow, Result};
use dashmap::{mapref::entry::Entry, DashMap};
use polixy_controller_core::{
    ClientAuthentication, ClientAuthorization, ClientNetwork, DiscoverInboundServer, InboundServer,
    InboundServerRx, InboundServerRxStream,
};
use std::{collections::HashMap, net::IpAddr, sync::Arc};
use tokio::sync::watch;

#[derive(Debug, Default)]
pub(crate) struct Writer(ByNs);

#[derive(Clone, Debug)]
pub struct Reader(ByNs);

type ByNs = Arc<DashMap<String, ByPod>>;
type ByPod = DashMap<String, ByPort>;

// Boxed to enforce immutability.
type ByPort = Box<HashMap<u16, Rx>>;

pub(crate) fn pair() -> (Writer, Reader) {
    let by_ns = ByNs::default();
    let w = Writer(by_ns.clone());
    let r = Reader(by_ns);
    (w, r)
}

#[derive(Clone, Debug)]
pub struct Rx {
    kubelet: KubeletIps,
    rx: watch::Receiver<watch::Receiver<InboundServer>>,
}

// === impl Reader ===

impl Reader {
    pub(crate) fn lookup(&self, ns: &str, pod: &str, port: u16) -> Option<Rx> {
        self.0.get(ns)?.get(pod)?.get(&port).cloned()
    }
}

#[async_trait::async_trait]
impl DiscoverInboundServer<(String, String, u16)> for Reader {
    type Rx = Rx;

    async fn discover_inbound_server(
        &self,
        (ns, pod, port): (String, String, u16),
    ) -> Result<Option<Rx>> {
        Ok(self.lookup(&*ns, &*pod, port))
    }
}

// === impl Writer ===

impl Writer {
    pub(crate) fn contains(&self, ns: impl AsRef<str>, pod: impl AsRef<str>) -> bool {
        self.0
            .get(ns.as_ref())
            .map(|ns| ns.contains_key(pod.as_ref()))
            .unwrap_or(false)
    }

    pub(crate) fn set(
        &mut self,
        ns: impl ToString,
        pod: impl ToString,
        ports: impl IntoIterator<Item = (u16, Rx)>,
    ) -> Result<()> {
        match self
            .0
            .entry(ns.to_string())
            .or_default()
            .entry(pod.to_string())
        {
            Entry::Vacant(entry) => {
                entry.insert(ports.into_iter().collect::<HashMap<_, _>>().into());
                Ok(())
            }
            Entry::Occupied(_) => Err(anyhow!(
                "pod {} already exists in namespace {}",
                pod.to_string(),
                ns.to_string()
            )),
        }
    }

    pub(crate) fn unset(&mut self, ns: impl AsRef<str>, pod: impl AsRef<str>) -> Result<ByPort> {
        let pods = self
            .0
            .get_mut(ns.as_ref())
            .ok_or_else(|| anyhow!("missing namespace {}", ns.as_ref()))?;

        let (_, ports) = pods
            .remove(pod.as_ref())
            .ok_or_else(|| anyhow!("missing pod {} in namespace {}", pod.as_ref(), ns.as_ref()))?;

        if (*pods).is_empty() {
            drop(pods);
            self.0.remove(ns.as_ref()).expect("namespace must exist");
        }

        Ok(ports)
    }
}

// === impl Rx ===

impl Rx {
    pub(crate) fn new(
        kubelet: KubeletIps,
        rx: watch::Receiver<watch::Receiver<InboundServer>>,
    ) -> Self {
        Self { kubelet, rx }
    }
}

#[inline]
fn mk_server(kubelet: &[IpAddr], mut inner: InboundServer) -> InboundServer {
    let networks = kubelet.iter().copied().map(ClientNetwork::from).collect();
    let authz = ClientAuthorization {
        networks,
        authentication: ClientAuthentication::Unauthenticated,
    };

    inner.authorizations.insert("_health_check".into(), authz);
    inner
}

impl InboundServerRx for Rx {
    fn get(&self) -> InboundServer {
        mk_server(&*self.kubelet, (*(*self.rx.borrow()).borrow()).clone())
    }

    fn into_stream(self) -> InboundServerRxStream {
        let kubelet = self.kubelet;
        let mut outer = self.rx;
        let mut inner = (*outer.borrow_and_update()).clone();
        Box::pin(async_stream::stream! {
            let mut server = (*inner.borrow_and_update()).clone();
            yield mk_server(&*kubelet, server.clone());

            loop {
                tokio::select! {
                    res = inner.changed() => match res {
                        Ok(()) => {
                            let s = (*inner.borrow()).clone();
                            if s != server {
                                yield mk_server(&*kubelet, s.clone());
                                server = s;
                            }
                        }
                        Err(_) => {},
                    },

                    res = outer.changed() => match res {
                        Ok(()) => {
                            inner = (*outer.borrow()).clone();
                            let s = (*inner.borrow_and_update()).clone();
                            if s != server {
                                yield mk_server(&*kubelet, s.clone());
                                server = s;
                            }
                        }
                        Err(_) => return,
                    },
                }
            }
        })
    }
}
