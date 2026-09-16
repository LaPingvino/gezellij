//! Gezellij: opt-in auto-freeze of idle sessions.
//!
//! When `auto_freeze_after "<duration>"` is set in the configuration, a session whose client
//! count has been zero for longer than that has every one of its pane cgroups frozen with the
//! cgroup v2 freezer (the same mechanism as `zellij freeze`), and is thawed again the instant a
//! client attaches.
//!
//! Shape of the thing:
//!
//! * one background thread per server, spawned only when the option is set *and* cgroup v2 is
//!   available; it wakes every [`TICK`] and does all the freezing. Whether the session really has
//!   pane cgroups is decided per tick, since the server creates them lazily with its first pane;
//! * `idle_since` is maintained by the server loop through [`AutoFreeze::clients_changed`]
//!   (called from the `remove_client!` chokepoint) and [`AutoFreeze::on_attach`], and
//!   re-derived on every tick so a path we did not think of self-corrects within 15 seconds;
//! * attaching thaws *inline* rather than waiting for the next tick - it is one idempotent
//!   sysfs write;
//! * the real freezer state always comes from sysfs, never from a flag we keep: a manual
//!   `zellij freeze`/`thaw`, or an upgrade that adopted somebody else's cgroups, must not
//!   confuse us. The only thing we remember is whether *we* froze during this idle period, so
//!   that we log once and so that a manual thaw while detached is not undone 15s later.
//!
//! Everything here is best-effort: a sysfs error is logged at warn level and the session carries
//! on running.

use crate::SessionState;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};
use zellij_utils::host_fabric::cgroups;

/// A duration as plain seconds, for log lines (zellij-server has no humantime dependency).
fn format_secs(duration: Duration) -> String {
    format!("{}s", duration.as_secs())
}

/// What one supervisor tick should do, given the session's idle clock (`None`: a client is
/// attached), whether every pane is currently frozen, and whether *we* froze it during this idle
/// period. `Some(true)` means freeze, `Some(false)` thaw, `None` leave alone.
///
/// Two deliberate asymmetries:
/// * a session we froze and that someone thawed by hand (`zellij thaw`) while still detached is
///   left thawed, until a client has attached and gone again;
/// * a session frozen by hand *while a client is attached* (the "look calmly at a runaway
///   process" case) is left frozen - we only ever undo our own freeze.
fn decide(
    idle_for: Option<Duration>,
    frozen: bool,
    we_froze: bool,
    threshold: Duration,
) -> Option<bool> {
    match idle_for {
        Some(idle_for) => {
            if frozen || we_froze || idle_for < threshold {
                None
            } else {
                Some(true)
            }
        },
        // attaching already thaws inline; this is the safety net for an attach path we missed
        None if frozen && we_froze => Some(false),
        None => None,
    }
}

/// How often the supervisor thread looks at the session.
const TICK: Duration = Duration::from_secs(15);

struct Inner {
    session_name: String,
    threshold: Duration,
    /// `Some(t)`: the session has had no clients since `t`. `None`: a client is attached.
    idle_since: Mutex<Option<Instant>>,
    /// whether *this* supervisor froze the session during the current idle period
    froze_this_idle_period: AtomicBool,
}

/// The one auto-freeze supervisor of this server, armed on first client connect when
/// `auto_freeze_after` is configured. A server serves exactly one session, so one is enough.
static AUTO_FREEZE: OnceLock<AutoFreeze> = OnceLock::new();

/// Arm auto-freeze once for this server. Later calls (e.g. a re-adopted session) are ignored.
pub(crate) fn arm(
    session_name: String,
    threshold: Duration,
    session_state: Arc<RwLock<SessionState>>,
) {
    if AUTO_FREEZE.get().is_some() {
        return;
    }
    if let Some(auto_freeze) = AutoFreeze::spawn(session_name, threshold, session_state) {
        let _ = AUTO_FREEZE.set(auto_freeze);
    }
}

/// The armed supervisor, if any. Cheap enough to call on every client removal.
pub(crate) fn get() -> Option<&'static AutoFreeze> {
    AUTO_FREEZE.get()
}

#[derive(Clone)]
pub(crate) struct AutoFreeze {
    inner: Arc<Inner>,
}

impl AutoFreeze {
    /// Arm auto-freeze for `session_name`, spawning the supervisor thread.
    ///
    /// Returns `None` (and logs why) when the feature cannot work here at all: no cgroup v2.
    ///
    /// Whether this session actually *has* pane cgroups cannot be decided here - the server
    /// creates its cgroup tree lazily, when it spawns the first pane, which happens after the
    /// first client connects. Every tick therefore re-checks it and does nothing while there is
    /// no recorded cgroup root.
    pub(crate) fn spawn(
        session_name: String,
        threshold: Duration,
        session_state: Arc<RwLock<SessionState>>,
    ) -> Option<Self> {
        if !cgroups::is_cgroup_v2_available() {
            log::warn!(
                "auto_freeze_after is set but cgroup v2 is not available; idle sessions will not \
                 be frozen"
            );
            return None;
        }
        let auto_freeze = AutoFreeze {
            inner: Arc::new(Inner {
                session_name,
                threshold,
                idle_since: Mutex::new(None),
                froze_this_idle_period: AtomicBool::new(false),
            }),
        };
        log::info!(
            "auto-freeze armed for session '{}': freezing after {} without an attached client",
            auto_freeze.inner.session_name,
            format_secs(threshold)
        );
        let supervisor = auto_freeze.clone();
        let _ = std::thread::Builder::new()
            .name("auto_freeze".to_string())
            .spawn(move || loop {
                std::thread::sleep(TICK);
                supervisor.tick(&session_state);
            });
        Some(auto_freeze)
    }

    /// The set of clients may have changed: start or stop the idle clock accordingly.
    ///
    /// Takes the `SessionState` lock only long enough to count the clients.
    pub(crate) fn clients_changed(&self, session_state: &Arc<RwLock<SessionState>>) {
        let has_clients = match session_state.read() {
            Ok(state) => !state.client_ids().is_empty(),
            Err(_) => return, // poisoned: something is very wrong, do not add to it
        };
        self.set_has_clients(has_clients);
    }

    /// A client just attached: thaw right away, do not wait for the next tick.
    pub(crate) fn on_attach(&self) {
        self.set_has_clients(true);
        self.thaw_now();
    }

    fn set_has_clients(&self, has_clients: bool) {
        let Ok(mut idle_since) = self.inner.idle_since.lock() else {
            return;
        };
        if has_clients {
            *idle_since = None;
            self.inner
                .froze_this_idle_period
                .store(false, Ordering::Relaxed);
        } else if idle_since.is_none() {
            *idle_since = Some(Instant::now());
        }
    }

    fn idle_for(&self) -> Option<Duration> {
        let idle_since = self.inner.idle_since.lock().ok()?;
        idle_since.map(|since| since.elapsed())
    }

    fn tick(&self, session_state: &Arc<RwLock<SessionState>>) {
        // read before `clients_changed`, which clears the marker when a client is attached
        let we_froze = self.inner.froze_this_idle_period.load(Ordering::Relaxed);
        // re-derive the idle clock from the truth, so a removal path we did not hook cannot
        // leave us stuck
        self.clients_changed(session_state);

        let session_name = &self.inner.session_name;
        let frozen = match cgroups::session_is_frozen(session_name) {
            Ok(Some(frozen)) => frozen,
            // no cgroup root, or no panes at all: nothing to freeze, and nothing to complain
            // about either
            Ok(None) => return,
            Err(e) => {
                log::warn!(
                    "auto-freeze: could not read the freezer state of '{}': {}",
                    session_name,
                    e
                );
                return;
            },
        };

        match decide(self.idle_for(), frozen, we_froze, self.inner.threshold) {
            Some(frozen) => self.freeze(frozen),
            None => {},
        }
    }

    /// Thaw the session now if it is frozen. Cheap, idempotent and safe to call from the server
    /// loop: two small sysfs reads/writes.
    fn thaw_now(&self) {
        match cgroups::session_is_frozen(&self.inner.session_name) {
            Ok(Some(true)) => self.freeze(false),
            _ => {},
        }
    }

    fn freeze(&self, frozen: bool) {
        let session_name = &self.inner.session_name;
        match cgroups::set_session_frozen(session_name, frozen) {
            Ok(Some((0, 0))) => {}, // no panes: nothing happened
            Ok(Some((ok, failed))) => {
                if frozen {
                    self.inner
                        .froze_this_idle_period
                        .store(true, Ordering::Relaxed);
                    log::info!(
                        "auto-freeze: froze {} pane(s) of '{}' after {} without an attached \
                         client; it will thaw when you attach",
                        ok,
                        session_name,
                        format_secs(self.inner.threshold)
                    );
                } else {
                    self.inner
                        .froze_this_idle_period
                        .store(false, Ordering::Relaxed);
                    log::info!("auto-freeze: thawed {} pane(s) of '{}'", ok, session_name);
                }
                if failed > 0 {
                    log::warn!(
                        "auto-freeze: {} pane(s) of '{}' could not be {}",
                        failed,
                        session_name,
                        if frozen { "frozen" } else { "thawed" }
                    );
                }
            },
            Ok(None) => {},
            Err(e) => log::warn!(
                "auto-freeze: could not {} '{}': {}",
                if frozen { "freeze" } else { "thaw" },
                session_name,
                e
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unarmed(threshold: Duration) -> AutoFreeze {
        AutoFreeze {
            inner: Arc::new(Inner {
                session_name: "test-session-that-does-not-exist".to_owned(),
                threshold,
                idle_since: Mutex::new(None),
                froze_this_idle_period: AtomicBool::new(false),
            }),
        }
    }

    #[test]
    fn idle_clock_starts_when_the_last_client_leaves_and_stops_on_attach() {
        let auto_freeze = unarmed(Duration::from_secs(600));
        assert_eq!(auto_freeze.idle_for(), None, "starts out not idle");

        auto_freeze.set_has_clients(false);
        let first = auto_freeze.idle_for().expect("idle clock running");
        // a second "no clients" must not restart the clock
        auto_freeze.set_has_clients(false);
        assert!(auto_freeze.idle_for().unwrap() >= first);

        auto_freeze.set_has_clients(true);
        assert_eq!(auto_freeze.idle_for(), None, "attached: not idle");
    }

    #[test]
    fn attaching_clears_the_froze_this_period_marker() {
        let auto_freeze = unarmed(Duration::from_secs(1));
        auto_freeze.set_has_clients(false);
        auto_freeze
            .inner
            .froze_this_idle_period
            .store(true, Ordering::Relaxed);
        auto_freeze.set_has_clients(true);
        assert!(!auto_freeze
            .inner
            .froze_this_idle_period
            .load(Ordering::Relaxed));
    }

    #[test]
    fn decide_freezes_only_an_idle_unfrozen_session_we_have_not_frozen_yet() {
        let threshold = Duration::from_secs(60);
        let long = Duration::from_secs(61);
        let short = Duration::from_secs(59);

        // idle past the threshold, not frozen, we have not frozen it: freeze
        assert_eq!(decide(Some(long), false, false, threshold), Some(true));
        // not idle long enough yet
        assert_eq!(decide(Some(short), false, false, threshold), None);
        // already frozen: nothing to do
        assert_eq!(decide(Some(long), true, true, threshold), None);
        // we froze it and somebody thawed it by hand while still detached: leave it thawed
        assert_eq!(decide(Some(long), false, true, threshold), None);
        // attached and frozen by us (an attach path that did not thaw inline): thaw
        assert_eq!(decide(None, true, true, threshold), Some(false));
        // attached and frozen by the user: leave it frozen
        assert_eq!(decide(None, true, false, threshold), None);
        // attached and running: nothing to do
        assert_eq!(decide(None, false, false, threshold), None);
    }

    #[test]
    fn a_session_without_cgroups_is_never_touched() {
        // no record file for this name: every operation is a no-op that must not panic
        let auto_freeze = unarmed(Duration::from_millis(1));
        auto_freeze.set_has_clients(false);
        let session_state = Arc::new(RwLock::new(SessionState::new()));
        auto_freeze.tick(&session_state);
        auto_freeze.thaw_now();
        assert!(!auto_freeze
            .inner
            .froze_this_idle_period
            .load(Ordering::Relaxed));
    }
}
