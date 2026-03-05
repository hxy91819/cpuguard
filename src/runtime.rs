use std::process::Command;

use anyhow::Result;

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
