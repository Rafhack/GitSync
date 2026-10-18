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
            bail!(
                "`git {}` failed ({}): {}",
                args.join(" "),
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
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
            _ => bail!(
                "`git {}` failed ({}): {}",
                args.join(" "),
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ),
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
