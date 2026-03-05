use std::process::Command;

use anyhow::Result;

#[derive(Debug, Clone)]
pub struct ProcessEntry {
    pub pid: u32,
    pub name: String,
    pub cpu: f32,
}

pub fn top_processes(count: usize) -> Result<Vec<ProcessEntry>> {
    let out = Command::new("ps")
        .args(["-Ao", "pid=,pcpu=,comm="])
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

fn parse_ps_line(line: &str) -> Option<ProcessEntry> {
    let mut parts = line.split_whitespace();
    let pid = parts.next()?.parse::<u32>().ok()?;
    let cpu = parts.next()?.parse::<f32>().ok()?;
    let name = parts.collect::<Vec<&str>>().join(" ");
    Some(ProcessEntry { pid, cpu, name })
}
