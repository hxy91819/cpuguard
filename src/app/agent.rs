use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, Local};

use crate::infra::cpulimit::CpulimitExecutor;
use crate::infra::process_snapshot::{ProcessEntry, all_processes, process_args};
use crate::infra::runtime::process_alive;
use crate::model::{Domain, ManagedMode, ManagedTarget, Rule};
use crate::store;

const HOT_REQUIRED_SAMPLES: u8 = 2;
const COLD_REQUIRED_SAMPLES: u8 = 2;
const BACKOFF_SECS: i64 = 60;
const MAX_WATCH_INSTANCES: usize = 8;
const IDLE_SCAN_SECS: u64 = 30;
const HOT_SCAN_SECS: u64 = 5;
const AGENT_LABEL: &str = "com.cpuguard.agent";

#[derive(Debug, Clone, Default)]
struct TargetRuntime {
    hot_samples: u8,
    cold_samples: u8,
    backoff_until: Option<DateTime<Local>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentTickActivity {
    Idle,
    Hot,
}

impl AgentTickActivity {
    pub fn next_sleep(self) -> Duration {
        match self {
            Self::Idle => Duration::from_secs(IDLE_SCAN_SECS),
            Self::Hot => Duration::from_secs(HOT_SCAN_SECS),
        }
    }
}

#[derive(Debug, Default)]
pub struct AgentRuntime {
    targets: HashMap<u32, TargetRuntime>,
}

impl AgentRuntime {
    pub fn new() -> Self {
        Self::default()
    }
}

pub struct Agent<'a, E: CpulimitExecutor> {
    pub executor: &'a E,
    pub rules_file: &'a Path,
    pub state_file: &'a Path,
    pub domain: Domain,
}

impl<E: CpulimitExecutor> Agent<'_, E> {
    pub fn tick(&self, runtime: &mut AgentRuntime) -> Result<AgentTickActivity> {
        let rules = store::load_rules(self.rules_file)?.rules;
        let snapshot = all_processes()?;
        self.tick_with_snapshot(runtime, &rules, &snapshot)
    }

    pub fn tick_with_snapshot(
        &self,
        runtime: &mut AgentRuntime,
        rules: &[Rule],
        snapshot: &[ProcessEntry],
    ) -> Result<AgentTickActivity> {
        let now = Local::now();
        let state = store::load_state(self.state_file)?;
        let domain_rules: Vec<&Rule> = rules
            .iter()
            .filter(|rule| rule.domain == self.domain)
            .collect();
        let snapshot_by_pid: HashMap<u32, &ProcessEntry> =
            snapshot.iter().map(|entry| (entry.pid, entry)).collect();
        let mut args_cache: HashMap<u32, Option<String>> = HashMap::new();
        let managed_cpulimit_pids = state
            .instances
            .iter()
            .map(|instance| instance.cpulimit_pid)
            .collect::<HashSet<_>>();
        let cpulimit_pids_by_target = cpulimit_pids_by_target().unwrap_or_default();

        let mut limited_targets = HashSet::new();
        let mut active_watch_count = 0usize;
        let mut activity = AgentTickActivity::Idle;

        for instance in state
            .instances
            .iter()
            .filter(|i| i.mode == ManagedMode::Watch)
            .filter(|i| i.domain == self.domain)
        {
            let target_pid = match instance.target {
                ManagedTarget::Pid(pid) => pid,
                ManagedTarget::Name(_) => continue,
            };
            limited_targets.insert(target_pid);
            active_watch_count += 1;

            let target_entry = snapshot_by_pid.get(&target_pid).copied();
            let matching_rule = domain_rules.iter().copied().find(|rule| {
                watch_instance_matches_rule(instance, target_entry, rule, &mut args_cache)
            });
            let rule_alive = matching_rule.is_some();
            let cpulimit_alive = process_alive(instance.cpulimit_pid);

            if !rule_alive || target_entry.is_none() || !cpulimit_alive {
                if cpulimit_alive && self.executor.stop_instance(instance.cpulimit_pid).is_err() {
                    continue;
                }
                if !cpulimit_alive && target_entry.is_some() {
                    if stop_duplicate_cpulimits_for_target(
                        self.executor,
                        cpulimit_pids_by_target
                            .get(&target_pid)
                            .map(Vec::as_slice)
                            .unwrap_or(&[]),
                        instance.cpulimit_pid,
                        &managed_cpulimit_pids,
                    )
                    .is_err()
                    {
                        continue;
                    }
                    runtime.targets.entry(target_pid).or_default().backoff_until =
                        Some(now + chrono::Duration::seconds(BACKOFF_SECS));
                }
                let _ = store::remove_instance_by_pid(self.state_file, instance.cpulimit_pid);
                active_watch_count = active_watch_count.saturating_sub(1);
                continue;
            }

            let entry = target_entry.expect("checked above");
            if let Err(err) = stop_duplicate_cpulimits_for_target(
                self.executor,
                cpulimit_pids_by_target
                    .get(&target_pid)
                    .map(Vec::as_slice)
                    .unwrap_or(&[]),
                instance.cpulimit_pid,
                &managed_cpulimit_pids,
            ) {
                eprintln!("cpuguard agent duplicate cleanup failed for PID {target_pid}: {err:#}");
                continue;
            }
            let _ = store::update_instance_cpu(self.state_file, instance.cpulimit_pid, entry.cpu);
            let Some(rule) = matching_rule else {
                continue;
            };
            if !rule_matches(rule, entry, &mut args_cache) {
                if self.executor.stop_instance(instance.cpulimit_pid).is_err() {
                    continue;
                }
                let _ = store::remove_instance_by_pid(self.state_file, instance.cpulimit_pid);
                active_watch_count = active_watch_count.saturating_sub(1);
                limited_targets.remove(&target_pid);
                continue;
            }
            let target = runtime.targets.entry(target_pid).or_default();
            if entry.cpu <= rule.release_cpu {
                target.cold_samples = target.cold_samples.saturating_add(1);
            } else {
                target.cold_samples = 0;
                activity = AgentTickActivity::Hot;
            }

            if target.cold_samples >= COLD_REQUIRED_SAMPLES {
                if self.executor.stop_instance(instance.cpulimit_pid).is_err() {
                    continue;
                }
                let _ = store::remove_instance_by_pid(self.state_file, instance.cpulimit_pid);
                active_watch_count = active_watch_count.saturating_sub(1);
                limited_targets.remove(&target_pid);
            }
        }

        for rule in domain_rules {
            for entry in snapshot {
                if !rule_matches(rule, entry, &mut args_cache) {
                    continue;
                }
                if entry.cpu < rule.trigger_cpu {
                    runtime.targets.entry(entry.pid).or_default().hot_samples = 0;
                    continue;
                }
                activity = AgentTickActivity::Hot;
                if limited_targets.contains(&entry.pid) {
                    continue;
                }
                if cpulimit_pids_by_target
                    .get(&entry.pid)
                    .is_some_and(|pids| !pids.is_empty())
                {
                    continue;
                }
                if active_watch_count >= MAX_WATCH_INSTANCES {
                    continue;
                }
                let target = runtime.targets.entry(entry.pid).or_default();
                if target.backoff_until.is_some_and(|until| until > now) {
                    continue;
                }
                target.hot_samples = target.hot_samples.saturating_add(1);
                if target.hot_samples < HOT_REQUIRED_SAMPLES {
                    continue;
                }

                let cpulimit_pid = match self.executor.start_adhoc(entry.pid, rule.limit) {
                    Ok(pid) => pid,
                    Err(_) => {
                        target.backoff_until = Some(now + chrono::Duration::seconds(BACKOFF_SECS));
                        target.hot_samples = 0;
                        continue;
                    }
                };
                if store::add_watch_instance(
                    self.state_file,
                    cpulimit_pid,
                    entry.pid,
                    &rule.name,
                    entry.cpu,
                    self.domain,
                    AGENT_LABEL,
                )
                .is_err()
                {
                    let _ = self.executor.stop_instance(cpulimit_pid);
                    target.backoff_until = Some(now + chrono::Duration::seconds(BACKOFF_SECS));
                    target.hot_samples = 0;
                    continue;
                }
                active_watch_count += 1;
                limited_targets.insert(entry.pid);
                target.hot_samples = 0;
                target.cold_samples = 0;
            }
        }

        runtime.targets.retain(|pid, state| {
            snapshot_by_pid.contains_key(pid)
                || state.backoff_until.is_some_and(|until| until > now)
        });

        Ok(activity)
    }
}

fn cpulimit_pids_by_target() -> Result<HashMap<u32, Vec<u32>>> {
    let output = Command::new("ps").args(["-eo", "pid=,args="]).output()?;
    if !output.status.success() {
        return Ok(HashMap::new());
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut by_target = HashMap::new();
    for (cpulimit_pid, target_pid) in text.lines().filter_map(parse_cpulimit_process_line) {
        by_target
            .entry(target_pid)
            .or_insert_with(Vec::new)
            .push(cpulimit_pid);
    }
    Ok(by_target)
}

#[cfg(test)]
fn cpulimit_pids_for_target_from_ps(ps_output: &str, target_pid: u32) -> Vec<u32> {
    ps_output
        .lines()
        .filter_map(parse_cpulimit_process_line)
        .filter_map(|(cpulimit_pid, parsed_target_pid)| {
            (parsed_target_pid == target_pid).then_some(cpulimit_pid)
        })
        .collect()
}

fn parse_cpulimit_process_line(line: &str) -> Option<(u32, u32)> {
    let args: Vec<&str> = line.split_whitespace().collect();
    let cpulimit_pid = args.first()?.parse::<u32>().ok()?;
    let command = args.get(1)?;
    if std::path::Path::new(command)
        .file_name()
        .and_then(|name| name.to_str())
        .is_none_or(|name| name != "cpulimit")
    {
        return None;
    }
    args.windows(2).find_map(|pair| {
        (pair[0] == "-p")
            .then(|| pair[1].parse::<u32>().ok())
            .flatten()
            .map(|target_pid| (cpulimit_pid, target_pid))
    })
}

fn stop_duplicate_cpulimits_for_target<E: CpulimitExecutor>(
    executor: &E,
    target_cpulimit_pids: &[u32],
    keep_cpulimit_pid: u32,
    managed_cpulimit_pids: &HashSet<u32>,
) -> Result<usize> {
    let duplicate_pids = target_cpulimit_pids
        .iter()
        .copied()
        .filter(|pid| *pid != keep_cpulimit_pid)
        .filter(|pid| !managed_cpulimit_pids.contains(pid))
        .collect::<Vec<_>>();

    stop_cpulimit_pids(executor, &duplicate_pids)
}

fn stop_cpulimit_pids<E: CpulimitExecutor>(executor: &E, pids: &[u32]) -> Result<usize> {
    let mut stopped = 0usize;
    let mut failed = Vec::new();
    for pid in pids {
        if executor.stop_instance(*pid).is_ok() {
            stopped += 1;
        } else {
            failed.push(*pid);
        }
    }
    if !failed.is_empty() {
        anyhow::bail!(
            "failed to stop duplicate cpulimit instance(s): {}",
            failed
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok(stopped)
}

fn watch_instance_matches_rule(
    instance: &crate::model::ManagedInstance,
    target_entry: Option<&ProcessEntry>,
    rule: &Rule,
    args_cache: &mut HashMap<u32, Option<String>>,
) -> bool {
    match instance.rule_name.as_deref() {
        Some(name) => name == rule.name,
        None => target_entry.is_some_and(|entry| rule_matches(rule, entry, args_cache)),
    }
}

fn rule_matches(
    rule: &Rule,
    entry: &ProcessEntry,
    args_cache: &mut HashMap<u32, Option<String>>,
) -> bool {
    let basename = std::path::Path::new(&entry.name)
        .file_name()
        .map(|s| s.to_string_lossy())
        .unwrap_or_default();
    if basename != rule.name {
        return false;
    }

    match &rule.args_contains {
        Some(needle) => process_args_cached(entry.pid, args_cache)
            .as_deref()
            .is_some_and(|args| args.contains(needle)),
        None => true,
    }
}

fn process_args_cached(pid: u32, args_cache: &mut HashMap<u32, Option<String>>) -> Option<String> {
    args_cache
        .entry(pid)
        .or_insert_with(|| process_args(pid).ok().flatten());
    args_cache.get(&pid).cloned().flatten()
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::{HashSet, VecDeque};
    use std::path::PathBuf;

    use tempfile::tempdir;

    use crate::infra::cpulimit::CpulimitExecutor;
    use crate::model::{DEFAULT_RELEASE_CPU, DEFAULT_TRIGGER_CPU, Rule};

    use super::*;

    #[derive(Default)]
    struct FakeExecutor {
        started: RefCell<Vec<(u32, u16)>>,
        stopped: RefCell<Vec<u32>>,
        next_pid: RefCell<VecDeque<u32>>,
        fail_start_targets: RefCell<HashSet<u32>>,
        fail_stop_pids: RefCell<HashSet<u32>>,
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
            if self.fail_start_targets.borrow().contains(&target_pid) {
                anyhow::bail!("injected start failure for {target_pid}");
            }
            self.started.borrow_mut().push((target_pid, limit));
            Ok(self.next_pid.borrow_mut().pop_front().unwrap_or(9999))
        }

        fn run_for_target(&self, target_pid: u32, limit: u16) -> anyhow::Result<()> {
            self.started.borrow_mut().push((target_pid, limit));
            Ok(())
        }

        fn stop_instance(&self, cpulimit_pid: u32) -> anyhow::Result<()> {
            if self.fail_stop_pids.borrow().contains(&cpulimit_pid) {
                anyhow::bail!("injected stop failure for {cpulimit_pid}");
            }
            self.stopped.borrow_mut().push(cpulimit_pid);
            Ok(())
        }
    }

    fn rule(name: &str) -> Rule {
        Rule {
            name: name.to_string(),
            limit: 20,
            trigger_cpu: DEFAULT_TRIGGER_CPU,
            release_cpu: DEFAULT_RELEASE_CPU,
            args_contains: None,
            domain: Domain::User,
            created_at: Local::now(),
            updated_at: Local::now(),
        }
    }

    fn rule_in_domain(name: &str, domain: Domain) -> Rule {
        Rule {
            domain,
            ..rule(name)
        }
    }

    fn entry(pid: u32, name: &str, cpu: f32) -> ProcessEntry {
        ProcessEntry {
            pid,
            ppid: 1,
            name: name.to_string(),
            cpu,
            elapsed_secs: 60,
        }
    }

    #[test]
    fn hot_matching_pid_starts_cpulimit_and_records_state() {
        let dir = tempdir().expect("tempdir");
        let state_file = PathBuf::from(dir.path()).join("state.json");
        let rules_file = PathBuf::from(dir.path()).join("rules.toml");
        let executor = FakeExecutor::with_next_pids(vec![4321]);
        let agent = Agent {
            executor: &executor,
            rules_file: &rules_file,
            state_file: &state_file,
            domain: Domain::User,
        };

        let mut runtime = AgentRuntime::new();
        agent
            .tick_with_snapshot(
                &mut runtime,
                &[rule("ztsmedr")],
                &[entry(100, "/Applications/foo/ztsmedr", 80.0)],
            )
            .expect("first tick");
        let activity = agent
            .tick_with_snapshot(
                &mut runtime,
                &[rule("ztsmedr")],
                &[entry(100, "/Applications/foo/ztsmedr", 80.0)],
            )
            .expect("second tick");

        assert_eq!(activity, AgentTickActivity::Hot);
        assert_eq!(*executor.started.borrow(), vec![(100, 20)]);
        let state = store::load_state(&state_file).expect("state");
        assert_eq!(state.instances.len(), 1);
        assert_eq!(state.instances[0].rule_name.as_deref(), Some("ztsmedr"));
    }

    #[test]
    fn cold_limited_pid_is_stopped_after_required_samples() {
        let dir = tempdir().expect("tempdir");
        let state_file = PathBuf::from(dir.path()).join("state.json");
        let rules_file = PathBuf::from(dir.path()).join("rules.toml");
        store::add_watch_instance(
            &state_file,
            std::process::id(),
            100,
            "ztsmedr",
            30.0,
            Domain::User,
            AGENT_LABEL,
        )
        .expect("watch instance");
        let executor = FakeExecutor::default();
        let agent = Agent {
            executor: &executor,
            rules_file: &rules_file,
            state_file: &state_file,
            domain: Domain::User,
        };
        let mut runtime = AgentRuntime::new();

        for _ in 0..2 {
            agent
                .tick_with_snapshot(
                    &mut runtime,
                    &[rule("ztsmedr")],
                    &[entry(100, "ztsmedr", 0.5)],
                )
                .expect("tick");
        }

        assert_eq!(*executor.stopped.borrow(), vec![std::process::id()]);
        let state = store::load_state(&state_file).expect("state");
        assert!(state.instances.is_empty());
    }

    #[test]
    fn cold_limited_pid_keeps_state_when_stop_fails() {
        let dir = tempdir().expect("tempdir");
        let state_file = PathBuf::from(dir.path()).join("state.json");
        let rules_file = PathBuf::from(dir.path()).join("rules.toml");
        let cpulimit_pid = std::process::id();
        store::add_watch_instance(
            &state_file,
            cpulimit_pid,
            100,
            "ztsmedr",
            30.0,
            Domain::User,
            AGENT_LABEL,
        )
        .expect("watch instance");
        let executor = FakeExecutor::default();
        executor.fail_stop_pids.borrow_mut().insert(cpulimit_pid);
        let agent = Agent {
            executor: &executor,
            rules_file: &rules_file,
            state_file: &state_file,
            domain: Domain::User,
        };
        let mut runtime = AgentRuntime::new();

        for _ in 0..2 {
            agent
                .tick_with_snapshot(
                    &mut runtime,
                    &[rule("ztsmedr")],
                    &[entry(100, "ztsmedr", 0.5)],
                )
                .expect("tick");
        }

        let state = store::load_state(&state_file).expect("state");
        assert_eq!(state.instances.len(), 1);
        assert_eq!(state.instances[0].cpulimit_pid, cpulimit_pid);
    }

    #[test]
    fn agent_ignores_rules_from_other_domain() {
        let dir = tempdir().expect("tempdir");
        let state_file = PathBuf::from(dir.path()).join("state.json");
        let rules_file = PathBuf::from(dir.path()).join("rules.toml");
        let executor = FakeExecutor::with_next_pids(vec![4321]);
        let agent = Agent {
            executor: &executor,
            rules_file: &rules_file,
            state_file: &state_file,
            domain: Domain::User,
        };

        let activity = agent
            .tick_with_snapshot(
                &mut AgentRuntime::new(),
                &[rule_in_domain("ztsmedr", Domain::System)],
                &[entry(100, "ztsmedr", 80.0)],
            )
            .expect("tick");

        assert_eq!(activity, AgentTickActivity::Idle);
        assert!(executor.started.borrow().is_empty());
        let state = store::load_state(&state_file).expect("state");
        assert!(state.instances.is_empty());
    }

    #[test]
    fn agent_ignores_instances_from_other_domain() {
        let dir = tempdir().expect("tempdir");
        let state_file = PathBuf::from(dir.path()).join("state.json");
        let rules_file = PathBuf::from(dir.path()).join("rules.toml");
        store::add_watch_instance(
            &state_file,
            std::process::id(),
            100,
            "ztsmedr",
            30.0,
            Domain::System,
            AGENT_LABEL,
        )
        .expect("watch instance");
        let executor = FakeExecutor::default();
        let agent = Agent {
            executor: &executor,
            rules_file: &rules_file,
            state_file: &state_file,
            domain: Domain::User,
        };

        agent
            .tick_with_snapshot(&mut AgentRuntime::new(), &[], &[])
            .expect("tick");

        assert!(executor.stopped.borrow().is_empty());
        let state = store::load_state(&state_file).expect("state");
        assert_eq!(state.instances.len(), 1);
        assert_eq!(state.instances[0].domain, Domain::System);
    }

    #[test]
    fn legacy_watch_instance_without_rule_name_uses_target_rule_match() {
        let dir = tempdir().expect("tempdir");
        let state_file = PathBuf::from(dir.path()).join("state.json");
        let rules_file = PathBuf::from(dir.path()).join("rules.toml");
        store::save_state(
            &state_file,
            &crate::model::StateFile {
                version: 1,
                instances: vec![crate::model::ManagedInstance {
                    id: "legacy_watch".to_string(),
                    mode: ManagedMode::Watch,
                    cpulimit_pid: std::process::id(),
                    target: ManagedTarget::Pid(100),
                    rule_name: None,
                    last_observed_cpu: Some(30.0),
                    domain: Domain::User,
                    started_at: Local::now(),
                    owner_label: Some(AGENT_LABEL.to_string()),
                }],
            },
        )
        .expect("state");
        let executor = FakeExecutor::default();
        let agent = Agent {
            executor: &executor,
            rules_file: &rules_file,
            state_file: &state_file,
            domain: Domain::User,
        };

        agent
            .tick_with_snapshot(
                &mut AgentRuntime::new(),
                &[rule("ztsmedr")],
                &[entry(100, "ztsmedr", 80.0)],
            )
            .expect("tick");

        assert!(executor.stopped.borrow().is_empty());
        let state = store::load_state(&state_file).expect("state");
        assert_eq!(state.instances.len(), 1);
        assert_eq!(state.instances[0].rule_name, None);
    }

    #[test]
    fn target_start_failure_does_not_abort_other_rules() {
        let dir = tempdir().expect("tempdir");
        let state_file = PathBuf::from(dir.path()).join("state.json");
        let rules_file = PathBuf::from(dir.path()).join("rules.toml");
        let executor = FakeExecutor::with_next_pids(vec![4321]);
        executor.fail_start_targets.borrow_mut().insert(100);
        let agent = Agent {
            executor: &executor,
            rules_file: &rules_file,
            state_file: &state_file,
            domain: Domain::User,
        };

        let mut runtime = AgentRuntime::new();
        agent
            .tick_with_snapshot(
                &mut runtime,
                &[rule("bad-target"), rule("good-target")],
                &[
                    entry(100, "bad-target", 80.0),
                    entry(200, "good-target", 80.0),
                ],
            )
            .expect("first tick");
        let activity = agent
            .tick_with_snapshot(
                &mut runtime,
                &[rule("bad-target"), rule("good-target")],
                &[
                    entry(100, "bad-target", 80.0),
                    entry(200, "good-target", 80.0),
                ],
            )
            .expect("tick should continue after one target fails");

        assert_eq!(activity, AgentTickActivity::Hot);
        assert_eq!(*executor.started.borrow(), vec![(200, 20)]);
        let state = store::load_state(&state_file).expect("state");
        assert_eq!(state.instances.len(), 1);
        assert_eq!(state.instances[0].rule_name.as_deref(), Some("good-target"));
    }

    #[test]
    fn existing_instance_is_stopped_when_rule_selector_no_longer_matches() {
        let dir = tempdir().expect("tempdir");
        let state_file = PathBuf::from(dir.path()).join("state.json");
        let rules_file = PathBuf::from(dir.path()).join("rules.toml");
        store::add_watch_instance(
            &state_file,
            std::process::id(),
            100,
            "iOABiz",
            30.0,
            Domain::User,
            AGENT_LABEL,
        )
        .expect("watch instance");
        let executor = FakeExecutor::default();
        let agent = Agent {
            executor: &executor,
            rules_file: &rules_file,
            state_file: &state_file,
            domain: Domain::User,
        };
        let mut narrowed_rule = rule("iOABiz");
        narrowed_rule.args_contains = Some("NGNAuditXPCClient".to_string());

        agent
            .tick_with_snapshot(
                &mut AgentRuntime::new(),
                &[narrowed_rule],
                &[entry(100, "/Applications/iOA/iOABiz", 80.0)],
            )
            .expect("tick");

        assert_eq!(*executor.stopped.borrow(), vec![std::process::id()]);
        let state = store::load_state(&state_file).expect("state");
        assert!(state.instances.is_empty());
    }

    #[test]
    fn parses_cpulimit_targets_from_ps_output() {
        let ps_output = r#"
13178 /opt/homebrew/bin/cpulimit -p 21495 -l 20 -i
21878 /opt/homebrew/bin/cpulimit -p 21243 -l 20 -i
38719 /opt/homebrew/bin/cpulimit -p 21243 -l 20 -i
50000 /bin/zsh -lc grep cpulimit
"#;

        let matches = cpulimit_pids_for_target_from_ps(ps_output, 21243);
        assert_eq!(matches, vec![21878, 38719]);
    }

    #[test]
    fn parse_cpulimit_process_line_ignores_non_cpulimit_commands() {
        assert_eq!(
            parse_cpulimit_process_line("50000 /bin/zsh -lc grep cpulimit -p 21243"),
            None
        );
        assert_eq!(
            parse_cpulimit_process_line("21878 /opt/homebrew/bin/cpulimit -p 21243 -l 20 -i"),
            Some((21878, 21243))
        );
    }

    #[test]
    fn stop_cpulimit_pids_reports_partial_failure() {
        let executor = FakeExecutor::default();
        executor.fail_stop_pids.borrow_mut().insert(222);

        let result = stop_cpulimit_pids(&executor, &[111, 222]);

        assert!(result.is_err());
        assert_eq!(*executor.stopped.borrow(), vec![111]);
    }
}
