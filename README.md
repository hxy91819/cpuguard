# cpuguard

`cpuguard` 是一个基于 Rust 的 macOS CPU 限速管理工具。
它不重写 `cpulimit` 算法，而是在其上构建：规则管理、进程选择、`launchd` 托管、实例状态追踪与安全清理。

## 设计目标
- 默认面向系统后台进程：默认使用 `launchd` 系统域（`LaunchDaemons`），适合限制 `root` 或系统服务账号拥有的安全、审计、VPN、EDR、设备管理类后台进程。
- 明确权限边界：系统域操作通常需要 `sudo`；需要限制当前用户拥有的普通应用时，显式使用 `--domain user`。
- 零误杀清理：`clean` 仅处理本工具托管实例。
- 低常驻开销：`top/status` 单次采样，不重复全表扫描。
- 文档先行：行为和接口以 `docs/` 为准。

## 能力边界（Do / Don't）
### 我们要做的
- 结合进程快照（top-like 视图）帮助用户发现高 CPU 进程并选择目标。
- 打通“发现目标 -> 施加限制 -> 托管/一次性”的完整操作链路。
- 持久化管理 watch 规则（增删改查、状态可见）。
- 通过单一 `launchd` agent 在重启后恢复规则生效能力。
- 在 `watch` 启动前检测一次性限速冲突，避免双重限制。

### 我们不要做的
- 不重写 `cpulimit` 的 CPU 节流算法。
- 不做全局模糊 kill（例如 `ps|grep cpulimit` 后批量清理）。
- 不承诺 V1 跨平台（V1 仅 macOS）。
- 不接管或篡改非本工具托管的外部 `cpulimit` 实例。

## V1 命令
- `watch <name> [--limit N] [--trigger-cpu F] [--release-cpu F] [--args-contains TEXT]`：新增或更新可执行名规则，并确保单一 agent 托管。
- `unwatch <name>`：删除规则，agent 在下一轮扫描中清理对应限速实例。
- `watches`：查看规则与运行状态。
- `top [--limit N] [--count K] [--risk-cpu F] [--allow-kill]`：展示高 CPU 进程并默认创建 watch 规则。
- `top --once [--limit N] [--count K]`：执行一次性 ad-hoc 限速，不写入规则。
- `status`：查看托管实例状态。
- `clean [--yes]`：清理本工具托管实例与本地状态。
- `install-agent`：安装/刷新单一 `com.cpuguard.agent`。
- 无子命令：展示只读 dashboard，包括规则、agent 状态和当前被限制的进程。
- 全局参数：`--domain user|system`（默认 `system`）。

`watch` 启动前会执行冲突检测：
- 若存在本工具托管的一次性实例（ad-hoc）命中同名目标，则先停止再启动 watch。
- 若存在非本工具托管的外部 `cpulimit` 命中同名目标，则拒绝启动并提示用户先手动处理冲突。

关于自动化托管行为：
- `launchd` 只托管一个 `com.cpuguard.agent`，规则只是配置数据，不为每条规则创建独立开机启动项。
- 默认配置目录按 domain 分离：`system` 使用 `/Library/Application Support/cpuguard`，`user` 使用 `~/.config/cpuguard`。
- agent 低频共享一次进程快照评估全部规则；只有目标 PID CPU 连续超过 `trigger_cpu` 时才启动 `cpulimit -p <pid>`。
- 目标退出、规则删除，或 CPU 连续低于 `release_cpu` 后，agent 停止并清理对应托管实例。
- watch 规则的 `limit` 更新后，agent 会替换仍在运行的旧限速实例，避免旧 `cpulimit -l` 参数继续生效。
- agent 内置 backoff 和实例上限，避免限速管理本身造成额外性能消耗。

## 依赖
- macOS 14+。
- `cpulimit` 可执行文件。
- Rust stable 工具链。

## 快速开始（开发）
```bash
# 1) 安装 cpulimit (Homebrew)
brew install cpulimit

# 2) 进入仓库
cd /path/to/cpuguard

# 3) 质量检查
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all
```

## 使用方法
```bash
# 构建
cargo build --release

# 默认只读 dashboard
./target/release/cpuguard

# 查看帮助或进入交互 top
./target/release/cpuguard --help
./target/release/cpuguard top
```

## 发布与安装（二进制）
```bash
# 1) release 构建
cargo build --release

# 2) 安装到 /usr/local/bin（推荐）
sudo install -m 755 ./target/release/cpuguard /usr/local/bin/cpuguard

# 3) 验证
which cpuguard
cpuguard --help
```

说明：
- macOS 上通常不建议安装到 `/usr/bin`（受系统保护机制限制）。
- 优先使用 `/usr/local/bin`（或你的 PATH 中其他可写目录）。

### 1) 从 top 视图选择并默认加入 watch（推荐）
```bash
./target/release/cpuguard top --count 10 --limit 20
```
- 默认行为：选择目标后会创建/更新 watch 规则（持久化）。
- top 视图默认每 5 秒自动刷新一次，可用 `--refresh` 调整刷新间隔（秒）。
- top 表格会在 `LIMITED` 列标记当前快照里已经由 `cpuguard` 托管限速的 PID。
- top 表格会在 `RISK` 列提示值得排查的高 CPU 后台进程；这只是风险提示，不表示进程一定是孤儿或应被终止。
- 交互时输入序号执行限制，输入 `q` 退出，直接回车立即刷新。终止类动作默认隐藏，只有传入 `--allow-kill` 后才启用 `k<序号>` / `x<序号>`。

### 2) 一次性限制（不持久化）
```bash
./target/release/cpuguard top --once --pid 12345 --limit 30
```
- `--once` 才会创建 ad-hoc 实例，不写入 `rules.toml`。

### 3) 直接管理 watch 规则
```bash
# 新增/更新规则
./target/release/cpuguard watch ztsmedr --limit 25

# 区分同名子进程，仅在 CPU 达到阈值后限速
./target/release/cpuguard watch iOABiz --limit 20 --trigger-cpu 20 --release-cpu 6 --args-contains NGNAuditXPCClient

# 查看规则状态（含 launchd 与目标进程状态）
./target/release/cpuguard watches

# 删除规则
./target/release/cpuguard unwatch ztsmedr
```

### 4) 查看托管实例状态与清理
```bash
# 默认 dashboard：规则 + 当前已限制进程
./target/release/cpuguard

# 查看托管实例（running/stale）
./target/release/cpuguard status

# 仅清理本工具托管实例
./target/release/cpuguard clean --yes
```

### 5) 域切换（默认 system）
```bash
# 系统域（默认，写入 LaunchDaemons 通常需要 sudo）
sudo /usr/local/bin/cpuguard watches
sudo /usr/local/bin/cpuguard watch myproc --limit 20

# 用户域（限制当前用户拥有的普通应用时显式指定）
./target/release/cpuguard --domain user watches
```

系统域适合限制 `root` 拥有的安全、审计、VPN、EDR、设备管理类后台进程。用户域适合限制当前用户拥有的应用进程，例如普通开发工具、浏览器子进程或手动启动的任务；用户域必须显式传入 `--domain user`。

系统域安装示例：
```bash
sudo install -m 755 ./target/release/cpuguard /usr/local/bin/cpuguard
sudo /usr/local/bin/cpuguard watch <name> --limit 20 --trigger-cpu 15 --release-cpu 6
sudo /usr/local/bin/cpuguard install-agent
sudo /usr/local/bin/cpuguard status
```

启用系统域后，通常应停掉不再需要的用户域 agent，避免重复扫描：
```bash
launchctl bootout gui/$(id -u)/com.cpuguard.agent
rm -f ~/Library/LaunchAgents/com.cpuguard.agent.plist
```

## 运行流程概览
```mermaid
flowchart TD
    A[CLI 输入] --> B{命令类型}
    B -->|watch/unwatch/watches| C[规则与 launchd 管理]
    B -->|top/status| D[进程采样与实例查询]
    B -->|clean| E[仅清理托管实例]
    C --> I[com.cpuguard.agent]
    I --> F[cpulimit_driver]
    D --> F
    E --> G[instance_registry]
    F --> H[外部 cpulimit]
```

## 推荐 Rust 依赖（文档约束）
基于 Context7 查询到的官方 API 能力，V1 建议使用：
- CLI：`clap`（derive 子命令、默认值、全局参数）。
- 序列化：`serde` + `toml` + `serde_json`（规则与状态文件）。
- 进程采样：`sysinfo`（进程刷新与 CPU 读取）。
- 错误处理：`anyhow` + `thiserror`。
- 日志：`tracing` + `tracing-subscriber`。

## 文档导航
- 行为定义：[docs/01-product-behavior.md](./docs/01-product-behavior.md)
- 架构设计：[docs/02-architecture.md](./docs/02-architecture.md)
- CLI 契约：[docs/03-cli-contract.md](./docs/03-cli-contract.md)
- 数据模型：[docs/04-data-model.md](./docs/04-data-model.md)
- 失败模型：[docs/05-failure-model.md](./docs/05-failure-model.md)
- 验收与发布：[docs/06-rollout-and-acceptance.md](./docs/06-rollout-and-acceptance.md)
