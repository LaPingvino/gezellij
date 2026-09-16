//! On-disk registry of Gezellij services.
//!
//! A *service* is a named, supervised command. At runtime it lives in a detached Zellij session
//! called `svc-<name>` whose single pane runs the command with the configured
//! [`RestartPolicy`]. The definition itself is a small JSON file in
//! `<config dir>/services/<name>.json`, so it survives reboots, can be edited by hand, and can be
//! exported to a `systemd --user` unit.

use crate::input::command::RestartPolicy;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Session-name prefix that marks a session as belonging to a service.
pub const SERVICE_SESSION_PREFIX: &str = "svc-";

/// Name of the sub-directory (under the config dir) holding service definitions.
pub const SERVICES_DIR_NAME: &str = "services";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServiceDefinition {
    pub name: String,
    /// Opaque, stable identity of this service (uuid-v4 hex). It is what the service's loopback
    /// IPv6 address is derived from, so that renaming a service does not move it. Definitions
    /// written before Phase 4 have no id; see [`ServiceDefinition::address_id`].
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    /// argv: program followed by its arguments
    pub command: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub restart: RestartPolicy,
    /// unix timestamp (seconds) of when the definition was created
    #[serde(default)]
    pub created_at: u64,
    /// Give the service a loopback IPv6 address of its own (see [`crate::host_fabric::net`]) and
    /// export it to the command as `GEZELLIJ_BIND_ADDR` / `GEZELLIJ_BIND_PORT` /
    /// `GEZELLIJ_BIND_URL`.
    #[serde(default)]
    pub bind_ip: bool,
}

impl ServiceDefinition {
    pub fn new(
        name: impl Into<String>,
        command: Vec<String>,
        cwd: Option<PathBuf>,
        restart: RestartPolicy,
    ) -> Self {
        ServiceDefinition {
            name: name.into(),
            id: uuid::Uuid::new_v4().simple().to_string(),
            command,
            cwd,
            restart,
            created_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            bind_ip: false,
        }
    }
    /// The string a service's loopback address is derived from.
    ///
    /// Normally the opaque [`ServiceDefinition::id`]. Definitions written before Phase 4 do not
    /// have one, so they fall back to a namespaced form of their name: stable (the address never
    /// moves under a running service) without having to rewrite old files.
    pub fn address_id(&self) -> String {
        if self.id.is_empty() {
            format!("name:{}", self.name)
        } else {
            self.id.clone()
        }
    }
    /// The Zellij session this service runs in.
    pub fn session_name(&self) -> String {
        session_name_for(&self.name)
    }
    /// The command as a single shell-ish string, for display.
    pub fn command_display(&self) -> String {
        self.command
            .iter()
            .map(|part| {
                if part.is_empty() || part.chars().any(char::is_whitespace) {
                    format!("{:?}", part)
                } else {
                    part.clone()
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
}

pub fn session_name_for(service_name: &str) -> String {
    format!("{}{}", SERVICE_SESSION_PREFIX, service_name)
}

/// Service names double as file names and as part of session names, so keep them simple.
pub fn validate_service_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("service name cannot be empty".to_string());
    }
    if name.len() > 64 {
        return Err("service name cannot be longer than 64 characters".to_string());
    }
    if name.starts_with('.') || name.starts_with('-') {
        return Err("service name cannot start with '.' or '-'".to_string());
    }
    if let Some(bad) = name
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')))
    {
        return Err(format!(
            "service name contains invalid character {:?}; allowed: letters, digits, '-', '_', '.'",
            bad
        ));
    }
    Ok(())
}

/// A directory of `<name>.json` service definitions.
#[derive(Debug, Clone)]
pub struct ServiceRegistry {
    dir: PathBuf,
}

impl ServiceRegistry {
    /// Registry inside the given Zellij config directory (`<config_dir>/services`).
    pub fn in_config_dir(config_dir: &Path) -> Self {
        ServiceRegistry {
            dir: config_dir.join(SERVICES_DIR_NAME),
        }
    }
    pub fn at(dir: PathBuf) -> Self {
        ServiceRegistry { dir }
    }
    pub fn dir(&self) -> &Path {
        &self.dir
    }
    fn file_for(&self, name: &str) -> PathBuf {
        self.dir.join(format!("{}.json", name))
    }
    pub fn exists(&self, name: &str) -> bool {
        self.file_for(name).is_file()
    }
    pub fn save(&self, service: &ServiceDefinition) -> io::Result<PathBuf> {
        validate_service_name(&service.name)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        fs::create_dir_all(&self.dir)?;
        let path = self.file_for(&service.name);
        let json = serde_json::to_string_pretty(service)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        // write-then-rename so a crash never leaves a truncated definition behind
        let tmp = self.dir.join(format!(".{}.json.tmp", service.name));
        fs::write(&tmp, json)?;
        fs::rename(&tmp, &path)?;
        Ok(path)
    }
    pub fn load(&self, name: &str) -> io::Result<Option<ServiceDefinition>> {
        validate_service_name(name).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        match fs::read_to_string(self.file_for(name)) {
            Ok(json) => serde_json::from_str(&json)
                .map(Some)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }
    /// Returns whether a definition was actually removed.
    pub fn remove(&self, name: &str) -> io::Result<bool> {
        validate_service_name(name).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        match fs::remove_file(self.file_for(name)) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }
    /// All definitions, sorted by name. Unreadable files are skipped (and logged).
    pub fn list(&self) -> io::Result<Vec<ServiceDefinition>> {
        let mut services = vec![];
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(services),
            Err(e) => return Err(e),
        };
        for entry in entries {
            let path = entry?.path();
            if path.extension().map(|e| e == "json").unwrap_or(false)
                && !path
                    .file_name()
                    .and_then(|f| f.to_str())
                    .map(|f| f.starts_with('.'))
                    .unwrap_or(true)
            {
                match fs::read_to_string(&path)
                    .map_err(|e| e.to_string())
                    .and_then(|json| {
                        serde_json::from_str::<ServiceDefinition>(&json).map_err(|e| e.to_string())
                    }) {
                    Ok(service) => services.push(service),
                    Err(e) => log::error!(
                        "Skipping unreadable service definition {}: {}",
                        path.display(),
                        e
                    ),
                }
            }
        }
        services.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(services)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_registry() -> ServiceRegistry {
        let dir = std::env::temp_dir().join(format!(
            "gezellij-services-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        ServiceRegistry::at(dir)
    }

    #[test]
    fn save_load_list_remove_roundtrip() {
        let registry = temp_registry();
        let api = ServiceDefinition::new(
            "api",
            vec!["./start-api.sh".into(), "--port".into(), "8080".into()],
            Some(PathBuf::from("/srv/api")),
            RestartPolicy::OnFailure,
        );
        let bot = ServiceDefinition::new(
            "bot",
            vec!["python".into(), "bot.py".into()],
            None,
            RestartPolicy::Always,
        );
        registry.save(&api).unwrap();
        registry.save(&bot).unwrap();

        assert_eq!(registry.load("api").unwrap(), Some(api.clone()));
        assert_eq!(registry.load("nope").unwrap(), None);
        let names: Vec<_> = registry
            .list()
            .unwrap()
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(names, vec!["api".to_string(), "bot".to_string()]);

        assert!(registry.remove("api").unwrap());
        assert!(!registry.remove("api").unwrap());
        assert_eq!(registry.list().unwrap().len(), 1);
        let _ = fs::remove_dir_all(registry.dir());
    }

    #[test]
    fn service_names_are_validated() {
        assert!(validate_service_name("api").is_ok());
        assert!(validate_service_name("my-bot_2.0").is_ok());
        assert!(validate_service_name("").is_err());
        assert!(validate_service_name("../etc").is_err());
        assert!(validate_service_name("has space").is_err());
        assert!(validate_service_name("-dash").is_err());
        let registry = temp_registry();
        assert!(registry.load("../x").is_err());
    }

    #[test]
    fn session_name_uses_prefix() {
        let svc = ServiceDefinition::new("api", vec!["true".into()], None, RestartPolicy::No);
        assert_eq!(svc.session_name(), "svc-api");
        assert_eq!(svc.command_display(), "true");
        let spaced = ServiceDefinition::new(
            "x",
            vec!["sh".into(), "-c".into(), "echo hi".into()],
            None,
            RestartPolicy::No,
        );
        assert_eq!(spaced.command_display(), "sh -c \"echo hi\"");
    }

    #[test]
    fn restart_policy_default_is_omitted_in_json() {
        let svc = ServiceDefinition::new("x", vec!["true".into()], None, RestartPolicy::No);
        let json = serde_json::to_string(&svc).unwrap();
        assert!(json.contains("\"restart\":\"no\""));
        let parsed: ServiceDefinition =
            serde_json::from_str(r#"{"name":"y","command":["true"]}"#).unwrap();
        assert_eq!(parsed.restart, RestartPolicy::No);
        let parsed: ServiceDefinition =
            serde_json::from_str(r#"{"name":"y","command":["true"],"restart":"on-failure"}"#)
                .unwrap();
        assert_eq!(parsed.restart, RestartPolicy::OnFailure);
    }
}
