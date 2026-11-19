//! Behaviour every [`Storage`] must have (run against the memory store and etcd).

use std::sync::Arc;

use futures::StreamExt;
use k8s_apimachinery::watch::EventType;
use serde_json::json;

use super::{Storage, StorageError};

fn rv(v: &serde_json::Value) -> u64 {
    v["metadata"]["resourceVersion"].as_str().unwrap().parse().unwrap()
}

pub async fn run<S: Storage + ?Sized>(s: Arc<S>) {
    let k = "/registry/pods/ns/a";
    // create / AlreadyExists / get
    let a = s.create(k, json!({"metadata": {"name": "a"}, "spec": 1})).await.unwrap();
    let r1 = rv(&a);
    assert!(r1 > 0);
    assert!(matches!(s.create(k, json!({"metadata": {"name": "a"}})).await, Err(StorageError::AlreadyExists(_))));
    let got = s.get(k).await.unwrap();
    assert_eq!((rv(&got), &got["spec"]), (r1, &json!(1)));
    assert!(matches!(s.get("/registry/pods/ns/missing").await, Err(StorageError::NotFound(_))));
    // list is scoped to the prefix and sorted by key
    s.create("/registry/pods/ns/b", json!({"metadata": {"name": "b"}})).await.unwrap();
    s.create("/registry/pods/other/c", json!({"metadata": {"name": "c"}})).await.unwrap();
    s.create("/registry/configmaps/ns/x", json!({"metadata": {"name": "x"}})).await.unwrap();
    let (items, list_rv) = s.list("/registry/pods/ns/").await.unwrap();
    assert_eq!(items.iter().map(|i| i["metadata"]["name"].as_str().unwrap()).collect::<Vec<_>>(), ["a", "b"]);
    assert!(list_rv >= r1);
    assert_eq!(s.list("/registry/pods/").await.unwrap().0.len(), 3);
    // update with preconditions
    assert!(matches!(s.update(k, Some(r1 + 1000), json!({"metadata": {"name": "a"}})).await, Err(StorageError::Conflict(_))));
    let a2 = s.update(k, Some(r1), json!({"metadata": {"name": "a"}, "spec": 2})).await.unwrap();
    assert!(rv(&a2) > r1);
    let a3 = s.update(k, None, json!({"metadata": {"name": "a"}, "spec": 3})).await.unwrap();
    assert!(rv(&a3) > rv(&a2));
    assert!(matches!(s.update("/registry/pods/ns/missing", None, json!({"metadata": {}})).await, Err(StorageError::NotFound(_))));
    // watch: replay from a revision, then live events
    let mut w = s.watch("/registry/pods/ns/", r1).await.unwrap();
    let e = w.next().await.unwrap().unwrap();
    assert_eq!(e.type_, EventType::Added);
    assert_eq!(e.object["metadata"]["name"], "b");
    let e = w.next().await.unwrap().unwrap();
    assert_eq!((e.type_, e.revision), (EventType::Modified, rv(&a2)));
    assert_eq!(e.prev.as_ref().unwrap()["spec"], 1);
    let e = w.next().await.unwrap().unwrap();
    assert_eq!((e.type_, &e.object["spec"]), (EventType::Modified, &json!(3)));
    // delete with a stale precondition, then for real
    assert!(matches!(s.delete(k, Some(r1)).await, Err(StorageError::Conflict(_))));
    let d = s.delete(k, Some(rv(&a3))).await.unwrap();
    assert!(rv(&d) > rv(&a3));
    assert_eq!(d["spec"], 3);
    let e = w.next().await.unwrap().unwrap();
    assert_eq!((e.type_, e.revision, &e.object["spec"]), (EventType::Deleted, rv(&d), &json!(3)));
    assert!(matches!(s.delete(k, None).await, Err(StorageError::NotFound(_))));
    // other prefixes do not leak into the watch
    s.create("/registry/configmaps/ns/y", json!({"metadata": {"name": "y"}})).await.unwrap();
    let c = s.create("/registry/pods/ns/d", json!({"metadata": {"name": "d"}})).await.unwrap();
    let e = w.next().await.unwrap().unwrap();
    assert_eq!((e.type_, e.revision), (EventType::Added, rv(&c)));
    // a revision the store never issued
    assert!(matches!(s.watch("/registry/pods/", s.current_revision() + 100).await, Err(StorageError::Gone { .. })));
    assert!(s.current_revision() >= rv(&c));
    assert!(s.check().await.is_ok());
}
