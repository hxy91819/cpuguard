use std::env;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow};

use crate::model::Domain;

pub trait LaunchdManager {
    fn ensure_watch(
        &self,
        name: &str,
        limit: u16,
        domain: Domain,
        cpulimit_bin: &str,
    ) -> Result<String>;
    fn remove_watch(&self, name: &str, domain: Domain) -> Result<()>;
}

#[derive(Debug, Clone)]
pub struct RealLaunchdManager {
    pub label_prefix: String,
    pub launch_agents_dir: PathBuf,
}

impl RealLaunchdManager {
    fn label(&self, name: &str) -> String {
        watch_label(&self.label_prefix, name)
    }

    fn plist_path(&self, name: &str, domain: Domain) -> PathBuf {
        match domain {
            Domain::User => self
                .launch_agents_dir
                .join(format!("{}.plist", self.label(name))),
            Domain::System => {
                PathBuf::from(format!("/Library/LaunchDaemons/{}.plist", self.label(name)))
            }
        }
    }

    fn write_plist(
        &self,
        path: &PathBuf,
        label: &str,
        name: &str,
        limit: u16,
        cpulimit_bin: &str,
    ) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create dir {}", parent.display()))?;
        }
        let content = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n  <key>Label</key><string>{label}</string>\n  <key>ProgramArguments</key>\n  <array>\n    <string>{cpulimit_bin}</string>\n    <string>-e</string><string>{name}</string>\n    <string>-l</string><string>{limit}</string>\n    <string>-i</string>\n  </array>\n  <key>RunAtLoad</key><true/>\n  <key>KeepAlive</key><true/>\n</dict>\n</plist>\n"
        );
        fs::write(path, content).with_context(|| format!("write plist {}", path.display()))?;
        Ok(())
    }
}

impl LaunchdManager for RealLaunchdManager {
    fn ensure_watch(
        &self,
        name: &str,
        limit: u16,
        domain: Domain,
        cpulimit_bin: &str,
    ) -> Result<String> {
        let label = self.label(name);
        let plist = self.plist_path(name, domain);
        self.write_plist(&plist, &label, name, limit, cpulimit_bin)?;

        if env::var("CPULIMIT_TOP_DISABLE_LAUNCHD").ok().as_deref() == Some("1") {
            return Ok(label);
        }

        let mut cmd = Command::new("launchctl");
        match domain {
            Domain::User => {
                let uid = unsafe { libc::geteuid() };
                cmd.args([
                    "bootstrap",
                    &format!("gui/{uid}"),
                    plist.to_string_lossy().as_ref(),
                ]);
            }
            Domain::System => {
                cmd.args(["bootstrap", "system", plist.to_string_lossy().as_ref()]);
            }
        }
        let status = cmd
            .status()
            .context("launchctl bootstrap failed to execute")?;
        if !status.success() {
            return Err(anyhow!("launchctl bootstrap failed for {}", label));
        }
        Ok(label)
    }

    fn remove_watch(&self, name: &str, domain: Domain) -> Result<()> {
        let label = self.label(name);
        let plist = self.plist_path(name, domain);

        if env::var("CPULIMIT_TOP_DISABLE_LAUNCHD").ok().as_deref() != Some("1") {
            let mut cmd = Command::new("launchctl");
            match domain {
                Domain::User => {
                    let uid = unsafe { libc::geteuid() };
                    cmd.args(["bootout", &format!("gui/{uid}/{label}")]);
                }
                Domain::System => {
                    cmd.args(["bootout", &format!("system/{label}")]);
                }
            }
            // Bootout is best-effort for cleanup. Suppress noisy "No such process" output.
            cmd.stdout(Stdio::null()).stderr(Stdio::null());
            let _ = cmd.status();
        }

        if plist.exists() {
            fs::remove_file(&plist).with_context(|| format!("remove {}", plist.display()))?;
        }
        Ok(())
    }
}

pub fn watch_label(label_prefix: &str, name: &str) -> String {
    let mut normalized = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() || c == '-' {
            normalized.push(c.to_ascii_lowercase());
        } else {
            normalized.push('-');
        }
    }
    while normalized.contains("--") {
        normalized = normalized.replace("--", "-");
    }
    normalized = normalized.trim_matches('-').to_string();
    if normalized.is_empty() {
        normalized = "proc".to_string();
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    name.hash(&mut hasher);
    let suffix = format!("{:08x}", hasher.finish() as u32);
    format!("{label_prefix}.{normalized}-{suffix}")
}

pub fn watch_loaded_status(label_prefix: &str, name: &str, domain: Domain) -> Option<bool> {
    if env::var("CPULIMIT_TOP_DISABLE_LAUNCHD").ok().as_deref() == Some("1") {
        return None;
    }

    let label = watch_label(label_prefix, name);
    let mut cmd = Command::new("launchctl");
    match domain {
        Domain::User => {
            let uid = unsafe { libc::geteuid() };
            cmd.args(["print", &format!("gui/{uid}/{label}")]);
        }
        Domain::System => {
            cmd.args(["print", &format!("system/{label}")]);
        }
    }
    cmd.stdout(Stdio::null()).stderr(Stdio::null());
    cmd.status().ok().map(|s| s.success())
}

#[derive(Debug, Clone)]
pub struct NoopLaunchdManager {
    pub label_prefix: String,
}

impl LaunchdManager for NoopLaunchdManager {
    fn ensure_watch(
        &self,
        name: &str,
        _limit: u16,
        _domain: Domain,
        _cpulimit_bin: &str,
    ) -> Result<String> {
        Ok(format!("{}.{}", self.label_prefix, name))
    }

    fn remove_watch(&self, _name: &str, _domain: Domain) -> Result<()> {
        Ok(())
    }
}
