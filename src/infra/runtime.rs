use std::process::Command;

use anyhow::{Context, Result, bail};

pub fn process_alive(pid: u32) -> bool {
    let output = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "stat="])
        .output();
    let Ok(output) = output else {
        return true;
    };
    if !output.status.success() {
        return false;
    }
    let stat = String::from_utf8_lossy(&output.stdout);
    !stat.trim_start().starts_with('Z')
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
