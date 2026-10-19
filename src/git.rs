//! Small wrapper that runs the system `git` program (DESIGN.md §5: we use the
//! user's own git and ssh instead of a git library).

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{bail, Context, Result};

/// A git repository, addressed by its working directory.
#[derive(Debug, Clone)]
pub struct Git {
    workdir: PathBuf,
}

impl Git {
    pub fn new(workdir: impl Into<PathBuf>) -> Self {
        Self {
            workdir: workdir.into(),
        }
    }

    pub fn workdir(&self) -> &Path {
        &self.workdir
    }

    /// Runs `git <args>` and returns its trimmed stdout. Fails if git fails.
    pub fn run(&self, args: &[&str]) -> Result<String> {
        self.run_env(args, &[])
    }

    /// Like [`Git::run`], with extra environment variables.
    pub fn run_env(&self, args: &[&str], envs: &[(&str, &str)]) -> Result<String> {
        let output = self.output(args, envs)?;
        if !output.status.success() {
            bail!(failure(args, &output));
        }
        Ok(stdout(&output))
    }

    /// For lookups that may legitimately find nothing, such as
    /// `rev-parse --verify -q` or `symbolic-ref -q`: exit code 1 means `None`.
    pub fn try_run(&self, args: &[&str]) -> Result<Option<String>> {
        let output = self.output(args, &[])?;
        match output.status.code() {
            Some(0) => Ok(Some(stdout(&output))),
            Some(1) => Ok(None),
            _ => bail!(failure(args, &output)),
        }
    }

    /// For commands where exit code 1 is an answer, not an error (such as
    /// `merge-tree`, which exits 1 on conflicts): returns whether it exited
    /// 0, and the raw, untrimmed stdout.
    pub fn run_ok_or_1(&self, args: &[&str]) -> Result<(bool, String)> {
        let output = self.output(args, &[])?;
        match output.status.code() {
            Some(code @ (0 | 1)) => Ok((
                code == 0,
                String::from_utf8_lossy(&output.stdout).into_owned(),
            )),
            _ => bail!(failure(args, &output)),
        }
    }

    /// Absolute path of the `.git` directory.
    pub fn git_dir(&self) -> Result<PathBuf> {
        Ok(PathBuf::from(
            self.run(&["rev-parse", "--absolute-git-dir"])?,
        ))
    }

    fn output(&self, args: &[&str], envs: &[(&str, &str)]) -> Result<Output> {
        Command::new("git")
            .arg("-C")
            .arg(&self.workdir)
            .args(args)
            .envs(envs.iter().copied())
            .output()
            .context("could not run `git`; is it installed and on PATH?")
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// A short, one-line description of a failed git command: the subcommand and
/// what git printed, minus its generic advice paragraphs.
fn failure(args: &[&str], output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let message: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| {
            !line.starts_with("Please make sure you have the correct access rights")
                && *line != "and the repository exists."
        })
        .collect();
    let subcommand = args.first().copied().unwrap_or("");
    if message.is_empty() {
        format!("`git {subcommand}` failed ({})", output.status)
    } else {
        format!("`git {subcommand}` failed: {}", message.join(" / "))
    }
}
