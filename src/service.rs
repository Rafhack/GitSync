//! `gitsync service`: runs `gitsync watch` for one repo in the background,
//! started when the user logs in and restarted if it stops with an error
//! (DESIGN.md §9). One service per repo, using what each OS provides:
//!
//! - Linux: a systemd user unit; the log goes to the journal.
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

/// The background service for one repo, and what it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Service {
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
        let log_file = if cfg!(windows) {
            let base = env::var_os("LOCALAPPDATA").context("LOCALAPPDATA is not set")?;
            Some(
                Path::new(&base)
                    .join(r"gitsync\logs")
                    .join(format!("{name}.log")),
            )
        } else if cfg!(target_os = "macos") {
            Some(
                home()?
                    .join("Library/Logs/gitsync")
                    .join(format!("{name}.log")),
            )
        } else {
            None
        };
        Ok(Self {
            name,
            repo,
            exe,
            log_file,
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
        if cfg!(windows) {
            self.run_args()
        } else {
            self.watch_args()
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
        if cfg!(windows) {
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
        } else if cfg!(target_os = "macos") {
            let plist = self.launchd_plist_path()?;
            write_file(&plist, &self.launchd_plist())?;
            let _ = run("launchctl", &["bootout", &self.launchd_target()?]);
            run(
                "launchctl",
                &["bootstrap", &launchd_domain()?, &path_str(&plist)?],
            )?;
        } else {
            write_file(&self.systemd_unit_path()?, &self.systemd_unit())?;
            run("systemctl", &["--user", "daemon-reload"])?;
            run("systemctl", &["--user", "enable", &self.unit()])?;
            // Starts it, or restarts it to pick up a new unit file, program
            // or config if it was running already.
            run("systemctl", &["--user", "restart", &self.unit()])?;
        }
        Ok(())
    }

    /// Stops the service and removes it.
    pub fn uninstall(&self) -> Result<()> {
        if cfg!(windows) {
            let _ = self.stop();
            run("schtasks", &["/Delete", "/TN", &self.name, "/F"])?;
        } else if cfg!(target_os = "macos") {
            let _ = run("launchctl", &["bootout", &self.launchd_target()?]);
            let plist = self.launchd_plist_path()?;
            fs::remove_file(&plist).with_context(|| format!("removing {}", plist.display()))?;
        } else {
            run("systemctl", &["--user", "disable", "--now", &self.unit()])?;
            let unit = self.systemd_unit_path()?;
            fs::remove_file(&unit).with_context(|| format!("removing {}", unit.display()))?;
            run("systemctl", &["--user", "daemon-reload"])?;
        }
        Ok(())
    }

    /// Starts the installed service (until the next stop; it also starts at
    /// every login).
    pub fn start(&self) -> Result<()> {
        if cfg!(windows) {
            run("schtasks", &["/Run", "/TN", &self.name])?;
        } else if cfg!(target_os = "macos") {
            run(
                "launchctl",
                &[
                    "bootstrap",
                    &launchd_domain()?,
                    &path_str(&self.launchd_plist_path()?)?,
                ],
            )?;
        } else {
            run("systemctl", &["--user", "start", &self.unit()])?;
        }
        Ok(())
    }

    /// Stops the service until `start` or the next login.
    pub fn stop(&self) -> Result<()> {
        if cfg!(windows) {
            // Ending the task only ends conhost, not gitsync under it: end
            // the service loop with its watch (/T), then any watch left.
            let _ = run("schtasks", &["/End", "/TN", &self.name]);
            let git_dir = self.git_dir()?;
            for lock in ["service", "watch"] {
                if let Some(pid) = ProcessLock::holder(&git_dir, lock)? {
                    run("taskkill", &["/PID", &pid.to_string(), "/T", "/F"])?;
                }
            }
        } else if cfg!(target_os = "macos") {
            run("launchctl", &["bootout", &self.launchd_target()?])?;
        } else {
            run("systemctl", &["--user", "stop", &self.unit()])?;
        }
        Ok(())
    }

    pub fn state(&self) -> Result<State> {
        if cfg!(windows) {
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
            Ok(State {
                installed: out.status.success(),
                running: ProcessLock::is_held(&self.git_dir()?, "service")?,
            })
        } else if cfg!(target_os = "macos") {
            let out = output("launchctl", &["print", &self.launchd_target()?])?;
            let text = String::from_utf8_lossy(&out.stdout);
            Ok(State {
                installed: self.launchd_plist_path()?.exists(),
                running: out.status.success() && text.contains("state = running"),
            })
        } else {
            let out = output("systemctl", &["--user", "is-active", &self.unit()])?;
            Ok(State {
                installed: self.systemd_unit_path()?.exists(),
                running: String::from_utf8_lossy(&out.stdout).trim() == "active",
            })
        }
    }

    /// Where the service is defined, for humans.
    pub fn location(&self) -> Result<String> {
        Ok(if cfg!(windows) {
            format!("Task Scheduler task \\{}", self.name)
        } else if cfg!(target_os = "macos") {
            self.launchd_plist_path()?.display().to_string()
        } else {
            self.systemd_unit_path()?.display().to_string()
        })
    }

    /// Linux only: a note if user services stop when the user logs out (no
    /// "linger"), which matters on machines nobody logs in to.
    pub fn linger_note(&self) -> Option<String> {
        if cfg!(windows) || cfg!(target_os = "macos") {
            return None;
        }
        let user = env::var("USER").ok()?;
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

    fn git_dir(&self) -> Result<PathBuf> {
        Git::new(&self.repo).git_dir()
    }

    fn unit(&self) -> String {
        format!("{}.service", self.name)
    }

    fn systemd_unit_path(&self) -> Result<PathBuf> {
        let config = match env::var_os("XDG_CONFIG_HOME") {
            Some(dir) if !dir.is_empty() => PathBuf::from(dir),
            _ => home()?.join(".config"),
        };
        Ok(config.join("systemd/user").join(self.unit()))
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
    if !out.status.success() {
        let mut message = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if message.is_empty() {
            message = String::from_utf8_lossy(&out.stdout).trim().to_string();
        }
        bail!(
            "`{program} {}` failed: {message}",
            args.first().copied().unwrap_or("")
        );
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service(repo: &str, exe: &str, log: Option<&str>) -> Service {
        Service {
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
