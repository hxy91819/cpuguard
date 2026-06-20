# 02 Architecture

## 1. 模块划分
- `cli`: 参数解析与命令分发（基于 clap derive）。
- `process_snapshot`: 单次刷新进程快照，输出统一模型。
- `cpulimit_driver`: 统一执行 `cpulimit` 命令与参数构造。
- `rule_store`: 读写 `rules.toml`（原子落盘）。
- `launchd_manager`: 生成、加载、卸载单一 `cpuguard agent` 的 `LaunchAgents`/`LaunchDaemons`。
- `instance_registry`: 记录并校验本工具托管实例。
- `conflict_guard`: `watch` 启动前检测并处理 ad-hoc 冲突实例。
- `agent_loop`: 常驻低开销协调器，共享一次进程快照评估全部规则，按需启动/停止外部 `cpulimit`。
- `cleaner`: 托管对象清理编排。

## 2. 组件图
```mermaid
flowchart LR
    CLI[CLI Layer] --> ORCH[Command Orchestrator]
    ORCH --> SNAP[process_snapshot]
    ORCH --> RULE[rule_store]
    ORCH --> LD[launchd_manager]
    ORCH --> REG[instance_registry]
    ORCH --> CG[conflict_guard]
    ORCH --> DRV[cpulimit_driver]
    ORCH --> AG[agent_loop]

    RULE --> FS1[(rules.toml)]
    REG --> FS2[(state.json)]
    LD --> LDCMD[launchctl]
    AG --> SNAP
    AG --> RULE
    AG --> REG
    AG --> DRV
    DRV --> CPL[cpulimit binary]
```

## 3. watch 时序图（单 agent）
```mermaid
sequenceDiagram
    participant U as User
    participant C as CLI
    participant R as rule_store
    participant G as conflict_guard
    participant L as launchd_manager

    U->>C: watch ztsmedr --limit 20
    C->>G: check_adhoc_conflict(name=ztsmedr)
    G-->>C: managed conflict stopped / external conflict blocked
    C->>R: upsert(rule)
    R-->>C: ok
    C->>L: ensure_agent(label=com.cpuguard.agent)
    C->>L: bootstrap current domain agent if missing
    L-->>C: ok
    C-->>U: rule saved + agent active
```

## 4. agent 限速时序
```mermaid
sequenceDiagram
    participant A as cpuguard agent
    participant P as process_snapshot
    participant R as rule_store
    participant S as instance_registry
    participant D as cpulimit_driver

    A->>R: load rules
    A->>P: sample_once()
    P-->>A: process snapshot
    A->>A: match all rules against one snapshot
    A->>A: apply trigger/release hysteresis
    A->>D: start cpulimit -p for hot unmanaged PIDs
    D-->>A: cpulimit_pid
    A->>S: record managed instance
    A->>D: stop cpulimit for exited/cold/deleted targets
    A->>S: prune managed instance
    A->>A: adaptive sleep/backoff
```

## 5. top 行为时序（默认 watch，`--once` 例外）
```mermaid
sequenceDiagram
    participant U as User
    participant C as CLI
    participant P as process_snapshot
    participant G as conflict_guard
    participant R as rule_store
    participant D as cpulimit_driver

    U->>C: top --limit 20
    C->>P: sample_once(count=K)
    P-->>C: top processes
    C->>C: user selects process
    C->>G: check_adhoc_conflict(name)
    G-->>C: pass
    C->>R: upsert(rule from selected process name)
    C->>D: ensure single cpuguard agent
    D-->>U: managed watch rule created
```

## 6. clean 时序图（零误杀）
```mermaid
sequenceDiagram
    participant U as User
    participant C as CLI
    participant G as cleaner
    participant R as instance_registry
    participant L as launchd_manager

    U->>C: clean --yes
    C->>G: cleanup_managed_only()
    G->>R: list_managed_instances()
    R-->>G: managed set
    G->>L: list_managed_labels(prefix)
    L-->>G: managed labels
    G->>G: intersect + verify ownership
    G->>G: stop/remove managed only
    G-->>C: summary
    C-->>U: cleaned N managed objects
```

## 7. 依赖边界图
```mermaid
flowchart TD
    A[Business Modules] --> B[OS Adapters]
    B --> C[launchctl]
    B --> D[cpulimit]
    B --> E[process API]

    F[Persistence] --> G[rules.toml]
    F --> H[state.json]

    I[Forbidden in core logic] -.-> J[Global ps|grep fuzzy kill]
    I -.-> K[Per-rule LaunchAgent fan-out]
```

## 8. 关键实现约束
- 所有系统调用通过 adapter 层，便于测试替身。
- `top/status` 在单次命令生命周期内只刷新一次进程快照。
- `clean` 必须在“实例登记 + label 前缀”双重验证通过后执行。
- `watch` 和 `top` 默认托管路径在启动前必须走 `conflict_guard`。
- `watch` 和 `top` 默认托管路径只能确保单一 `com.cpuguard.agent`，不得为每条规则生成独立 plist。
- agent 共享一次进程快照评估全部规则；规则数量增长不应线性增加扫描次数。
- agent 使用 `trigger_cpu`/`release_cpu` 滞回和异常 backoff，避免 `launchd` 高速重启或 `cpulimit` 高频重建。
- 对于 state 中已经登记且存活的 target PID，agent 每轮会收敛同 target 上未登记的重复 `cpulimit`；没有 state 托管关系的外部 `cpulimit` 仍不自动接管或清理。
