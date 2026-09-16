//! Gezellij host-native process fabric.
//!
//! Everything in this module is about running processes *on the host* - under the user's own
//! account, with plain POSIX permissions - while still giving them the lifecycle amenities one
//! expects from a service manager:
//!
//! * [`services`]: the on-disk registry of supervised background services managed by
//!   `zellij service ...` (each service is a detached session running one command with a
//!   [`crate::input::command::RestartPolicy`]).
//! * [`systemd`]: rendering of `systemd --user` units so a service comes up at login.
//!
//! Later phases add cgroup v2 freezing and loopback ULA networking helpers here.

#[cfg(unix)]
pub mod cgroups;
#[cfg(unix)]
pub mod handover;
pub mod services;
pub mod systemd;
#[cfg(unix)]
pub mod upgrade;

pub use services::{ServiceDefinition, ServiceRegistry, SERVICE_SESSION_PREFIX};
