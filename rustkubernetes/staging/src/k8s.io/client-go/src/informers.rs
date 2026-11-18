//! Port of `client-go/informers`: a `SharedInformerFactory`.
//!
//! Every controller asks the factory for the informers it needs; each resource
//! type gets exactly one informer (one LIST+WATCH) shared by all controllers.
//! Controllers subscribe to events *before* [`SharedInformerFactory::start`].

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use k8s_api::Resource;

use crate::api::{Api, ListParams};
use crate::rest::Client;
use crate::tools::cache::Informer;

type Starter = Box<dyn Fn() + Send + Sync>;
type Waiter = Box<dyn Fn() -> futures::future::BoxFuture<'static, ()> + Send + Sync>;

pub struct SharedInformerFactory {
    client: Client,
    informers: Mutex<HashMap<TypeId, Arc<dyn Any + Send + Sync>>>,
    starters: Mutex<Vec<(Starter, Waiter)>>,
}

impl SharedInformerFactory {
    pub fn new(client: Client) -> Arc<Self> {
        Arc::new(Self { client, informers: Mutex::new(HashMap::new()), starters: Mutex::new(vec![]) })
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    /// The shared informer for `K` (all namespaces), created on first use.
    pub fn informer<K: Resource>(&self) -> Arc<Informer<K>> {
        let mut map = self.informers.lock().unwrap();
        if let Some(i) = map.get(&TypeId::of::<K>()) {
            return i.clone().downcast::<Informer<K>>().unwrap();
        }
        let inf = Informer::new(Api::<K>::all(self.client.clone()), ListParams::default());
        map.insert(TypeId::of::<K>(), inf.clone());
        let (s, w) = (inf.clone(), inf.clone());
        self.starters.lock().unwrap().push((
            Box::new(move || s.start()),
            Box::new(move || {
                let w = w.clone();
                Box::pin(async move { w.wait_synced().await })
            }),
        ));
        inf
    }

    /// Starts every informer requested so far (idempotent).
    pub fn start(&self) {
        for (start, _) in self.starters.lock().unwrap().iter() {
            start();
        }
    }

    /// Waits until every requested informer has completed its first LIST.
    pub async fn wait_for_cache_sync(&self) {
        let waits: Vec<_> = self.starters.lock().unwrap().iter().map(|(_, w)| w()).collect();
        futures::future::join_all(waits).await;
    }
}
