use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use std::io::Write;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use cpulimit_top::config::AppConfig;
use cpulimit_top::cpulimit::RealCpulimitExecutor;
use cpulimit_top::launchd::{RealLaunchdManager, watch_loaded_status};
use cpulimit_top::model::{Domain, ManagedTarget};
use cpulimit_top::process_snapshot::{process_name, top_processes};
use cpulimit_top::runtime::{first_pid_by_name, process_alive};
use cpulimit_top::service::Service;
use cpulimit_top::store;

#[derive(Parser, Debug)]
#[command(version, about = "cpulimit-top: macOS cpulimit manager")]
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
    },
    Status,
    Clean {
        #[arg(long)]
        yes: bool,
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

    match cli.command.unwrap_or(Commands::Top {
        limit: 20,
        count: 10,
        refresh: 5,
        once: false,
        pid: None,
    }) {
        Commands::Watch { name, limit } => {
            validate_limit(limit)?;
            let rule = service.watch(
                &config.rules_file,
                &config.state_file,
                &name,
                limit,
                domain,
                config.cpulimit_bin.to_string_lossy().as_ref(),
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
        } => {
            validate_limit(limit)?;
            let target_pid = match pid {
                Some(v) => v,
                None => match pick_pid_from_live_top(count, refresh)? {
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
                let _ = service.watch(
                    &config.rules_file,
                    &config.state_file,
                    &name,
                    limit,
                    domain,
                    config.cpulimit_bin.to_string_lossy().as_ref(),
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
                service.clean_all(&config.rules_file, &config.state_file)?;
            println!("cleaned watch rules: {rules_removed}, managed instances: {stopped}");
        }
    }

    Ok(())
}

fn validate_limit(limit: u16) -> Result<()> {
    if !(1..=1200).contains(&limit) {
        bail!("limit must be between 1 and 1200");
    }
    Ok(())
}

fn fit_col(input: &str, width: usize) -> String {
    let len = input.chars().count();
    if len == width {
        return input.to_string();
    }
    if len < width {
        return format!("{input:<width$}");
    }
    if width <= 3 {
        return ".".repeat(width);
    }
    let mut out = String::with_capacity(width);
    for ch in input.chars().take(width - 3) {
        out.push(ch);
    }
    out.push_str("...");
    out
}

fn pick_pid_from_live_top(count: usize, refresh_secs: u64) -> Result<Option<u32>> {
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

        print!("\x1B[2J\x1B[H");
        println!(
            "{}  {}  {}  {}",
            fit_col("#", 4),
            fit_col("PID", 8),
            fit_col("CPU", 7),
            "NAME"
        );
        for (idx, p) in list.iter().enumerate() {
            println!(
                "{}  {}  {}  {}",
                fit_col(&(idx + 1).to_string(), 4),
                fit_col(&p.pid.to_string(), 8),
                fit_col(&format!("{:.1}", p.cpu), 7),
                p.name
            );
        }
        println!();
        print!(
            "每{}秒自动刷新，输入序号限速，q退出，回车立即刷新: ",
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
