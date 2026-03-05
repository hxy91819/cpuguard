# 02 Architecture

## 1. 模块划分
- `cli`: 参数解析与命令分发（基于 clap derive）。
- `process_snapshot`: 单次刷新进程快照，输出统一模型。
- `cpulimit_driver`: 统一执行 `cpulimit` 命令与参数构造。
- `rule_store`: 读写 `rules.toml`（原子落盘）。
- `launchd_manager`: 生成、加载、卸载 `LaunchAgents`/`LaunchDaemons`。
- `instance_registry`: 记录并校验本工具托管实例。
- `conflict_guard`: `watch` 启动前检测并处理 ad-hoc 冲突实例。
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

    RULE --> FS1[(rules.toml)]
    REG --> FS2[(state.json)]
    LD --> LDCMD[launchctl]
    DRV --> CPL[cpulimit binary]
```

## 3. watch 时序图
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
    C->>L: render plist(label, cpulimit -e ztsmedr -l 20 -i)
    C->>L: bootstrap user/<uid>/label
    L-->>C: ok
    C-->>U: rule created + service active
```

## 4. top 行为时序（默认 watch，`--once` 例外）
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
    C->>D: start watch pipeline via launchd
    D-->>U: managed watch created
```

## 5. clean 时序图（零误杀）
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

## 6. 依赖边界图
```mermaid
flowchart TD
    A[Business Modules] --> B[OS Adapters]
    B --> C[launchctl]
    B --> D[cpulimit]
    B --> E[process API]

    F[Persistence] --> G[rules.toml]
    F --> H[state.json]

    I[Forbidden in core logic] -.-> J[Global ps|grep fuzzy kill]
```

## 7. 关键实现约束
- 所有系统调用通过 adapter 层，便于测试替身。
- `top/status` 在单次命令生命周期内只刷新一次进程快照。
- `clean` 必须在“实例登记 + label 前缀”双重验证通过后执行。
- `watch` 和 `top` 默认托管路径在启动前必须走 `conflict_guard`。
