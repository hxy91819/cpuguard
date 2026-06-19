use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use std::collections::HashSet;
use std::io::Write;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use cpuguard::app::agent::{Agent, AgentRuntime, AgentTickActivity};
use cpuguard::app::service::{Service, WatchRuntime};
use cpuguard::cli::output::fit_col;
use cpuguard::infra::config::AppConfig;
use cpuguard::infra::cpulimit::{CpulimitExecutor, RealCpulimitExecutor};
use cpuguard::infra::launchd::{LaunchdManager, RealLaunchdManager, agent_loaded_status};
use cpuguard::infra::process_snapshot::{
    ProcessEntry, all_processes, is_high_risk_process, is_protected_system_process, process_args,
    process_name, top_processes,
};
use cpuguard::infra::runtime::{first_pid_by_name, kill_process, process_alive};
use cpuguard::model::{Domain, ManagedMode, ManagedTarget, StateFile};
use cpuguard::store;

/// 风险提示运行时间阈值：30 分钟（1800 秒）。
const RISK_ELAPSED_THRESHOLD_SECS: u64 = 1800;

#[derive(Parser, Debug)]
#[command(version, about = "cpuguard: macOS cpulimit manager")]
struct Cli {
    #[arg(long, default_value = "user")]
    domain: DomainArg,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    Watch {
        name: String,
        #[arg(long, default_value_t = 20)]
        limit: u16,
        #[arg(long, default_value_t = cpuguard::model::DEFAULT_TRIGGER_CPU)]
        trigger_cpu: f32,
        #[arg(long, default_value_t = cpuguard::model::DEFAULT_RELEASE_CPU)]
        release_cpu: f32,
        #[arg(long)]
        args_contains: Option<String>,
    },
    Unwatch {
        name: String,
    },
    Watches,
    Top {
        #[arg(long, default_value_t = 20)]
        limit: u16,
        #[arg(long, default_value_t = 10)]
        count: usize,
        #[arg(long, default_value_t = 5)]
        refresh: u64,
        #[arg(long)]
        once: bool,
        #[arg(long)]
        pid: Option<u32>,
        /// 风险提示的 CPU 阈值（百分比），默认 50.0
        #[arg(
            long = "risk-cpu",
            visible_alias = "orphan-cpu",
            default_value_t = 50.0
        )]
        risk_cpu: f32,
        /// 显式启用 k/x 终止命令
        #[arg(long)]
        allow_kill: bool,
    },
    Status,
    Clean {
        #[arg(long)]
        yes: bool,
    },
    InstallAgent,
    #[command(name = "__agent", hide = true)]
    Agent {
        #[arg(long)]
        config_dir: std::path::PathBuf,
        #[arg(long)]
        cpulimit_bin: std::path::PathBuf,
    },
}

#[derive(clap::ValueEnum, Clone, Debug, Default)]
enum DomainArg {
    #[default]
    User,
    System,
}

impl From<DomainArg> for Domain {
    fn from(value: DomainArg) -> Self {
        match value {
            DomainArg::User => Domain::User,
            DomainArg::System => Domain::System,
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    run(cli)
}

fn run(cli: Cli) -> Result<()> {
    let config = AppConfig::load();
    let domain: Domain = cli.domain.into();

    let service = Service {
        executor: RealCpulimitExecutor {
            bin: config.cpulimit_bin.clone(),
        },
        launchd: RealLaunchdManager {
            label_prefix: config.label_prefix.clone(),
            launch_agents_dir: config.launch_agents_dir.clone(),
        },
    };

    let Some(command) = cli.command else {
        show_dashboard(&config, domain)?;
        return Ok(());
    };

    match command {
        Commands::Agent {
            config_dir,
            cpulimit_bin,
        } => {
            run_agent_loop(config_dir, cpulimit_bin, domain)?;
        }
        Commands::Watch {
            name,
            limit,
            trigger_cpu,
            release_cpu,
            args_contains,
        } => {
            validate_limit(limit)?;
            validate_thresholds(trigger_cpu, release_cpu)?;
            let cpuguard_bin = std::env::current_exe()?;
            let rule = service.watch(
                &config.rules_file,
                &config.state_file,
                cpuguard::app::service::WatchOptions {
                    name: &name,
                    limit,
                    trigger_cpu,
                    release_cpu,
                    args_contains,
                },
                domain,
                WatchRuntime {
                    cpuguard_bin: cpuguard_bin.to_string_lossy().as_ref(),
                    cpulimit_bin: config.cpulimit_bin.to_string_lossy().as_ref(),
                    config_dir: config.config_dir.to_string_lossy().as_ref(),
                },
            )?;
            println!(
                "watch active: {} {}% trigger={:.1} release={:.1} ({})",
                rule.name, rule.limit, rule.trigger_cpu, rule.release_cpu, rule.domain
            );
        }
        Commands::Unwatch { name } => {
            let removed = service.unwatch(&config.rules_file, &config.state_file, &name, domain)?;
            if removed {
                println!("watch removed: {name}");
            } else {
                println!("watch not found: {name}");
            }
        }
        Commands::Watches => {
            print_watches(&config, domain)?;
        }
        Commands::Top {
            limit,
            count,
            refresh,
            once,
            pid,
            risk_cpu,
            allow_kill,
        } => {
            validate_limit(limit)?;
            validate_cpu_threshold("risk-cpu", risk_cpu)?;
            let target_pid = match pid {
                Some(v) => v,
                None => {
                    match pick_pid_from_live_top(
                        count,
                        refresh,
                        risk_cpu,
                        allow_kill,
                        &config.state_file,
                    )? {
                        Some(pid) => pid,
                        None => {
                            println!("bye.");
                            return Ok(());
                        }
                    }
                }
            };

            if once {
                let cpulimit_pid =
                    service.limit_once(&config.state_file, target_pid, limit, domain)?;
                println!(
                    "one-shot limit started: target={} cpulimit_pid={cpulimit_pid}",
                    target_pid
                );
            } else {
                let name = process_name(target_pid)?
                    .ok_or_else(|| anyhow::anyhow!("target process not found"))?;
                let cpuguard_bin = std::env::current_exe()?;
                let _ = service.watch(
                    &config.rules_file,
                    &config.state_file,
                    cpuguard::app::service::WatchOptions {
                        name: &name,
                        limit,
                        trigger_cpu: cpuguard::model::DEFAULT_TRIGGER_CPU,
                        release_cpu: cpuguard::model::DEFAULT_RELEASE_CPU,
                        args_contains: None,
                    },
                    domain,
                    WatchRuntime {
                        cpuguard_bin: cpuguard_bin.to_string_lossy().as_ref(),
                        cpulimit_bin: config.cpulimit_bin.to_string_lossy().as_ref(),
                        config_dir: config.config_dir.to_string_lossy().as_ref(),
                    },
                )?;
                println!("top default action applied as watch: {} {}%", name, limit);
            }
        }
        Commands::Status => {
            print_status(&config, domain)?;
        }
        Commands::Clean { yes } => {
            if !yes {
                bail!("clean requires --yes");
            }
            let (rules_removed, stopped) =
                service.clean_all(&config.rules_file, &config.state_file, domain)?;
            println!("cleaned watch rules: {rules_removed}, managed instances: {stopped}");
        }
        Commands::InstallAgent => {
            service.executor.ensure_available()?;
            let cpuguard_bin = std::env::current_exe()?;
            let label = service.launchd.ensure_agent(
                domain,
                cpuguard_bin.to_string_lossy().as_ref(),
                config.cpulimit_bin.to_string_lossy().as_ref(),
                config.config_dir.to_string_lossy().as_ref(),
            )?;
            println!("agent active: {label}");
        }
    }

    Ok(())
}

fn show_dashboard(config: &AppConfig, domain: Domain) -> Result<()> {
    println!("CPU Guard Dashboard");
    println!();
    println!("AGENT");
    print_agent_status(config, domain);
    println!();
    println!("WATCH RULES");
    print_watches(config, domain)?;
    println!();
    println!("LIMITED PROCESSES");
    print_status(config, domain)?;
    println!();
    println!(
        "Tip: use `cpuguard top` to pick a new target; use `cpuguard status` for instances only."
    );
    Ok(())
}

fn print_agent_status(config: &AppConfig, domain: Domain) {
    let launchd_status = match agent_loaded_status(&config.label_prefix, domain) {
        Some(true) => "loaded",
        Some(false) => "missing",
        None => "skipped",
    };
    println!("{}  {}", fit_col("DOMAIN", 8), fit_col("LAUNCHD", 10));
    println!(
        "{}  {}",
        fit_col(&domain.to_string(), 8),
        fit_col(launchd_status, 10)
    );
}

fn print_watches(config: &AppConfig, domain: Domain) -> Result<()> {
    let rules = store::load_rules(&config.rules_file)?;
    let domain_rules = rules
        .rules
        .into_iter()
        .filter(|rule| rule.domain == domain)
        .collect::<Vec<_>>();
    if domain_rules.is_empty() {
        println!("no watch rules");
        return Ok(());
    }
    let state = store::load_state(&config.state_file)?;
    let running_watch_rules = state
        .instances
        .iter()
        .filter(|instance| instance.mode == ManagedMode::Watch)
        .filter(|instance| process_alive(instance.cpulimit_pid))
        .filter_map(|instance| running_watch_rule_key(instance).ok().flatten())
        .collect::<HashSet<_>>();
    let current_user = current_user().ok().flatten();

    println!(
        "{}  {}  {}  {}  {}  {}",
        fit_col("NAME", 28),
        fit_col("LIMIT", 8),
        fit_col("DOMAIN", 8),
        fit_col("LAUNCHD", 10),
        fit_col("TARGET", 22),
        fit_col("HINT", 20)
    );
    for r in domain_rules {
        let launchd_status = match agent_loaded_status(&config.label_prefix, r.domain) {
            Some(true) => "loaded",
            Some(false) => "missing",
            None => "skipped",
        };
        let target_pid = first_matching_pid_for_rule(&r)?;
        let target = match target_pid {
            Some(pid) => format!("PID {pid}"),
            None => "waiting".to_string(),
        };
        let hint = watch_hint(
            &r,
            launchd_status,
            target_pid,
            &running_watch_rules,
            current_user.as_deref(),
        )?;
        println!(
            "{}  {}  {}  {}  {}  {}",
            fit_col(&r.name, 28),
            fit_col(&format!("{}%", r.limit), 8),
            fit_col(&r.domain.to_string(), 8),
            fit_col(launchd_status, 10),
            fit_col(&target, 22),
            fit_col(&hint, 20)
        );
    }
    Ok(())
}

fn running_watch_rule_key(
    instance: &cpuguard::model::ManagedInstance,
) -> Result<Option<(Domain, String)>> {
    if let Some(name) = &instance.rule_name {
        return Ok(Some((instance.domain, name.clone())));
    }
    match &instance.target {
        ManagedTarget::Pid(pid) => Ok(process_name(*pid)?.map(|name| (instance.domain, name))),
        ManagedTarget::Name(name) => Ok(Some((instance.domain, name.clone()))),
    }
}

fn watch_hint(
    rule: &cpuguard::model::Rule,
    launchd_status: &str,
    target_pid: Option<u32>,
    running_watch_rules: &HashSet<(Domain, String)>,
    current_user: Option<&str>,
) -> Result<String> {
    let has_running_instance = running_watch_rules.contains(&(rule.domain, rule.name.clone()));
    let owner = match target_pid {
        Some(pid) => process_owner(pid)?,
        None => None,
    };
    Ok(watch_hint_for_owner(
        rule.domain,
        launchd_status,
        target_pid.is_some(),
        has_running_instance,
        owner.as_deref(),
        current_user,
    ))
}

fn watch_hint_for_owner(
    domain: Domain,
    launchd_status: &str,
    target_exists: bool,
    has_running_instance: bool,
    owner: Option<&str>,
    current_user: Option<&str>,
) -> String {
    if domain == Domain::User
        && launchd_status == "loaded"
        && target_exists
        && !has_running_instance
        && owner
            .zip(current_user)
            .is_some_and(|(owner, current_user)| owner != current_user)
    {
        "use --domain system".to_string()
    } else {
        String::new()
    }
}

fn current_user() -> Result<Option<String>> {
    let output = std::process::Command::new("id").arg("-un").output()?;
    if !output.status.success() {
        return Ok(None);
    }
    let user = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok((!user.is_empty()).then_some(user))
}

fn process_owner(pid: u32) -> Result<Option<String>> {
    let output = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "user="])
        .output()?;
    if !output.status.success() {
        return Ok(None);
    }
    let owner = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok((!owner.is_empty()).then_some(owner))
}

fn print_status(config: &AppConfig, domain: Domain) -> Result<()> {
    let state = store::load_state(&config.state_file)?;
    let instances = state
        .instances
        .into_iter()
        .filter(|instance| instance.domain == domain)
        .collect::<Vec<_>>();
    if instances.is_empty() {
        println!("no managed instances");
        return Ok(());
    }

    println!(
        "{}  {}  {}  {}  {}  {}  {}  {}",
        fit_col("ID", 16),
        fit_col("DOMAIN", 8),
        fit_col("RULE", 24),
        fit_col("MODE", 8),
        fit_col("CPULIMIT", 10),
        fit_col("TARGET", 42),
        fit_col("CPU", 8),
        fit_col("STATE", 8)
    );
    for i in instances {
        let target_desc = target_description(&i.target)?;
        let state_text = if process_alive(i.cpulimit_pid) {
            "running"
        } else {
            "stale"
        };
        let rule_name = i.rule_name.as_deref().unwrap_or("-");
        let cpu = i
            .last_observed_cpu
            .map(|cpu| format!("{cpu:.1}"))
            .unwrap_or_else(|| "-".to_string());
        println!(
            "{}  {}  {}  {}  {}  {}  {}  {}",
            fit_col(&i.id, 16),
            fit_col(&i.domain.to_string(), 8),
            fit_col(rule_name, 24),
            fit_col(&format!("{:?}", i.mode).to_lowercase(), 8),
            fit_col(&i.cpulimit_pid.to_string(), 10),
            fit_col(&target_desc, 42),
            fit_col(&cpu, 8),
            fit_col(state_text, 8)
        );
    }
    Ok(())
}

fn target_description(target: &ManagedTarget) -> Result<String> {
    match target {
        ManagedTarget::Pid(pid) => match process_name(*pid)? {
            Some(name) => Ok(format!("PID {pid} {name}")),
            None => Ok(format!("PID {pid} (exited)")),
        },
        ManagedTarget::Name(name) => match first_pid_by_name(name)? {
            Some(pid) => Ok(format!("PID {pid} {name}")),
            None => Ok(format!("{name} (waiting)")),
        },
    }
}

fn run_agent_loop(
    config_dir: std::path::PathBuf,
    cpulimit_bin: std::path::PathBuf,
    domain: Domain,
) -> Result<()> {
    let executor = RealCpulimitExecutor { bin: cpulimit_bin };
    executor.ensure_available()?;
    let rules_file = config_dir.join("rules.toml");
    let state_file = config_dir.join("state.json");
    let agent = Agent {
        executor: &executor,
        rules_file: &rules_file,
        state_file: &state_file,
        domain,
    };
    let mut runtime = AgentRuntime::new();
    loop {
        match agent.tick(&mut runtime) {
            Ok(activity) => thread::sleep(activity.next_sleep()),
            Err(err) => {
                eprintln!("cpuguard agent tick failed: {err:#}");
                thread::sleep(AgentTickActivity::Idle.next_sleep());
            }
        }
    }
}

fn validate_limit(limit: u16) -> Result<()> {
    if !(1..=1200).contains(&limit) {
        bail!("limit must be between 1 and 1200");
    }
    Ok(())
}

fn validate_thresholds(trigger_cpu: f32, release_cpu: f32) -> Result<()> {
    validate_cpu_threshold("trigger-cpu", trigger_cpu)?;
    validate_cpu_threshold("release-cpu", release_cpu)?;
    if release_cpu > trigger_cpu {
        bail!("release-cpu must be less than or equal to trigger-cpu");
    }
    Ok(())
}

fn validate_cpu_threshold(name: &str, value: f32) -> Result<()> {
    if !value.is_finite() || !(0.0..=1200.0).contains(&value) {
        bail!("{name} must be between 0 and 1200");
    }
    Ok(())
}

fn first_matching_pid_for_rule(rule: &cpuguard::model::Rule) -> Result<Option<u32>> {
    if rule.args_contains.is_none() {
        return first_pid_by_name(&rule.name);
    }
    for entry in all_processes()? {
        let basename = std::path::Path::new(&entry.name)
            .file_name()
            .map(|s| s.to_string_lossy())
            .unwrap_or_default();
        if basename != rule.name {
            continue;
        }
        if let Some(needle) = &rule.args_contains
            && process_args(entry.pid)?
                .as_deref()
                .is_some_and(|args| args.contains(needle))
        {
            return Ok(Some(entry.pid));
        }
    }
    Ok(None)
}

fn parse_prefixed_choice(input: &str, prefix: char) -> Option<usize> {
    input
        .strip_prefix(prefix)
        .or_else(|| input.strip_prefix(prefix.to_ascii_uppercase()))
        .and_then(|rest| rest.trim().parse::<usize>().ok())
}

fn choice_index(choice: usize, len: usize) -> Option<usize> {
    let idx = choice.checked_sub(1)?;
    (idx < len).then_some(idx)
}

fn current_snapshot_name_matches<'a>(
    list: &'a [ProcessEntry],
    target_name: &str,
) -> Vec<&'a ProcessEntry> {
    list.iter()
        .filter(|entry| entry.name == target_name)
        .collect()
}

fn limited_target_pids(state: &StateFile) -> std::collections::HashSet<u32> {
    state
        .instances
        .iter()
        .filter(|instance| process_alive(instance.cpulimit_pid))
        .filter_map(|instance| match instance.target {
            ManagedTarget::Pid(pid) => Some(pid),
            ManagedTarget::Name(_) => None,
        })
        .collect()
}

fn top_prompt(refresh_secs: u64, allow_kill: bool) -> String {
    if allow_kill {
        format!(
            "每{}秒自动刷新，输入序号限速，k<序号>终止高风险进程，x<序号>批量终止同名进程，q退出，回车立即刷新: ",
            refresh_secs
        )
    } else {
        format!(
            "每{}秒自动刷新，输入序号限速，q退出，回车立即刷新（终止命令需 --allow-kill）: ",
            refresh_secs
        )
    }
}

fn pick_pid_from_live_top(
    count: usize,
    refresh_secs: u64,
    risk_cpu: f32,
    allow_kill: bool,
    state_file: &std::path::Path,
) -> Result<Option<u32>> {
    let (tx, rx) = mpsc::channel::<String>();
    thread::spawn(move || {
        let stdin = std::io::stdin();
        loop {
            let mut buf = String::new();
            if stdin.read_line(&mut buf).is_err() {
                break;
            }
            if tx.send(buf).is_err() {
                break;
            }
        }
    });

    let interval = Duration::from_secs(refresh_secs.max(1));
    loop {
        let list = top_processes(count)?;
        if list.is_empty() {
            return Ok(None);
        }

        // 预计算每个进程的风险提示标记；这不是“应终止”的判断。
        let risk_flags: Vec<bool> = list
            .iter()
            .map(|p| is_high_risk_process(p, risk_cpu, RISK_ELAPSED_THRESHOLD_SECS))
            .collect();
        let state = store::load_state(state_file)?;
        let limited_pids = limited_target_pids(&state);

        print!("\x1B[2J\x1B[H");
        println!(
            "{}  {}  {}  {}  {}  NAME",
            fit_col("#", 4),
            fit_col("PID", 8),
            fit_col("CPU", 7),
            fit_col("LIMITED", 8),
            fit_col("RISK", 8),
        );
        for (idx, p) in list.iter().enumerate() {
            let limited_label = if limited_pids.contains(&p.pid) {
                fit_col("YES", 8)
            } else {
                fit_col("", 8)
            };
            let risk_label = if risk_flags[idx] {
                "\x1B[33mHIGH\x1B[0m".to_string()
            } else {
                fit_col("", 8)
            };
            println!(
                "{}  {}  {}  {}  {}  {}",
                fit_col(&(idx + 1).to_string(), 4),
                fit_col(&p.pid.to_string(), 8),
                fit_col(&format!("{:.1}", p.cpu), 7),
                limited_label,
                risk_label,
                p.name
            );
        }
        println!();
        print!("{}", top_prompt(interval.as_secs(), allow_kill));
        std::io::stdout().flush()?;

        match rx.recv_timeout(interval) {
            Ok(line) => {
                let input = line.trim();
                if input.eq_ignore_ascii_case("q") {
                    return Ok(None);
                }
                if input.is_empty() {
                    continue;
                }

                if !allow_kill
                    && (parse_prefixed_choice(input, 'k').is_some()
                        || parse_prefixed_choice(input, 'x').is_some())
                {
                    println!("终止命令需要显式传入 --allow-kill");
                    thread::sleep(Duration::from_secs(1));
                    continue;
                }

                // 处理 k<N> kill 高风险进程命令
                if let Some(choice) = parse_prefixed_choice(input, 'k') {
                    let Some(idx) = choice_index(choice, list.len()) else {
                        println!("序号超出范围");
                        thread::sleep(Duration::from_secs(1));
                        continue;
                    };
                    if !risk_flags[idx] {
                        println!("该进程未被标记为高风险，无法通过 k 命令终止");
                        thread::sleep(Duration::from_secs(1));
                        continue;
                    }
                    let target = &list[idx];
                    print!("确认终止进程 {} (PID {})? [y/N]: ", target.name, target.pid);
                    std::io::stdout().flush()?;

                    // 等待用户确认（最多 30 秒）
                    match rx.recv_timeout(Duration::from_secs(30)) {
                        Ok(confirm) => {
                            if confirm.trim().eq_ignore_ascii_case("y") {
                                match kill_process(target.pid) {
                                    Ok(()) => {
                                        println!(
                                            "已发送 SIGTERM 到进程 {} (PID {})",
                                            target.name, target.pid
                                        );
                                    }
                                    Err(e) => {
                                        println!("终止失败: {e}");
                                    }
                                }
                            } else {
                                println!("已取消");
                            }
                        }
                        Err(_) => {
                            println!("确认超时，已取消");
                        }
                    }
                    thread::sleep(Duration::from_secs(1));
                    continue;
                }

                // 处理 x<N> 批量终止当前快照中同名进程
                if let Some(choice) = parse_prefixed_choice(input, 'x') {
                    let Some(idx) = choice_index(choice, list.len()) else {
                        println!("序号超出范围");
                        thread::sleep(Duration::from_secs(1));
                        continue;
                    };
                    let target = &list[idx];
                    if is_protected_system_process(&target.name) {
                        println!("系统进程不允许通过 x 命令批量终止");
                        thread::sleep(Duration::from_secs(1));
                        continue;
                    }
                    let targets = current_snapshot_name_matches(&list, &target.name);
                    let pid_list = targets
                        .iter()
                        .map(|entry| entry.pid.to_string())
                        .collect::<Vec<_>>()
                        .join(", ");
                    print!(
                        "确认终止当前列表中 NAME 相同的 {} 个进程 \"{}\"? PIDs: {} [y/N]: ",
                        targets.len(),
                        target.name,
                        pid_list
                    );
                    std::io::stdout().flush()?;

                    match rx.recv_timeout(Duration::from_secs(30)) {
                        Ok(confirm) => {
                            if confirm.trim().eq_ignore_ascii_case("y") {
                                let mut stopped = 0usize;
                                let mut failed = Vec::new();
                                for entry in targets {
                                    match kill_process(entry.pid) {
                                        Ok(()) => stopped += 1,
                                        Err(e) => failed.push(format!("PID {}: {e}", entry.pid)),
                                    }
                                }

                                if failed.is_empty() {
                                    println!("已发送 SIGTERM 到 {} 个进程", stopped);
                                } else {
                                    println!(
                                        "已发送 SIGTERM 到 {} 个进程，失败 {} 个: {}",
                                        stopped,
                                        failed.len(),
                                        failed.join("; ")
                                    );
                                }
                            } else {
                                println!("已取消");
                            }
                        }
                        Err(_) => {
                            println!("确认超时，已取消");
                        }
                    }
                    thread::sleep(Duration::from_secs(1));
                    continue;
                }

                // 处理数字序号：选择进程进行限速
                let choice = input.parse::<usize>().ok().unwrap_or(0);
                if (1..=list.len()).contains(&choice) {
                    return Ok(Some(list[choice - 1].pid));
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_entry(pid: u32, name: &str) -> ProcessEntry {
        ProcessEntry {
            pid,
            ppid: 1,
            name: name.to_string(),
            cpu: 80.0,
            elapsed_secs: 3600,
        }
    }

    #[test]
    fn parse_prefixed_choice_supports_uppercase_and_spaces() {
        assert_eq!(parse_prefixed_choice("x3", 'x'), Some(3));
        assert_eq!(parse_prefixed_choice("X 12", 'x'), Some(12));
        assert_eq!(parse_prefixed_choice("k7", 'k'), Some(7));
        assert_eq!(parse_prefixed_choice("z1", 'x'), None);
    }

    #[test]
    fn choice_index_rejects_out_of_range_values() {
        assert_eq!(choice_index(1, 3), Some(0));
        assert_eq!(choice_index(3, 3), Some(2));
        assert_eq!(choice_index(0, 3), None);
        assert_eq!(choice_index(4, 3), None);
    }

    #[test]
    fn current_snapshot_name_matches_use_exact_name() {
        let list = vec![
            sample_entry(101, "/Users/demo/bin/codex"),
            sample_entry(102, "/Users/demo/bin/codex"),
            sample_entry(103, "/Users/demo/bin/codex-helper"),
            sample_entry(104, "/usr/libexec/syspolicyd"),
        ];

        let matches = current_snapshot_name_matches(&list, "/Users/demo/bin/codex");
        let pids: Vec<u32> = matches.iter().map(|entry| entry.pid).collect();
        assert_eq!(pids, vec![101, 102]);
    }

    #[test]
    fn top_prompt_hides_kill_commands_by_default() {
        let prompt = top_prompt(5, false);
        assert!(!prompt.contains("k<序号>"));
        assert!(!prompt.contains("x<序号>"));
        assert!(prompt.contains("--allow-kill"));

        let kill_prompt = top_prompt(5, true);
        assert!(kill_prompt.contains("k<序号>"));
        assert!(kill_prompt.contains("x<序号>"));
    }

    #[test]
    fn watch_hint_suggests_system_domain_for_other_owner() {
        assert_eq!(
            watch_hint_for_owner(
                Domain::User,
                "loaded",
                true,
                false,
                Some("root"),
                Some("demo")
            ),
            "use --domain system"
        );
        assert_eq!(
            watch_hint_for_owner(
                Domain::User,
                "loaded",
                true,
                true,
                Some("root"),
                Some("demo")
            ),
            ""
        );
        assert_eq!(
            watch_hint_for_owner(
                Domain::System,
                "loaded",
                true,
                false,
                Some("root"),
                Some("demo")
            ),
            ""
        );
    }

    #[test]
    fn limited_target_pids_includes_running_managed_pid_targets() {
        let self_pid = std::process::id();
        let state = StateFile {
            version: 2,
            instances: vec![
                cpuguard::model::ManagedInstance {
                    id: "watch_running".to_string(),
                    mode: cpuguard::model::ManagedMode::Watch,
                    cpulimit_pid: self_pid,
                    target: ManagedTarget::Pid(101),
                    rule_name: Some("demo".to_string()),
                    last_observed_cpu: Some(80.0),
                    domain: Domain::User,
                    started_at: chrono::Local::now(),
                    owner_label: Some("com.cpuguard.agent".to_string()),
                },
                cpuguard::model::ManagedInstance {
                    id: "adhoc_running".to_string(),
                    mode: cpuguard::model::ManagedMode::Adhoc,
                    cpulimit_pid: self_pid,
                    target: ManagedTarget::Pid(102),
                    rule_name: None,
                    last_observed_cpu: None,
                    domain: Domain::User,
                    started_at: chrono::Local::now(),
                    owner_label: None,
                },
                cpuguard::model::ManagedInstance {
                    id: "watch_stale".to_string(),
                    mode: cpuguard::model::ManagedMode::Watch,
                    cpulimit_pid: u32::MAX,
                    target: ManagedTarget::Pid(103),
                    rule_name: Some("demo".to_string()),
                    last_observed_cpu: Some(80.0),
                    domain: Domain::User,
                    started_at: chrono::Local::now(),
                    owner_label: Some("com.cpuguard.agent".to_string()),
                },
            ],
        };

        let pids = limited_target_pids(&state);
        assert!(pids.contains(&101));
        assert!(pids.contains(&102));
        assert!(!pids.contains(&103));
    }
}
