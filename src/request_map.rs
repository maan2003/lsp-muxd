use std::collections::HashMap;
use std::sync::Mutex;

use crate::{ClientId, RequestId, ServerRequestId};

pub struct RequestMap {
    inner: Mutex<HashMap<ServerRequestId, (ClientId, RequestId)>>,
}

impl RequestMap {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    pub fn insert(&self, server_id: ServerRequestId, client_id: ClientId, request_id: RequestId) {
        let mut map = self.inner.lock().unwrap();
        map.insert(server_id, (client_id, request_id));
    }

    pub fn remove(&self, server_id: &ServerRequestId) -> Option<(ClientId, RequestId)> {
        let mut map = self.inner.lock().unwrap();
        map.remove(server_id)
    }
}