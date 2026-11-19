//! Bounded local execution. Process limits are not an OS security sandbox.
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::process::Command;

use crate::model::CommandRun;

const OUTPUT_LIMIT: u64 = 8 * 1024 * 1024;

pub async fn run(
    program: &str,
    args: &[String],
    cwd: &Path,
    logs: &Path,
    label: &str,
    seconds: u64,
) -> Result<CommandRun> {
    std::fs::create_dir_all(logs)?;
    let stdout_path = logs.join(format!("{label}.stdout"));
    let stderr_path = logs.join(format!("{label}.stderr"));
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(std::fs::File::create(&stdout_path)?)
        .stderr(std::fs::File::create(&stderr_path)?)
        .kill_on_drop(true);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.as_std_mut().process_group(0);
    }
    let started = Instant::now();
    let mut child = command
        .spawn()
        .with_context(|| format!("cannot execute {program}"))?;
    let pid = child.id();
    let mut group = ProcessGroup(pid);
    let mut timed_out = false;
    let mut output_limit_exceeded = false;
    let status = loop {
        output_limit_exceeded = output_limit_exceeded
            || std::fs::metadata(&stdout_path)?.len() > OUTPUT_LIMIT
            || std::fs::metadata(&stderr_path)?.len() > OUTPUT_LIMIT;
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        if exited_without_reaping(pid)? {
            // Keep the group leader as a zombie until cleanup, preventing PID/PGID reuse.
            group.stop();
            break child.wait().await?;
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        if let Some(status) = child.try_wait()? {
            // The PID is already reaped on this fallback path; do not signal a reused PGID.
            group.0 = None;
            break status;
        }
        timed_out = started.elapsed() >= Duration::from_secs(seconds);
        if timed_out || output_limit_exceeded {
            group.stop();
            let _ = child.kill().await;
            break child.wait().await?;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    // Stop descendants before reading final output or reporting success.
    drop(group);
    output_limit_exceeded = output_limit_exceeded
        || std::fs::metadata(&stdout_path)?.len() > OUTPUT_LIMIT
        || std::fs::metadata(&stderr_path)?.len() > OUTPUT_LIMIT;
    let read = |path: &Path| -> Result<String> {
        use std::io::Read;
        let mut bytes = Vec::new();
        std::fs::File::open(path)?
            .take(OUTPUT_LIMIT)
            .read_to_end(&mut bytes)?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    };
    Ok(CommandRun {
        program: program.to_owned(),
        args: args.to_vec(),
        cwd: cwd.display().to_string(),
        exit_code: status.code(),
        signal: {
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                status.signal()
            }
            #[cfg(not(unix))]
            {
                None
            }
        },
        timed_out,
        output_limit_exceeded,
        duration_ms: started.elapsed().as_millis() as u64,
        stdout: read(&stdout_path)?,
        stderr: read(&stderr_path)?,
        stdout_path: stdout_path.display().to_string(),
        stderr_path: stderr_path.display().to_string(),
    })
}

fn kill_group(pid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = pid {
        // Only the new process group created above is targeted.
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    let _ = pid;
}

struct ProcessGroup(Option<u32>);
impl ProcessGroup {
    fn stop(&mut self) {
        kill_group(self.0.take());
    }
}
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn exited_without_reaping(pid: Option<u32>) -> Result<bool> {
    let pid = pid.context("child PID is unavailable")?;
    // WNOWAIT observes an exited child without freeing its PID. It stays owned
    // until tokio wait reaps it after process-group cleanup.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(unsafe { info.si_pid() } != 0)
}
