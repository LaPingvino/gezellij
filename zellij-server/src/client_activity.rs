//! Gezellij: per-client presence — when each client last proved a human was there, and which
//! clients are currently *parked* on the ghost tab because they did not.
//!
//! ## Why this exists
//!
//! Tab size in Zellij is the **minimum** over the clients whose active tab is that tab (see
//! `Screen::recompute_tab_size`). That is the right thing while everyone is watching, and the
//! wrong thing the moment somebody walks away: the phone you attached from this morning keeps
//! capping the desktop you are sitting at now, and a client whose machine rebooted keeps capping
//! it forever.
//!
//! The hedge (opt-in, `park_inactive_clients_after "10m"`) is to move an idle client *to another
//! tab*. A client on another tab constrains nobody — that is existing, well-tested upstream
//! behaviour, not something we bolt on to the sizing code. Any input from a parked client proves
//! a human is back and restores it to the exact tab it left, instantly.
//!
//! ## Why a process-global
//!
//! Three threads need this state and none of them share a struct:
//!
//! * `route` records input (it is the only place that sees *every* key) and intercepts the first
//!   keypress of a parked client;
//! * `screen` decides who to park, owns `active_tab_ids`/`client_sizes`, and renders the listing;
//! * the server loop seeds on attach and clears on removal.
//!
//! `SessionState` (the obvious home) is not reachable from `Screen`, and plumbing an
//! `Arc<RwLock<SessionState>>` through `screen_thread_main` would still not give `route` the
//! parked set. A server process serves exactly one session, so a global is exact — the same
//! reasoning (and shape) as [`crate::auto_freeze`] and `SESSION_SCAN_STATE`.

use crate::ClientId;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

#[derive(Default)]
struct ClientActivity {
    /// When each client last sent input. Seeded on attach, so a fresh client is never reported
    /// as ancient.
    last_input: HashMap<ClientId, Instant>,
    /// Clients currently parked on the ghost tab → the id of the tab they were on.
    parked: HashMap<ClientId, usize>,
}

fn activity() -> &'static Mutex<ClientActivity> {
    static ACTIVITY: OnceLock<Mutex<ClientActivity>> = OnceLock::new();
    ACTIVITY.get_or_init(Default::default)
}

/// Note that `client_id` just proved a human is there. Called for every key and every
/// non-CLI action.
pub(crate) fn record_input(client_id: ClientId) {
    let mut activity = activity().lock().unwrap();
    activity.last_input.insert(client_id, Instant::now());
}

/// Start (or restart) `client_id`'s idle clock without implying presence — used when a client
/// attaches, so that it starts out "just active" rather than "unknown/ancient".
pub(crate) fn seed(client_id: ClientId) {
    record_input(client_id);
}

/// Forget everything about a client. Called from the one client-removal chokepoint.
pub(crate) fn forget(client_id: ClientId) {
    let mut activity = activity().lock().unwrap();
    activity.last_input.remove(&client_id);
    activity.parked.remove(&client_id);
}

/// Idle times for many clients at once (one lock acquisition).
pub(crate) fn idle_for_many(client_ids: &[ClientId]) -> HashMap<ClientId, Duration> {
    let activity = activity().lock().unwrap();
    client_ids
        .iter()
        .filter_map(|client_id| {
            activity
                .last_input
                .get(client_id)
                .map(|last| (*client_id, last.elapsed()))
        })
        .collect()
}

/// Record that `client_id` has been parked, remembering the tab id it came from.
pub(crate) fn park(client_id: ClientId, previous_tab_id: usize) {
    let mut activity = activity().lock().unwrap();
    activity.parked.insert(client_id, previous_tab_id);
}

pub(crate) fn is_parked(client_id: ClientId) -> bool {
    activity().lock().unwrap().parked.contains_key(&client_id)
}

/// Un-park `client_id`, returning the tab id it should be restored to.
pub(crate) fn unpark(client_id: ClientId) -> Option<usize> {
    let mut activity = activity().lock().unwrap();
    activity.last_input.insert(client_id, Instant::now());
    activity.parked.remove(&client_id)
}

pub(crate) fn parked_client_count() -> usize {
    activity().lock().unwrap().parked.len()
}

/// A duration as a human would say it: `12s`, `4m`, `2h 10m`, `3d 4h`.
///
/// zellij-server has no humantime dependency and this is only ever used for display, so it is
/// a handful of lines rather than a new crate.
pub(crate) fn humanise(duration: Duration) -> String {
    let total_secs = duration.as_secs();
    if total_secs < 60 {
        return format!("{}s", total_secs);
    }
    let minutes = total_secs / 60;
    if minutes < 60 {
        return format!("{}m", minutes);
    }
    let hours = minutes / 60;
    let minutes = minutes % 60;
    if hours < 24 {
        return if minutes == 0 {
            format!("{}h", hours)
        } else {
            format!("{}h {}m", hours, minutes)
        };
    }
    let days = hours / 24;
    let hours = hours % 24;
    if hours == 0 {
        format!("{}d", days)
    } else {
        format!("{}d {}h", days, hours)
    }
}

/// Test-only: drop all state so unit tests in the same process do not see each other's clients.
#[cfg(test)]
pub(crate) fn reset_for_test() {
    let mut activity = activity().lock().unwrap();
    activity.last_input.clear();
    activity.parked.clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    // One test function on purpose: the state under test is process-global, so two tests
    // resetting it in parallel would flake.
    #[test]
    fn park_unpark_and_forget() {
        reset_for_test();
        seed(7);
        assert_eq!(idle_for_many(&[7]).len(), 1);
        assert!(!is_parked(7));
        park(7, 3);
        assert!(is_parked(7));
        assert_eq!(parked_client_count(), 1);
        assert_eq!(unpark(7), Some(3));
        assert!(!is_parked(7));
        assert_eq!(unpark(7), None);

        seed(11);
        park(11, 1);
        forget(11);
        assert!(idle_for_many(&[11]).is_empty());
        assert!(!is_parked(11));
        reset_for_test();
    }

    #[test]
    fn durations_read_the_way_a_human_would_say_them() {
        assert_eq!(humanise(Duration::from_secs(0)), "0s");
        assert_eq!(humanise(Duration::from_secs(12)), "12s");
        assert_eq!(humanise(Duration::from_secs(59)), "59s");
        assert_eq!(humanise(Duration::from_secs(60)), "1m");
        assert_eq!(humanise(Duration::from_secs(4 * 60 + 30)), "4m");
        assert_eq!(humanise(Duration::from_secs(60 * 60)), "1h");
        assert_eq!(humanise(Duration::from_secs(2 * 3600 + 10 * 60)), "2h 10m");
        assert_eq!(humanise(Duration::from_secs(24 * 3600)), "1d");
        assert_eq!(
            humanise(Duration::from_secs(3 * 24 * 3600 + 4 * 3600)),
            "3d 4h"
        );
    }
}
