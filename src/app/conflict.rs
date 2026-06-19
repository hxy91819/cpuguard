use std::collections::HashSet;
use std::process::Command;

use anyhow::Result;

use crate::infra::process_snapshot::{process_args, process_name};

pub fn has_external_adhoc_conflict(
    target_name: &str,
    args_contains: Option<&str>,
    managed_cpulimit_pids: &HashSet<u32>,
) -> Result<bool> {
    let output = Command::new("ps").args(["-eo", "pid=,args="]).output()?;
    if !output.status.success() {
        return Ok(false);
    }

    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        if !line.contains("cpulimit") {
            continue;
        }
        let mut parts = line.split_whitespace();
        let pid = match parts.next().and_then(|x| x.parse::<u32>().ok()) {
            Some(v) => v,
            None => continue,
        };

        if managed_cpulimit_pids.contains(&pid) {
            continue;
        }

        let args: Vec<&str> = line.split_whitespace().collect();
        let mut idx = 0;
        let mut target_pid: Option<u32> = None;
        while idx + 1 < args.len() {
            if args[idx] == "-p" {
                target_pid = args[idx + 1].parse::<u32>().ok();
                break;
            }
            idx += 1;
        }

        if let Some(tp) = target_pid
            && let Some(name) = process_name(tp)?
            && name == target_name
            && args_contains.is_none_or(|needle| {
                process_args(tp)
                    .ok()
                    .flatten()
                    .is_some_and(|args| args.contains(needle))
            })
        {
            return Ok(true);
        }
    }
    Ok(false)
}
