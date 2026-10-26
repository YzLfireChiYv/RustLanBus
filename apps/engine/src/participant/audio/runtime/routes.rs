//! Reservations prevent local output from entering a loopback capture route.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug, Default)]
pub(crate) struct RouteGuard {
    state: Arc<Mutex<Reservations>>,
}

/// Keep this lease alive until the corresponding native stream has been dropped.
#[derive(Debug)]
pub(crate) struct RouteLease {
    state: Arc<Mutex<Reservations>>,
    id: u64,
}

#[derive(Debug, Default)]
struct Reservations {
    next_id: u64,
    entries: HashMap<u64, Reservation>,
}

#[derive(Debug)]
struct Reservation {
    endpoint: String,
    kind: RouteKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RouteKind {
    Output,
    Loopback,
}

impl RouteGuard {
    pub(crate) fn reserve_output(&self, endpoint: &str) -> Result<RouteLease, String> {
        self.reserve(endpoint, RouteKind::Output)
    }

    pub(crate) fn reserve_loopback(&self, endpoint: &str) -> Result<RouteLease, String> {
        self.reserve(endpoint, RouteKind::Loopback)
    }

    fn reserve(&self, endpoint: &str, kind: RouteKind) -> Result<RouteLease, String> {
        if endpoint.is_empty() {
            return Err("audio route requires a resolved endpoint ID".into());
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| "audio route reservation lock is poisoned".to_owned())?;
        if state.entries.values().any(|entry| {
            entry.endpoint == endpoint
                && (entry.kind == RouteKind::Output || kind == RouteKind::Output)
        }) {
            return Err("System loopback capture and local playback need separate audio endpoints. Select a dedicated virtual speaker for system sound and a different playback device.".into());
        }
        let id = state.next_id;
        state.next_id = id
            .checked_add(1)
            .ok_or_else(|| "audio route reservation IDs exhausted".to_owned())?;
        state.entries.insert(
            id,
            Reservation {
                endpoint: endpoint.to_owned(),
                kind,
            },
        );
        Ok(RouteLease {
            state: Arc::clone(&self.state),
            id,
        })
    }
}

impl Drop for RouteLease {
    fn drop(&mut self) {
        // Cleanup must release this reservation even if another owner panicked.
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.entries.remove(&self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::RouteGuard;

    #[test]
    fn opposite_routes_conflict_in_both_orders_until_all_owners_release() {
        let routes = RouteGuard::default();
        let output = routes.reserve_output("endpoint").unwrap();
        let other_thread = routes.clone();
        assert!(
            std::thread::spawn(move || other_thread.reserve_loopback("endpoint").is_err())
                .join()
                .unwrap()
        );
        drop(output);

        let first = routes.reserve_loopback("endpoint").unwrap();
        let second = routes.reserve_loopback("endpoint").unwrap();
        assert!(routes.reserve_output("endpoint").is_err());
        drop(first);
        assert!(routes.reserve_output("endpoint").is_err());
        drop(second);
        let replacement = routes.reserve_output("endpoint").unwrap();
        assert!(routes.reserve_output("endpoint").is_err());
        let _independent = routes.reserve_loopback("different endpoint").unwrap();
        drop(replacement);
        assert!(routes.reserve_loopback("endpoint").is_ok());
    }
}
