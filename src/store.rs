use std::fs;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::Local;

use crate::model::{
    Domain, ManagedInstance, ManagedMode, ManagedTarget, Rule, RulesFile, StateFile,
};

pub fn ensure_parent_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create dir {}", parent.display()))?;
    }
    Ok(())
}

pub fn load_rules(path: &Path) -> Result<RulesFile> {
    if !path.exists() {
        return Ok(RulesFile::default());
    }
    let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let data: RulesFile =
        toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    Ok(data)
}

pub fn save_rules(path: &Path, rules: &RulesFile) -> Result<()> {
    ensure_parent_dir(path)?;
    let text = toml::to_string_pretty(rules).context("serialize rules")?;
    atomic_write(path, text.as_bytes())
}

pub fn upsert_rule(path: &Path, name: &str, limit: u16, domain: Domain) -> Result<Rule> {
    let mut rules = load_rules(path)?;
    let now = Local::now();
    if let Some(existing) = rules.rules.iter_mut().find(|r| r.name == name) {
        existing.limit = limit;
        existing.domain = domain;
        existing.updated_at = now;
        let rule = existing.clone();
        save_rules(path, &rules)?;
        return Ok(rule);
    }

    let rule = Rule {
        name: name.to_string(),
        limit,
        domain,
        created_at: now,
        updated_at: now,
    };
    rules.rules.push(rule.clone());
    save_rules(path, &rules)?;
    Ok(rule)
}

pub fn remove_rule(path: &Path, name: &str) -> Result<bool> {
    let mut rules = load_rules(path)?;
    let before = rules.rules.len();
    rules.rules.retain(|r| r.name != name);
    let changed = before != rules.rules.len();
    if changed {
        save_rules(path, &rules)?;
    }
    Ok(changed)
}

pub fn load_state(path: &Path) -> Result<StateFile> {
    if !path.exists() {
        return Ok(StateFile::default());
    }
    let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let data: StateFile =
        serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    Ok(data)
}

pub fn save_state(path: &Path, state: &StateFile) -> Result<()> {
    ensure_parent_dir(path)?;
    let text = serde_json::to_string_pretty(state).context("serialize state")?;
    atomic_write(path, text.as_bytes())
}

pub fn add_adhoc_instance(
    path: &Path,
    cpulimit_pid: u32,
    target_pid: u32,
    domain: Domain,
) -> Result<()> {
    let mut state = load_state(path)?;
    state.instances.push(ManagedInstance {
        id: format!(
            "ins_{}",
            Local::now()
                .timestamp_nanos_opt()
                .unwrap_or_default()
                .unsigned_abs()
        ),
        mode: ManagedMode::Adhoc,
        cpulimit_pid,
        target: ManagedTarget::Pid(target_pid),
        domain,
        started_at: Local::now(),
        owner_label: None,
    });
    save_state(path, &state)
}

pub fn remove_instance_by_pid(path: &Path, cpulimit_pid: u32) -> Result<bool> {
    let mut state = load_state(path)?;
    let before = state.instances.len();
    state.instances.retain(|i| i.cpulimit_pid != cpulimit_pid);
    let changed = before != state.instances.len();
    if changed {
        save_state(path, &state)?;
    }
    Ok(changed)
}

pub fn list_adhoc_instances_by_target_name(
    path: &Path,
    name: &str,
) -> Result<Vec<ManagedInstance>> {
    let state = load_state(path)?;
    let items = state
        .instances
        .into_iter()
        .filter(|i| i.mode == ManagedMode::Adhoc)
        .filter(|i| match i.target {
            ManagedTarget::Pid(pid) => process_name_by_pid(pid).map(|n| n == name).unwrap_or(false),
            ManagedTarget::Name(ref n) => n == name,
        })
        .collect();
    Ok(items)
}

pub fn clear_all_instances(path: &Path) -> Result<usize> {
    let state = load_state(path)?;
    let count = state.instances.len();
    save_state(path, &StateFile::default())?;
    Ok(count)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    let mut f = fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
    f.write_all(bytes)
        .with_context(|| format!("write {}", tmp.display()))?;
    f.sync_all()
        .with_context(|| format!("sync {}", tmp.display()))?;
    fs::rename(&tmp, path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

fn process_name_by_pid(pid: u32) -> Option<String> {
    let output = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() {
        None
    } else {
        Some(
            std::path::Path::new(&text)
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or(text),
        )
    }
}
