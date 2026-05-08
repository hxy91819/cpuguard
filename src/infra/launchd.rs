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
        cpuguard_bin: &str,
        cpulimit_bin: &str,
    ) -> Result<String>;
    fn remove_watch(&self, name: &str, domain: Domain) -> Result<()>;
    fn clean_managed_watches(&self, domain: Domain) -> Result<usize>;
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

    fn domain_plist_dir(&self, domain: Domain) -> PathBuf {
        match domain {
            Domain::User => self.launch_agents_dir.clone(),
            Domain::System => PathBuf::from("/Library/LaunchDaemons"),
        }
    }

    fn bootout_label(&self, label: &str, domain: Domain) -> Result<()> {
        if env::var("CPULIMIT_TOP_DISABLE_LAUNCHD").ok().as_deref() == Some("1") {
            return Ok(());
        }

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
        cmd.stdout(Stdio::null()).stderr(Stdio::null());
        let _ = cmd.status();
        Ok(())
    }

    fn managed_label_from_plist(&self, path: &std::path::Path) -> Option<String> {
        let text = fs::read_to_string(path).ok()?;
        let (_, rest) = text.split_once("<key>Label</key><string>")?;
        let (label, _) = rest.split_once("</string>")?;
        label
            .starts_with(&format!("{}.", self.label_prefix))
            .then(|| label.to_string())
    }

    fn write_plist(
        &self,
        path: &PathBuf,
        label: &str,
        name: &str,
        limit: u16,
        cpuguard_bin: &str,
        cpulimit_bin: &str,
    ) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create dir {}", parent.display()))?;
        }
        let content = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n  <key>Label</key><string>{label}</string>\n  <key>ProgramArguments</key>\n  <array>\n    <string>{cpuguard_bin}</string>\n    <string>__watch-runner</string>\n    <string>--name</string><string>{name}</string>\n    <string>--limit</string><string>{limit}</string>\n    <string>--cpulimit-bin</string><string>{cpulimit_bin}</string>\n  </array>\n  <key>RunAtLoad</key><true/>\n  <key>KeepAlive</key><true/>\n  <key>ThrottleInterval</key><integer>10</integer>\n</dict>\n</plist>\n"
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
        cpuguard_bin: &str,
        cpulimit_bin: &str,
    ) -> Result<String> {
        let label = self.label(name);
        let plist = self.plist_path(name, domain);
        self.remove_watch(name, domain)?;
        self.write_plist(&plist, &label, name, limit, cpuguard_bin, cpulimit_bin)?;

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

        self.bootout_label(&label, domain)?;

        if plist.exists() {
            fs::remove_file(&plist).with_context(|| format!("remove {}", plist.display()))?;
        }
        Ok(())
    }

    fn clean_managed_watches(&self, domain: Domain) -> Result<usize> {
        let dir = self.domain_plist_dir(domain);
        if !dir.exists() {
            return Ok(0);
        }

        let mut removed = 0usize;
        for entry in fs::read_dir(&dir).with_context(|| format!("read dir {}", dir.display()))? {
            let path = entry?.path();
            if path.extension().and_then(|s| s.to_str()) != Some("plist") {
                continue;
            }
            let Some(label) = self.managed_label_from_plist(&path) else {
                continue;
            };
            self.bootout_label(&label, domain)?;
            fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
            removed += 1;
        }
        Ok(removed)
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

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn generated_plist_uses_cpuguard_runner() {
        let dir = tempdir().expect("tempdir");
        let manager = RealLaunchdManager {
            label_prefix: "com.cpuguard".to_string(),
            launch_agents_dir: dir.path().to_path_buf(),
        };
        let plist = dir.path().join("demo.plist");

        manager
            .write_plist(
                &plist,
                "com.cpuguard.demo",
                "demo",
                20,
                "/usr/local/bin/cpuguard",
                "/opt/homebrew/bin/cpulimit",
            )
            .expect("write plist");

        let text = fs::read_to_string(plist).expect("read plist");
        assert!(text.contains("/usr/local/bin/cpuguard"));
        assert!(text.contains("__watch-runner"));
        assert!(text.contains("--cpulimit-bin"));
        assert!(text.contains("ThrottleInterval"));
        assert!(!text.contains("<string>-e</string>"));
    }

    #[test]
    fn managed_label_from_plist_requires_cpuguard_prefix() {
        let dir = tempdir().expect("tempdir");
        let manager = RealLaunchdManager {
            label_prefix: "com.cpuguard".to_string(),
            launch_agents_dir: dir.path().to_path_buf(),
        };
        let managed = dir.path().join("managed.plist");
        let external = dir.path().join("external.plist");
        fs::write(
            &managed,
            "<plist><dict><key>Label</key><string>com.cpuguard.demo</string></dict></plist>",
        )
        .expect("write managed");
        fs::write(
            &external,
            "<plist><dict><key>Label</key><string>com.example.demo</string></dict></plist>",
        )
        .expect("write external");

        assert_eq!(
            manager.managed_label_from_plist(&managed).as_deref(),
            Some("com.cpuguard.demo")
        );
        assert_eq!(manager.managed_label_from_plist(&external), None);
    }
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
        _cpuguard_bin: &str,
        _cpulimit_bin: &str,
    ) -> Result<String> {
        Ok(format!("{}.{}", self.label_prefix, name))
    }

    fn remove_watch(&self, _name: &str, _domain: Domain) -> Result<()> {
        Ok(())
    }

    fn clean_managed_watches(&self, _domain: Domain) -> Result<usize> {
        Ok(0)
    }
}
