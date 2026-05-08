use std::collections::HashSet;
use std::path::Path;

use anyhow::{Result, bail};

use crate::app::conflict::has_external_adhoc_conflict;
use crate::infra::cpulimit::CpulimitExecutor;
use crate::infra::launchd::LaunchdManager;
use crate::model::{Domain, ManagedMode, ManagedTarget, Rule};
use crate::store;

pub struct Service<E: CpulimitExecutor, L: LaunchdManager> {
    pub executor: E,
    pub launchd: L,
}

pub struct WatchRuntime<'a> {
    pub cpuguard_bin: &'a str,
    pub cpulimit_bin: &'a str,
}

impl<E: CpulimitExecutor, L: LaunchdManager> Service<E, L> {
    pub fn watch(
        &self,
        rules_file: &Path,
        state_file: &Path,
        name: &str,
        limit: u16,
        domain: Domain,
        runtime: WatchRuntime<'_>,
    ) -> Result<Rule> {
        self.executor.ensure_available()?;
        let state = store::load_state(state_file)?;

        let managed_adhoc: Vec<_> = state
            .instances
            .iter()
            .filter(|i| i.mode == ManagedMode::Adhoc)
            .filter(|i| match &i.target {
                ManagedTarget::Pid(pid) => store_process_name(*pid).is_some_and(|n| n == name),
                ManagedTarget::Name(n) => n == name,
            })
            .map(|i| i.cpulimit_pid)
            .collect();

        for pid in &managed_adhoc {
            let _ = self.executor.stop_instance(*pid);
            let _ = store::remove_instance_by_pid(state_file, *pid);
        }

        let managed_set: HashSet<u32> = managed_adhoc.into_iter().collect();
        if has_external_adhoc_conflict(name, &managed_set)? {
            bail!("external cpulimit conflict detected for {name}. stop it first or use --once");
        }

        let _ = self.launchd.remove_watch(name, domain);
        let rule = store::upsert_rule(rules_file, name, limit, domain)?;
        let _label = self.launchd.ensure_watch(
            name,
            limit,
            domain,
            runtime.cpuguard_bin,
            runtime.cpulimit_bin,
        )?;
        Ok(rule)
    }

    pub fn unwatch(&self, rules_file: &Path, name: &str, domain: Domain) -> Result<bool> {
        self.launchd.remove_watch(name, domain)?;
        let removed = store::remove_rule(rules_file, name)?;
        Ok(removed)
    }

    pub fn limit_once(
        &self,
        state_file: &Path,
        target_pid: u32,
        limit: u16,
        domain: Domain,
    ) -> Result<u32> {
        self.executor.ensure_available()?;
        let cpulimit_pid = self.executor.start_adhoc(target_pid, limit)?;
        store::add_adhoc_instance(state_file, cpulimit_pid, target_pid, domain)?;
        Ok(cpulimit_pid)
    }

    pub fn clean_managed_only(&self, state_file: &Path) -> Result<usize> {
        let state = store::load_state(state_file)?;
        let mut stopped = 0usize;
        for instance in state.instances {
            if self.executor.stop_instance(instance.cpulimit_pid).is_ok() {
                stopped += 1;
            }
        }
        let _ = store::clear_all_instances(state_file)?;
        Ok(stopped)
    }

    pub fn clean_all(
        &self,
        rules_file: &Path,
        state_file: &Path,
        domain: Domain,
    ) -> Result<(usize, usize)> {
        let rules = store::load_rules(rules_file)?;
        let mut removed_rules = 0usize;
        for r in rules.rules {
            let _ = self.launchd.remove_watch(&r.name, r.domain);
            removed_rules += 1;
        }
        removed_rules += self.launchd.clean_managed_watches(domain)?;
        store::save_rules(rules_file, &crate::model::RulesFile::default())?;
        let stopped = self.clean_managed_only(state_file)?;
        Ok((removed_rules, stopped))
    }
}

fn store_process_name(pid: u32) -> Option<String> {
    let output = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if raw.is_empty() {
        return None;
    }
    Some(
        std::path::Path::new(&raw)
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or(raw),
    )
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::path::PathBuf;

    use tempfile::tempdir;

    use crate::infra::cpulimit::CpulimitExecutor;
    use crate::infra::launchd::LaunchdManager;
    use crate::model::Domain;

    use super::{Service, WatchRuntime};

    #[derive(Default)]
    struct FakeExecutor {
        started: RefCell<Vec<(u32, u16)>>,
        stopped: RefCell<Vec<u32>>,
        next_pid: RefCell<VecDeque<u32>>,
    }

    impl FakeExecutor {
        fn with_next_pids(pids: Vec<u32>) -> Self {
            Self {
                next_pid: RefCell::new(VecDeque::from(pids)),
                ..Self::default()
            }
        }
    }

    impl CpulimitExecutor for FakeExecutor {
        fn ensure_available(&self) -> anyhow::Result<()> {
            Ok(())
        }

        fn start_adhoc(&self, target_pid: u32, limit: u16) -> anyhow::Result<u32> {
            self.started.borrow_mut().push((target_pid, limit));
            Ok(self.next_pid.borrow_mut().pop_front().unwrap_or(9999))
        }

        fn run_for_target(&self, target_pid: u32, limit: u16) -> anyhow::Result<()> {
            self.started.borrow_mut().push((target_pid, limit));
            Ok(())
        }

        fn stop_instance(&self, cpulimit_pid: u32) -> anyhow::Result<()> {
            self.stopped.borrow_mut().push(cpulimit_pid);
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeLaunchd {
        ensured: RefCell<Vec<(String, u16, Domain, String)>>,
        removed: RefCell<Vec<(String, Domain)>>,
    }

    impl LaunchdManager for FakeLaunchd {
        fn ensure_watch(
            &self,
            name: &str,
            limit: u16,
            domain: Domain,
            cpuguard_bin: &str,
            cpulimit_bin: &str,
        ) -> anyhow::Result<String> {
            self.ensured.borrow_mut().push((
                name.to_string(),
                limit,
                domain,
                format!("{cpuguard_bin}:{cpulimit_bin}"),
            ));
            Ok(format!("com.cpuguard.{name}"))
        }

        fn remove_watch(&self, name: &str, domain: Domain) -> anyhow::Result<()> {
            self.removed.borrow_mut().push((name.to_string(), domain));
            Ok(())
        }

        fn clean_managed_watches(&self, domain: Domain) -> anyhow::Result<usize> {
            self.removed
                .borrow_mut()
                .push(("*managed*".to_string(), domain));
            Ok(0)
        }
    }

    #[test]
    fn top_once_records_adhoc_instance() {
        let dir = tempdir().expect("tempdir");
        let state_file = PathBuf::from(dir.path()).join("state.json");

        let executor = FakeExecutor::with_next_pids(vec![4321]);
        let launchd = FakeLaunchd::default();
        let service = Service { executor, launchd };

        let pid = service
            .limit_once(&state_file, 1234, 20, Domain::User)
            .expect("limit once should succeed");

        assert_eq!(pid, 4321);
        let state = crate::store::load_state(&state_file).expect("load state");
        assert_eq!(state.instances.len(), 1);
        assert_eq!(state.instances[0].cpulimit_pid, 4321);
    }

    #[test]
    fn watch_writes_rule_and_calls_launchd() {
        let dir = tempdir().expect("tempdir");
        let rules_file = PathBuf::from(dir.path()).join("rules.toml");
        let state_file = PathBuf::from(dir.path()).join("state.json");

        let executor = FakeExecutor::default();
        let launchd = FakeLaunchd::default();
        let service = Service { executor, launchd };

        let rule = service
            .watch(
                &rules_file,
                &state_file,
                "demo-proc",
                20,
                Domain::User,
                WatchRuntime {
                    cpuguard_bin: "/mock/cpuguard",
                    cpulimit_bin: "/mock/cpulimit",
                },
            )
            .expect("watch should succeed");

        assert_eq!(rule.name, "demo-proc");
        let rules = crate::store::load_rules(&rules_file).expect("load rules");
        assert_eq!(rules.rules.len(), 1);
        assert_eq!(rules.rules[0].name, "demo-proc");
    }

    #[test]
    fn clean_all_removes_rules_and_instances() {
        let dir = tempdir().expect("tempdir");
        let rules_file = PathBuf::from(dir.path()).join("rules.toml");
        let state_file = PathBuf::from(dir.path()).join("state.json");

        crate::store::upsert_rule(&rules_file, "demo-proc", 20, Domain::User).expect("rule");
        crate::store::add_adhoc_instance(&state_file, 4321, std::process::id(), Domain::User)
            .expect("state");

        let executor = FakeExecutor::default();
        let launchd = FakeLaunchd::default();
        let service = Service { executor, launchd };

        let (rules_removed, instances_stopped) = service
            .clean_all(&rules_file, &state_file, Domain::User)
            .expect("clean all");
        assert_eq!(rules_removed, 1);
        assert_eq!(instances_stopped, 1);

        let rules = crate::store::load_rules(&rules_file).expect("load rules");
        assert!(rules.rules.is_empty());
        let state = crate::store::load_state(&state_file).expect("load state");
        assert!(state.instances.is_empty());
    }
}
