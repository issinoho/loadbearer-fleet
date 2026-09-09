//! Running as a service.
//!
//! A dashboard that has to be started from a console window is not something
//! anyone deploys, so this is what makes the difference between a command and a
//! piece of infrastructure: it starts at boot, it restarts after a crash, it
//! stops when the service controller says so rather than being killed after a
//! timeout, and it writes its log somewhere a person can find afterwards.
//!
//! Two traps this deliberately avoids.
//!
//! **Relative paths.** Windows starts a service in `%SystemRoot%\System32` and
//! systemd starts one in `/`, so a relative `index = "fleet-index.db"` would
//! put the database somewhere nobody expects — and somewhere *different*
//! depending on how the process was started. Paths in the config file are
//! resolved against the config file (see `Config::load`), and the installer
//! refuses to register a service without one.
//!
//! **Nowhere to log.** A service has no stdout. `[log] file` is therefore the
//! only way to find out why it did not start, and the installer says so if it
//! is missing.

use std::path::Path;

use anyhow::Result;

use crate::config::Config;

/// The service name, and the name the unit file gets.
pub const SERVICE_NAME: &str = "loadbearer-fleet";
pub const DISPLAY_NAME: &str = "loadbearer fleet dashboard";

/// A systemd unit, printed rather than installed: on Linux the file belongs to
/// the packaging, and an operator reviewing it before it lands in
/// `/etc/systemd/system` is the normal way round.
///
/// The hardening directives are not decoration. This process reads a share and
/// writes one database; it never needs a new privilege, an executable mapping,
/// or a raw socket, so saying so limits what a bug in it can reach.
pub fn systemd_unit(exe: &Path, config_path: &Path, config: &Config, user: &str) -> String {
    let mut writable = vec![];
    if let Some(dir) = config.server.index.parent()
        && !dir.as_os_str().is_empty()
    {
        writable.push(dir.display().to_string());
    }
    if let Some(dir) = config.log.file.as_ref().and_then(|f| f.parent())
        && !dir.as_os_str().is_empty()
    {
        let dir = dir.display().to_string();
        if !writable.contains(&dir) {
            writable.push(dir);
        }
    }
    let read_write = if writable.is_empty() {
        String::new()
    } else {
        format!("ReadWritePaths={}\n", writable.join(" "))
    };

    format!(
        "# {SERVICE_NAME}.service — install to /etc/systemd/system/{SERVICE_NAME}.service\n\
         #\n\
         # systemctl daemon-reload && systemctl enable --now {SERVICE_NAME}\n\
         [Unit]\n\
         Description={DISPLAY_NAME}\n\
         # The collection folder is usually a network share, so wait for real\n\
         # networking rather than for an interface to merely exist.\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         Type=exec\n\
         ExecStart={} serve --config {}\n\
         User={user}\n\
         Group={user}\n\
         Restart=on-failure\n\
         RestartSec=5s\n\
         # It reads a share and writes one database. Nothing else.\n\
         NoNewPrivileges=yes\n\
         PrivateTmp=yes\n\
         ProtectSystem=strict\n\
         ProtectHome=yes\n\
         {read_write}\
         CapabilityBoundingSet=\n\
         AmbientCapabilities=\n\
         RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX\n\
         RestrictNamespaces=yes\n\
         RestrictSUIDSGID=yes\n\
         LockPersonality=yes\n\
         MemoryDenyWriteExecute=yes\n\
         ProtectKernelTunables=yes\n\
         ProtectKernelModules=yes\n\
         ProtectControlGroups=yes\n\
         SystemCallArchitectures=native\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n",
        exe.display(),
        config_path.display(),
    )
}

/// Checks that apply wherever the service is being registered.
pub fn preflight<'a>(config_path: Option<&'a Path>, config: &Config) -> Result<&'a Path> {
    let Some(path) = config_path else {
        anyhow::bail!(
            "installing a service needs --config: a service starts in a directory nobody chose, \
             so every path it uses has to come from a file it can find. Run `init-config` first."
        );
    };
    if !path.is_absolute() {
        anyhow::bail!(
            "--config must be an absolute path when installing a service ({} is relative, and a \
             service does not start in this directory)",
            path.display()
        );
    }
    if config.log.file.is_none() {
        anyhow::bail!(
            "set [log] file in {} before installing: a service has no console, so without a log \
             file there is no way to find out why it failed to start",
            path.display()
        );
    }
    Ok(path)
}

#[cfg(windows)]
pub use windows_impl::{install, run, uninstall};

#[cfg(windows)]
mod windows_impl {
    use std::ffi::OsString;
    use std::path::Path;
    use std::sync::OnceLock;
    use std::time::Duration;

    use anyhow::{Context, Result};
    use windows_service::service::{
        ServiceAccess, ServiceAction, ServiceActionType, ServiceControl, ServiceControlAccept,
        ServiceErrorControl, ServiceExitCode, ServiceFailureActions, ServiceFailureResetPeriod,
        ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
    };
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
    use windows_service::{define_windows_service, service_control_handler, service_dispatcher};

    use super::{DISPLAY_NAME, SERVICE_NAME};

    /// Only the Windows service database has a field for this; a systemd unit
    /// carries the short description alone.
    const DESCRIPTION: &str =
        "Indexes collected loadbearer results and serves the fleet dashboard.";
    use crate::config::Config;

    const SERVICE_TYPE: ServiceType = ServiceType::OWN_PROCESS;

    /// The service entry point runs on a thread the controller calls, with no
    /// access to anything `main` parsed — so what it needs is left here first.
    static SETTINGS: OnceLock<(Config, bool)> = OnceLock::new();

    /// Hand control to the service controller. Returns only once the service
    /// has stopped.
    pub fn run(config: Config, allow_remote: bool) -> Result<()> {
        let _ = SETTINGS.set((config, allow_remote));
        service_dispatcher::start(SERVICE_NAME, ffi_service_main).context(
            "starting the service dispatcher — `service run` is what the service controller \
             calls, not something to run by hand. Use `serve` for that.",
        )?;
        Ok(())
    }

    define_windows_service!(ffi_service_main, service_main);

    fn service_main(_arguments: Vec<OsString>) {
        if let Err(e) = serve_as_service() {
            // The controller has already been told the service stopped; this is
            // for the log file, which is the only place anyone can read it.
            tracing::error!(
                error = format!("{e:#}"),
                "the service stopped with an error"
            );
        }
    }

    fn serve_as_service() -> Result<()> {
        let (config, allow_remote) = SETTINGS.get().context("service settings were not set")?;

        // The controller's stop request arrives on its own thread, so it is
        // forwarded into the async world through a watch channel that
        // `web::serve` selects on alongside the signals.
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);

        let handler = move |control| match control {
            // "Are you still there?" — answering is mandatory.
            ServiceControl::Interrogate => {
                service_control_handler::ServiceControlHandlerResult::NoError
            }
            ServiceControl::Stop | ServiceControl::Shutdown => {
                let _ = stop_tx.send(true);
                service_control_handler::ServiceControlHandlerResult::NoError
            }
            _ => service_control_handler::ServiceControlHandlerResult::NotImplemented,
        };
        let status_handle = service_control_handler::register(SERVICE_NAME, handler)
            .context("registering the service control handler")?;

        let report = |state: ServiceState, accept: ServiceControlAccept, wait: Duration| {
            status_handle.set_service_status(ServiceStatus {
                service_type: SERVICE_TYPE,
                current_state: state,
                controls_accepted: accept,
                exit_code: ServiceExitCode::Win32(0),
                checkpoint: 0,
                wait_hint: wait,
                process_id: None,
            })
        };

        // StartPending first, with a generous hint: the initial scan of a large
        // share can take longer than the controller's default patience, and
        // this is how a service says "still working" instead of being declared
        // hung and killed.
        report(
            ServiceState::StartPending,
            ServiceControlAccept::empty(),
            Duration::from_secs(120),
        )?;

        // All of this is blocking and none of it needs the runtime, so it runs
        // before one is built — which also means Running is not reported until
        // the folder has actually been read once.
        let result = (|| -> Result<()> {
            let index = crate::index::Index::open(&config.server.index)?;
            let state =
                crate::web::AppState::new(index, config, crate::analytics::Thresholds::default())?;
            if config.server.collection_dir.is_some() {
                crate::web::report_startup_scan(&state);
            }
            report(
                ServiceState::Running,
                ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
                Duration::default(),
            )?;

            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .context("building the async runtime")?
                .block_on(crate::web::serve(
                    state,
                    config,
                    *allow_remote,
                    Some(stop_rx),
                ))
        })();

        // Reported whether the run succeeded or not: leaving the controller
        // believing this is still starting means `sc stop` hangs and the next
        // start refuses.
        report(
            ServiceState::Stopped,
            ServiceControlAccept::empty(),
            Duration::default(),
        )?;
        result
    }

    pub fn install(
        exe: &Path,
        config_path: &Path,
        allow_remote: bool,
        account: Option<&str>,
        password: Option<&str>,
    ) -> Result<()> {
        let manager = ServiceManager::local_computer(
            None::<&str>,
            ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
        )
        .context("connecting to the service controller — this needs an elevated command prompt")?;

        let mut launch_arguments = vec![
            OsString::from("--config"),
            OsString::from(config_path),
            OsString::from("service"),
            OsString::from("run"),
        ];
        if allow_remote {
            launch_arguments.push(OsString::from("--allow-remote"));
        }

        let info = ServiceInfo {
            name: OsString::from(SERVICE_NAME),
            display_name: OsString::from(DISPLAY_NAME),
            service_type: SERVICE_TYPE,
            start_type: ServiceStartType::AutoStart,
            error_control: ServiceErrorControl::Normal,
            executable_path: exe.to_path_buf(),
            launch_arguments,
            dependencies: vec![],
            account_name: account.map(OsString::from),
            account_password: password.map(OsString::from),
        };

        let access = ServiceAccess::QUERY_CONFIG | ServiceAccess::CHANGE_CONFIG;
        let service = manager
            .create_service(&info, access)
            .context("creating the service")?;
        service
            .set_description(DESCRIPTION)
            .context("setting the description")?;
        // A service that does not come back after a crash is not a service.
        service
            .update_failure_actions(ServiceFailureActions {
                reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(86400)),
                reboot_msg: None,
                command: None,
                actions: Some(vec![
                    ServiceAction {
                        action_type: ServiceActionType::Restart,
                        delay: Duration::from_secs(5),
                    },
                    ServiceAction {
                        action_type: ServiceActionType::Restart,
                        delay: Duration::from_secs(30),
                    },
                    ServiceAction {
                        action_type: ServiceActionType::Restart,
                        delay: Duration::from_secs(300),
                    },
                ]),
            })
            .context("setting the restart policy")?;

        println!("Installed {SERVICE_NAME}.");
        println!("  binary: {}", exe.display());
        println!("  config: {}", config_path.display());
        match account {
            Some(a) => println!("  account: {a}"),
            None => println!(
                "  account: LocalSystem — which reaches a network share as the *machine* \
                 account. If your collection folder is a domain share, grant the computer \
                 object read access, or reinstall with --account DOMAIN\\\\user."
            ),
        }
        println!("\nStart it with:  sc start {SERVICE_NAME}");
        Ok(())
    }

    pub fn uninstall() -> Result<()> {
        let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
            .context(
            "connecting to the service controller — this needs an elevated command prompt",
        )?;
        let service = manager
            .open_service(
                SERVICE_NAME,
                ServiceAccess::QUERY_STATUS | ServiceAccess::DELETE,
            )
            .with_context(|| format!("opening {SERVICE_NAME}"))?;
        service.delete().context("deleting the service")?;
        println!(
            "Removed {SERVICE_NAME}. If it is still running it will disappear when it stops; \
             the index and the config file are left alone."
        );
        Ok(())
    }
}

#[cfg(not(windows))]
pub fn run(_config: Config, _allow_remote: bool) -> Result<()> {
    anyhow::bail!(
        "`service run` is the Windows service-controller entry point. On Linux, run \
         `service unit` and install the systemd unit it prints."
    )
}

#[cfg(not(windows))]
pub fn install(
    _exe: &Path,
    _config_path: &Path,
    _allow_remote: bool,
    _account: Option<&str>,
    _password: Option<&str>,
) -> Result<()> {
    anyhow::bail!(
        "there is no service database to install into here. Run `service unit` and put the \
         systemd unit it prints in /etc/systemd/system."
    )
}

#[cfg(not(windows))]
pub fn uninstall() -> Result<()> {
    anyhow::bail!("nothing to uninstall: use `systemctl disable --now loadbearer-fleet`.")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// "Absolute" is platform-specific, and `preflight` rightly asks the
    /// platform. The systemd tests below keep their Unix paths, because a unit
    /// file is only ever read on Linux.
    fn absolute(unixish: &str) -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(format!("C:{}", unixish.replace('/', "\\")))
        } else {
            PathBuf::from(unixish)
        }
    }

    fn config() -> Config {
        let mut c = Config::default();
        c.server.index = PathBuf::from("/var/lib/loadbearer-fleet/fleet-index.db");
        c.log.file = Some(PathBuf::from("/var/log/loadbearer-fleet/fleet.log"));
        c
    }

    #[test]
    fn the_unit_starts_the_binary_with_its_config() {
        let unit = systemd_unit(
            Path::new("/usr/local/bin/loadbearer-fleet"),
            Path::new("/etc/loadbearer-fleet/fleet.toml"),
            &config(),
            "loadbearer",
        );
        assert!(
            unit.contains(
                "ExecStart=/usr/local/bin/loadbearer-fleet serve \
                 --config /etc/loadbearer-fleet/fleet.toml"
            ),
            "{unit}"
        );
        assert!(
            unit.contains("[Unit]") && unit.contains("[Service]") && unit.contains("[Install]")
        );
        assert!(unit.contains("User=loadbearer"));
        assert!(unit.contains("Restart=on-failure"));
        // The share is usually on the network, so an interface merely existing
        // is not enough.
        assert!(unit.contains("After=network-online.target"));
    }

    /// `ProtectSystem=strict` makes the whole filesystem read-only, so the two
    /// places this process must write have to be named or it cannot start.
    #[test]
    fn the_unit_grants_write_access_to_exactly_what_it_writes() {
        let unit = systemd_unit(
            Path::new("/usr/local/bin/loadbearer-fleet"),
            Path::new("/etc/loadbearer-fleet/fleet.toml"),
            &config(),
            "loadbearer",
        );
        assert!(unit.contains("ProtectSystem=strict"));
        let line = unit
            .lines()
            .find(|l| l.starts_with("ReadWritePaths="))
            .expect("a ReadWritePaths line");
        assert!(line.contains("/var/lib/loadbearer-fleet"), "{line}");
        assert!(line.contains("/var/log/loadbearer-fleet"), "{line}");
    }

    #[test]
    fn the_unit_drops_every_privilege_it_does_not_need() {
        let unit = systemd_unit(
            Path::new("/usr/local/bin/loadbearer-fleet"),
            Path::new("/etc/loadbearer-fleet/fleet.toml"),
            &config(),
            "loadbearer",
        );
        for directive in [
            "NoNewPrivileges=yes",
            "CapabilityBoundingSet=",
            "MemoryDenyWriteExecute=yes",
            "ProtectHome=yes",
            "RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX",
        ] {
            assert!(unit.contains(directive), "missing {directive}:\n{unit}");
        }
    }

    #[test]
    fn installing_without_a_config_is_refused_with_the_reason() {
        let err = preflight(None, &config())
            .expect_err("must refuse")
            .to_string();
        assert!(err.contains("--config"), "{err}");
    }

    #[test]
    fn a_relative_config_path_is_refused() {
        let err = preflight(Some(Path::new("fleet.toml")), &config())
            .expect_err("must refuse")
            .to_string();
        assert!(err.contains("absolute"), "{err}");
    }

    /// A service with no log file cannot be diagnosed at all: there is no
    /// console for it to have printed to.
    #[test]
    fn installing_without_a_log_file_is_refused() {
        let mut c = config();
        c.log.file = None;
        let path = absolute("/etc/loadbearer-fleet/fleet.toml");
        let err = preflight(Some(&path), &c)
            .expect_err("must refuse")
            .to_string();
        assert!(err.contains("[log] file"), "{err}");
    }

    #[test]
    fn a_usable_config_passes_preflight() {
        let path = absolute("/etc/loadbearer-fleet/fleet.toml");
        assert_eq!(preflight(Some(&path), &config()).unwrap(), path.as_path());
    }
}
