use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::app::conflict::has_external_adhoc_conflict;
use crate::infra::cpulimit::CpulimitExecutor;
use crate::infra::launchd::LaunchdManager;
use crate::model::{Domain, ManagedMode, ManagedTarget, Rule};
use crate::store::{self, RuleUpdate};

pub struct Service<E: CpulimitExecutor, L: LaunchdManager> {
    pub executor: E,
    pub launchd: L,
}

pub struct WatchRuntime<'a> {
    pub cpuguard_bin: &'a str,
    pub cpulimit_bin: &'a str,
    pub config_dir: &'a str,
}

pub struct WatchOptions<'a> {
    pub name: &'a str,
    pub limit: u16,
    pub trigger_cpu: f32,
    pub release_cpu: f32,
    pub args_contains: Option<String>,
}

impl<E: CpulimitExecutor, L: LaunchdManager> Service<E, L> {
    pub fn watch(
        &self,
        rules_file: &Path,
        state_file: &Path,
        options: WatchOptions<'_>,
        domain: Domain,
        runtime: WatchRuntime<'_>,
    ) -> Result<Rule> {
        self.executor.ensure_available()?;
        let state = store::load_state(state_file)?;

        let managed_cpulimit_set: HashSet<u32> =
            state.instances.iter().map(|i| i.cpulimit_pid).collect();

        let managed_adhoc_to_stop: Vec<_> = state
            .instances
            .iter()
            .filter(|i| i.mode == ManagedMode::Adhoc)
            .filter(|i| i.domain == domain)
            .filter(|i| match &i.target {
                ManagedTarget::Pid(pid) => target_pid_matches_options(*pid, &options),
                ManagedTarget::Name(n) => n == options.name,
            })
            .map(|i| i.cpulimit_pid)
            .collect();
        if managed_adhoc_to_stop.len() > 1 {
            bail!(
                "multiple managed ad-hoc cpulimit instances match {}. run clean or stop them first",
                options.name
            );
        }

        if has_external_adhoc_conflict(
            options.name,
            options.args_contains.as_deref(),
            &managed_cpulimit_set,
        )? {
            bail!(
                "external cpulimit conflict detected for {}. stop it first or use --once",
                options.name
            );
        }

        let previous_rules = store::load_rules(rules_file)?;
        let rule = store::upsert_rule(
            rules_file,
            RuleUpdate {
                name: options.name.to_string(),
                limit: options.limit,
                trigger_cpu: options.trigger_cpu,
                release_cpu: options.release_cpu,
                args_contains: options.args_contains,
                domain,
            },
        )?;
        if let Err(err) = self.launchd.ensure_agent(
            domain,
            runtime.cpuguard_bin,
            runtime.cpulimit_bin,
            runtime.config_dir,
        ) {
            let _ = store::save_rules(rules_file, &previous_rules);
            return Err(err);
        }
        for pid in &managed_adhoc_to_stop {
            if let Err(err) = self
                .executor
                .stop_instance(*pid)
                .with_context(|| format!("stop managed ad-hoc cpulimit {pid}"))
            {
                let _ = store::save_rules(rules_file, &previous_rules);
                return Err(err);
            }
            if let Err(err) = store::remove_instance_by_pid(state_file, *pid)
                .with_context(|| format!("remove managed ad-hoc state {pid}"))
            {
                let _ = store::save_rules(rules_file, &previous_rules);
                return Err(err);
            }
        }
        Ok(rule)
    }

    pub fn unwatch(
        &self,
        rules_file: &Path,
        state_file: &Path,
        name: &str,
        domain: Domain,
    ) -> Result<bool> {
        let rules = store::load_rules(rules_file)?;
        let exists = rules
            .rules
            .iter()
            .any(|rule| rule.name == name && rule.domain == domain);
        let is_last_domain_rule = rules
            .rules
            .iter()
            .filter(|rule| rule.domain == domain)
            .count()
            == 1;
        if exists {
            let _ = self.stop_watch_instances_for_rule(state_file, name, domain)?;
        }
        let previous_rules = rules.clone();
        let removed = store::remove_rule(rules_file, name, domain)?;
        if is_last_domain_rule
            && removed
            && let Err(err) = self.launchd.remove_agent(domain)
        {
            let _ = store::save_rules(rules_file, &previous_rules);
            return Err(err);
        }
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

    pub fn clean_managed_only(&self, state_file: &Path, domain: Domain) -> Result<usize> {
        let mut state = store::load_state(state_file)?;
        let mut stopped = 0usize;
        let mut stopped_pids = HashSet::new();
        let mut failed_pids = Vec::new();
        for instance in state.instances.iter().filter(|i| i.domain == domain) {
            match self.executor.stop_instance(instance.cpulimit_pid) {
                Ok(()) => {
                    stopped += 1;
                    stopped_pids.insert(instance.cpulimit_pid);
                }
                Err(_) => failed_pids.push(instance.cpulimit_pid),
            }
        }
        if !stopped_pids.is_empty() {
            state
                .instances
                .retain(|i| i.domain != domain || !stopped_pids.contains(&i.cpulimit_pid));
            state.version = 2;
            store::save_state(state_file, &state)?;
        }
        if !failed_pids.is_empty() {
            bail!(
                "failed to stop managed cpulimit instance(s): {}",
                failed_pids
                    .iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        Ok(stopped)
    }

    pub fn clean_all(
        &self,
        rules_file: &Path,
        state_file: &Path,
        domain: Domain,
    ) -> Result<(usize, usize)> {
        let previous_rules = store::load_rules(rules_file)?;
        let stopped = self.clean_managed_only(state_file, domain)?;
        let mut removed_rules = store::remove_rules_by_domain(rules_file, domain)?;
        if let Err(err) = self.launchd.remove_agent(domain) {
            let _ = store::save_rules(rules_file, &previous_rules);
            return Err(err);
        }
        removed_rules += self.launchd.clean_managed_watches(domain)?;
        Ok((removed_rules, stopped))
    }

    fn stop_watch_instances_for_rule(
        &self,
        state_file: &Path,
        rule_name: &str,
        domain: Domain,
    ) -> Result<usize> {
        let mut state = store::load_state(state_file)?;
        let mut stopped = 0usize;
        let matching_pids = state
            .instances
            .iter()
            .filter(|i| {
                i.mode == ManagedMode::Watch
                    && i.domain == domain
                    && watch_instance_matches_rule_name(i, rule_name)
            })
            .map(|instance| instance.cpulimit_pid)
            .collect::<Vec<_>>();
        for cpulimit_pid in &matching_pids {
            self.executor
                .stop_instance(*cpulimit_pid)
                .with_context(|| format!("stop watch cpulimit {cpulimit_pid}"))?;
            stopped += 1;
        }
        state.instances.retain(|i| {
            !(i.mode == ManagedMode::Watch
                && i.domain == domain
                && watch_instance_matches_rule_name(i, rule_name))
        });
        state.version = 2;
        store::save_state(state_file, &state)?;
        Ok(stopped)
    }
}

fn watch_instance_matches_rule_name(
    instance: &crate::model::ManagedInstance,
    rule_name: &str,
) -> bool {
    match instance.rule_name.as_deref() {
        Some(name) => name == rule_name,
        None => match &instance.target {
            ManagedTarget::Pid(pid) => {
                store_process_name(*pid).is_some_and(|name| name == rule_name)
            }
            ManagedTarget::Name(name) => name == rule_name,
        },
    }
}

fn target_pid_matches_options(pid: u32, options: &WatchOptions<'_>) -> bool {
    store_process_name(pid).is_some_and(|name| name == options.name)
        && options.args_contains.as_deref().is_none_or(|needle| {
            store_process_args(pid)
                .as_deref()
                .is_some_and(|args| args.contains(needle))
        })
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

fn store_process_args(pid: u32) -> Option<String> {
    let output = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "args="])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!raw.is_empty()).then_some(raw)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::{HashSet, VecDeque};
    use std::path::PathBuf;

    use chrono::Local;
    use tempfile::tempdir;

    use crate::infra::cpulimit::CpulimitExecutor;
    use crate::infra::launchd::LaunchdManager;
    use crate::model::{
        DEFAULT_RELEASE_CPU, DEFAULT_TRIGGER_CPU, Domain, ManagedMode, ManagedTarget,
    };
    use crate::store::RuleUpdate;

    use super::{Service, WatchOptions, WatchRuntime, store_process_name};

    #[derive(Default)]
    struct FakeExecutor {
        started: RefCell<Vec<(u32, u16)>>,
        stopped: RefCell<Vec<u32>>,
        next_pid: RefCell<VecDeque<u32>>,
        fail_stop: RefCell<HashSet<u32>>,
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
            if self.fail_stop.borrow().contains(&cpulimit_pid) {
                anyhow::bail!("injected stop failure for {cpulimit_pid}");
            }
            self.stopped.borrow_mut().push(cpulimit_pid);
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeLaunchd {
        ensured: RefCell<Vec<(String, u16, Domain, String)>>,
        removed: RefCell<Vec<(String, Domain)>>,
        fail_ensure: RefCell<bool>,
        fail_remove: RefCell<bool>,
    }

    impl LaunchdManager for FakeLaunchd {
        fn ensure_agent(
            &self,
            domain: Domain,
            cpuguard_bin: &str,
            cpulimit_bin: &str,
            config_dir: &str,
        ) -> anyhow::Result<String> {
            if *self.fail_ensure.borrow() {
                anyhow::bail!("injected ensure failure");
            }
            self.ensured.borrow_mut().push((
                "agent".to_string(),
                0,
                domain,
                format!("{cpuguard_bin}:{cpulimit_bin}:{config_dir}"),
            ));
            Ok("com.cpuguard.agent".to_string())
        }

        fn remove_agent(&self, domain: Domain) -> anyhow::Result<()> {
            if *self.fail_remove.borrow() {
                anyhow::bail!("injected remove failure");
            }
            self.removed
                .borrow_mut()
                .push(("agent".to_string(), domain));
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
                WatchOptions {
                    name: "demo-proc",
                    limit: 20,
                    trigger_cpu: DEFAULT_TRIGGER_CPU,
                    release_cpu: DEFAULT_RELEASE_CPU,
                    args_contains: None,
                },
                Domain::User,
                WatchRuntime {
                    cpuguard_bin: "/mock/cpuguard",
                    cpulimit_bin: "/mock/cpulimit",
                    config_dir: "/mock/config",
                },
            )
            .expect("watch should succeed");

        assert_eq!(rule.name, "demo-proc");
        let rules = crate::store::load_rules(&rules_file).expect("load rules");
        assert_eq!(rules.rules.len(), 1);
        assert_eq!(rules.rules[0].name, "demo-proc");
    }

    #[test]
    fn watch_does_not_persist_rule_when_agent_ensure_fails() {
        let dir = tempdir().expect("tempdir");
        let rules_file = PathBuf::from(dir.path()).join("rules.toml");
        let state_file = PathBuf::from(dir.path()).join("state.json");
        let self_pid = std::process::id();
        crate::store::add_adhoc_instance(&state_file, 4321, self_pid, Domain::User)
            .expect("adhoc state");
        let process_name = store_process_name(self_pid).expect("process name");

        let executor = FakeExecutor::default();
        let launchd = FakeLaunchd::default();
        *launchd.fail_ensure.borrow_mut() = true;
        let service = Service { executor, launchd };

        let result = service.watch(
            &rules_file,
            &state_file,
            WatchOptions {
                name: &process_name,
                limit: 20,
                trigger_cpu: DEFAULT_TRIGGER_CPU,
                release_cpu: DEFAULT_RELEASE_CPU,
                args_contains: None,
            },
            Domain::User,
            WatchRuntime {
                cpuguard_bin: "/mock/cpuguard",
                cpulimit_bin: "/mock/cpulimit",
                config_dir: "/mock/config",
            },
        );

        assert!(result.is_err());
        let rules = crate::store::load_rules(&rules_file).expect("load rules");
        assert!(rules.rules.is_empty());
        let state = crate::store::load_state(&state_file).expect("load state");
        assert_eq!(state.instances.len(), 1);
        assert_eq!(state.instances[0].cpulimit_pid, 4321);
        assert!(service.executor.stopped.borrow().is_empty());
    }

    #[test]
    fn clean_all_removes_rules_and_instances() {
        let dir = tempdir().expect("tempdir");
        let rules_file = PathBuf::from(dir.path()).join("rules.toml");
        let state_file = PathBuf::from(dir.path()).join("state.json");

        crate::store::upsert_rule(
            &rules_file,
            RuleUpdate {
                name: "demo-proc".to_string(),
                limit: 20,
                trigger_cpu: DEFAULT_TRIGGER_CPU,
                release_cpu: DEFAULT_RELEASE_CPU,
                args_contains: None,
                domain: Domain::User,
            },
        )
        .expect("rule");
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

    #[test]
    fn same_rule_name_can_exist_in_different_domains() {
        let dir = tempdir().expect("tempdir");
        let rules_file = PathBuf::from(dir.path()).join("rules.toml");

        crate::store::upsert_rule(
            &rules_file,
            RuleUpdate {
                name: "demo-proc".to_string(),
                limit: 20,
                trigger_cpu: DEFAULT_TRIGGER_CPU,
                release_cpu: DEFAULT_RELEASE_CPU,
                args_contains: None,
                domain: Domain::User,
            },
        )
        .expect("user rule");
        crate::store::upsert_rule(
            &rules_file,
            RuleUpdate {
                name: "demo-proc".to_string(),
                limit: 30,
                trigger_cpu: 30.0,
                release_cpu: 10.0,
                args_contains: None,
                domain: Domain::System,
            },
        )
        .expect("system rule");

        let rules = crate::store::load_rules(&rules_file).expect("load rules");
        assert_eq!(rules.rules.len(), 2);
        assert!(
            rules
                .rules
                .iter()
                .any(|r| { r.name == "demo-proc" && r.domain == Domain::User && r.limit == 20 })
        );
        assert!(
            rules
                .rules
                .iter()
                .any(|r| { r.name == "demo-proc" && r.domain == Domain::System && r.limit == 30 })
        );
    }

    #[test]
    fn unwatch_removes_only_current_domain_rule_and_agent() {
        let dir = tempdir().expect("tempdir");
        let rules_file = PathBuf::from(dir.path()).join("rules.toml");
        let state_file = PathBuf::from(dir.path()).join("state.json");
        for domain in [Domain::User, Domain::System] {
            crate::store::upsert_rule(
                &rules_file,
                RuleUpdate {
                    name: "demo-proc".to_string(),
                    limit: 20,
                    trigger_cpu: DEFAULT_TRIGGER_CPU,
                    release_cpu: DEFAULT_RELEASE_CPU,
                    args_contains: None,
                    domain,
                },
            )
            .expect("rule");
        }
        crate::store::add_watch_instance(
            &state_file,
            crate::store::WatchInstanceUpdate {
                cpulimit_pid: 3333,
                target_pid: std::process::id(),
                rule_name: "demo-proc",
                limit: 20,
                last_observed_cpu: 30.0,
                domain: Domain::User,
                owner_label: "com.cpuguard.agent",
            },
        )
        .expect("watch instance");

        let executor = FakeExecutor::default();
        let launchd = FakeLaunchd::default();
        let service = Service { executor, launchd };

        let removed = service
            .unwatch(&rules_file, &state_file, "demo-proc", Domain::User)
            .expect("unwatch");
        assert!(removed);

        let rules = crate::store::load_rules(&rules_file).expect("load rules");
        assert_eq!(rules.rules.len(), 1);
        assert_eq!(rules.rules[0].domain, Domain::System);
        assert_eq!(
            *service.launchd.removed.borrow(),
            vec![("agent".to_string(), Domain::User)]
        );
        assert_eq!(*service.executor.stopped.borrow(), vec![3333]);
        let state = crate::store::load_state(&state_file).expect("load state");
        assert!(state.instances.is_empty());
    }

    #[test]
    fn unwatch_keeps_rule_and_state_when_stop_fails() {
        let dir = tempdir().expect("tempdir");
        let rules_file = PathBuf::from(dir.path()).join("rules.toml");
        let state_file = PathBuf::from(dir.path()).join("state.json");
        crate::store::upsert_rule(
            &rules_file,
            RuleUpdate {
                name: "demo-proc".to_string(),
                limit: 20,
                trigger_cpu: DEFAULT_TRIGGER_CPU,
                release_cpu: DEFAULT_RELEASE_CPU,
                args_contains: None,
                domain: Domain::User,
            },
        )
        .expect("rule");
        crate::store::add_watch_instance(
            &state_file,
            crate::store::WatchInstanceUpdate {
                cpulimit_pid: 3333,
                target_pid: std::process::id(),
                rule_name: "demo-proc",
                limit: 20,
                last_observed_cpu: 30.0,
                domain: Domain::User,
                owner_label: "com.cpuguard.agent",
            },
        )
        .expect("watch instance");

        let executor = FakeExecutor::default();
        executor.fail_stop.borrow_mut().insert(3333);
        let launchd = FakeLaunchd::default();
        let service = Service { executor, launchd };

        let result = service.unwatch(&rules_file, &state_file, "demo-proc", Domain::User);

        assert!(result.is_err());
        let rules = crate::store::load_rules(&rules_file).expect("load rules");
        assert_eq!(rules.rules.len(), 1);
        let state = crate::store::load_state(&state_file).expect("load state");
        assert_eq!(state.instances.len(), 1);
        assert_eq!(state.instances[0].cpulimit_pid, 3333);
        assert!(service.launchd.removed.borrow().is_empty());
    }

    #[test]
    fn unwatch_keeps_last_rule_when_agent_remove_fails() {
        let dir = tempdir().expect("tempdir");
        let rules_file = PathBuf::from(dir.path()).join("rules.toml");
        let state_file = PathBuf::from(dir.path()).join("state.json");
        crate::store::upsert_rule(
            &rules_file,
            RuleUpdate {
                name: "demo-proc".to_string(),
                limit: 20,
                trigger_cpu: DEFAULT_TRIGGER_CPU,
                release_cpu: DEFAULT_RELEASE_CPU,
                args_contains: None,
                domain: Domain::User,
            },
        )
        .expect("rule");

        let executor = FakeExecutor::default();
        let launchd = FakeLaunchd::default();
        *launchd.fail_remove.borrow_mut() = true;
        let service = Service { executor, launchd };

        let result = service.unwatch(&rules_file, &state_file, "demo-proc", Domain::User);

        assert!(result.is_err());
        let rules = crate::store::load_rules(&rules_file).expect("load rules");
        assert_eq!(rules.rules.len(), 1);
        assert_eq!(rules.rules[0].name, "demo-proc");
    }

    #[test]
    fn unwatch_stops_legacy_watch_instance_without_rule_name() {
        let dir = tempdir().expect("tempdir");
        let rules_file = PathBuf::from(dir.path()).join("rules.toml");
        let state_file = PathBuf::from(dir.path()).join("state.json");
        let self_pid = std::process::id();
        let process_name = store_process_name(self_pid).expect("process name");
        crate::store::upsert_rule(
            &rules_file,
            RuleUpdate {
                name: process_name.clone(),
                limit: 20,
                trigger_cpu: DEFAULT_TRIGGER_CPU,
                release_cpu: DEFAULT_RELEASE_CPU,
                args_contains: None,
                domain: Domain::User,
            },
        )
        .expect("rule");
        crate::store::save_state(
            &state_file,
            &crate::model::StateFile {
                version: 1,
                instances: vec![crate::model::ManagedInstance {
                    id: "legacy_watch".to_string(),
                    mode: ManagedMode::Watch,
                    cpulimit_pid: 3333,
                    target: ManagedTarget::Pid(self_pid),
                    rule_name: None,
                    limit: Some(20),
                    last_observed_cpu: Some(30.0),
                    domain: Domain::User,
                    started_at: Local::now(),
                    owner_label: Some("com.cpuguard.agent".to_string()),
                }],
            },
        )
        .expect("state");

        let executor = FakeExecutor::default();
        let launchd = FakeLaunchd::default();
        let service = Service { executor, launchd };

        let removed = service
            .unwatch(&rules_file, &state_file, &process_name, Domain::User)
            .expect("unwatch");

        assert!(removed);
        assert_eq!(*service.executor.stopped.borrow(), vec![3333]);
        let state = crate::store::load_state(&state_file).expect("state");
        assert!(state.instances.is_empty());
    }

    #[test]
    fn clean_all_is_scoped_to_requested_domain() {
        let dir = tempdir().expect("tempdir");
        let rules_file = PathBuf::from(dir.path()).join("rules.toml");
        let state_file = PathBuf::from(dir.path()).join("state.json");
        for domain in [Domain::User, Domain::System] {
            crate::store::upsert_rule(
                &rules_file,
                RuleUpdate {
                    name: format!("demo-{domain}"),
                    limit: 20,
                    trigger_cpu: DEFAULT_TRIGGER_CPU,
                    release_cpu: DEFAULT_RELEASE_CPU,
                    args_contains: None,
                    domain,
                },
            )
            .expect("rule");
            crate::store::add_adhoc_instance(
                &state_file,
                match domain {
                    Domain::User => 1111,
                    Domain::System => 2222,
                },
                std::process::id(),
                domain,
            )
            .expect("state");
        }

        let executor = FakeExecutor::default();
        let launchd = FakeLaunchd::default();
        let service = Service { executor, launchd };

        let (rules_removed, instances_stopped) = service
            .clean_all(&rules_file, &state_file, Domain::User)
            .expect("clean all");

        assert_eq!(rules_removed, 1);
        assert_eq!(instances_stopped, 1);
        assert_eq!(*service.executor.stopped.borrow(), vec![1111]);
        let rules = crate::store::load_rules(&rules_file).expect("load rules");
        assert_eq!(rules.rules.len(), 1);
        assert_eq!(rules.rules[0].domain, Domain::System);
        let state = crate::store::load_state(&state_file).expect("load state");
        assert_eq!(state.instances.len(), 1);
        assert_eq!(state.instances[0].domain, Domain::System);
        assert_eq!(state.instances[0].cpulimit_pid, 2222);
    }

    #[test]
    fn clean_all_keeps_rules_and_state_when_stop_fails() {
        let dir = tempdir().expect("tempdir");
        let rules_file = PathBuf::from(dir.path()).join("rules.toml");
        let state_file = PathBuf::from(dir.path()).join("state.json");
        crate::store::upsert_rule(
            &rules_file,
            RuleUpdate {
                name: "demo-proc".to_string(),
                limit: 20,
                trigger_cpu: DEFAULT_TRIGGER_CPU,
                release_cpu: DEFAULT_RELEASE_CPU,
                args_contains: None,
                domain: Domain::User,
            },
        )
        .expect("rule");
        crate::store::add_adhoc_instance(&state_file, 4321, std::process::id(), Domain::User)
            .expect("state");

        let executor = FakeExecutor::default();
        executor.fail_stop.borrow_mut().insert(4321);
        let launchd = FakeLaunchd::default();
        let service = Service { executor, launchd };

        let result = service.clean_all(&rules_file, &state_file, Domain::User);

        assert!(result.is_err());
        let rules = crate::store::load_rules(&rules_file).expect("load rules");
        assert_eq!(rules.rules.len(), 1);
        let state = crate::store::load_state(&state_file).expect("load state");
        assert_eq!(state.instances.len(), 1);
        assert_eq!(state.instances[0].cpulimit_pid, 4321);
        assert!(service.launchd.removed.borrow().is_empty());
    }

    #[test]
    fn clean_all_reports_agent_remove_failure() {
        let dir = tempdir().expect("tempdir");
        let rules_file = PathBuf::from(dir.path()).join("rules.toml");
        let state_file = PathBuf::from(dir.path()).join("state.json");
        crate::store::upsert_rule(
            &rules_file,
            RuleUpdate {
                name: "demo-proc".to_string(),
                limit: 20,
                trigger_cpu: DEFAULT_TRIGGER_CPU,
                release_cpu: DEFAULT_RELEASE_CPU,
                args_contains: None,
                domain: Domain::User,
            },
        )
        .expect("rule");

        let executor = FakeExecutor::default();
        let launchd = FakeLaunchd::default();
        *launchd.fail_remove.borrow_mut() = true;
        let service = Service { executor, launchd };

        let result = service.clean_all(&rules_file, &state_file, Domain::User);

        assert!(result.is_err());
        let rules = crate::store::load_rules(&rules_file).expect("load rules");
        assert_eq!(rules.rules.len(), 1);
        assert_eq!(rules.rules[0].name, "demo-proc");
    }

    #[test]
    fn watch_conflict_cleanup_respects_args_contains() {
        let dir = tempdir().expect("tempdir");
        let rules_file = PathBuf::from(dir.path()).join("rules.toml");
        let state_file = PathBuf::from(dir.path()).join("state.json");
        let self_pid = std::process::id();
        crate::store::add_adhoc_instance(&state_file, 4321, self_pid, Domain::User)
            .expect("adhoc state");
        let process_name = store_process_name(self_pid).expect("process name");

        let executor = FakeExecutor::default();
        let launchd = FakeLaunchd::default();
        let service = Service { executor, launchd };

        service
            .watch(
                &rules_file,
                &state_file,
                WatchOptions {
                    name: &process_name,
                    limit: 20,
                    trigger_cpu: DEFAULT_TRIGGER_CPU,
                    release_cpu: DEFAULT_RELEASE_CPU,
                    args_contains: Some("definitely-not-in-current-test-args".to_string()),
                },
                Domain::User,
                WatchRuntime {
                    cpuguard_bin: "/mock/cpuguard",
                    cpulimit_bin: "/mock/cpulimit",
                    config_dir: "/mock/config",
                },
            )
            .expect("watch");

        assert!(service.executor.stopped.borrow().is_empty());
        let state = crate::store::load_state(&state_file).expect("load state");
        assert_eq!(state.instances.len(), 1);
        assert_eq!(state.instances[0].cpulimit_pid, 4321);
    }

    #[test]
    fn watch_fails_if_matching_managed_adhoc_cannot_be_stopped() {
        let dir = tempdir().expect("tempdir");
        let rules_file = PathBuf::from(dir.path()).join("rules.toml");
        let state_file = PathBuf::from(dir.path()).join("state.json");
        let self_pid = std::process::id();
        crate::store::add_adhoc_instance(&state_file, 4321, self_pid, Domain::User)
            .expect("adhoc state");
        let process_name = store_process_name(self_pid).expect("process name");

        let executor = FakeExecutor::default();
        executor.fail_stop.borrow_mut().insert(4321);
        let launchd = FakeLaunchd::default();
        let service = Service { executor, launchd };

        let result = service.watch(
            &rules_file,
            &state_file,
            WatchOptions {
                name: &process_name,
                limit: 20,
                trigger_cpu: DEFAULT_TRIGGER_CPU,
                release_cpu: DEFAULT_RELEASE_CPU,
                args_contains: None,
            },
            Domain::User,
            WatchRuntime {
                cpuguard_bin: "/mock/cpuguard",
                cpulimit_bin: "/mock/cpulimit",
                config_dir: "/mock/config",
            },
        );

        assert!(result.is_err());
        let state = crate::store::load_state(&state_file).expect("load state");
        assert_eq!(state.instances.len(), 1);
        assert_eq!(state.instances[0].cpulimit_pid, 4321);
        let rules = crate::store::load_rules(&rules_file).expect("load rules");
        assert!(rules.rules.is_empty());
    }

    #[test]
    fn watch_does_not_treat_other_domain_managed_adhoc_as_external_conflict() {
        let dir = tempdir().expect("tempdir");
        let rules_file = PathBuf::from(dir.path()).join("rules.toml");
        let state_file = PathBuf::from(dir.path()).join("state.json");
        let self_pid = std::process::id();
        crate::store::add_adhoc_instance(&state_file, 4321, self_pid, Domain::System)
            .expect("adhoc state");
        let process_name = store_process_name(self_pid).expect("process name");

        let executor = FakeExecutor::default();
        let launchd = FakeLaunchd::default();
        let service = Service { executor, launchd };

        service
            .watch(
                &rules_file,
                &state_file,
                WatchOptions {
                    name: &process_name,
                    limit: 20,
                    trigger_cpu: DEFAULT_TRIGGER_CPU,
                    release_cpu: DEFAULT_RELEASE_CPU,
                    args_contains: None,
                },
                Domain::User,
                WatchRuntime {
                    cpuguard_bin: "/mock/cpuguard",
                    cpulimit_bin: "/mock/cpulimit",
                    config_dir: "/mock/config",
                },
            )
            .expect("watch");

        assert!(service.executor.stopped.borrow().is_empty());
        let state = crate::store::load_state(&state_file).expect("load state");
        assert_eq!(state.instances.len(), 1);
        assert_eq!(state.instances[0].domain, Domain::System);
    }
}
