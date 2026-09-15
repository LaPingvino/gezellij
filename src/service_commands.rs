//! Implementation of the `zellij service ...` CLI subcommands.
//!
//! A *service* is a named command supervised by Gezellij: its definition lives in the on-disk
//! [`ServiceRegistry`] and, while running, it occupies a detached session named `svc-<name>` whose
//! single pane runs the command under the configured restart policy. Everything here is plumbing
//! between the CLI, that registry, and the ordinary client entrypoints in [`crate::commands`].

use std::fs;
use std::io;
use std::path::PathBuf;
use std::process::{Command as ProcessCommand, Stdio};

use zellij_utils::{
    cli::{CliArgs, Command, ServiceCommand, Sessions, SubscribeCli, SubscribeFormat},
    consts::session_info_folder_for_session,
    home::find_default_config_dir,
    host_fabric::{services::validate_service_name, systemd, ServiceDefinition, ServiceRegistry},
    input::config::Config,
    sessions::{kill_session, session_exists},
};

const GREEN: &str = "\u{1b}[32m";
const RED: &str = "\u{1b}[31m";
const RESET: &str = "\u{1b}[0m";

fn fail(message: impl std::fmt::Display) -> ! {
    eprintln!("{}", message);
    std::process::exit(2);
}

fn is_running(session: &str) -> bool {
    session_exists(session).unwrap_or(false)
}

/// The resurrection cache holds the *previous* layout of a session; for services it would silently
/// override an updated definition, so we drop it whenever a service is (re)started or stopped.
fn clear_resurrection_cache(session: &str) {
    match fs::remove_dir_all(session_info_folder_for_session(session)) {
        Ok(()) => {},
        Err(e) if e.kind() == io::ErrorKind::NotFound => {},
        Err(e) => eprintln!(
            "Warning: could not clear the saved layout of session '{}': {}",
            session, e
        ),
    }
}

fn registry_from_opts(opts: &CliArgs) -> ServiceRegistry {
    let config_dir = opts
        .config_dir
        .clone()
        .or_else(find_default_config_dir)
        .unwrap_or_else(|| {
            fail("Could not determine the Zellij config directory; pass --config-dir explicitly")
        });
    ServiceRegistry::in_config_dir(&config_dir)
}

fn load_or_fail(registry: &ServiceRegistry, name: &str) -> ServiceDefinition {
    if let Err(e) = validate_service_name(name) {
        fail(format!("Invalid service name: {}", e));
    }
    match registry.load(name) {
        Ok(Some(service)) => service,
        Ok(None) => fail(format!(
            "No service named '{}'. Define one with: zellij service add --name {} -- <command>",
            name, name
        )),
        Err(e) => fail(format!(
            "Could not read the definition of '{}': {}",
            name, e
        )),
    }
}

/// Build the `CliArgs` that attach (or detached-create) the service session.
fn attach_opts(
    opts: &CliArgs,
    session_name: String,
    service: Option<&ServiceDefinition>,
) -> CliArgs {
    let mut new_opts = opts.clone();
    new_opts.session = None;
    new_opts.command = Some(Command::Sessions(Sessions::Attach {
        session_name: Some(session_name),
        create: false,
        create_background: service.is_some(),
        force_run_commands: false,
        index: None,
        options: None,
        token: None,
        remember: false,
        forget: false,
        ca_cert: None,
        insecure: false,
        initial_command: service.map(|s| s.command.clone()).unwrap_or_default(),
        close_on_exit: false,
        start_suspended: false,
        restart: service.map(|s| s.restart),
    }));
    new_opts
}

fn start_service(opts: &CliArgs, service: &ServiceDefinition) {
    let session = service.session_name();
    if is_running(&session) {
        println!(
            "Service '{}' is already running (session {})",
            service.name, session
        );
        return;
    }
    clear_resurrection_cache(&session);
    if let Some(cwd) = service.cwd.as_ref() {
        if let Err(e) = std::env::set_current_dir(cwd) {
            fail(format!(
                "Could not enter the working directory {} of service '{}': {}",
                cwd.display(),
                service.name,
                e
            ));
        }
    }
    crate::commands::start_client(attach_opts(opts, session.clone(), Some(service)));
    println!(
        "Started service '{}' in background session {} (restart: {})",
        service.name, session, service.restart
    );
}

fn stop_service(service: &ServiceDefinition) {
    let session = service.session_name();
    if is_running(&session) {
        kill_session(&session);
        println!("Stopped service '{}' (session {})", service.name, session);
    } else {
        println!("Service '{}' is not running", service.name);
    }
    clear_resurrection_cache(&session);
}

/// Ask the running session which pane belongs to the service. The service's command always runs in
/// the session's first (and normally only) terminal pane, so the first `terminal_*` id will do.
fn service_pane_id(session: &str) -> Option<String> {
    let executable = std::env::current_exe().ok()?;
    let output = ProcessCommand::new(executable)
        .args(["--session", session, "action", "list-clients"])
        .stderr(Stdio::inherit())
        .output()
        .ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .split_whitespace()
        .find(|token| token.starts_with("terminal_"))
        .map(|token| token.to_string())
}

fn print_logs(
    service: &ServiceDefinition,
    tail: Option<usize>,
    follow: bool,
    config: Option<Config>,
) {
    let session = service.session_name();
    if !is_running(&session) {
        fail(format!(
            "Service '{}' is not running; start it with: zellij service start {}",
            service.name, service.name
        ));
    }
    // A headless session has no focused pane, so "dump the focused pane" would dump nothing:
    // always address the service pane explicitly (it is the session's first terminal pane).
    let pane_id = service_pane_id(&session).unwrap_or_else(|| "terminal_0".to_string());
    if follow {
        crate::commands::subscribe_to_session(
            SubscribeCli {
                pane_id: vec![pane_id],
                scrollback: Some(tail.unwrap_or(0)),
                format: SubscribeFormat::Raw,
                ansi: false,
            },
            Some(session),
            config,
        );
        return;
    }
    let tmp = std::env::temp_dir().join(format!(
        "gezellij-logs-{}-{}.txt",
        service.name,
        std::process::id()
    ));
    // `commands::send_action_to_session` exits the process once the action completes, so run the
    // dump as a child process and read the file afterwards.
    let executable = match std::env::current_exe() {
        Ok(executable) => executable,
        Err(e) => fail(format!("Could not determine the zellij executable: {}", e)),
    };
    let status = ProcessCommand::new(executable)
        .arg("--session")
        .arg(&session)
        .args([
            "action",
            "dump-screen",
            "--full",
            "--pane-id",
            &pane_id,
            "--path",
        ])
        .arg(&tmp)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status();
    match status {
        Ok(status) if status.success() => {},
        Ok(status) => fail(format!(
            "Could not read the output of service '{}' (dump-screen exited with {})",
            service.name, status
        )),
        Err(e) => fail(format!(
            "Could not read the output of service '{}': {}",
            service.name, e
        )),
    }
    let contents = fs::read_to_string(&tmp);
    let _ = fs::remove_file(&tmp);
    let contents = match contents {
        Ok(contents) => contents,
        Err(e) => fail(format!(
            "Could not read the output of service '{}': {}",
            service.name, e
        )),
    };
    // a dumped pane is padded with blank lines up to its height - those are not output
    let mut lines: Vec<&str> = contents.lines().collect();
    while lines.last().map(|l| l.trim().is_empty()).unwrap_or(false) {
        lines.pop();
    }
    let start = match tail {
        Some(tail) => lines.len().saturating_sub(tail),
        None => 0,
    };
    for line in &lines[start..] {
        println!("{}", line);
    }
}

fn list_services(registry: &ServiceRegistry, no_formatting: bool) {
    let services = match registry.list() {
        Ok(services) => services,
        Err(e) => fail(format!("Could not read the service registry: {}", e)),
    };
    if services.is_empty() {
        println!(
            "No services defined. Add one with: zellij service add --name <name> -- <command>"
        );
        return;
    }
    struct Row {
        name: String,
        status: &'static str,
        running: bool,
        restart: String,
        cwd: String,
        command: String,
    }
    let rows: Vec<Row> = services
        .iter()
        .map(|service| {
            let running = is_running(&service.session_name());
            Row {
                name: service.name.clone(),
                status: if running { "running" } else { "stopped" },
                running,
                restart: service.restart.to_string(),
                cwd: service
                    .cwd
                    .as_ref()
                    .map(|c| c.display().to_string())
                    .unwrap_or_else(|| "-".to_string()),
                command: service.command_display(),
            }
        })
        .collect();

    // widths are computed on the plain text, before any colouring is applied
    let width = |header: &str, values: Vec<&str>| {
        values
            .into_iter()
            .map(|v| v.chars().count())
            .chain(std::iter::once(header.chars().count()))
            .max()
            .unwrap_or(0)
    };
    let name_w = width("NAME", rows.iter().map(|r| r.name.as_str()).collect());
    let status_w = width("STATUS", rows.iter().map(|r| r.status).collect());
    let restart_w = width("RESTART", rows.iter().map(|r| r.restart.as_str()).collect());
    let cwd_w = width("CWD", rows.iter().map(|r| r.cwd.as_str()).collect());

    println!(
        "{:<name_w$}  {:<status_w$}  {:<restart_w$}  {:<cwd_w$}  {}",
        "NAME",
        "STATUS",
        "RESTART",
        "CWD",
        "COMMAND",
        name_w = name_w,
        status_w = status_w,
        restart_w = restart_w,
        cwd_w = cwd_w,
    );
    for row in &rows {
        let padded_status = format!("{:<width$}", row.status, width = status_w);
        let status = if no_formatting {
            padded_status
        } else {
            let colour = if row.running { GREEN } else { RED };
            format!("{}{}{}", colour, padded_status, RESET)
        };
        println!(
            "{:<name_w$}  {}  {:<restart_w$}  {:<cwd_w$}  {}",
            row.name,
            status,
            row.restart,
            row.cwd,
            row.command,
            name_w = name_w,
            restart_w = restart_w,
            cwd_w = cwd_w,
        );
    }
}

fn export_systemd(opts: &CliArgs, service: &ServiceDefinition, install: bool) {
    let executable = match std::env::current_exe() {
        Ok(executable) => executable,
        Err(e) => fail(format!("Could not determine the zellij executable: {}", e)),
    };
    let mut extra_env: Vec<(String, String)> = vec![];
    if let Ok(socket_dir) = std::env::var("ZELLIJ_SOCKET_DIR") {
        extra_env.push(("ZELLIJ_SOCKET_DIR".to_string(), socket_dir));
    }
    if let Some(config_dir) = opts.config_dir.as_ref() {
        extra_env.push((
            "ZELLIJ_CONFIG_DIR".to_string(),
            config_dir.to_string_lossy().into_owned(),
        ));
    }
    if install {
        match systemd::install_user_unit(service, &executable, &extra_env) {
            Ok(path) => {
                println!("Wrote {}", path.display());
                println!(
                    "Enable it with: systemctl --user daemon-reload && systemctl --user enable --now {}",
                    systemd::unit_name(&service.name)
                );
            },
            Err(e) => fail(format!("Could not install the systemd unit: {}", e)),
        }
    } else {
        print!(
            "{}",
            systemd::render_user_unit(service, &executable, &extra_env)
        );
    }
}

pub(crate) fn run_service_command(opts: CliArgs, command: ServiceCommand) {
    let registry = registry_from_opts(&opts);
    match command {
        ServiceCommand::Add {
            name,
            restart,
            cwd,
            no_start,
            force,
            command,
        } => {
            if let Err(e) = validate_service_name(&name) {
                fail(format!("Invalid service name: {}", e));
            }
            if registry.exists(&name) && !force {
                fail(format!(
                    "service '{}' already exists (use --force to overwrite)",
                    name
                ));
            }
            let current_dir = match std::env::current_dir() {
                Ok(current_dir) => current_dir,
                Err(e) => fail(format!("Could not determine the current directory: {}", e)),
            };
            let cwd: PathBuf = match cwd {
                Some(cwd) => current_dir.join(cwd),
                None => current_dir,
            };
            let service = ServiceDefinition::new(name.clone(), command, Some(cwd), restart);
            match registry.save(&service) {
                Ok(path) => println!("Saved service '{}' -> {}", name, path.display()),
                Err(e) => fail(format!("Could not save service '{}': {}", name, e)),
            }
            if !no_start {
                start_service(&opts, &service);
            }
        },
        ServiceCommand::Start { name } => {
            let service = load_or_fail(&registry, &name);
            start_service(&opts, &service);
        },
        ServiceCommand::Stop { name } => {
            let service = load_or_fail(&registry, &name);
            stop_service(&service);
        },
        ServiceCommand::Remove { name } => {
            let service = load_or_fail(&registry, &name);
            stop_service(&service);
            match registry.remove(&name) {
                Ok(true) => println!("Removed service '{}'", name),
                Ok(false) => println!("Service '{}' had no definition to remove", name),
                Err(e) => fail(format!("Could not remove service '{}': {}", name, e)),
            }
        },
        ServiceCommand::List { no_formatting } => list_services(&registry, no_formatting),
        ServiceCommand::Attach { name } => {
            let service = load_or_fail(&registry, &name);
            let session = service.session_name();
            if !is_running(&session) {
                fail(format!(
                    "Service '{}' is not running; start it with: zellij service start {}",
                    name, name
                ));
            }
            crate::commands::start_client(attach_opts(&opts, session, None));
        },
        ServiceCommand::Logs { name, tail, follow } => {
            let service = load_or_fail(&registry, &name);
            let config = Config::try_from(&opts).ok();
            print_logs(&service, tail, follow, config);
        },
        ServiceCommand::ExportSystemd { name, install } => {
            let service = load_or_fail(&registry, &name);
            export_systemd(&opts, &service, install);
        },
    }
}
