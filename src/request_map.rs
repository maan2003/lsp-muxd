use std::sync::atomic::Ordering;
use std::sync::Mutex;
use std::{collections::HashMap, sync::atomic::AtomicI64};

use tower_lsp::jsonrpc;

use crate::{ClientId, RequestId, ServerRequestId};

pub struct RequestMap {
    inner: Mutex<HashMap<ServerRequestId, (ClientId, RequestId)>>,
    next_server_request_id: AtomicI64,
}

impl RequestMap {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            next_server_request_id: AtomicI64::new(1),
        }
    }

    pub fn insert(&self, client_id: ClientId, request_id: RequestId) -> jsonrpc::Id {
        let server_id = self.next_server_request_id.fetch_add(1, Ordering::SeqCst);
        let mut map = self.inner.lock().unwrap();
        map.insert(server_id, (client_id, request_id));
        server_id.into()
    }

    pub fn remove(&self, server_id: &jsonrpc::Id) -> Option<(ClientId, RequestId)> {
        match server_id {
            jsonrpc::Id::Number(id) => {
                let mut map = self.inner.lock().unwrap();
                map.remove(id)
            }
            _ => None,
        }
    }
}
