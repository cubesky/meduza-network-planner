use std::ffi::{OsStr, OsString};
#[cfg(target_os = "linux")]
use std::fs::{File, OpenOptions};
#[cfg(target_os = "linux")]
use std::io::{Read, Seek, SeekFrom};
#[cfg(target_os = "linux")]
use std::os::unix::fs::OpenOptionsExt;
#[cfg(target_os = "linux")]
use std::process::ExitStatus;
use std::process::{Command, Output, Stdio};
#[cfg(target_os = "linux")]
use std::time::{Duration, Instant};

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

/// No OpenWrt helper is allowed to hold the controller transaction forever.
/// Service reloads are normally much faster than this, while 30 seconds still
/// leaves ample room for slow flash and low-end routers.
#[cfg(target_os = "linux")]
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

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
        #[cfg(target_os = "linux")]
        {
            run_output_linux(program, &args, COMMAND_TIMEOUT)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let mut command = Command::new(program);
            command.args(&args).stdin(Stdio::null());
            command
                .output()
                .with_context(|| format!("could not execute {program}"))
        }
    }

    fn status<I, S>(&self, program: &str, args: I) -> Result<()>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args: Vec<OsString> = args
            .into_iter()
            .map(|value| value.as_ref().to_os_string())
            .collect();
        #[cfg(target_os = "linux")]
        let status = run_status_linux(program, &args, COMMAND_TIMEOUT)?;
        #[cfg(not(target_os = "linux"))]
        let status = Command::new(program)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .with_context(|| format!("could not execute {program}"))?;
        if status.success() {
            Ok(())
        } else {
            bail!("{program} exited with status {status}")
        }
    }
}

#[cfg(target_os = "linux")]
fn configure_linux_command(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    // SAFETY: only async-signal-safe libc calls run between fork and exec. The
    // child is placed in a dedicated process group so a timed-out wrapper and
    // all descendants can be terminated together. PDEATHSIG also prevents a
    // helper surviving a SIGKILL of the controller.
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

#[cfg(target_os = "linux")]
fn run_status_linux(program: &str, args: &[OsString], timeout: Duration) -> Result<ExitStatus> {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        // Status callers never inspect command output. Using /dev/null also
        // prevents a daemonized grandchild from retaining a capture pipe and
        // making wait_with_output block forever after its wrapper exited.
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    configure_linux_command(&mut command);
    let mut child = command
        .spawn()
        .with_context(|| format!("could not execute {program}"))?;
    wait_bounded(&mut child, program, timeout)
}

#[cfg(target_os = "linux")]
fn run_output_linux(program: &str, args: &[OsString], timeout: Duration) -> Result<Output> {
    let mut stdout = anonymous_capture_file("stdout")?;
    let mut stderr = anonymous_capture_file("stderr")?;
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout.try_clone()?))
        .stderr(Stdio::from(stderr.try_clone()?));
    configure_linux_command(&mut command);
    let mut child = command
        .spawn()
        .with_context(|| format!("could not execute {program}"))?;
    let status = wait_bounded(&mut child, program, timeout)?;
    stdout.seek(SeekFrom::Start(0))?;
    stderr.seek(SeekFrom::Start(0))?;
    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();
    stdout.read_to_end(&mut stdout_bytes)?;
    stderr.read_to_end(&mut stderr_bytes)?;
    Ok(Output {
        status,
        stdout: stdout_bytes,
        stderr: stderr_bytes,
    })
}

#[cfg(target_os = "linux")]
fn wait_bounded(
    child: &mut std::process::Child,
    program: &str,
    timeout: Duration,
) -> Result<ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            terminate_process_group(child.id(), libc::SIGTERM);
            let grace = Instant::now() + Duration::from_millis(500);
            while Instant::now() < grace {
                if child.try_wait()?.is_some() {
                    bail!("{program} timed out after {} seconds", timeout.as_secs());
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            terminate_process_group(child.id(), libc::SIGKILL);
            let _ = child.kill();
            let _ = child.wait();
            bail!("{program} timed out after {} seconds", timeout.as_secs());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(target_os = "linux")]
fn terminate_process_group(pid: u32, signal: libc::c_int) {
    if let Ok(pid) = i32::try_from(pid) {
        // SAFETY: a negative PID addresses only the dedicated process group
        // created for this child; errors merely mean it already exited.
        unsafe {
            libc::kill(-pid, signal);
        }
    }
}

#[cfg(target_os = "linux")]
fn anonymous_capture_file(kind: &str) -> Result<File> {
    for _ in 0..8 {
        let nonce = rand::random::<u128>();
        let path = std::env::temp_dir().join(format!(
            ".meduza-command-{}-{nonce:032x}-{kind}",
            std::process::id()
        ));
        let file = match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).context("could not create anonymous command capture");
            }
        };
        // Unix keeps the opened inode alive while removing the pathname
        // immediately, so command execution cannot leave runtime artifacts.
        std::fs::remove_file(&path).context("could not unlink command capture")?;
        return Ok(file);
    }
    bail!("could not allocate a unique anonymous command capture")
}

pub fn command_exists(name: &str) -> bool {
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .any(|path| path.join(name).is_file())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn status_timeout_kills_the_entire_command_group() {
        let started = Instant::now();
        let error = run_status_linux(
            "/bin/sh",
            &[OsString::from("-c"), OsString::from("sleep 30")],
            Duration::from_millis(100),
        )
        .unwrap_err();
        assert!(error.to_string().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn status_does_not_capture_daemon_descendant_pipes() {
        let started = Instant::now();
        let status = run_status_linux(
            "/bin/sh",
            &[OsString::from("-c"), OsString::from("(sleep 5) & exit 0")],
            Duration::from_secs(1),
        )
        .unwrap();
        assert!(status.success());
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
