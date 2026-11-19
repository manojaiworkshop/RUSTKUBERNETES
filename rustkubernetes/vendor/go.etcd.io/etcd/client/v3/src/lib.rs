//! A small etcd v3 client (the subset of `go.etcd.io/etcd/client/v3` that the
//! Kubernetes API server's etcd3 storage uses): KV (range, txn, compact), Watch
//! and Maintenance (status), over gRPC with optional mutual TLS.

#![allow(clippy::all, clippy::pedantic, clippy::doc_lazy_continuation)]

pub mod pb {
    //! Generated protobuf/gRPC types (`etcdserverpb`, `mvccpb`, …).
    #![allow(missing_docs, clippy::all)]
    pub mod etcdserverpb {
        tonic::include_proto!("etcdserverpb");
    }
    pub mod mvccpb {
        tonic::include_proto!("mvccpb");
    }
    pub mod authpb {
        tonic::include_proto!("authpb");
    }
}

use std::time::Duration;

pub use pb::etcdserverpb::{
    compare, request_op, response_op, watch_request, CompactionRequest, Compare, DeleteRangeRequest, PutRequest, RangeRequest,
    RangeResponse, RequestOp, ResponseHeader, StatusRequest, TxnRequest, TxnResponse, WatchCreateRequest, WatchRequest, WatchResponse,
};
use pb::etcdserverpb::{kv_client::KvClient, maintenance_client::MaintenanceClient, watch_client::WatchClient};
pub use pb::mvccpb::{event::EventType, Event, KeyValue};
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint, Identity};

/// TLS material (`--etcd-cafile`, `--etcd-certfile`, `--etcd-keyfile`), PEM.
#[derive(Clone, Default)]
pub struct Tls {
    pub ca: Option<Vec<u8>>,
    pub cert: Option<Vec<u8>>,
    pub key: Option<Vec<u8>>,
}

/// A connection to an etcd cluster (several endpoints are balanced by tonic).
#[derive(Clone)]
pub struct Client {
    kv: KvClient<Channel>,
    watch: WatchClient<Channel>,
    maintenance: MaintenanceClient<Channel>,
}

pub type Error = tonic::Status;

impl Client {
    /// `clientv3.New` with `DialTimeout`: endpoints like `https://127.0.0.1:2379`.
    pub async fn connect(endpoints: &[String], tls: Option<Tls>, dial_timeout: Duration) -> Result<Self, String> {
        let mut eps = vec![];
        for e in endpoints {
            let mut ep = Endpoint::from_shared(e.clone()).map_err(|x| format!("invalid etcd endpoint {e}: {x}"))?;
            ep = ep
                .connect_timeout(dial_timeout)
                .keep_alive_while_idle(true)
                .http2_keep_alive_interval(Duration::from_secs(30))
                .keep_alive_timeout(Duration::from_secs(10));
            if let (true, Some(t)) = (e.starts_with("https://"), &tls) {
                let mut c = ClientTlsConfig::new().with_enabled_roots();
                if let Some(ca) = &t.ca {
                    c = ClientTlsConfig::new().ca_certificate(Certificate::from_pem(ca));
                }
                if let (Some(cert), Some(key)) = (&t.cert, &t.key) {
                    c = c.identity(Identity::from_pem(cert, key));
                }
                ep = ep.tls_config(c).map_err(|x| format!("etcd TLS: {x}"))?;
            }
            eps.push(ep);
        }
        let channel = Channel::balance_list(eps.into_iter());
        Ok(Self {
            kv: KvClient::new(channel.clone()),
            watch: WatchClient::new(channel.clone()),
            maintenance: MaintenanceClient::new(channel),
        })
    }

    pub async fn range(&self, req: RangeRequest) -> Result<RangeResponse, Error> {
        Ok(self.kv.clone().range(req).await?.into_inner())
    }

    pub async fn txn(&self, req: TxnRequest) -> Result<TxnResponse, Error> {
        Ok(self.kv.clone().txn(req).await?.into_inner())
    }

    pub async fn compact(&self, revision: i64) -> Result<(), Error> {
        self.kv.clone().compact(CompactionRequest { revision, physical: false }).await?;
        Ok(())
    }

    /// `Maintenance.Status` of the first reachable endpoint (health check).
    pub async fn status(&self) -> Result<ResponseHeader, Error> {
        let r = self.maintenance.clone().status(StatusRequest {}).await?.into_inner();
        Ok(r.header.unwrap_or_default())
    }

    /// Opens a watch stream: the server answers `created`, then event batches.
    pub async fn watch(&self, req: WatchCreateRequest) -> Result<tonic::Streaming<WatchResponse>, Error> {
        let (tx, rx) = tokio::sync::mpsc::channel::<WatchRequest>(1);
        tx.send(WatchRequest { request_union: Some(watch_request::RequestUnion::CreateRequest(req)) })
            .await
            .map_err(|_| Error::internal("watch channel closed"))?;
        // Keep the request stream open for the life of the watch.
        let outbound = async_stream_like(rx, tx);
        Ok(self.watch.clone().watch(outbound).await?.into_inner())
    }
}

/// A request stream that stays open while the response stream is consumed.
fn async_stream_like(
    rx: tokio::sync::mpsc::Receiver<WatchRequest>,
    tx: tokio::sync::mpsc::Sender<WatchRequest>,
) -> impl tokio_stream_like::Stream<Item = WatchRequest> {
    tokio_stream_like::ReceiverStream { rx, _keep: tx }
}

mod tokio_stream_like {
    use std::pin::Pin;
    use std::task::{Context, Poll};

    pub use futures_core_like::Stream;

    pub struct ReceiverStream<T> {
        pub rx: tokio::sync::mpsc::Receiver<T>,
        pub _keep: tokio::sync::mpsc::Sender<T>,
    }

    impl<T> Stream for ReceiverStream<T> {
        type Item = T;
        fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<T>> {
            self.rx.poll_recv(cx)
        }
    }

    mod futures_core_like {
        pub use tokio_stream::Stream;
    }
}

/// `clientv3.Compare` helpers.
pub fn cmp_create_revision(key: Vec<u8>, result: compare::CompareResult, rev: i64) -> Compare {
    Compare {
        key,
        result: result as i32,
        target: compare::CompareTarget::Create as i32,
        target_union: Some(compare::TargetUnion::CreateRevision(rev)),
        range_end: vec![],
    }
}

pub fn cmp_mod_revision(key: Vec<u8>, result: compare::CompareResult, rev: i64) -> Compare {
    Compare {
        key,
        result: result as i32,
        target: compare::CompareTarget::Mod as i32,
        target_union: Some(compare::TargetUnion::ModRevision(rev)),
        range_end: vec![],
    }
}

pub fn cmp_version(key: Vec<u8>, result: compare::CompareResult, version: i64) -> Compare {
    Compare {
        key,
        result: result as i32,
        target: compare::CompareTarget::Version as i32,
        target_union: Some(compare::TargetUnion::Version(version)),
        range_end: vec![],
    }
}

pub fn op_put(key: Vec<u8>, value: Vec<u8>) -> RequestOp {
    RequestOp { request: Some(request_op::Request::RequestPut(PutRequest { key, value, ..Default::default() })) }
}

pub fn op_get(key: Vec<u8>) -> RequestOp {
    RequestOp { request: Some(request_op::Request::RequestRange(RangeRequest { key, ..Default::default() })) }
}

pub fn op_delete(key: Vec<u8>) -> RequestOp {
    RequestOp { request: Some(request_op::Request::RequestDeleteRange(DeleteRangeRequest { key, prev_kv: true, ..Default::default() })) }
}

/// `clientv3.GetPrefixRangeEnd`.
pub fn prefix_range_end(prefix: &[u8]) -> Vec<u8> {
    let mut e = prefix.to_vec();
    while let Some(last) = e.pop() {
        if last < 0xff {
            e.push(last + 1);
            return e;
        }
    }
    vec![0]
}
