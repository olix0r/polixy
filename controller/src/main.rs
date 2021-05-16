use anyhow::{Context, Result};
use futures::{future, prelude::*};
use polixy_controller::{DefaultAllow, LookupHandle};
use structopt::StructOpt;
use tokio::time;
use tracing::{debug, info, instrument};

#[derive(Debug, StructOpt)]
#[structopt(name = "polixy", about = "A policy resource prototype")]
struct Command {
    #[structopt(short, long, default_value = "8910")]
    port: u16,
    #[structopt(long, default_value = "cluster.local")]
    identity_domain: String,

    /// Network CIDRs of pod IPs
    #[structopt(long, default_value = "10.42.0.0/16")]
    cluster_networks: Vec<ipnet::IpNet>,

    #[structopt(long, default_value = "external-unauthenticated")]
    default_allow: DefaultAllow,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let Command {
        port,
        identity_domain,
        cluster_networks,
        default_allow,
    } = Command::from_args();

    let (drain_tx, drain_rx) = linkerd_drain::channel();

    let client = kube::Client::try_default()
        .await
        .context("failed to initialize kubernetes client")?;

    const DETECT_TIMEOUT: time::Duration = time::Duration::from_secs(10);
    let (handle, index_task) =
        LookupHandle::run(client, cluster_networks, default_allow, DETECT_TIMEOUT);
    let index_task = tokio::spawn(index_task);

    let grpc = tokio::spawn(grpc(port, handle, drain_rx, identity_domain));

    tokio::select! {
        _ = shutdown(drain_tx) => Ok(()),
        res = grpc => match res {
            Ok(res) => res.context("grpc server failed"),
            Err(e) if e.is_cancelled() => Ok(()),
            Err(e) => Err(e).context("grpc server panicked"),
        },
        res = index_task => match res {
            Ok(e) => Err(e).context("indexer failed"),
            Err(e) if e.is_cancelled() => Ok(()),
            Err(e) => Err(e).context("indexer panicked"),
        },
    }
}

#[instrument(skip(handle, drain, identity_domain))]
async fn grpc(
    port: u16,
    handle: LookupHandle,
    drain: linkerd_drain::Watch,
    identity_domain: String,
) -> Result<()> {
    let addr = ([0, 0, 0, 0], port).into();
    let server = polixy_controller::grpc::Server::new(handle, drain.clone(), identity_domain);
    let (close_tx, close_rx) = tokio::sync::oneshot::channel();
    tokio::pin! {
        let srv = server.serve(addr, close_rx.map(|_| {}));
    }
    info!(%addr, "gRPC server listening");
    tokio::select! {
        res = (&mut srv) => res?,
        handle = drain.signaled() => {
            let _ = close_tx.send(());
            handle.release_after(srv).await?
        }
    }
    Ok(())
}

async fn shutdown(drain: linkerd_drain::Signal) {
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            debug!("Received ctrl-c");
        },
        _ = sigterm() => {
            debug!("Received SIGTERM");
        }
    }
    info!("Shutting down");
    drain.drain().await;
}

async fn sigterm() {
    use tokio::signal::unix::{signal, SignalKind};
    match signal(SignalKind::terminate()) {
        Ok(mut term) => term.recv().await,
        _ => future::pending().await,
    };
}