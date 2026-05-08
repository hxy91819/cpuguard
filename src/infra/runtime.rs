use std::process::Command;

use anyhow::{Context, Result, bail};

pub fn process_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub fn first_pid_by_name(name: &str) -> Result<Option<u32>> {
    let out = Command::new("pgrep").args(["-x", name]).output()?;
    if !out.status.success() {
        return Ok(None);
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let pid = text
        .lines()
        .next()
        .and_then(|line| line.trim().parse::<u32>().ok());
    Ok(pid)
}

/// 向指定 PID 发送 SIGTERM 信号终止进程。
pub fn kill_process(pid: u32) -> Result<()> {
    let status = Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .context("failed to send SIGTERM")?;
    if !status.success() {
        bail!("kill -TERM {} failed", pid);
    }
    Ok(())
}
