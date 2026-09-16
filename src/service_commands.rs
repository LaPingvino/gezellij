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
    consts::{session_info_folder_for_session, ZELLIJ_SOCK_DIR},
    home::find_default_config_dir,
    host_fabric::{
        net, services::validate_service_name, systemd, ServiceDefinition, ServiceRegistry,
    },
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

fn config_dir_from_opts(opts: &CliArgs) -> PathBuf {
    opts.config_dir
        .clone()
        .or_else(find_default_config_dir)
        .unwrap_or_else(|| {
            fail("Could not determine the Zellij config directory; pass --config-dir explicitly")
        })
}

fn registry_from_opts(opts: &CliArgs) -> ServiceRegistry {
    ServiceRegistry::in_config_dir(&config_dir_from_opts(opts))
}

/// Export a service's own loopback address into *this* process' environment.
///
/// The pane that runs the service inherits its environment from the session server, and the
/// session server is spawned as a child of this very process (`spawn_server` in the background
/// case, the explicit `--server --server-foreground` child in `service run`); neither clears the
/// environment, and pane spawning only *adds* variables. So plain `set_var` here is enough, and
/// unlike prepending `env FOO=bar ...` to the command it leaves the stored definition, the
/// displayed command and the restart/resurrect machinery untouched.
///
/// Returns the address, if the service asked for one.
fn export_bind_env(opts: &CliArgs, service: &ServiceDefinition) -> Option<std::net::SocketAddrV6> {
    if !service.bind_ip {
        return None;
    }
    let config_dir = config_dir_from_opts(opts);
    let prefix = match net::load_or_create_prefix(&config_dir) {
        Ok(prefix) => prefix,
        Err(e) => fail(format!(
            "Could not read or create the loopback IPv6 prefix in {}: {}",
            config_dir.display(),
            e
        )),
    };
    let address = net::bind_address_for(&prefix, service)?;
    std::env::set_var("GEZELLIJ_BIND_ADDR", address.ip().to_string());
    std::env::set_var("GEZELLIJ_BIND_PORT", address.port().to_string());
    std::env::set_var("GEZELLIJ_BIND_URL", net::bind_url(&address));
    if !net::is_prefix_routed_locally(&prefix) {
        eprintln!(
            "Warning: {} is not routed on this machine yet, so service '{}' cannot bind {}.",
            prefix,
            service.name,
            address.ip()
        );
        eprintln!("{}", net::setup_instructions(&prefix));
    }
    Some(address)
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
    let bind_address = export_bind_env(opts, service);
    crate::commands::start_client(attach_opts(opts, session.clone(), Some(service)));
    println!(
        "Started service '{}' in background session {} (restart: {})",
        service.name, session, service.restart
    );
    if let Some(address) = bind_address {
        println!("  listening on {}", net::bind_url(&address));
    }
}

/// Host the service session's server as a foreground child of this process (for systemd).
///
/// 1. spawn `zellij --server <socket> --server-foreground` as a child (no daemonizing),
/// 2. wait for its socket, then perform the same first-client handshake `attach
///    --create-background` does, which creates the session with the supervised command,
/// 3. block until the server exits and exit with its status.
fn run_service_in_foreground(opts: &CliArgs, service: &ServiceDefinition) -> ! {
    let session = service.session_name();
    if is_running(&session) {
        fail(format!(
            "Service '{}' is already running (session {}); stop it first if a supervisor should own it",
            service.name, session
        ));
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
    // before the server child is spawned: it (and thus the service pane) inherits this environment
    let bind_address = export_bind_env(opts, service);
    let executable = match std::env::current_exe() {
        Ok(executable) => executable,
        Err(e) => fail(format!("Could not determine the zellij executable: {}", e)),
    };
    let socket_path = ZELLIJ_SOCK_DIR.join(&session);
    if let Err(e) = fs::create_dir_all(&*ZELLIJ_SOCK_DIR) {
        fail(format!(
            "Could not create the socket directory {}: {}",
            ZELLIJ_SOCK_DIR.display(),
            e
        ));
    }
    let mut command = ProcessCommand::new(executable);
    command
        .arg("--server")
        .arg(&socket_path)
        .arg("--server-foreground");
    if opts.debug {
        command.arg("--debug");
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => fail(format!("Could not start the session server: {}", e)),
    };
    // wait for the server to be listening before we connect as its first client
    let started = std::time::Instant::now();
    loop {
        if socket_path.exists() {
            break;
        }
        if let Ok(Some(status)) = child.try_wait() {
            fail(format!(
                "The session server exited before it was ready (status {})",
                status
            ));
        }
        if started.elapsed() > std::time::Duration::from_secs(15) {
            let _ = child.kill();
            fail("Timed out waiting for the session server to start");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    crate::commands::create_session_on_running_server(
        opts.clone(),
        session.clone(),
        service.command.clone(),
        service.restart,
    );
    eprintln!(
        "Running service '{}' in the foreground (session {}, restart: {}); the session server is pid {}",
        service.name,
        session,
        service.restart,
        child.id()
    );
    if let Some(address) = bind_address {
        eprintln!("  listening on {}", net::bind_url(&address));
    }
    match child.wait() {
        Ok(status) => {
            clear_resurrection_cache(&session);
            std::process::exit(status.code().unwrap_or(1));
        },
        Err(e) => fail(format!("Lost track of the session server: {}", e)),
    }
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

fn list_services(opts: &CliArgs, registry: &ServiceRegistry, no_formatting: bool) {
    // read-only: listing must never mint a prefix as a side effect
    let prefix = match net::load_prefix(&config_dir_from_opts(opts)) {
        Ok(prefix) => prefix,
        Err(e) => {
            eprintln!("Warning: could not read the loopback IPv6 prefix: {}", e);
            None
        },
    };
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
        address: String,
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
                address: prefix
                    .as_ref()
                    .and_then(|prefix| net::bind_address_for(prefix, service))
                    .map(|address| format!("[{}]:{}", address.ip(), address.port()))
                    .unwrap_or_else(|| "-".to_string()),
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
    let address_w = width("ADDRESS", rows.iter().map(|r| r.address.as_str()).collect());
    let cwd_w = width("CWD", rows.iter().map(|r| r.cwd.as_str()).collect());

    println!(
        "{:<name_w$}  {:<status_w$}  {:<restart_w$}  {:<address_w$}  {:<cwd_w$}  {}",
        "NAME",
        "STATUS",
        "RESTART",
        "ADDRESS",
        "CWD",
        "COMMAND",
        name_w = name_w,
        status_w = status_w,
        restart_w = restart_w,
        address_w = address_w,
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
            "{:<name_w$}  {}  {:<restart_w$}  {:<address_w$}  {:<cwd_w$}  {}",
            row.name,
            status,
            row.restart,
            row.address,
            row.cwd,
            row.command,
            name_w = name_w,
            restart_w = restart_w,
            address_w = address_w,
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

fn net_setup(opts: &CliArgs) {
    let config_dir = config_dir_from_opts(opts);
    let prefix = match net::load_or_create_prefix(&config_dir) {
        Ok(prefix) => prefix,
        Err(e) => fail(format!(
            "Could not read or create the loopback IPv6 prefix in {}: {}",
            config_dir.display(),
            e
        )),
    };
    println!("Prefix:  {}", prefix);
    println!(
        "Stored:  {}",
        config_dir.join(net::NETWORK_FILE_NAME).display()
    );
    println!(
        "Port:    {} (the same for every service)",
        net::default_port()
    );
    if net::is_prefix_routed_locally(&prefix) {
        println!("Routed:  yes - services can bind their own address right now");
        println!();
        println!(
            "    sudo ip -6 route add local {} dev lo    # already done",
            prefix
        );
    } else {
        println!("Routed:  no - run the command below once, as root");
        println!();
        print!("{}", net::setup_instructions(&prefix));
    }
}

fn net_export(
    opts: &CliArgs,
    registry: &ServiceRegistry,
    name: Option<String>,
    format: net::NetExportFormat,
) {
    let config_dir = config_dir_from_opts(opts);
    let prefix = match net::load_prefix(&config_dir) {
        Ok(Some(prefix)) => prefix,
        Ok(None) => {
            fail("No loopback IPv6 prefix has been generated yet; run: zellij service net-setup")
        },
        Err(e) => fail(format!("Could not read the loopback IPv6 prefix: {}", e)),
    };
    let services: Vec<ServiceDefinition> = match name {
        Some(name) => vec![load_or_fail(registry, &name)],
        None => match registry.list() {
            Ok(services) => services,
            Err(e) => fail(format!("Could not read the service registry: {}", e)),
        },
    };
    let mut exported = 0;
    for service in &services {
        let Some(address) = net::bind_address_for(&prefix, service) else {
            continue;
        };
        exported += 1;
        let hostname = net::hostname_for(&service.name);
        match format {
            net::NetExportFormat::Caddy => {
                print!(
                    "{}",
                    net::caddy_snippet(&hostname, address.ip(), address.port())
                )
            },
            net::NetExportFormat::Hosts => print!("{}", net::hosts_line(address.ip(), &hostname)),
        }
    }
    if exported == 0 {
        eprintln!(
            "No service has an address of its own; add one with: zellij service add --bind-ip ..."
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
            bind_ip,
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
            let mut service = ServiceDefinition::new(name.clone(), command, Some(cwd), restart);
            service.bind_ip = bind_ip;
            match registry.save(&service) {
                Ok(path) => println!("Saved service '{}' -> {}", name, path.display()),
                Err(e) => fail(format!("Could not save service '{}': {}", name, e)),
            }
            if bind_ip {
                let config_dir = config_dir_from_opts(&opts);
                match net::load_or_create_prefix(&config_dir) {
                    Ok(prefix) => {
                        if let Some(address) = net::bind_address_for(&prefix, &service) {
                            println!(
                                "  own address: {} (GEZELLIJ_BIND_URL={})",
                                address,
                                net::bind_url(&address)
                            );
                        }
                    },
                    Err(e) => fail(format!(
                        "Could not read or create the loopback IPv6 prefix in {}: {}",
                        config_dir.display(),
                        e
                    )),
                }
            }
            if !no_start {
                start_service(&opts, &service);
            }
        },
        ServiceCommand::Start { name } => {
            let service = load_or_fail(&registry, &name);
            start_service(&opts, &service);
        },
        ServiceCommand::Run { name } => {
            let service = load_or_fail(&registry, &name);
            run_service_in_foreground(&opts, &service);
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
        ServiceCommand::List { no_formatting } => list_services(&opts, &registry, no_formatting),
        ServiceCommand::NetSetup => net_setup(&opts),
        ServiceCommand::NetExport { name, format } => net_export(&opts, &registry, name, format),
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
