use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

use anyhow::{Context, Result, bail};

pub trait CpulimitExecutor {
    fn ensure_available(&self) -> Result<()>;
    fn start_adhoc(&self, target_pid: u32, limit: u16) -> Result<u32>;
    fn run_for_target(&self, target_pid: u32, limit: u16) -> Result<()>;
    fn stop_instance(&self, cpulimit_pid: u32) -> Result<()>;
}

#[derive(Debug, Clone)]
pub struct RealCpulimitExecutor {
    pub bin: PathBuf,
}

impl RealCpulimitExecutor {
    fn spawn_cpulimit(&self, args: &[String]) -> Result<Child> {
        let child = Command::new(&self.bin)
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("spawn cpulimit {:?}", args))?;
        Ok(child)
    }
}

fn pid_limit_args(target_pid: u32, limit: u16) -> Vec<String> {
    vec![
        "-p".to_string(),
        target_pid.to_string(),
        "-l".to_string(),
        limit.to_string(),
        "-i".to_string(),
    ]
}

impl CpulimitExecutor for RealCpulimitExecutor {
    fn ensure_available(&self) -> Result<()> {
        let output = Command::new(&self.bin)
            .arg("--help")
            .output()
            .with_context(|| format!("run {} --help", self.bin.display()))?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let usage_found = stdout.contains("Usage: cpulimit") || stderr.contains("Usage: cpulimit");
        if !usage_found {
            bail!("cpulimit not available: {}", self.bin.display());
        }
        Ok(())
    }

    fn start_adhoc(&self, target_pid: u32, limit: u16) -> Result<u32> {
        let args = pid_limit_args(target_pid, limit);
        let child = self.spawn_cpulimit(&args)?;
        Ok(child.id())
    }

    fn run_for_target(&self, target_pid: u32, limit: u16) -> Result<()> {
        let args = pid_limit_args(target_pid, limit);
        let mut child = self.spawn_cpulimit(&args)?;
        let status = child.wait().context("wait cpulimit instance")?;
        if !status.success() {
            bail!("cpulimit exited with status {status}");
        }
        Ok(())
    }

    fn stop_instance(&self, cpulimit_pid: u32) -> Result<()> {
        let status = Command::new("kill")
            .args(["-TERM", &cpulimit_pid.to_string()])
            .status()
            .context("kill cpulimit instance")?;
        if !status.success() {
            bail!("failed to stop cpulimit pid {}", cpulimit_pid);
        }
        Ok(())
    }
}
