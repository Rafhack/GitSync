//! `gitsync service`: runs `gitsync watch` for one repo in the background,
//! started when the user logs in and restarted if it stops with an error
//! (DESIGN.md §9). One service per repo, using what each OS provides:
//!
//! - Linux with systemd: a user unit; the log goes to the journal.
//! - Linux with OpenRC (0.60 or newer): a user service, kept running by
//!   supervise-daemon; the log goes to `~/.local/state/gitsync/logs/`.
//! - Linux with runit: a service in the user's own runsvdir (`~/service`,
//!   or `$SVDIR`); the log goes to `~/.local/state/gitsync/logs/`.
//! - macOS: a launchd agent; the log goes to `~/Library/Logs/gitsync/`.
//! - Windows: a Task Scheduler task started at logon, run through
//!   `conhost --headless` so no console window appears; the log goes to
//!   `%LOCALAPPDATA%\gitsync\logs\`.

use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tracing::warn;

use crate::git::Git;
use crate::lock::ProcessLock;

/// What runs the service: the init system or service manager.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Manager {
    Systemd,
    OpenRc,
    Runit,
    Launchd,
    TaskScheduler,
}

impl Manager {
    /// The one this machine runs.
    pub fn detect() -> Result<Self> {
        if cfg!(windows) {
            return Ok(Self::TaskScheduler);
        }
        if cfg!(target_os = "macos") {
            return Ok(Self::Launchd);
        }
        // Each one marks that it booted the system (for systemd this is the
        // check of sd_booted()); the first process's name is the fallback.
        let init = fs::read_to_string("/proc/1/comm").unwrap_or_default();
        if Path::new("/run/systemd/system").is_dir() {
            Ok(Self::Systemd)
        } else if Path::new("/run/openrc").is_dir() || init.trim() == "openrc-init" {
            Ok(Self::OpenRc)
        } else if Path::new("/run/runit").is_dir() || init.trim() == "runit" {
            Ok(Self::Runit)
        } else {
            bail!(
                "gitsync service works with systemd, OpenRC and runit, and none of them \
                 runs this system; run `gitsync watch` from your own startup script instead"
            )
        }
    }

    /// When the service starts by itself.
    pub fn starts_at(self) -> &'static str {
        match self {
            // runit runs the user's services from boot (see `note`).
            Self::Runit => "boot",
            _ => "login",
        }
    }
}

/// The background service for one repo, and what it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Service {
    pub manager: Manager,
    /// Unit / agent / task name: `gitsync-<folder>-<hash of the path>`.
    pub name: String,
    /// The repo's top folder, as git prints it.
    pub repo: String,
    /// The gitsync program the service runs.
    pub exe: PathBuf,
    /// Where `watch` writes its log (`None`: the system journal).
    pub log_file: Option<PathBuf>,
    /// `PATH` at install time, so the service finds the same `git` and `ssh`
    /// as the shell it was installed from (Linux and macOS).
    pub path_env: Option<String>,
}

/// Whether the service is set up and running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct State {
    pub installed: bool,
    pub running: bool,
}

impl Service {
    /// The service for the repo `git` points at, running this program.
    pub fn for_repo(git: &Git) -> Result<Self> {
        let repo = git.run(&["rev-parse", "--show-toplevel"])?;
        let name = service_name(&repo);
        let exe = env::current_exe().context("finding the gitsync program")?;
        let manager = Manager::detect()?;
        let log_dir = match manager {
            Manager::TaskScheduler => {
                let base = env::var_os("LOCALAPPDATA").context("LOCALAPPDATA is not set")?;
                Some(Path::new(&base).join(r"gitsync\logs"))
            }
            Manager::Launchd => Some(home()?.join("Library/Logs/gitsync")),
            Manager::OpenRc | Manager::Runit => {
                Some(xdg_dir("XDG_STATE_HOME", ".local/state")?.join("gitsync/logs"))
            }
            Manager::Systemd => None,
        };
        Ok(Self {
            manager,
            log_file: log_dir.map(|dir| dir.join(format!("{name}.log"))),
            name,
            repo,
            exe,
            path_env: env::var("PATH").ok(),
        })
    }

    /// Arguments after the program name.
    pub fn watch_args(&self) -> Vec<String> {
        let mut args = vec!["-C".to_string(), self.repo.clone(), "watch".to_string()];
        if let Some(log) = &self.log_file {
            args.push("--log-file".to_string());
            args.push(log.display().to_string());
        }
        args
    }

    /// Arguments for `gitsync service run` (what the Windows task runs; it
    /// keeps `watch` running, see [`supervise`]).
    pub fn run_args(&self) -> Vec<String> {
        let mut args = self.watch_args();
        args.splice(2..3, ["service".to_string(), "run".to_string()]);
        args
    }

    /// What the service runs, after the program name.
    pub fn program_args(&self) -> Vec<String> {
        match self.manager {
            Manager::TaskScheduler => self.run_args(),
            _ => self.watch_args(),
        }
    }

    /// How to read the service's log.
    pub fn log_hint(&self) -> String {
        match &self.log_file {
            Some(path) => path.display().to_string(),
            // (`--user-unit` also works where the journal keeps no per-user files.)
            None => format!("journalctl --user-unit {} -f", self.name),
        }
    }

    /// The systemd user unit (Linux).
    pub fn systemd_unit(&self) -> String {
        let mut exec = systemd_quote(&self.exe.display().to_string());
        for arg in self.watch_args() {
            exec.push(' ');
            exec.push_str(&systemd_quote(&arg));
        }
        let mut unit = format!(
            "# Written by `gitsync service install`; remove with `gitsync service uninstall`.\n\
             [Unit]\n\
             Description=gitsync: keep {} in sync\n\
             \n\
             [Service]\n\
             ExecStart={exec}\n",
            self.repo.replace('%', "%%")
        );
        if let Some(path) = &self.path_env {
            unit.push_str(&format!(
                "Environment={}\n",
                systemd_quote(&format!("PATH={path}")).replace("$$", "$")
            ));
        }
        unit.push_str(
            "Restart=on-failure\n\
             RestartSec=10\n\
             # Stopped with SIGTERM (or Ctrl+C when run by hand): a normal end.\n\
             SuccessExitStatus=130 143\n\
             \n\
             [Install]\n\
             WantedBy=default.target\n",
        );
        unit
    }

    /// The launchd agent (macOS).
    pub fn launchd_plist(&self) -> String {
        let mut args = format!(
            "    <string>{}</string>\n",
            xml(&self.exe.display().to_string())
        );
        for arg in self.watch_args() {
            args.push_str(&format!("    <string>{}</string>\n", xml(&arg)));
        }
        let env = match &self.path_env {
            Some(path) => format!(
                "  <key>EnvironmentVariables</key>\n  <dict>\n    <key>PATH</key>\n    <string>{}</string>\n  </dict>\n",
                xml(path)
            ),
            None => String::new(),
        };
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<!-- Written by `gitsync service install`; remove with `gitsync service uninstall`. -->
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{label}</string>
  <key>ProgramArguments</key>
  <array>
{args}  </array>
{env}  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key>
    <false/>
  </dict>
  <key>ThrottleInterval</key>
  <integer>10</integer>
  <key>ProcessType</key>
  <string>Background</string>
</dict>
</plist>
"#,
            label = xml(&self.name),
        )
    }

    /// The OpenRC user service script.
    pub fn openrc_script(&self) -> String {
        // OpenRC splits `command` and `command_args` again with `eval`, so
        // each argument is quoted for that, then the whole for the variable.
        let args: Vec<String> = self.watch_args().iter().map(|a| sh_quote(a)).collect();
        let mut script = format!(
            "#!/sbin/openrc-run\n\
             # Written by `gitsync service install`; remove with `gitsync service uninstall`.\n\
             description={}\n\
             supervisor=supervise-daemon\n\
             command={}\n\
             command_args={}\n",
            sh_quote(&format!("gitsync: keep {} in sync", self.repo)),
            sh_quote(&sh_quote(&self.exe.display().to_string())),
            sh_quote(&args.join(" ")),
        );
        if let Some(path) = &self.path_env {
            // For gitsync only: a PATH set in the script would also be the one
            // OpenRC runs its own commands with.
            let env = format!("--env {}", sh_quote(&format!("PATH={path}")));
            script.push_str(&format!("supervise_daemon_args={}\n", sh_quote(&env)));
        }
        script.push_str(
            "# Start it again 10 s after it stops, however often that happens.\n\
             respawn_delay=10\n\
             respawn_max=0\n",
        );
        script
    }

    /// The runit `run` script.
    pub fn runit_run(&self) -> String {
        let mut script = String::from(
            "#!/bin/sh\n\
             # Written by `gitsync service install`; remove with `gitsync service uninstall`.\n",
        );
        if let Some(path) = &self.path_env {
            script.push_str(&format!("export PATH={}\n", sh_quote(path)));
        }
        script.push_str(&format!(
            "exec {}",
            sh_quote(&self.exe.display().to_string())
        ));
        for arg in self.watch_args() {
            script.push(' ');
            script.push_str(&sh_quote(&arg));
        }
        script.push_str(" 2>&1\n");
        script
    }

    /// The runit `finish` script: runit starts the service again a second
    /// after it stops; after a failure, wait 10 s as on the other systems.
    pub fn runit_finish() -> &'static str {
        "#!/bin/sh\n\
         # Written by `gitsync service install`; remove with `gitsync service uninstall`.\n\
         # $1: how gitsync watch ended: its exit code, or -1 if signal $2 ended\n\
         # it. Stopped with SIGTERM (by `sv`) or Ctrl+C is a normal end.\n\
         case \"$1:$2\" in\n\
         \x20   0:* | 130:* | -1:15 | -1:2) ;;\n\
         \x20   *) sleep 10 ;;\n\
         esac\n"
    }

    /// The Task Scheduler task (Windows), for the user `user` (`DOMAIN\name`).
    pub fn windows_task_xml(&self, user: &str) -> String {
        let mut args = format!(
            "--headless {}",
            windows_quote(&self.exe.display().to_string())
        );
        for arg in self.run_args() {
            args.push(' ');
            args.push_str(&windows_quote(&arg));
        }
        format!(
            r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>gitsync: keep {repo} in sync. Written by `gitsync service install`; remove with `gitsync service uninstall`.</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{user}</UserId>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>7</Priority>
    <RestartOnFailure>
      <Interval>PT1M</Interval>
      <Count>999</Count>
    </RestartOnFailure>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{conhost}</Command>
      <Arguments>{args}</Arguments>
    </Exec>
  </Actions>
</Task>
"#,
            repo = xml(&self.repo),
            user = xml(user),
            conhost = xml(&conhost()),
            args = xml(&args),
        )
    }

    /// Sets up the service and starts it now.
    pub fn install(&self) -> Result<()> {
        match self.manager {
            Manager::TaskScheduler => {
                // An older version may be running: replace it, as systemd does.
                if self.state()?.installed {
                    self.stop()?;
                }
                let xml = self.windows_task_xml(&windows_user()?);
                // schtasks reads task XML as UTF-16 (with a byte order mark).
                let mut bytes = vec![0xFF, 0xFE];
                bytes.extend(xml.encode_utf16().flat_map(u16::to_le_bytes));
                let file = env::temp_dir().join(format!("{}.xml", self.name));
                fs::write(&file, bytes).with_context(|| format!("writing {}", file.display()))?;
                let created = run(
                    "schtasks",
                    &[
                        "/Create",
                        "/TN",
                        &self.name,
                        "/XML",
                        &path_str(&file)?,
                        "/F",
                    ],
                );
                let _ = fs::remove_file(&file);
                created?;
                run("schtasks", &["/Run", "/TN", &self.name])?;
            }
            Manager::Launchd => {
                let plist = self.launchd_plist_path()?;
                write_file(&plist, &self.launchd_plist())?;
                let _ = run("launchctl", &["bootout", &self.launchd_target()?]);
                run(
                    "launchctl",
                    &["bootstrap", &launchd_domain()?, &path_str(&plist)?],
                )?;
            }
            Manager::Systemd => {
                write_file(&self.systemd_unit_path()?, &self.systemd_unit())?;
                run("systemctl", &["--user", "daemon-reload"])?;
                run("systemctl", &["--user", "enable", &self.unit()])?;
                // Starts it, or restarts it to pick up a new unit file, program
                // or config if it was running already.
                run("systemctl", &["--user", "restart", &self.unit()])?;
            }
            Manager::OpenRc => {
                let help = output("rc-service", &["--help"])?;
                if !String::from_utf8_lossy(&help.stdout).contains("--user") {
                    bail!(
                        "this OpenRC is older than 0.60, which added the user services \
                         gitsync service needs; run `gitsync watch` from your own startup \
                         script instead"
                    );
                }
                let script = self.openrc_script_path()?;
                write_file(&script, &self.openrc_script())?;
                make_executable(&script)?;
                // What `rc-update --user add` does, which also needs a user
                // session; OpenRC makes the runlevel folder at the first one.
                let link = self.openrc_runlevel_link()?;
                let runlevel = link.parent().unwrap();
                fs::create_dir_all(runlevel)
                    .with_context(|| format!("creating {}", runlevel.display()))?;
                if !exists(&link) {
                    symlink(&script, &link)?;
                }
                if openrc_session_running() {
                    // Starts it, or restarts it with the new script or program.
                    rc_service(&[&self.name, "restart"])?;
                }
            }
            Manager::Runit => {
                let running = self.state()?.running;
                let dir = self.runit_dir()?;
                for (file, text) in [
                    ("run", self.runit_run()),
                    ("finish", Self::runit_finish().into()),
                ] {
                    write_file(&dir.join(file), &text)?;
                    make_executable(&dir.join(file))?;
                }
                let svdir = runit_svdir()?;
                fs::create_dir_all(&svdir)
                    .with_context(|| format!("creating {}", svdir.display()))?;
                let link = self.runit_link()?;
                if !exists(&link) {
                    symlink(&dir, &link)?;
                }
                if runsvdir_running(&svdir) {
                    // runsvdir looks for new services every 5 s.
                    wait_for(&dir.join("supervise/ok"), Duration::from_secs(10))?;
                    let action = if running { "restart" } else { "up" };
                    run("sv", &[action, &path_str(&link)?])?;
                }
            }
        }
        Ok(())
    }

    /// Stops the service and removes it.
    pub fn uninstall(&self) -> Result<()> {
        match self.manager {
            Manager::TaskScheduler => {
                let _ = self.stop();
                run("schtasks", &["/Delete", "/TN", &self.name, "/F"])?;
            }
            Manager::Launchd => {
                let _ = run("launchctl", &["bootout", &self.launchd_target()?]);
                remove(&self.launchd_plist_path()?)?;
            }
            Manager::Systemd => {
                run("systemctl", &["--user", "disable", "--now", &self.unit()])?;
                remove(&self.systemd_unit_path()?)?;
                run("systemctl", &["--user", "daemon-reload"])?;
            }
            Manager::OpenRc => {
                if openrc_session_running() {
                    let _ = rc_service(&[&self.name, "stop"]);
                }
                let link = self.openrc_runlevel_link()?;
                if exists(&link) {
                    remove(&link)?;
                }
                remove(&self.openrc_script_path()?)?;
            }
            Manager::Runit => {
                let link = self.runit_link()?;
                let _ = run("sv", &["stop", &path_str(&link)?]);
                remove(&link)?;
                // runsvdir then ends the service's runsv within 5 s; it
                // writes to the folder until then.
                let dir = self.runit_dir()?;
                let deadline = std::time::Instant::now() + Duration::from_secs(10);
                while std::time::Instant::now() < deadline
                    && output("sv", &["status", &path_str(&dir)?])
                        .is_ok_and(|out| out.status.success())
                {
                    std::thread::sleep(Duration::from_millis(250));
                }
                fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))?;
            }
        }
        Ok(())
    }

    /// Starts the installed service (until the next stop; it also starts at
    /// every login).
    pub fn start(&self) -> Result<()> {
        match self.manager {
            Manager::TaskScheduler => {
                run("schtasks", &["/Run", "/TN", &self.name])?;
            }
            Manager::Launchd => {
                run(
                    "launchctl",
                    &[
                        "bootstrap",
                        &launchd_domain()?,
                        &path_str(&self.launchd_plist_path()?)?,
                    ],
                )?;
            }
            Manager::Systemd => {
                run("systemctl", &["--user", "start", &self.unit()])?;
            }
            Manager::OpenRc => {
                self.check_started_by_init()?;
                rc_service(&[&self.name, "start"])?;
            }
            Manager::Runit => {
                self.check_started_by_init()?;
                run("sv", &["up", &path_str(&self.runit_link()?)?])?;
            }
        }
        Ok(())
    }

    /// Stops the service until `start` or the next login.
    pub fn stop(&self) -> Result<()> {
        match self.manager {
            Manager::TaskScheduler => {
                // Ending the task only ends conhost, not gitsync under it: end
                // the service loop with its watch (/T), then any watch left.
                let _ = run("schtasks", &["/End", "/TN", &self.name]);
                let git_dir = self.git_dir()?;
                for lock in ["service", "watch"] {
                    if let Some(pid) = ProcessLock::holder(&git_dir, lock)? {
                        run("taskkill", &["/PID", &pid.to_string(), "/T", "/F"])?;
                    }
                }
            }
            Manager::Launchd => {
                run("launchctl", &["bootout", &self.launchd_target()?])?;
            }
            Manager::Systemd => {
                run("systemctl", &["--user", "stop", &self.unit()])?;
            }
            Manager::OpenRc => {
                self.check_started_by_init()?;
                rc_service(&[&self.name, "stop"])?;
            }
            Manager::Runit => {
                self.check_started_by_init()?;
                // Waits until it has stopped.
                run("sv", &["stop", &path_str(&self.runit_link()?)?])?;
            }
        }
        Ok(())
    }

    pub fn state(&self) -> Result<State> {
        Ok(match self.manager {
            Manager::TaskScheduler => {
                // schtasks prints the status in the system's language; this
                // state name is always English.
                let query = format!(
                    "(Get-ScheduledTask -TaskName '{}' -ErrorAction Stop).State",
                    self.name
                );
                let out = output(
                    "powershell",
                    &["-NoProfile", "-NonInteractive", "-Command", &query],
                )?;
                State {
                    installed: out.status.success(),
                    running: ProcessLock::is_held(&self.git_dir()?, "service")?,
                }
            }
            Manager::Launchd => {
                let out = output("launchctl", &["print", &self.launchd_target()?])?;
                let text = String::from_utf8_lossy(&out.stdout);
                State {
                    installed: self.launchd_plist_path()?.exists(),
                    running: out.status.success() && text.contains("state = running"),
                }
            }
            Manager::Systemd => {
                let out = output("systemctl", &["--user", "is-active", &self.unit()])?;
                State {
                    installed: self.systemd_unit_path()?.exists(),
                    running: String::from_utf8_lossy(&out.stdout).trim() == "active",
                }
            }
            Manager::OpenRc => State {
                installed: self.openrc_script_path()?.exists(),
                running: openrc_session_running() && rc_service(&[&self.name, "status"]).is_ok(),
            },
            Manager::Runit => {
                let link = self.runit_link()?;
                let running = exists(&link) && {
                    let out = output("sv", &["status", &path_str(&link)?])?;
                    String::from_utf8_lossy(&out.stdout).starts_with("run:")
                };
                State {
                    installed: exists(&link),
                    running,
                }
            }
        })
    }

    /// Where the service is defined, for humans.
    pub fn location(&self) -> Result<String> {
        Ok(match self.manager {
            Manager::TaskScheduler => format!("Task Scheduler task \\{}", self.name),
            Manager::Launchd => self.launchd_plist_path()?.display().to_string(),
            Manager::Systemd => self.systemd_unit_path()?.display().to_string(),
            Manager::OpenRc => self.openrc_script_path()?.display().to_string(),
            Manager::Runit => format!(
                "{} -> {}",
                self.runit_link()?.display(),
                self.runit_dir()?.display()
            ),
        })
    }

    /// Linux only: what the user still has to set up once, as root, for the
    /// service to start by itself (or to run without anyone logged in).
    pub fn note(&self) -> Option<String> {
        let user = env::var("USER").ok()?;
        match self.manager {
            Manager::Systemd => {
                let out = output(
                    "loginctl",
                    &["show-user", &user, "--property=Linger", "--value"],
                )
                .ok()?;
                (String::from_utf8_lossy(&out.stdout).trim() == "no").then(|| {
                    format!(
                        "it runs while you are logged in; to run it from boot without logging in: \
                         sudo loginctl enable-linger {user}"
                    )
                })
            }
            Manager::OpenRc => {
                // /etc/init.d/user.<name> runs that user's services from boot.
                let from_boot = fs::read_dir("/etc/runlevels")
                    .ok()?
                    .flatten()
                    .any(|level| exists(&level.path().join(format!("user.{user}"))));
                if from_boot {
                    return None;
                }
                let first = if openrc_session_running() {
                    "it runs while you are logged in"
                } else {
                    "OpenRC is not running your user services now, so it starts at your \
                     next login (with openrc-user-pam set up)"
                };
                Some(format!(
                    "{first}; to run it from boot without logging in, once, as root \
                     (with the openrc-user package, and XDG_RUNTIME_DIR set by your \
                     login, e.g. by pam_rundir, elogind or your ~/.profile):\n\
                     \x20          ln -s user /etc/init.d/user.{user}\n\
                     \x20          rc-update add user.{user} default\n\
                     \x20          rc-service user.{user} start"
                ))
            }
            Manager::Runit => {
                let svdir = runit_svdir().ok()?;
                if runsvdir_running(&svdir) {
                    return None;
                }
                let home = home().ok()?;
                let (defs, enabled) = runit_system_dirs();
                let def = format!("{defs}/runsvdir-{user}");
                Some(format!(
                    "runit does not run the services in {svdir} yet, so it is not running. \
                     Once, as root, give your user its own runsvdir (it starts at boot):\n\
                     \x20          mkdir {def}\n\
                     \x20          printf '%s\\n' '#!/bin/sh' 'export USER={user} HOME={home}' \
                     'exec chpst -u \"{user}:$(id -Gn {user} | tr \" \" :)\" runsvdir {svdir}' \
                     > {def}/run\n\
                     \x20          chmod +x {def}/run\n\
                     \x20          ln -s {def} {enabled}/",
                    svdir = svdir.display(),
                    home = home.display(),
                ))
            }
            Manager::Launchd | Manager::TaskScheduler => None,
        }
    }

    /// False if the init system does not run the user's services now (OpenRC
    /// with no user session, runit with no runsvdir for the user), so the
    /// service cannot start yet.
    pub fn can_start(&self) -> bool {
        match self.manager {
            Manager::OpenRc => openrc_session_running(),
            Manager::Runit => runit_svdir().is_ok_and(|svdir| runsvdir_running(&svdir)),
            _ => true,
        }
    }

    /// Fails with what to do if the service cannot start now.
    fn check_started_by_init(&self) -> Result<()> {
        if !self.can_start() {
            match self.note() {
                Some(note) => bail!("{note}"),
                None => bail!("the init system is not running your user services"),
            }
        }
        Ok(())
    }

    fn git_dir(&self) -> Result<PathBuf> {
        Git::new(&self.repo).git_dir()
    }

    fn unit(&self) -> String {
        format!("{}.service", self.name)
    }

    fn systemd_unit_path(&self) -> Result<PathBuf> {
        Ok(xdg_dir("XDG_CONFIG_HOME", ".config")?
            .join("systemd/user")
            .join(self.unit()))
    }

    fn openrc_script_path(&self) -> Result<PathBuf> {
        Ok(xdg_dir("XDG_CONFIG_HOME", ".config")?
            .join("rc/init.d")
            .join(&self.name))
    }

    fn openrc_runlevel_link(&self) -> Result<PathBuf> {
        Ok(xdg_dir("XDG_CONFIG_HOME", ".config")?
            .join("rc/runlevels/default")
            .join(&self.name))
    }

    /// The service's folder (`run`, `finish`), linked into the runsvdir.
    fn runit_dir(&self) -> Result<PathBuf> {
        Ok(xdg_dir("XDG_DATA_HOME", ".local/share")?
            .join("gitsync/runit")
            .join(&self.name))
    }

    fn runit_link(&self) -> Result<PathBuf> {
        Ok(runit_svdir()?.join(&self.name))
    }

    fn launchd_plist_path(&self) -> Result<PathBuf> {
        Ok(home()?
            .join("Library/LaunchAgents")
            .join(format!("{}.plist", self.name)))
    }

    fn launchd_target(&self) -> Result<String> {
        Ok(format!("{}/{}", launchd_domain()?, self.name))
    }
}

/// What the Windows task runs (`gitsync service run`): keeps `gitsync watch`
/// running, starting it again 10 s after it stops for any reason and logging
/// why. Needed because Task Scheduler restarts a task only when it fails to
/// start, not when the program it started stops. Linux and macOS use the
/// restart settings of systemd and launchd instead.
pub fn supervise(git: &Git, log_file: &Path) -> Result<()> {
    let Some(_lock) = ProcessLock::acquire(&git.git_dir()?, "service")? else {
        bail!("the gitsync service is already running for this repo");
    };
    let exe = env::current_exe().context("finding the gitsync program")?;
    loop {
        let child = Command::new(&exe)
            .arg("-C")
            .arg(git.workdir())
            .args(["watch", "--log-file"])
            .arg(log_file)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn();
        let reason = match child {
            Ok(mut child) => {
                // What it printed before stopping (its log goes to the file).
                let mut printed = String::new();
                if let Some(mut stderr) = child.stderr.take() {
                    let _ = stderr.read_to_string(&mut printed);
                }
                let status = child.wait()?;
                match printed.trim() {
                    "" => format!("stopped ({status})"),
                    text => format!("stopped ({status}): {text}"),
                }
            }
            Err(err) => format!("could not start: {err}"),
        };
        warn!("gitsync watch {reason}; starting it again in 10 s");
        std::thread::sleep(Duration::from_secs(10));
    }
}

/// A log writer that opens `path` for each message: the service loop
/// writes rarely, and must not keep the file open while `watch` renames it
/// to start a new one, which Windows would refuse.
pub fn append_to(path: PathBuf) -> impl Fn() -> Box<dyn Write> {
    move || match OpenOptions::new().create(true).append(true).open(&path) {
        Ok(file) => Box::new(file),
        Err(_) => Box::new(io::sink()),
    }
}

/// `gitsync-<folder>-<8 hex digits>`: readable, and unique per repo path.
/// The hash is FNV-1a, which (unlike Rust's own hasher) never changes, so
/// a newer gitsync still finds the service an older one installed.
pub fn service_name(repo: &str) -> String {
    let folder = repo
        .trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("");
    let mut base: String = folder
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .take(32)
        .collect();
    base = base.trim_matches('-').to_string();
    if base.is_empty() {
        base = "repo".to_string();
    }
    let mut hash: u32 = 0x811c_9dc5;
    for byte in repo.bytes() {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    format!("gitsync-{base}-{hash:08x}")
}

/// One argument in a systemd `ExecStart=` line.
fn systemd_quote(arg: &str) -> String {
    let escaped = arg
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%")
        .replace('$', "$$");
    format!("\"{escaped}\"")
}

/// One argument on a Windows command line (as programs built with Rust or
/// the C runtime split it).
fn windows_quote(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
        return arg.to_string();
    }
    let mut out = String::from('"');
    let mut backslashes = 0;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.push_str(&"\\".repeat(backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.push_str(&"\\".repeat(backslashes));
                out.push(c);
                backslashes = 0;
            }
        }
    }
    out.push_str(&"\\".repeat(backslashes * 2));
    out.push('"');
    out
}

fn xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn conhost() -> String {
    let windir = env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_string());
    format!(r"{windir}\System32\conhost.exe")
}

/// `DOMAIN\user` of whoever runs this, as Task Scheduler wants it.
fn windows_user() -> Result<String> {
    let out = run("whoami", &[])?;
    let user = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if user.is_empty() {
        bail!("`whoami` printed nothing");
    }
    Ok(user)
}

fn launchd_domain() -> Result<String> {
    let out = run("id", &["-u"])?;
    Ok(format!(
        "gui/{}",
        String::from_utf8_lossy(&out.stdout).trim()
    ))
}

fn home() -> Result<PathBuf> {
    env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .context("HOME is not set")
}

/// `$var`, or `~/<default>` if it is not set (the XDG base directories).
fn xdg_dir(var: &str, default: &str) -> Result<PathBuf> {
    match env::var_os(var) {
        Some(dir) if !dir.is_empty() => Ok(PathBuf::from(dir)),
        _ => Ok(home()?.join(default)),
    }
}

/// One argument for a POSIX shell.
fn sh_quote(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', r"'\''"))
}

/// The user's runtime folder, where OpenRC keeps the user session's state:
/// `$XDG_RUNTIME_DIR`, else `/run/user/<uid>` (which is what sets it, where
/// it is set; an ssh session may not have it).
fn runtime_dir() -> Option<PathBuf> {
    if let Some(dir) = env::var_os("XDG_RUNTIME_DIR").filter(|dir| !dir.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    #[cfg(unix)]
    {
        // SAFETY: getuid cannot fail and has no side effects.
        let uid = unsafe { libc::getuid() };
        let dir = PathBuf::from(format!("/run/user/{uid}"));
        if dir.is_dir() {
            return Some(dir);
        }
    }
    None
}

/// True if OpenRC runs a user session for this user.
fn openrc_session_running() -> bool {
    runtime_dir().is_some_and(|dir| dir.join("openrc/softlevel").exists())
}

/// `rc-service --user ...`, finding the user session as above.
fn rc_service(args: &[&str]) -> Result<Output> {
    let mut cmd = Command::new("rc-service");
    cmd.arg("--user").args(args);
    if let Some(dir) = runtime_dir() {
        cmd.env("XDG_RUNTIME_DIR", dir);
    }
    let out = cmd.output().context("could not run `rc-service`")?;
    check("rc-service", args.last().copied().unwrap_or(""), out)
}

/// The folder whose services the user's runsvdir runs: `$SVDIR` (which
/// `sv` also reads), else `~/service` as Void Linux sets it up.
fn runit_svdir() -> Result<PathBuf> {
    match env::var_os("SVDIR") {
        Some(dir) if !dir.is_empty() => Ok(PathBuf::from(dir)),
        _ => Ok(home()?.join("service")),
    }
}

/// True if some `runsvdir` process runs the services in `svdir`.
fn runsvdir_running(svdir: &Path) -> bool {
    let want = fs::canonicalize(svdir).unwrap_or_else(|_| svdir.to_path_buf());
    let Ok(procs) = fs::read_dir("/proc") else {
        return false;
    };
    procs.flatten().any(|proc| {
        let Ok(cmdline) = fs::read(proc.path().join("cmdline")) else {
            return false;
        };
        let mut args = cmdline
            .split(|&b| b == 0)
            .map(|arg| String::from_utf8_lossy(arg).into_owned());
        let is_runsvdir = args
            .next()
            .is_some_and(|prog| prog.rsplit('/').next() == Some("runsvdir"));
        // `runsvdir [-P] dir [log]`
        is_runsvdir
            && args.any(|arg| {
                let path = Path::new(&arg);
                path.is_absolute()
                    && fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()) == want
            })
    })
}

/// Where this system keeps runit's service folders, and the folder of
/// those that run (Void: /etc/sv, /var/service; Artix: /etc/runit/sv,
/// /run/runit/service; Debian and Alpine: /etc/sv, /etc/service).
fn runit_system_dirs() -> (&'static str, &'static str) {
    let defs = if Path::new("/etc/runit/sv").is_dir() {
        "/etc/runit/sv"
    } else {
        "/etc/sv"
    };
    let enabled = ["/var/service", "/run/runit/service", "/etc/service"]
        .into_iter()
        .find(|dir| Path::new(dir).is_dir())
        .unwrap_or("/var/service");
    (defs, enabled)
}

/// Waits until `path` exists.
fn wait_for(path: &Path, timeout: Duration) -> Result<()> {
    let deadline = std::time::Instant::now() + timeout;
    while !path.exists() {
        if std::time::Instant::now() > deadline {
            bail!("timed out waiting for {}", path.display());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok(())
}

/// True if `path` exists, or is a link (even to nothing).
fn exists(path: &Path) -> bool {
    path.symlink_metadata().is_ok()
}

fn remove(path: &Path) -> Result<()> {
    fs::remove_file(path).with_context(|| format!("removing {}", path.display()))
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
        .with_context(|| format!("making {} executable", path.display()))
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn symlink(target: &Path, link: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, link)
        .with_context(|| format!("linking {} to {}", link.display(), target.display()))
}

#[cfg(not(unix))]
fn symlink(_target: &Path, link: &Path) -> Result<()> {
    bail!("cannot link {}: not a Unix system", link.display())
}

fn path_str(path: &Path) -> Result<String> {
    path.to_str()
        .map(String::from)
        .context("path is not valid UTF-8")
}

fn write_file(path: &Path, contents: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    fs::write(path, contents).with_context(|| format!("writing {}", path.display()))
}

fn output(program: &str, args: &[&str]) -> Result<Output> {
    Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("could not run `{program}`"))
}

/// Runs a system tool; fails with what it printed if it fails.
fn run(program: &str, args: &[&str]) -> Result<Output> {
    let out = output(program, args)?;
    check(program, args.first().copied().unwrap_or(""), out)
}

/// `out`, or an error with what `program` printed if it failed.
fn check(program: &str, what: &str, out: Output) -> Result<Output> {
    if !out.status.success() {
        let mut message = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if message.is_empty() {
            message = String::from_utf8_lossy(&out.stdout).trim().to_string();
        }
        bail!("`{program} {what}` failed: {message}");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service(repo: &str, exe: &str, log: Option<&str>) -> Service {
        Service {
            manager: Manager::Systemd,
            name: service_name(repo),
            repo: repo.to_string(),
            exe: PathBuf::from(exe),
            log_file: log.map(PathBuf::from),
            path_env: Some("/usr/local/bin:/usr/bin".to_string()),
        }
    }

    #[test]
    fn names_are_readable_stable_and_unique_per_path() {
        assert_eq!(
            service_name("/home/me/code/My Project"),
            service_name("/home/me/code/My Project")
        );
        assert!(service_name("/home/me/code/My Project").starts_with("gitsync-my-project-"));
        assert_ne!(service_name("/a/demo"), service_name("/b/demo"));
        assert!(service_name("C:/Users/me/repos/Demo").starts_with("gitsync-demo-"));
        // Fixed forever: FNV-1a of the path.
        assert_eq!(service_name("/a/demo"), "gitsync-demo-f4e174a9");
        assert!(service_name("/").starts_with("gitsync-repo-"));
    }

    #[test]
    fn systemd_unit_quotes_paths() {
        let unit = service("/home/me/my repo", "/home/me/.local/bin/gitsync", None).systemd_unit();
        assert!(
            unit.contains(
                "ExecStart=\"/home/me/.local/bin/gitsync\" \"-C\" \"/home/me/my repo\" \"watch\"\n"
            ),
            "{unit}"
        );
        assert!(unit.contains("Environment=\"PATH=/usr/local/bin:/usr/bin\"\n"));
        assert!(unit.contains("WantedBy=default.target"));
        let odd = service("/home/me/100% $HOME", "/bin/gitsync", None).systemd_unit();
        assert!(odd.contains("\"/home/me/100%% $$HOME\""), "{odd}");
    }

    #[test]
    fn shell_quoting() {
        assert_eq!(sh_quote("/a b/c"), "'/a b/c'");
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
        assert_eq!(sh_quote("$HOME"), "'$HOME'");
    }

    #[test]
    fn openrc_script_quotes_for_eval() {
        let script = service(
            "/home/me/it's mine",
            "/home/me/my bin/gitsync",
            Some("/home/me/.local/state/gitsync/logs/x.log"),
        )
        .openrc_script();
        assert!(script.starts_with("#!/sbin/openrc-run\n"), "{script}");
        assert!(script.contains("supervisor=supervise-daemon\n"));
        assert!(
            script.contains(r#"command=''\''/home/me/my bin/gitsync'\'''"#),
            "{script}"
        );
        assert!(
            script.contains(
                r#"command_args=''\''-C'\'' '\''/home/me/it'\''\'\'''\''s mine'\'' '\''watch'\'' "#
            ),
            "{script}"
        );
        assert!(
            script
                .contains(r#"supervise_daemon_args='--env '\''PATH=/usr/local/bin:/usr/bin'\'''"#),
            "{script}"
        );
        assert!(script.contains("respawn_max=0\n"));
    }

    #[test]
    fn runit_scripts() {
        let run = service(
            "/home/me/my repo",
            "/home/me/.local/bin/gitsync",
            Some("/home/me/.local/state/gitsync/logs/x.log"),
        )
        .runit_run();
        assert!(
            run.contains("export PATH='/usr/local/bin:/usr/bin'\n"),
            "{run}"
        );
        assert!(
            run.contains(
                "exec '/home/me/.local/bin/gitsync' '-C' '/home/me/my repo' 'watch' \
                 '--log-file' '/home/me/.local/state/gitsync/logs/x.log' 2>&1\n"
            ),
            "{run}"
        );
        assert!(Service::runit_finish().contains("0:* | 130:* | -1:15 | -1:2) ;;"));
    }

    #[test]
    fn launchd_plist_lists_arguments() {
        let plist = service(
            "/Users/me/a&b",
            "/opt/homebrew/bin/gitsync",
            Some("/Users/me/Library/Logs/gitsync/x.log"),
        )
        .launchd_plist();
        assert!(
            plist.contains("<string>/Users/me/a&amp;b</string>"),
            "{plist}"
        );
        assert!(plist.contains("<string>--log-file</string>"));
        assert!(plist.contains("<key>RunAtLoad</key>"));
    }

    #[test]
    fn windows_task_runs_hidden_and_on_battery() {
        let xml = service(
            "C:/Users/me/repos/My Demo",
            r"C:\Users\me\.cargo\bin\gitsync.exe",
            Some(r"C:\Users\me\AppData\Local\gitsync\logs\x.log"),
        )
        .windows_task_xml(r"AzureAD\Me");
        assert!(xml.contains(r"<UserId>AzureAD\Me</UserId>"));
        assert!(xml.contains(r"\System32\conhost.exe</Command>"), "{xml}");
        assert!(xml.contains(
            r#"<Arguments>--headless C:\Users\me\.cargo\bin\gitsync.exe -C &quot;C:/Users/me/repos/My Demo&quot; service run --log-file C:\Users\me\AppData\Local\gitsync\logs\x.log</Arguments>"#
        ), "{xml}");
        assert!(xml.contains("<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>"));
        assert!(xml.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"));
    }

    #[test]
    fn windows_quoting() {
        assert_eq!(windows_quote(r"C:\a\b"), r"C:\a\b");
        assert_eq!(windows_quote(r"C:\my dir\"), r#""C:\my dir\\""#);
        assert_eq!(windows_quote(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(windows_quote(""), r#""""#);
    }
}
