//! Node IPAM against a real in-process API server
//! (Go: `test/integration/ipamperf`, `pkg/controller/nodeipam` tests).

use std::time::Duration;

use k8s_api::core::v1::{Event, Node};
use k8s_apimachinery::meta::ObjectMeta;
use k8s_client_go::{Api, ListParams};
use kube_controller::nodeipam::ipam::range_allocator::AllocatorParams;
use kube_controller::nodeipam::NodeIpamConfig;
use kube_controller::{controller_descriptors, ControllerContext};
use kube_integration_tests::{eventually, start_test_server, test_controller_config};
use serde_json::json;

const T: Duration = Duration::from_secs(20);

fn node(name: &str, cidr: Option<&str>) -> Node {
    let mut n = Node::new(ObjectMeta { name: Some(name.into()), ..Default::default() });
    if let Some(c) = cidr {
        n = serde_json::from_value(json!({"metadata": {"name": name}, "spec": {"podCIDR": c}})).unwrap();
    }
    n
}

async fn cidr_of(api: &Api<Node>, name: &str) -> Option<String> {
    api.get_opt(name).await.ok().flatten()?.spec?.pod_cidr.filter(|c| !c.is_empty())
}

#[tokio::test]
async fn allocates_occupies_and_releases_node_cidrs() {
    let s = start_test_server().await;
    let nodes: Api<Node> = Api::all(s.client.clone());
    // A node that already has a CIDR before the controller starts: it must be kept and never handed out again.
    nodes.create(&node("a", Some("10.244.0.0/24"))).await.unwrap();
    nodes.create(&node("b", None)).await.unwrap();

    let mut cfg = test_controller_config();
    cfg.node_ipam = NodeIpamConfig {
        allocate_node_cidrs: true,
        allocator_type: "RangeAllocator".into(),
        params: AllocatorParams {
            // 4 node subnets; the "Service range" takes the last one.
            cluster_cidrs: vec!["10.244.0.0/22".parse().unwrap()],
            service_cidr: Some("10.244.3.0/24".parse().unwrap()),
            node_cidr_mask_sizes: vec![24],
            ..Default::default()
        },
    };
    let ctx = ControllerContext::new(s.client.clone(), cfg);
    let d = controller_descriptors().into_iter().find(|d| d.name == "node-ipam-controller").unwrap();
    let run = (d.constructor)(&ctx);
    ctx.informers.start();
    tokio::spawn(run);

    let b = eventually("b gets a CIDR", T, || async { cidr_of(&nodes, "b").await }).await;
    assert_eq!(b, "10.244.1.0/24");
    let n = nodes.get("b").await.unwrap();
    assert_eq!(n.spec.unwrap().pod_cidrs.unwrap(), vec!["10.244.1.0/24".to_string()]);
    assert_eq!(cidr_of(&nodes, "a").await.unwrap(), "10.244.0.0/24");

    nodes.create(&node("c", None)).await.unwrap();
    assert_eq!(eventually("c gets a CIDR", T, || async { cidr_of(&nodes, "c").await }).await, "10.244.2.0/24");

    // The pool (minus the Service range) is exhausted.
    nodes.create(&node("d", None)).await.unwrap();
    let events: Api<Event> = Api::namespaced(s.client.clone(), "default");
    eventually("CIDRNotAvailable event", T, || async {
        let list = events.list(&ListParams::default()).await.ok()?;
        list.items.into_iter().find(|e| e.reason.as_deref() == Some("CIDRNotAvailable") && e.involved_object.name.as_deref() == Some("d"))
    })
    .await;
    assert!(cidr_of(&nodes, "d").await.is_none());

    // Deleting a node returns its CIDR; the retried allocation for d picks it up.
    nodes.delete("c").await.unwrap();
    assert_eq!(eventually("d gets c's CIDR", T, || async { cidr_of(&nodes, "d").await }).await, "10.244.2.0/24");
}
