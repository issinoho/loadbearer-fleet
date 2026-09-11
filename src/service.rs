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

use crate::report::Report;

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
    // Everything the service writes. `ProtectSystem=strict` makes the rest of
    // the filesystem read-only, so anything missed here fails at runtime — and
    // `archive_dir` was missed for a while, which shows up months later as an
    // archive that is mysteriously empty rather than as a startup failure.
    let mut writable = vec![];
    let mut want = |dir: Option<&Path>| {
        if let Some(dir) = dir
            && !dir.as_os_str().is_empty()
        {
            let dir = dir.display().to_string();
            if !writable.contains(&dir) {
                writable.push(dir);
            }
        }
    };
    want(config.server.index.parent());
    want(config.log.file.as_ref().and_then(|f| f.parent()));
    // The archive is a directory in its own right, not a file in one.
    want(config.server.archive_dir.as_deref());
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

/// Paths a systemd unit cannot reach, however the permissions look.
///
/// The generated unit sets `ProtectHome=yes`, which makes `/home`, `/root` and
/// `/run/user` **invisible** to the service rather than merely unreadable. A
/// path there does not fail with a permission error, it fails as though it were
/// never there — so this is refused at generation time for the same reason a
/// relative `--config` and a missing `[log] file` already are: all three are
/// knowable now and produce a confusing failure later.
fn unreachable_under_protect_home(config: &Config) -> Vec<(&'static str, String)> {
    const HIDDEN: [&str; 3] = ["/home/", "/root/", "/run/user/"];

    let mut found = Vec::new();
    let mut check = |name: &'static str, path: Option<&Path>| {
        if let Some(p) = path {
            let shown = p.display().to_string();
            // `/home` exactly, as well as anything beneath it.
            if HIDDEN
                .iter()
                .any(|h| shown.starts_with(h) || shown == h.trim_end_matches('/'))
            {
                found.push((name, shown));
            }
        }
    };

    check("server.index", Some(&config.server.index));
    check(
        "server.collection_dir",
        config.server.collection_dir.as_deref(),
    );
    check("server.archive_dir", config.server.archive_dir.as_deref());
    check("log.file", config.log.file.as_deref());
    found
}

/// Everything a service needs, checked while the answer is still readable.
///
/// The generated unit is hardened, and hardening means it refuses to start
/// unless what it names already exists: `User=` must be a real account
/// (`217/USER`), and every `ReadWritePaths=` directory must exist
/// (`226/NAMESPACE`). Neither code says anything about the cause, and by the
/// time systemd reports one the operator is reading journal output rather than
/// a sentence. So all of it is checked here instead.
///
/// Writability is judged from ownership and mode rather than by trying a write
/// as the target account, which would need to be that account. That is a
/// heuristic and says so.
pub fn service_preflight(config_path: Option<&Path>, config: &Config, user: &str) -> Report {
    let mut r = Report::default();

    match config_path {
        Some(p) if p.is_absolute() => r.pass("config", p.display().to_string()),
        Some(p) => r.fail(
            "config",
            format!("{} is a relative path", p.display()),
            "A service does not start in this directory. Pass an absolute --config.",
        ),
        None => r.fail(
            "config",
            "not given".to_string(),
            "Every path a service uses has to come from a file it can find. Run `init-config` \
             and pass --config.",
        ),
    }

    match &config.log.file {
        Some(f) => r.pass("log file", f.display().to_string()),
        None => r.fail(
            "log file",
            "not set".to_string(),
            "A service has no console. Without [log] file there is no way to find out why it \
             failed to start.",
        ),
    }

    match std::env::current_exe() {
        Ok(exe) => {
            let shown = exe.display().to_string();
            if under_home(&shown) {
                r.fail(
                    "binary",
                    shown,
                    "ExecStart is whichever binary prints the unit, and the unit sets \
                     ProtectHome=yes — so a binary here is one the service cannot execute. \
                     Install it somewhere like /usr/local/bin and generate the unit from there.",
                );
            } else {
                r.pass("binary", shown);
            }
        }
        Err(e) => r.note("binary", format!("could not determine: {e}")),
    }

    for (field, path) in unreachable_under_protect_home(config) {
        r.fail(
            field,
            path,
            "Under a home directory, which ProtectHome=yes makes invisible to the service — not \
             merely unreadable. Use /var/lib/loadbearer-fleet and /var/log/loadbearer-fleet.",
        );
    }

    check_account(&mut r, user);

    // The directories the unit will name as writable.
    let mut writable: Vec<std::path::PathBuf> = Vec::new();
    if let Some(d) = config.server.index.parent() {
        writable.push(d.to_path_buf());
    }
    if let Some(d) = config.log.file.as_ref().and_then(|f| f.parent()) {
        writable.push(d.to_path_buf());
    }
    if let Some(d) = &config.server.archive_dir {
        writable.push(d.clone());
    }
    writable.sort();
    writable.dedup();
    for dir in &writable {
        check_writable_dir(&mut r, dir, user);
    }

    // Read-only, and a missing one is survivable — the startup scan reports it
    // and carries on — so this is a note rather than a failure.
    match &config.server.collection_dir {
        Some(d) if d.exists() => r.pass("collection folder", d.display().to_string()),
        Some(d) => r.note(
            "collection folder",
            format!("{} does not exist yet", d.display()),
        ),
        None => r.note(
            "collection folder",
            "not set — it will serve whatever the index already holds".to_string(),
        ),
    }

    match std::net::TcpListener::bind(config.server.bind) {
        Ok(l) => {
            drop(l);
            r.pass("bind", config.server.bind.to_string());
        }
        Err(e) => r.fail(
            "bind",
            format!("{} is not available: {e}", config.server.bind),
            "Something already holds that address — often an earlier copy of this service still \
             running.",
        ),
    }

    r.unknown = vec![
        "Whether the account can actually read the collection folder, which for a network share \
         depends on the share's own permissions rather than on anything here."
            .to_string(),
        "Sign-in, if configured: run `check-auth` for that.".to_string(),
    ];
    r
}

fn under_home(path: &str) -> bool {
    ["/home/", "/root/", "/run/user/"]
        .iter()
        .any(|h| path.starts_with(h))
}

/// Does the account the unit names exist, and what are its ids?
#[cfg(unix)]
fn account(user: &str) -> Option<(u32, u32)> {
    let passwd = std::fs::read_to_string("/etc/passwd").ok()?;
    for line in passwd.lines() {
        let mut f = line.split(':');
        if f.next() == Some(user) {
            let _ = f.next();
            let uid = f.next()?.parse().ok()?;
            let gid = f.next()?.parse().ok()?;
            return Some((uid, gid));
        }
    }
    None
}

#[cfg(unix)]
fn check_account(r: &mut Report, user: &str) {
    match account(user) {
        Some((uid, gid)) => r.pass(
            "service account",
            format!("{user} exists (uid {uid}, gid {gid})"),
        ),
        None => r.fail(
            "service account",
            format!("{user} does not exist"),
            "systemd fails the unit with 217/USER. Create it:  sudo useradd --system \
             --no-create-home --shell /usr/sbin/nologin loadbearer-fleet",
        ),
    }
}

/// Does the directory exist at all? Platform-independent, because the answer
/// is and because the unit being previewed is systemd's wherever it was
/// printed — so the code it will die with is worth naming either way.
fn check_dir_exists(r: &mut Report, dir: &Path) -> bool {
    match std::fs::metadata(dir) {
        Err(_) => {
            r.fail(
                "writable dir",
                format!("{} does not exist", dir.display()),
                "ReadWritePaths cannot create a directory, and a missing one fails the unit with \
                 226/NAMESPACE, which says nothing about the cause. Create it and chown it to \
                 the service account.",
            );
            false
        }
        Ok(m) if !m.is_dir() => {
            r.fail(
                "writable dir",
                format!("{} is not a directory", dir.display()),
                "The unit mounts it read-write; it has to be a directory.",
            );
            false
        }
        Ok(_) => true,
    }
}

#[cfg(unix)]
fn check_writable_dir(r: &mut Report, dir: &Path, user: &str) {
    use std::os::unix::fs::MetadataExt;

    if !check_dir_exists(r, dir) {
        return;
    }
    let meta = match std::fs::metadata(dir) {
        Ok(m) => m,
        Err(_) => return,
    };

    let Some((uid, gid)) = account(user) else {
        // No account to compare against; its own check already failed.
        r.note(
            "writable dir",
            format!(
                "{} exists; cannot judge access without the account",
                dir.display()
            ),
        );
        return;
    };

    let mode = meta.mode();
    let writable = (meta.uid() == uid && mode & 0o200 != 0)
        || (meta.gid() == gid && mode & 0o020 != 0)
        || mode & 0o002 != 0;
    if writable {
        r.pass("writable dir", dir.display().to_string());
    } else {
        r.fail(
            "writable dir",
            format!(
                "{} is owned by {}:{} with mode {:o} — {user} cannot write it",
                dir.display(),
                meta.uid(),
                meta.gid(),
                mode & 0o777
            ),
            "sudo chown loadbearer-fleet: <dir>",
        );
    }
}

#[cfg(not(unix))]
fn check_account(r: &mut Report, user: &str) {
    r.note(
        "service account",
        format!("{user} is only meaningful for the systemd unit; not checked here"),
    );
}

#[cfg(not(unix))]
fn check_writable_dir(r: &mut Report, dir: &Path, _user: &str) {
    // Existence is checkable here; which account may write it is not, without
    // reading Windows ACLs for an account that is only meaningful to systemd.
    if check_dir_exists(r, dir) {
        r.pass("writable dir", dir.display().to_string());
    }
}

/// The systemd-specific check, on top of [`preflight`].
///
/// Separate from `preflight` because a Windows service has no equivalent of
/// `ProtectHome`, so this would be refusing something that works there.
pub fn systemd_preflight(config: &Config) -> Result<()> {
    let hidden = unreachable_under_protect_home(config);
    if hidden.is_empty() {
        return Ok(());
    }
    let list = hidden
        .iter()
        .map(|(field, path)| format!("\n  {field} = {path}"))
        .collect::<String>();
    anyhow::bail!(
        "these paths are under a home directory, and the unit this would print sets \
         ProtectHome=yes — which makes /home, /root and /run/user *invisible* to the service, \
         not merely unreadable, so it would fail as though the paths did not exist:{list}\n\n\
         Move them somewhere a system service can reach — /var/lib/loadbearer-fleet for the \
         index and the archive, /var/log/loadbearer-fleet for the log — or pass \
         --user <your-own-account> and edit ProtectHome out of the unit yourself, knowing why \
         it was there."
    )
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
            let index = crate::index::Index::open(&config.server.index)?
                .with_archive(config.server.archive_dir.as_deref())?;
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

    /// The point of the command: every one of these cost real time on a real
    /// host, and systemd reported each as a numbered code that says nothing
    /// about the cause. If a check stops naming its own fix, the command has
    /// stopped being worth running.
    #[test]
    fn preflight_names_what_is_missing_and_how_to_fix_it() {
        let dir = std::env::temp_dir().join(format!("lbf-pf-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        let cfg = dir.join("fleet.toml");

        // A missing directory must fail: ReadWritePaths cannot create one, and
        // the unit dies at 226/NAMESPACE without saying why.
        let mut c = config();
        c.server.index = dir.join("nowhere/fleet-index.db");
        c.log.file = Some(dir.join("nowhere/fleet.log"));
        let r = service_preflight(Some(&cfg), &c, "a-user-that-does-not-exist");
        let shown = r.to_string();
        assert!(!r.ok(), "a missing directory must fail:\n{shown}");
        assert!(shown.contains("226/NAMESPACE"), "{shown}");

        // A relative --config, which a service cannot resolve.
        let r = service_preflight(Some(Path::new("fleet.toml")), &config(), "root");
        assert!(!r.ok());
        assert!(r.to_string().contains("relative"), "{}", r.to_string());

        // None at all.
        assert!(!service_preflight(None, &config(), "root").ok());

        // No log file — the one that makes every later failure undiagnosable.
        let mut c = config();
        c.log.file = None;
        let r = service_preflight(Some(&cfg), &c, "root");
        assert!(!r.ok());
        assert!(r.to_string().contains("no console"), "{}", r.to_string());

        // A home-directory path, which ProtectHome makes invisible.
        let mut c = config();
        c.server.archive_dir = Some(PathBuf::from("/home/someone/archive"));
        let r = service_preflight(Some(&cfg), &c, "root");
        assert!(!r.ok());
        assert!(r.to_string().contains("ProtectHome"), "{}", r.to_string());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The one failure here that is about the machine rather than the
    /// configuration, and worth catching because the usual cause is an earlier
    /// copy of this service still running.
    #[test]
    fn preflight_notices_the_port_is_taken() {
        let held = std::net::TcpListener::bind("127.0.0.1:0").expect("an ephemeral port");
        let addr = held.local_addr().expect("addr");

        let dir = std::env::temp_dir().join(format!("lbf-pf-port-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");

        let mut c = config();
        c.server.bind = addr;
        c.server.index = dir.join("fleet-index.db");
        c.log.file = Some(dir.join("fleet.log"));

        let r = service_preflight(Some(&dir.join("fleet.toml")), &c, "root");
        let shown = r.to_string();
        assert!(!r.ok(), "a held port must fail:\n{shown}");
        assert!(shown.contains("not available"), "{shown}");

        drop(held);
        let _ = std::fs::remove_dir_all(&dir);
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

    /// The archive is a third writable place and was missing from the unit for
    /// several releases. It fails in the worst available way: everything starts,
    /// the dashboard works, and only the archive writes are refused — so the
    /// symptom is an empty archive noticed months later, not a failure to boot.
    #[test]
    fn the_unit_grants_write_access_to_the_archive_too() {
        let mut c = config();
        c.server.archive_dir = Some(PathBuf::from("/srv/loadbearer-archive"));
        let unit = systemd_unit(
            Path::new("/usr/local/bin/loadbearer-fleet"),
            Path::new("/etc/loadbearer-fleet/fleet.toml"),
            &c,
            "loadbearer",
        );
        let line = unit
            .lines()
            .find(|l| l.starts_with("ReadWritePaths="))
            .expect("a ReadWritePaths line");
        assert!(
            line.contains("/srv/loadbearer-archive"),
            "archive_dir is writable at runtime and must be named: {line}"
        );
    }

    /// `ProtectHome=yes` makes a home directory *invisible*, so a path there
    /// fails as though it were never created. Refusing at generation time is
    /// the same bargain as refusing a relative `--config`: both are knowable
    /// now and both produce a baffling failure later.
    #[test]
    fn a_config_under_a_home_directory_is_refused_for_systemd() {
        for (field, path) in [
            ("server.index", "/home/iain/loadbearer/fleet-index.db"),
            ("log.file", "/home/iain/loadbearer/fleet.log"),
            ("server.archive_dir", "/root/archive"),
        ] {
            let mut c = config();
            match field {
                "server.index" => c.server.index = PathBuf::from(path),
                "log.file" => c.log.file = Some(PathBuf::from(path)),
                _ => c.server.archive_dir = Some(PathBuf::from(path)),
            }
            let err = format!(
                "{:#}",
                systemd_preflight(&c).expect_err("a home-directory path must be refused")
            );
            assert!(err.contains("ProtectHome"), "{err}");
            assert!(err.contains(path), "the message must name the path: {err}");
            assert!(err.contains(field), "and the setting: {err}");
        }

        // And the layout the docs recommend is accepted.
        let mut ok = config();
        ok.server.archive_dir = Some(PathBuf::from("/var/lib/loadbearer-fleet/archive"));
        assert!(systemd_preflight(&ok).is_ok());
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
