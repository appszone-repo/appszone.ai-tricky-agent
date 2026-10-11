use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::Mutex as AsyncMutex;

/// One conversation. The mutex serialises runs on the session; the value is the
/// agent's own session id (None until the first run reports it).
pub type Slot = Arc<AsyncMutex<Option<String>>>;

struct Entry {
    slot: Slot,
    last_used: Instant,
}

#[derive(Default)]
pub struct Sessions {
    map: Mutex<HashMap<(String, String), Entry>>,
}

impl Sessions {
    /// Find or create the slot for (agent, client-chosen id).
    /// `native` pre-fills the agent's own id when the client sent `<agent>:<id>`.
    pub fn slot(&self, agent: &str, id: &str, native: Option<String>, ttl: Duration) -> Slot {
        let mut map = self.map.lock().unwrap();
        if !ttl.is_zero() {
            // never drop a slot that a running request still holds
            map.retain(|_, e| Arc::strong_count(&e.slot) > 1 || e.last_used.elapsed() < ttl);
        }
        let entry = map
            .entry((agent.to_string(), id.to_string()))
            .or_insert_with(|| Entry {
                slot: Arc::new(AsyncMutex::new(native)),
                last_used: Instant::now(),
            });
        entry.last_used = Instant::now();
        entry.slot.clone()
    }
}
