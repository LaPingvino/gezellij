//! Trigger a command
use crate::data::{Direction, OriginatingPlugin};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::str::FromStr;

#[derive(Debug, Clone)]
pub enum TerminalAction {
    OpenFile(OpenFilePayload),
    RunCommand(RunCommand),
}

impl TerminalAction {
    pub fn change_cwd(&mut self, new_cwd: PathBuf) {
        match self {
            TerminalAction::OpenFile(open_file_payload) => {
                open_file_payload.cwd = Some(new_cwd);
            },
            TerminalAction::RunCommand(run_command) => {
                run_command.cwd = Some(new_cwd);
            },
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct OpenFilePayload {
    pub path: PathBuf,
    pub line_number: Option<usize>,
    pub cwd: Option<PathBuf>,
    pub originating_plugin: Option<OriginatingPlugin>,
}

impl Default for OpenFilePayload {
    fn default() -> Self {
        OpenFilePayload {
            path: PathBuf::new(),
            line_number: None,
            cwd: None,
            originating_plugin: None,
        }
    }
}

impl OpenFilePayload {
    pub fn new(path: PathBuf, line_number: Option<usize>, cwd: Option<PathBuf>) -> Self {
        OpenFilePayload {
            path,
            line_number,
            cwd,
            originating_plugin: None,
        }
    }
    pub fn with_originating_plugin(mut self, originating_plugin: OriginatingPlugin) -> Self {
        self.originating_plugin = Some(originating_plugin);
        self
    }
}

/// What the server should do when a command pane's process exits.
///
/// This is the supervision primitive behind `zellij service` (Gezellij): a pane whose command
/// carries a restart policy other than [`RestartPolicy::No`] is automatically re-run (with
/// exponential backoff) instead of merely being held for the user to press ENTER.
#[derive(Clone, Copy, Debug, Deserialize, Default, Serialize, PartialEq, Eq, Hash, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum RestartPolicy {
    /// Never restart automatically (the classic Zellij behaviour).
    #[default]
    No,
    /// Restart only when the command exits with a non-zero status (or is killed by a signal).
    OnFailure,
    /// Restart whenever the command exits, regardless of status.
    Always,
}

impl RestartPolicy {
    pub fn is_no(&self) -> bool {
        matches!(self, RestartPolicy::No)
    }
    /// Whether a process that exited with `exit_status` (`None` = killed by a signal or unknown)
    /// should be restarted under this policy.
    pub fn should_restart(&self, exit_status: Option<i32>) -> bool {
        match self {
            RestartPolicy::No => false,
            RestartPolicy::Always => true,
            RestartPolicy::OnFailure => exit_status.map(|s| s != 0).unwrap_or(true),
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            RestartPolicy::No => "no",
            RestartPolicy::OnFailure => "on-failure",
            RestartPolicy::Always => "always",
        }
    }
}

impl std::fmt::Display for RestartPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl FromStr for RestartPolicy {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "no" | "never" | "false" => Ok(RestartPolicy::No),
            "on-failure" | "on_failure" | "onfailure" => Ok(RestartPolicy::OnFailure),
            "always" | "true" => Ok(RestartPolicy::Always),
            other => Err(format!(
                "invalid restart policy '{}', expected one of: no, on-failure, always",
                other
            )),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Default, Serialize, PartialEq, Eq)]
pub struct RunCommand {
    #[serde(alias = "cmd")]
    pub command: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub hold_on_close: bool,
    #[serde(default)]
    pub hold_on_start: bool,
    #[serde(default)]
    pub originating_plugin: Option<OriginatingPlugin>,
    #[serde(default)]
    pub use_terminal_title: bool,
    /// Gezellij: supervision policy applied when the command exits.
    #[serde(default, skip_serializing_if = "RestartPolicy::is_no")]
    pub restart: RestartPolicy,
}

impl std::fmt::Display for RunCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut command: String = self
            .command
            .as_path()
            .as_os_str()
            .to_string_lossy()
            .to_string();
        for arg in &self.args {
            command.push(' ');
            command.push_str(arg);
        }
        write!(f, "{}", command)
    }
}

/// Intermediate representation
#[derive(Clone, Debug, Deserialize, Default, Serialize, PartialEq, Eq)]
pub struct RunCommandAction {
    #[serde(rename = "cmd")]
    pub command: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub direction: Option<Direction>,
    #[serde(default)]
    pub hold_on_close: bool,
    #[serde(default)]
    pub hold_on_start: bool,
    #[serde(default)]
    pub originating_plugin: Option<OriginatingPlugin>,
    #[serde(default)]
    pub use_terminal_title: bool,
    /// Gezellij: supervision policy applied when the command exits.
    #[serde(default, skip_serializing_if = "RestartPolicy::is_no")]
    pub restart: RestartPolicy,
}

impl From<RunCommandAction> for RunCommand {
    fn from(action: RunCommandAction) -> Self {
        RunCommand {
            command: action.command,
            args: action.args,
            cwd: action.cwd,
            hold_on_close: action.hold_on_close,
            hold_on_start: action.hold_on_start,
            originating_plugin: action.originating_plugin,
            use_terminal_title: action.use_terminal_title,
            restart: action.restart,
        }
    }
}

impl From<RunCommand> for RunCommandAction {
    fn from(run_command: RunCommand) -> Self {
        RunCommandAction {
            command: run_command.command,
            args: run_command.args,
            cwd: run_command.cwd,
            direction: None,
            hold_on_close: run_command.hold_on_close,
            hold_on_start: run_command.hold_on_start,
            originating_plugin: run_command.originating_plugin,
            use_terminal_title: run_command.use_terminal_title,
            restart: run_command.restart,
        }
    }
}

impl RunCommandAction {
    pub fn new(mut command: Vec<String>) -> Self {
        if command.is_empty() {
            Default::default()
        } else {
            RunCommandAction {
                command: PathBuf::from(command.remove(0)),
                args: command,
                ..Default::default()
            }
        }
    }
    pub fn populate_originating_plugin(&mut self, originating_plugin: OriginatingPlugin) {
        self.originating_plugin = Some(originating_plugin);
    }
}

impl RunCommand {
    pub fn new(command: PathBuf) -> Self {
        RunCommand {
            command,
            ..Default::default()
        }
    }
    pub fn with_cwd(mut self, cwd: PathBuf) -> Self {
        self.cwd = Some(cwd);
        self
    }
    pub fn with_restart(mut self, restart: RestartPolicy) -> Self {
        self.restart = restart;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_policy(s: &str) -> Result<RestartPolicy, String> {
        <RestartPolicy as FromStr>::from_str(s)
    }

    #[test]
    fn restart_policy_from_str_accepts_known_spellings() {
        for s in ["no", "No", "NO", "never", "false", " no "] {
            assert_eq!(parse_policy(s).unwrap(), RestartPolicy::No, "{}", s);
        }
        for s in [
            "on-failure",
            "on_failure",
            "onfailure",
            "On-Failure",
            "ONFAILURE",
        ] {
            assert_eq!(parse_policy(s).unwrap(), RestartPolicy::OnFailure, "{}", s);
        }
        for s in ["always", "Always", "ALWAYS", "true"] {
            assert_eq!(parse_policy(s).unwrap(), RestartPolicy::Always, "{}", s);
        }
    }

    #[test]
    fn restart_policy_from_str_rejects_unknown_spellings() {
        for s in ["sometimes", "", "on failure", "maybe", "0", "1"] {
            assert!(parse_policy(s).is_err(), "expected '{}' to be rejected", s);
        }
    }

    #[test]
    fn restart_policy_display_round_trips_through_from_str() {
        for policy in [
            RestartPolicy::No,
            RestartPolicy::OnFailure,
            RestartPolicy::Always,
        ] {
            assert_eq!(parse_policy(&policy.to_string()).unwrap(), policy);
            assert_eq!(policy.to_string(), policy.as_str());
        }
    }

    #[test]
    fn restart_policy_should_restart_truth_table() {
        // No: never restarts
        assert!(!RestartPolicy::No.should_restart(Some(0)));
        assert!(!RestartPolicy::No.should_restart(Some(1)));
        assert!(!RestartPolicy::No.should_restart(None));

        // Always: restarts regardless of exit status
        assert!(RestartPolicy::Always.should_restart(Some(0)));
        assert!(RestartPolicy::Always.should_restart(Some(1)));
        assert!(RestartPolicy::Always.should_restart(None));

        // OnFailure: restarts on non-zero exit or unknown status (signal), not on success
        assert!(!RestartPolicy::OnFailure.should_restart(Some(0)));
        assert!(RestartPolicy::OnFailure.should_restart(Some(1)));
        assert!(RestartPolicy::OnFailure.should_restart(None));
    }

    #[test]
    fn restart_policy_is_no() {
        assert!(RestartPolicy::No.is_no());
        assert!(RestartPolicy::default().is_no());
        assert!(!RestartPolicy::OnFailure.is_no());
        assert!(!RestartPolicy::Always.is_no());
    }

    #[test]
    fn run_command_debug_shows_the_restart_policy() {
        // This used to assert the opposite: a hand-written Debug hid a default `restart` so that
        // upstream Zellij's insta snapshots stayed byte-identical. We no longer optimise for
        // merging upstream, so the derived Debug is back and it shows every field.
        let run_command = RunCommand {
            command: PathBuf::from("tail"),
            ..Default::default()
        };
        let debug = format!("{:?}", run_command);
        assert!(
            debug.contains("restart: No"),
            "expected the restart policy in debug output, got: {}",
            debug
        );
    }

    #[test]
    fn run_command_debug_includes_non_default_restart() {
        let run_command = RunCommand {
            command: PathBuf::from("tail"),
            restart: RestartPolicy::Always,
            ..Default::default()
        };
        let debug = format!("{:?}", run_command);
        assert!(
            debug.contains("restart"),
            "expected a restart field in debug output, got: {}",
            debug
        );
        assert!(
            debug.contains("Always"),
            "expected the restart policy in debug output, got: {}",
            debug
        );
    }

    #[test]
    fn restart_policy_serializes_as_kebab_case() {
        assert_eq!(
            serde_json::to_string(&RestartPolicy::OnFailure).unwrap(),
            "\"on-failure\""
        );
        assert_eq!(
            serde_json::to_string(&RestartPolicy::Always).unwrap(),
            "\"always\""
        );
        assert_eq!(serde_json::to_string(&RestartPolicy::No).unwrap(), "\"no\"");
    }

    #[test]
    fn run_command_serde_omits_default_restart() {
        let run_command = RunCommand {
            command: PathBuf::from("tail"),
            ..Default::default()
        };
        let serialized = serde_json::to_string(&run_command).unwrap();
        assert!(
            !serialized.contains("restart"),
            "expected restart to be skipped when No, got: {}",
            serialized
        );
        let deserialized: RunCommand = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized, run_command);
    }

    #[test]
    fn run_command_serde_includes_non_default_restart() {
        let run_command = RunCommand {
            command: PathBuf::from("tail"),
            restart: RestartPolicy::Always,
            ..Default::default()
        };
        let serialized = serde_json::to_string(&run_command).unwrap();
        assert!(
            serialized.contains("\"restart\":\"always\""),
            "expected restart in serialized output, got: {}",
            serialized
        );
        let deserialized: RunCommand = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized, run_command);
        assert_eq!(deserialized.restart, RestartPolicy::Always);
    }

    #[test]
    fn run_command_deserializes_missing_restart_as_no() {
        let deserialized: RunCommand = serde_json::from_str(r#"{"command":"tail"}"#).unwrap();
        assert_eq!(deserialized.restart, RestartPolicy::No);
    }

    #[test]
    fn run_command_with_restart_builder() {
        let run_command = RunCommand {
            command: PathBuf::from("tail"),
            ..Default::default()
        }
        .with_restart(RestartPolicy::OnFailure);
        assert_eq!(run_command.restart, RestartPolicy::OnFailure);
    }
}
