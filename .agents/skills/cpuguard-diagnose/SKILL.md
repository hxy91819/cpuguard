---
name: cpuguard-diagnose
description: Diagnose macOS CPU pressure and recommend safe cpuguard configuration. Use when a user asks why fans/CPU are high, whether cpuguard can help, how to configure cpuguard rules, whether user or system domain is required, or how to identify security, audit, VPN, EDR, MDM, browser, IDE, or developer-tool processes before applying CPU limits.
---

# Cpuguard Diagnose

## Workflow

1. Gather evidence with read-only commands first. Prefer `scripts/collect_snapshot.sh` when available.
2. Classify high-CPU processes by owner, path, command line, and role.
3. Recommend `cpuguard` only for persistent background offenders, not for interactive apps where throttling hurts user work.
4. Choose domain from ownership:
   - Current user owns the target: `--domain user` is usually enough.
   - `root` or a system service account owns the target: recommend `--domain system` and explain sudo is required.
5. Propose conservative rules with `trigger_cpu`, `release_cpu`, and `args_contains` when needed.
6. Verify actual effect with `cpuguard status` and `ps`; `watches loaded` alone does not prove limiting is active.

## Evidence Commands

Use these as read-only probes:

```bash
pmset -g therm
pmset -g assertions
ps -axo pid,ppid,user,%cpu,%mem,stat,comm,args -r | head -n 30
launchctl print gui/$(id -u) | grep cpuguard || true
sudo launchctl print system | grep cpuguard || true
cpuguard watches
cpuguard status
sudo cpuguard --domain system watches
sudo cpuguard --domain system status
```

If sudo is unavailable, do not block. State which checks require sudo and continue from non-sudo evidence.

## Classification Heuristics

Usually good `cpuguard` candidates:

- Security, audit, EDR, MDM, compliance, device-management, DLP, and VPN background daemons.
- Root-owned or launchd-owned helper processes with sustained CPU and no direct interactive UI.
- Repeatedly respawned background services whose process name is stable.

Usually poor long-term `cpuguard` candidates:

- Browsers, Electron apps, IDEs, compilers, language servers, terminals, containers, and Codex-like developer tools while the user is actively working.
- macOS core services such as `WindowServer`, `kernel_task`, `syspolicyd`, `trustd`, `mds`, `launchd`, `powerd`, and driver processes. For these, recommend identifying the trigger rather than throttling.

Ambiguous cases:

- If an app has many same-name helpers, use `args_contains` to target only the hot helper.
- If the process owner is `root`, user-domain rules may load but not limit; recommend system domain.
- If `cpulimit` appears but target CPU remains high, confirm the `cpulimit -p <pid>` PID matches the hot target and is not stale or defunct.

## Recommendation Format

Report:

- Current state: top offenders, thermal warnings, existing `cpuguard` agent domain, running `cpulimit` instances.
- Suitability: which targets are good candidates, which should not be throttled, and why.
- Commands: exact `cpuguard watch`/`install-agent` commands, using placeholders only when the target is uncertain.
- Verification: commands to prove the rule is active.

Avoid including local usernames, home paths, full proprietary paths, PIDs, or company-specific names in reusable docs or skill edits. It is fine to show them in a one-off user response when they came from the user's live machine and are necessary for diagnosis.
