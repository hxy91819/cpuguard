use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use std::io::Write;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use cpuguard::app::service::{Service, WatchRuntime};
use cpuguard::cli::output::fit_col;
use cpuguard::infra::config::AppConfig;
use cpuguard::infra::cpulimit::{CpulimitExecutor, RealCpulimitExecutor};
use cpuguard::infra::launchd::{RealLaunchdManager, watch_loaded_status};
use cpuguard::infra::process_snapshot::{
    ProcessEntry, is_protected_system_process, is_suspicious_orphan, process_name, top_processes,
};
use cpuguard::infra::runtime::{first_pid_by_name, kill_process, process_alive};
use cpuguard::model::{Domain, ManagedTarget};
use cpuguard::store;

/// 孤儿进程运行时间阈值：30 分钟（1800 秒）。
const ORPHAN_ELAPSED_THRESHOLD_SECS: u64 = 1800;

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
        /// 可疑孤儿进程的 CPU 阈值（百分比），默认 50.0
        #[arg(long, default_value_t = 50.0)]
        orphan_cpu: f32,
    },
    Status,
    Clean {
        #[arg(long)]
        yes: bool,
    },
    #[command(name = "__watch-runner", hide = true)]
    WatchRunner {
        #[arg(long)]
        name: String,
        #[arg(long)]
        limit: u16,
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

    let command = cli.command.unwrap_or(Commands::Top {
        limit: 20,
        count: 10,
        refresh: 5,
        once: false,
        pid: None,
        orphan_cpu: 50.0,
    });
    match command {
        Commands::WatchRunner {
            name,
            limit,
            cpulimit_bin,
        } => {
            validate_limit(limit)?;
            run_watch_runner(&name, limit, cpulimit_bin)?;
        }
        Commands::Watch { name, limit } => {
            validate_limit(limit)?;
            let cpuguard_bin = std::env::current_exe()?;
            let rule = service.watch(
                &config.rules_file,
                &config.state_file,
                &name,
                limit,
                domain,
                WatchRuntime {
                    cpuguard_bin: cpuguard_bin.to_string_lossy().as_ref(),
                    cpulimit_bin: config.cpulimit_bin.to_string_lossy().as_ref(),
                },
            )?;
            println!(
                "watch active: {} {}% ({})",
                rule.name, rule.limit, rule.domain
            );
        }
        Commands::Unwatch { name } => {
            let removed = service.unwatch(&config.rules_file, &name, domain)?;
            if removed {
                println!("watch removed: {name}");
            } else {
                println!("watch not found: {name}");
            }
        }
        Commands::Watches => {
            let rules = store::load_rules(&config.rules_file)?;
            if rules.rules.is_empty() {
                println!("no watch rules");
            } else {
                println!(
                    "{}  {}  {}  {}  {}",
                    fit_col("NAME", 28),
                    fit_col("LIMIT", 8),
                    fit_col("DOMAIN", 8),
                    fit_col("LAUNCHD", 10),
                    fit_col("TARGET", 22)
                );
                for r in rules.rules {
                    let launchd_status =
                        match watch_loaded_status(&config.label_prefix, &r.name, r.domain) {
                            Some(true) => "loaded",
                            Some(false) => "missing",
                            None => "skipped",
                        };
                    let target = match first_pid_by_name(&r.name)? {
                        Some(pid) => format!("PID {pid}"),
                        None => "waiting".to_string(),
                    };
                    println!(
                        "{}  {}  {}  {}  {}",
                        fit_col(&r.name, 28),
                        fit_col(&format!("{}%", r.limit), 8),
                        fit_col(&r.domain.to_string(), 8),
                        fit_col(launchd_status, 10),
                        fit_col(&target, 22)
                    );
                }
            }
        }
        Commands::Top {
            limit,
            count,
            refresh,
            once,
            pid,
            orphan_cpu,
        } => {
            validate_limit(limit)?;
            let target_pid = match pid {
                Some(v) => v,
                None => match pick_pid_from_live_top(count, refresh, orphan_cpu)? {
                    Some(pid) => pid,
                    None => {
                        println!("bye.");
                        return Ok(());
                    }
                },
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
                    &name,
                    limit,
                    domain,
                    WatchRuntime {
                        cpuguard_bin: cpuguard_bin.to_string_lossy().as_ref(),
                        cpulimit_bin: config.cpulimit_bin.to_string_lossy().as_ref(),
                    },
                )?;
                println!("top default action applied as watch: {} {}%", name, limit);
            }
        }
        Commands::Status => {
            let state = store::load_state(&config.state_file)?;
            if state.instances.is_empty() {
                println!("no managed instances");
            } else {
                println!(
                    "{}  {}  {}  {}  {}",
                    fit_col("ID", 16),
                    fit_col("MODE", 8),
                    fit_col("CPULIMIT", 10),
                    fit_col("TARGET", 42),
                    fit_col("STATE", 8)
                );
                for i in state.instances {
                    let target_desc = match i.target {
                        ManagedTarget::Pid(pid) => match process_name(pid)? {
                            Some(name) => format!("PID {pid} {name}"),
                            None => format!("PID {pid} (exited)"),
                        },
                        ManagedTarget::Name(name) => match first_pid_by_name(&name)? {
                            Some(pid) => format!("PID {pid} {name}"),
                            None => format!("{name} (waiting)"),
                        },
                    };
                    let state_text = if process_alive(i.cpulimit_pid) {
                        "running"
                    } else {
                        "stale"
                    };
                    println!(
                        "{}  {}  {}  {}  {}",
                        fit_col(&i.id, 16),
                        fit_col(&format!("{:?}", i.mode).to_lowercase(), 8),
                        fit_col(&i.cpulimit_pid.to_string(), 10),
                        fit_col(&target_desc, 42),
                        fit_col(state_text, 8)
                    );
                }
            }
        }
        Commands::Clean { yes } => {
            if !yes {
                bail!("clean requires --yes");
            }
            let (rules_removed, stopped) =
                service.clean_all(&config.rules_file, &config.state_file, domain)?;
            println!("cleaned watch rules: {rules_removed}, managed instances: {stopped}");
        }
    }

    Ok(())
}

fn run_watch_runner(name: &str, limit: u16, cpulimit_bin: std::path::PathBuf) -> Result<()> {
    let executor = RealCpulimitExecutor { bin: cpulimit_bin };
    executor.ensure_available()?;
    loop {
        if let Some(pid) = first_pid_by_name(name)? {
            executor.run_for_target(pid, limit)?;
            return Ok(());
        }
        thread::sleep(Duration::from_secs(5));
    }
}

fn validate_limit(limit: u16) -> Result<()> {
    if !(1..=1200).contains(&limit) {
        bail!("limit must be between 1 and 1200");
    }
    Ok(())
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

fn pick_pid_from_live_top(count: usize, refresh_secs: u64, orphan_cpu: f32) -> Result<Option<u32>> {
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

        // 预计算每个进程的孤儿标记
        let orphan_flags: Vec<bool> = list
            .iter()
            .map(|p| is_suspicious_orphan(p, orphan_cpu, ORPHAN_ELAPSED_THRESHOLD_SECS))
            .collect();

        print!("\x1B[2J\x1B[H");
        println!(
            "{}  {}  {}  {}  NAME",
            fit_col("#", 4),
            fit_col("PID", 8),
            fit_col("CPU", 7),
            fit_col("ORPHAN", 8),
        );
        for (idx, p) in list.iter().enumerate() {
            let orphan_label = if orphan_flags[idx] {
                "\x1B[31m*YES*\x1B[0m".to_string()
            } else {
                fit_col("", 8)
            };
            println!(
                "{}  {}  {}  {}  {}",
                fit_col(&(idx + 1).to_string(), 4),
                fit_col(&p.pid.to_string(), 8),
                fit_col(&format!("{:.1}", p.cpu), 7),
                orphan_label,
                p.name
            );
        }
        println!();
        print!(
            "每{}秒自动刷新，输入序号限速，k<序号>终止孤儿进程，x<序号>批量终止同名进程，q退出，回车立即刷新: ",
            interval.as_secs()
        );
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

                // 处理 k<N> kill 孤儿进程命令
                if let Some(choice) = parse_prefixed_choice(input, 'k') {
                    let Some(idx) = choice_index(choice, list.len()) else {
                        println!("序号超出范围");
                        thread::sleep(Duration::from_secs(1));
                        continue;
                    };
                    if !orphan_flags[idx] {
                        println!("该进程未被标记为可疑孤儿，无法通过 k 命令终止");
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
}
