use std::{
    io::{self, Read},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Clone, Copy)]
pub(super) struct Limits {
    pub(super) timeout_ms: Option<u64>,
    pub(super) output_limit: usize,
    pub(super) max_concurrency: usize,
}

impl Limits {
    pub(super) const DEFAULT_OUTPUT_LIMIT: usize = 1024 * 1024;
    pub(super) const DEFAULT_MAX_CONCURRENCY: usize = 1;
}

pub(super) struct Invocation {
    pub(super) command: String,
    pub(super) args: Vec<String>,
}

pub(super) struct Output {
    pub(super) exit_code: Option<i32>,
    pub(super) stdout: String,
    pub(super) stderr: String,
    pub(super) timed_out: bool,
    pub(super) stdout_truncated: bool,
    pub(super) stderr_truncated: bool,
}

pub(super) enum RunResult {
    Complete(Output),
    Cancelled,
}

fn read_capped<R: Read>(mut reader: R, limit: usize) -> io::Result<(Vec<u8>, bool)> {
    let mut output = Vec::with_capacity(limit.min(64 * 1024));
    let mut truncated = false;
    let mut buffer = [0_u8; 8192];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        let remaining = limit.saturating_sub(output.len());
        let kept = count.min(remaining);
        output.extend_from_slice(&buffer[..kept]);
        truncated |= kept < count;
    }
    Ok((output, truncated))
}

#[cfg(unix)]
fn prepare_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

#[cfg(not(unix))]
fn prepare_process_group(_command: &mut Command) {}

fn terminate_process_tree(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        unsafe extern "C" {
            fn kill(pid: i32, signal: i32) -> i32;
        }
        const SIGKILL: i32 = 9;
        if let Ok(pid) = i32::try_from(child.id()) {
            // SAFETY: the child is the leader of the process group created before spawn;
            // `kill` is called with a valid signal number and does not dereference pointers.
            unsafe {
                let _ = kill(-pid, SIGKILL);
            }
        }
    }

    #[cfg(windows)]
    {
        // `taskkill.exe` is part of Windows and is used only to terminate descendants.
        // Resolve it from SystemRoot rather than PATH.
        if let Some(system_root) = std::env::var_os("SystemRoot") {
            let taskkill = std::path::PathBuf::from(system_root)
                .join("System32")
                .join("taskkill.exe");
            let _ = Command::new(taskkill)
                .args(["/PID", &child.id().to_string(), "/T", "/F"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }

    let _ = child.kill();
}

pub(super) fn run(
    invocation: Invocation,
    limits: Limits,
    cancelled: Arc<AtomicBool>,
) -> Result<RunResult, String> {
    if cancelled.load(Ordering::Acquire) {
        return Ok(RunResult::Cancelled);
    }

    let deadline = limits
        .timeout_ms
        .map(|timeout_ms| {
            Instant::now()
                .checked_add(Duration::from_millis(timeout_ms))
                .ok_or("process timeout is too large")
        })
        .transpose()?;

    let mut command = Command::new(invocation.command);
    command.args(&invocation.args);
    prepare_process_group(&mut command);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command
        .spawn()
        .map_err(|error| format!("failed to spawn process: {error}"))?;
    let stdout = child.stdout.take().ok_or_else(|| {
        terminate_process_tree(&mut child);
        let _ = child.wait();
        "process stdout unavailable".to_string()
    })?;
    let stderr = child.stderr.take().ok_or_else(|| {
        terminate_process_tree(&mut child);
        let _ = child.wait();
        "process stderr unavailable".to_string()
    })?;

    let output_limit = limits.output_limit;
    let stdout_thread = match thread::Builder::new()
        .name("process-stdout".into())
        .spawn(move || read_capped(stdout, output_limit))
    {
        Ok(thread) => thread,
        Err(error) => {
            terminate_process_tree(&mut child);
            let _ = child.wait();
            return Err(format!("failed to start stdout reader: {error}"));
        }
    };
    let stderr_thread = match thread::Builder::new()
        .name("process-stderr".into())
        .spawn(move || read_capped(stderr, output_limit))
    {
        Ok(thread) => thread,
        Err(error) => {
            terminate_process_tree(&mut child);
            let _ = child.wait();
            let _ = stdout_thread.join();
            return Err(format!("failed to start stderr reader: {error}"));
        }
    };

    let (status, timed_out, was_cancelled) = loop {
        if cancelled.load(Ordering::Acquire) {
            terminate_process_tree(&mut child);
            let status = child.wait().ok();
            break (status, false, true);
        }

        match child.try_wait() {
            Ok(Some(status)) => {
                // A synchronous tool call owns its subprocess tree. Terminate any
                // descendants still holding inherited stdout/stderr after the root
                // process exits so output collection cannot wait on background work.
                terminate_process_tree(&mut child);
                break (Some(status), false, false);
            }
            Ok(None) => {}
            Err(error) => {
                terminate_process_tree(&mut child);
                let _ = child.wait();
                let _ = stdout_thread.join();
                let _ = stderr_thread.join();
                return Err(format!("failed to query process status: {error}"));
            }
        }

        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            terminate_process_tree(&mut child);
            let status = child.wait().ok();
            break (status, true, false);
        }
        thread::sleep(Duration::from_millis(10));
    };

    let (stdout, stdout_truncated) = stdout_thread
        .join()
        .map_err(|_| "stdout reader panicked")?
        .map_err(|error| error.to_string())?;
    let (stderr, stderr_truncated) = stderr_thread
        .join()
        .map_err(|_| "stderr reader panicked")?
        .map_err(|error| error.to_string())?;

    if was_cancelled || cancelled.load(Ordering::Acquire) {
        return Ok(RunResult::Cancelled);
    }

    Ok(RunResult::Complete(Output {
        exit_code: status.as_ref().and_then(std::process::ExitStatus::code),
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
        timed_out,
        stdout_truncated,
        stderr_truncated,
    }))
}
