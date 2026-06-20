use std::env;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow};

use crate::model::Domain;

pub trait LaunchdManager {
    fn ensure_agent(
        &self,
        domain: Domain,
        cpuguard_bin: &str,
        cpulimit_bin: &str,
        config_dir: &str,
    ) -> Result<String>;
    fn remove_agent(&self, domain: Domain) -> Result<()>;
    fn clean_managed_watches(&self, domain: Domain) -> Result<usize>;
}

#[derive(Debug, Clone)]
pub struct RealLaunchdManager {
    pub label_prefix: String,
    pub launch_agents_dir: PathBuf,
    pub launch_daemons_dir: PathBuf,
}

impl RealLaunchdManager {
    pub fn agent_label(&self) -> String {
        format!("{}.agent", self.label_prefix)
    }

    fn agent_plist_path(&self, domain: Domain) -> PathBuf {
        match domain {
            Domain::User => self
                .launch_agents_dir
                .join(format!("{}.plist", self.agent_label())),
            Domain::System => self
                .launch_daemons_dir
                .join(format!("{}.plist", self.agent_label())),
        }
    }

    fn domain_plist_dir(&self, domain: Domain) -> PathBuf {
        match domain {
            Domain::User => self.launch_agents_dir.clone(),
            Domain::System => self.launch_daemons_dir.clone(),
        }
    }

    fn bootout_label_strict(&self, label: &str, domain: Domain) -> Result<()> {
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
        let output = cmd
            .output()
            .context("launchctl bootout failed to execute")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!(
                "launchctl bootout failed for {label}: {}",
                stderr.trim()
            ));
        }
        Ok(())
    }

    fn bootout_label_allow_missing(&self, label: &str, domain: Domain) -> Result<()> {
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
        let output = cmd
            .output()
            .context("launchctl bootout failed to execute")?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("No such process")
            || stderr.contains("Could not find service")
            || stderr.contains("does not exist")
        {
            return Ok(());
        }
        Err(anyhow!("launchctl bootout failed for {label}: {stderr}"))
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
        cpuguard_bin: &str,
        cpulimit_bin: &str,
        config_dir: &str,
        domain: Domain,
    ) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create dir {}", parent.display()))?;
        }
        let content = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n  <key>Label</key><string>{label}</string>\n  <key>ProgramArguments</key>\n  <array>\n    <string>{cpuguard_bin}</string>\n    <string>--domain</string><string>{domain}</string>\n    <string>__agent</string>\n    <string>--config-dir</string><string>{config_dir}</string>\n    <string>--cpulimit-bin</string><string>{cpulimit_bin}</string>\n  </array>\n  <key>RunAtLoad</key><true/>\n  <key>KeepAlive</key><true/>\n  <key>ThrottleInterval</key><integer>30</integer>\n</dict>\n</plist>\n"
        );
        let tmp = path.with_extension("plist.tmp");
        fs::write(&tmp, content).with_context(|| format!("write plist {}", tmp.display()))?;
        fs::rename(&tmp, path)
            .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
        Ok(())
    }

    fn bootstrap_agent(&self, domain: Domain, plist: &std::path::Path) -> Result<()> {
        if env::var("CPULIMIT_TOP_DISABLE_LAUNCHD").ok().as_deref() == Some("1") {
            return Ok(());
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
        let output = cmd
            .output()
            .context("launchctl bootstrap failed to execute")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!("launchctl bootstrap failed: {}", stderr.trim()));
        }
        Ok(())
    }

    fn restore_plist(path: &std::path::Path, previous: Option<&[u8]>) -> Result<()> {
        match previous {
            Some(bytes) => fs::write(path, bytes)
                .with_context(|| format!("restore plist {}", path.display()))?,
            None if path.exists() => {
                fs::remove_file(path)
                    .with_context(|| format!("remove failed plist {}", path.display()))?;
            }
            None => {}
        }
        Ok(())
    }
}

impl LaunchdManager for RealLaunchdManager {
    fn ensure_agent(
        &self,
        domain: Domain,
        cpuguard_bin: &str,
        cpulimit_bin: &str,
        config_dir: &str,
    ) -> Result<String> {
        let label = self.agent_label();
        let plist = self.agent_plist_path(domain);
        let was_loaded = agent_loaded_status(&self.label_prefix, domain) == Some(true);
        let previous_plist = fs::read(&plist).ok();
        self.write_plist(
            &plist,
            &label,
            cpuguard_bin,
            cpulimit_bin,
            config_dir,
            domain,
        )?;

        if env::var("CPULIMIT_TOP_DISABLE_LAUNCHD").ok().as_deref() == Some("1") {
            return Ok(label);
        }
        if was_loaded && let Err(err) = self.bootout_label_strict(&label, domain) {
            let _ = Self::restore_plist(&plist, previous_plist.as_deref());
            return Err(err.context(format!("refresh launchd agent {label}")));
        }
        if let Err(err) = self.bootstrap_agent(domain, &plist) {
            let _ = Self::restore_plist(&plist, previous_plist.as_deref());
            if was_loaded {
                let _ = self.bootstrap_agent(domain, &plist);
            }
            return Err(err.context(format!("refresh launchd agent {label}")));
        }

        let _ = self.clean_managed_watches(domain);
        Ok(label)
    }

    fn remove_agent(&self, domain: Domain) -> Result<()> {
        let label = self.agent_label();
        let plist = self.agent_plist_path(domain);
        let previous_plist = fs::read(&plist).ok();

        if plist.exists() {
            fs::remove_file(&plist).with_context(|| format!("remove {}", plist.display()))?;
        }
        if let Err(err) = self.bootout_label_allow_missing(&label, domain) {
            let _ = Self::restore_plist(&plist, previous_plist.as_deref());
            return Err(err.context(format!("remove launchd agent {label}")));
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
            if label == self.agent_label() {
                continue;
            }
            if label_loaded_status(&label, domain) == Some(true) {
                self.bootout_label_strict(&label, domain)?;
            }
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

pub fn agent_loaded_status(label_prefix: &str, domain: Domain) -> Option<bool> {
    let label = format!("{label_prefix}.agent");
    label_loaded_status(&label, domain)
}

fn label_loaded_status(label: &str, domain: Domain) -> Option<bool> {
    if env::var("CPULIMIT_TOP_DISABLE_LAUNCHD").ok().as_deref() == Some("1") {
        return None;
    }

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
    fn generated_plist_uses_single_cpuguard_agent() {
        let dir = tempdir().expect("tempdir");
        let manager = RealLaunchdManager {
            label_prefix: "com.cpuguard".to_string(),
            launch_agents_dir: dir.path().to_path_buf(),
            launch_daemons_dir: dir.path().to_path_buf(),
        };
        let plist = dir.path().join("demo.plist");

        manager
            .write_plist(
                &plist,
                "com.cpuguard.agent",
                "/usr/local/bin/cpuguard",
                "/opt/homebrew/bin/cpulimit",
                "/Users/demo/.config/cpuguard",
                Domain::User,
            )
            .expect("write plist");

        let text = fs::read_to_string(plist).expect("read plist");
        assert!(text.contains("/usr/local/bin/cpuguard"));
        assert!(text.contains("__agent"));
        assert!(text.contains("--config-dir"));
        assert!(text.contains("--cpulimit-bin"));
        assert!(text.contains("ThrottleInterval"));
        assert!(!text.contains("__watch-runner"));
        assert!(!text.contains("<string>-e</string>"));
    }

    #[test]
    fn managed_label_from_plist_requires_cpuguard_prefix() {
        let dir = tempdir().expect("tempdir");
        let manager = RealLaunchdManager {
            label_prefix: "com.cpuguard".to_string(),
            launch_agents_dir: dir.path().to_path_buf(),
            launch_daemons_dir: dir.path().to_path_buf(),
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

    #[test]
    fn clean_managed_watches_keeps_single_agent_plist() {
        let dir = tempdir().expect("tempdir");
        let manager = RealLaunchdManager {
            label_prefix: "com.cpuguard".to_string(),
            launch_agents_dir: dir.path().to_path_buf(),
            launch_daemons_dir: dir.path().to_path_buf(),
        };
        let agent = dir.path().join("com.cpuguard.agent.plist");
        let legacy = dir.path().join("com.cpuguard.legacy.plist");
        fs::write(
            &agent,
            "<plist><dict><key>Label</key><string>com.cpuguard.agent</string></dict></plist>",
        )
        .expect("write agent");
        fs::write(
            &legacy,
            "<plist><dict><key>Label</key><string>com.cpuguard.legacy</string></dict></plist>",
        )
        .expect("write legacy");

        let removed = manager
            .clean_managed_watches(Domain::User)
            .expect("clean legacy");

        assert_eq!(removed, 1);
        assert!(agent.exists());
        assert!(!legacy.exists());
    }

    #[test]
    fn restore_plist_restores_previous_contents() {
        let dir = tempdir().expect("tempdir");
        let plist = dir.path().join("com.cpuguard.agent.plist");
        fs::write(&plist, "new broken plist").expect("write plist");

        RealLaunchdManager::restore_plist(&plist, Some(b"previous good plist"))
            .expect("restore plist");

        let text = fs::read_to_string(plist).expect("read plist");
        assert_eq!(text, "previous good plist");
    }
}

#[derive(Debug, Clone)]
pub struct NoopLaunchdManager {
    pub label_prefix: String,
}

impl LaunchdManager for NoopLaunchdManager {
    fn ensure_agent(
        &self,
        _domain: Domain,
        _cpuguard_bin: &str,
        _cpulimit_bin: &str,
        _config_dir: &str,
    ) -> Result<String> {
        Ok(format!("{}.agent", self.label_prefix))
    }

    fn remove_agent(&self, _domain: Domain) -> Result<()> {
        Ok(())
    }

    fn clean_managed_watches(&self, _domain: Domain) -> Result<usize> {
        Ok(0)
    }
}
