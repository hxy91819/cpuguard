# AGENTS.md

## Communication
- 与用户沟通默认使用中文。
- 命令、路径、参数、环境变量、代码标识符保留英文原文。
- 每次交付必须包含三项：改了什么、为什么这么改、如何验证。

## Project Scope
- 项目名称：`cpulimit-top`。
- 目标：用 Rust 实现 CPU 限速管理层，底层执行器使用外部 `cpulimit`。
- 平台：V1 仅支持 macOS。
- 默认运行域：`launchd` 用户域（`LaunchAgents`）。

## Development Workflow
- 文档先行：任何行为变更，先更新 `docs/`，再修改代码。
- 变更必须可追踪：PR 或提交说明中必须关联变更的文档章节。
- 代码改动前后都要执行约定检查：
  - `cargo fmt --all -- --check`
  - `cargo clippy --all-targets --all-features -- -D warnings`
  - `cargo test --all`

## Safety Boundaries
- 非用户明确要求时，禁止破坏性命令，包括但不限于：
  - `git reset --hard`
  - `git checkout -- <file>`
  - 强制覆盖式 rebase / push
- `clean` 的实现必须满足硬约束：
  - 仅清理本工具托管实例（基于 `instance_registry` + `launchd label` 双重确认）。
  - 不允许通过模糊匹配（如 `ps | grep cpulimit`）全局清理。

## Technical Constraints
- 使用 Rust 实现业务逻辑，不重写 CPU 节流算法。
- 执行器固定为外部 `cpulimit`（通过命令调用）。
- 仅当用户显式指定 `--domain system` 时才允许系统域操作。

## Documentation Consistency
- 新增或修改 CLI 参数、配置字段、状态字段时，必须同步更新：
  - `docs/03-cli-contract.md`
  - `README.md`
- 复杂流程必须配图，优先 Mermaid（状态图、时序图、流程图至少一种）。

## Context7 Rule
- 涉及安装步骤、配置步骤、第三方库 API 说明时，必须先查 Context7，再写入文档或代码注释。
- 允许使用的资料应优先来自官方文档源。

## Definition of Done
- 功能满足对应文档章节中的行为定义与验收标准。
- 所有约定检查通过。
- 文档与实现一致，无未解释偏差。
