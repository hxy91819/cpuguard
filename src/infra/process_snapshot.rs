use std::process::Command;

use anyhow::Result;

#[derive(Debug, Clone)]
pub struct ProcessEntry {
    pub pid: u32,
    pub ppid: u32,
    pub name: String,
    pub cpu: f32,
    pub elapsed_secs: u64,
}

/// macOS 上 PPID=1 且已知系统进程不应被标记为可疑孤儿。
const KNOWN_SYSTEM_PROCESSES: &[&str] = &[
    "Finder",
    "Dock",
    "WindowServer",
    "loginwindow",
    "SystemUIServer",
    "Spotlight",
    "mds",
    "mds_stores",
    "kernel_task",
    "launchd",
    "syslogd",
    "configd",
    "diskarbitrationd",
    "logd",
    "opendirectoryd",
    "UserEventAgent",
    "sharedfilelistd",
    "trustd",
    "containermanagerd",
    "endpointsecurityd",
];

pub fn top_processes(count: usize) -> Result<Vec<ProcessEntry>> {
    let out = Command::new("ps")
        .args(["-Ao", "pid=,ppid=,pcpu=,etime=,comm="])
        .output()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut list: Vec<ProcessEntry> = text
        .lines()
        .filter_map(parse_ps_line)
        .filter(|p| p.pid > 0 && !p.name.is_empty())
        .collect();

    list.sort_by(|a, b| {
        b.cpu
            .partial_cmp(&a.cpu)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    list.truncate(count);
    Ok(list)
}

pub fn process_name(pid: u32) -> Result<Option<String>> {
    let out = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .output()?;
    if !out.status.success() {
        return Ok(None);
    }
    let raw = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if raw.is_empty() {
        return Ok(None);
    }
    let name = std::path::Path::new(&raw)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or(raw);
    Ok(Some(name))
}

/// 判定进程是否为可疑孤儿进程。
/// 五条件全满足：PPID=1、CPU ≥ 阈值、运行时间 ≥ 阈值、非系统路径、非已知系统进程。
pub fn is_suspicious_orphan(
    entry: &ProcessEntry,
    cpu_threshold: f32,
    elapsed_threshold_secs: u64,
) -> bool {
    entry.ppid == 1
        && entry.cpu >= cpu_threshold
        && entry.elapsed_secs >= elapsed_threshold_secs
        && !is_protected_system_process(&entry.name)
}

pub fn is_protected_system_process(name: &str) -> bool {
    is_system_path(name) || is_known_system_process(name)
}

/// `/System/` 和 `/usr/libexec/` 路径下的进程视为 macOS 系统组件。
fn is_system_path(name: &str) -> bool {
    name.starts_with("/System/") || name.starts_with("/usr/libexec/")
}

/// 用 basename 匹配已知系统进程排除列表。
fn is_known_system_process(name: &str) -> bool {
    let basename = std::path::Path::new(name)
        .file_name()
        .map(|s| s.to_string_lossy())
        .unwrap_or_default();
    KNOWN_SYSTEM_PROCESSES.iter().any(|&s| basename == s)
}

fn parse_ps_line(line: &str) -> Option<ProcessEntry> {
    let mut parts = line.split_whitespace();
    let pid = parts.next()?.parse::<u32>().ok()?;
    let ppid = parts.next()?.parse::<u32>().ok()?;
    let cpu = parts.next()?.parse::<f32>().ok()?;
    let etime = parts.next()?;
    let elapsed_secs = parse_etime(etime)?;
    let name = parts.collect::<Vec<&str>>().join(" ");
    Some(ProcessEntry {
        pid,
        ppid,
        cpu,
        elapsed_secs,
        name,
    })
}

/// 解析 macOS `ps -o etime=` 输出的 elapsed time。
/// 格式：`[[dd-]hh:]mm:ss`
///
/// 例：
/// - `"01:23"` → 83
/// - `"02:01:23"` → 7283
/// - `"1-02:01:23"` → 93683
fn parse_etime(s: &str) -> Option<u64> {
    let (days, rest) = if let Some((d, r)) = s.split_once('-') {
        (d.parse::<u64>().ok()?, r)
    } else {
        (0, s)
    };

    let parts: Vec<&str> = rest.split(':').collect();
    let (hours, minutes, seconds) = match parts.len() {
        2 => {
            let m = parts[0].parse::<u64>().ok()?;
            let s = parts[1].parse::<u64>().ok()?;
            (0, m, s)
        }
        3 => {
            let h = parts[0].parse::<u64>().ok()?;
            let m = parts[1].parse::<u64>().ok()?;
            let s = parts[2].parse::<u64>().ok()?;
            (h, m, s)
        }
        _ => return None,
    };

    Some(days * 86400 + hours * 3600 + minutes * 60 + seconds)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_etime_minutes_seconds() {
        assert_eq!(parse_etime("01:23"), Some(83));
        assert_eq!(parse_etime("00:00"), Some(0));
        assert_eq!(parse_etime("59:59"), Some(3599));
    }

    #[test]
    fn parse_etime_hours_minutes_seconds() {
        assert_eq!(parse_etime("02:01:23"), Some(7283));
        assert_eq!(parse_etime("00:00:01"), Some(1));
        assert_eq!(parse_etime("23:59:59"), Some(86399));
    }

    #[test]
    fn parse_etime_days() {
        assert_eq!(parse_etime("1-02:01:23"), Some(93683));
        assert_eq!(parse_etime("0-00:00"), Some(0));
        assert_eq!(parse_etime("7-00:00:00"), Some(604800));
    }

    #[test]
    fn parse_etime_invalid() {
        assert_eq!(parse_etime(""), None);
        assert_eq!(parse_etime("abc"), None);
        assert_eq!(parse_etime("1:2:3:4"), None);
    }

    #[test]
    fn parse_ps_line_valid() {
        let line = "  1234    1  99.5     02:30 node";
        let entry = parse_ps_line(line).unwrap();
        assert_eq!(entry.pid, 1234);
        assert_eq!(entry.ppid, 1);
        assert!((entry.cpu - 99.5).abs() < 0.01);
        assert_eq!(entry.elapsed_secs, 150);
        assert_eq!(entry.name, "node");
    }

    #[test]
    fn parse_ps_line_with_spaces_in_name() {
        let line = "  5678    100  12.3     1-00:00:00 Google Chrome Helper";
        let entry = parse_ps_line(line).unwrap();
        assert_eq!(entry.pid, 5678);
        assert_eq!(entry.ppid, 100);
        assert_eq!(entry.elapsed_secs, 86400);
        assert_eq!(entry.name, "Google Chrome Helper");
    }

    #[test]
    fn suspicious_orphan_all_conditions_met() {
        let entry = ProcessEntry {
            pid: 1234,
            ppid: 1,
            name: "node".to_string(),
            cpu: 100.0,
            elapsed_secs: 3600,
        };
        assert!(is_suspicious_orphan(&entry, 50.0, 1800));
    }

    #[test]
    fn suspicious_orphan_ppid_not_1() {
        let entry = ProcessEntry {
            pid: 1234,
            ppid: 500,
            name: "node".to_string(),
            cpu: 100.0,
            elapsed_secs: 3600,
        };
        assert!(!is_suspicious_orphan(&entry, 50.0, 1800));
    }

    #[test]
    fn suspicious_orphan_cpu_below_threshold() {
        let entry = ProcessEntry {
            pid: 1234,
            ppid: 1,
            name: "node".to_string(),
            cpu: 49.9,
            elapsed_secs: 3600,
        };
        assert!(!is_suspicious_orphan(&entry, 50.0, 1800));
    }

    #[test]
    fn suspicious_orphan_cpu_at_threshold() {
        let entry = ProcessEntry {
            pid: 1234,
            ppid: 1,
            name: "node".to_string(),
            cpu: 50.0,
            elapsed_secs: 3600,
        };
        assert!(is_suspicious_orphan(&entry, 50.0, 1800));
    }

    #[test]
    fn suspicious_orphan_elapsed_below_threshold() {
        let entry = ProcessEntry {
            pid: 1234,
            ppid: 1,
            name: "node".to_string(),
            cpu: 100.0,
            elapsed_secs: 1799,
        };
        assert!(!is_suspicious_orphan(&entry, 50.0, 1800));
    }

    #[test]
    fn suspicious_orphan_system_process_excluded() {
        let entry = ProcessEntry {
            pid: 1234,
            ppid: 1,
            name: "WindowServer".to_string(),
            cpu: 100.0,
            elapsed_secs: 3600,
        };
        assert!(!is_suspicious_orphan(&entry, 50.0, 1800));
    }

    #[test]
    fn suspicious_orphan_system_process_full_path_excluded() {
        // ps -o comm= 返回完整路径时，basename 匹配排除列表
        let entry = ProcessEntry {
            pid: 164,
            ppid: 1,
            name: "/System/Library/PrivateFrameworks/SkyLight.framework/Resources/WindowServer"
                .to_string(),
            cpu: 100.0,
            elapsed_secs: 86400,
        };
        assert!(!is_suspicious_orphan(&entry, 50.0, 1800));
    }

    #[test]
    fn suspicious_orphan_system_path_excluded() {
        // /System/ 路径下的进程一律排除
        let entry = ProcessEntry {
            pid: 5000,
            ppid: 1,
            name: "/System/Library/Frameworks/SomeNew.framework/SomeNewDaemon".to_string(),
            cpu: 100.0,
            elapsed_secs: 7200,
        };
        assert!(!is_suspicious_orphan(&entry, 50.0, 1800));
    }

    #[test]
    fn suspicious_orphan_usr_libexec_excluded() {
        let entry = ProcessEntry {
            pid: 6000,
            ppid: 1,
            name: "/usr/libexec/some_daemon".to_string(),
            cpu: 80.0,
            elapsed_secs: 7200,
        };
        assert!(!is_suspicious_orphan(&entry, 50.0, 1800));
    }

    #[test]
    fn protected_system_process_matches_known_name_and_system_path() {
        assert!(is_protected_system_process("WindowServer"));
        assert!(is_protected_system_process("/usr/libexec/syspolicyd"));
        assert!(!is_protected_system_process("/Users/demo/bin/codex"));
    }

    #[test]
    fn suspicious_orphan_user_app_not_excluded() {
        // /Applications/ 下的进程不应被系统路径规则排除
        let entry = ProcessEntry {
            pid: 7000,
            ppid: 1,
            name: "/Applications/SomeApp.app/Contents/MacOS/SomeApp".to_string(),
            cpu: 100.0,
            elapsed_secs: 7200,
        };
        assert!(is_suspicious_orphan(&entry, 50.0, 1800));
    }
}
