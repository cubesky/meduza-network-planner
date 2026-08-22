use std::ffi::{OsStr, OsString};
use std::process::{Command, Output, Stdio};

use anyhow::{Context, Result, bail};

pub trait Runner: Clone + Send + Sync + 'static {
    fn output<I, S>(&self, program: &str, args: I) -> Result<Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>;

    fn status<I, S>(&self, program: &str, args: I) -> Result<()>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = self.output(program, args)?;
        if output.status.success() {
            Ok(())
        } else {
            bail!("{program} exited with status {}", output.status)
        }
    }

    fn text<I, S>(&self, program: &str, args: I) -> Result<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = self.output(program, args)?;
        if !output.status.success() {
            bail!("{program} exited with status {}", output.status);
        }
        String::from_utf8(output.stdout).context("command output was not UTF-8")
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemRunner;

impl Runner for SystemRunner {
    fn output<I, S>(&self, program: &str, args: I) -> Result<Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args: Vec<OsString> = args
            .into_iter()
            .map(|value| value.as_ref().to_os_string())
            .collect();
        let mut command = Command::new(program);
        command.args(&args).stdin(Stdio::null());
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::process::CommandExt;

            // SAFETY: only async-signal-safe libc calls run between fork and
            // exec. The parent-death signal prevents a blocking OpenWrt helper
            // from surviving if procd must SIGKILL the controller, while the
            // dedicated process group keeps its descendants isolated.
            unsafe {
                command.pre_exec(|| {
                    if libc::setpgid(0, 0) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    if libc::getppid() == 1 {
                        return Err(std::io::Error::from_raw_os_error(libc::ECHILD));
                    }
                    Ok(())
                });
            }
        }
        command
            .output()
            .with_context(|| format!("could not execute {program}"))
    }
}

pub fn command_exists(name: &str) -> bool {
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .any(|path| path.join(name).is_file())
}
