# cc-gateway 维护说明

本文件是仓库级开发约束，适用于本仓库内的代码、配置、文档和发布工作。

## 项目定位

- 当前仓库：`Loveyless/cc-gateway`，默认分支为 `main`。
- 上游基线：`farion1231/cc-switch`。本维护版采用独立并存身份，不能重新引入会覆盖上游安装或数据的标识。
- 技术栈：Tauri 2；React + TypeScript 前端；Rust 后端；pnpm 管理前端依赖；SQLite 保存本地业务数据。
- 主要功能边界：Claude Code / Claude Desktop / Codex / Grok Build 的 Provider 配置切换，以及 MCP、Prompts、Skills、Proxy、Usage 和 Deep Link。

## 修改前的基本要求

1. 先查看 `git status --short --branch` 和目标文件的当前实现，只提交本次任务授权范围内的文件。
2. 优先沿用现有的前端、Tauri command、Rust service、数据库 migration、i18n 和测试模式；不要因为仓库改名顺手做无关重构。
3. 用户可见文本改动时，检查 `src/i18n/locales/` 下的 `en.json`、`zh.json`、`zh-TW.json`、`ja.json`，以及项目实际支持的其他语言文件。
4. 影响配置格式、数据库 schema、IPC command、深链、文件路径、权限或发布行为时，先写清兼容性影响，并补对应测试或迁移说明。

## Fork 身份与许可证

- `LICENSE` 是 MIT。必须保留上游版权和许可文本；可以在此基础上增加本维护版的版权或变更说明，但不要删除原始通知。
- `package.json`、`src-tauri/Cargo.toml`、`src-tauri/tauri.conf.json`、Flatpak 元数据、README、Support/Security 文档和 GitHub Actions 都包含项目身份信息。改名时必须按用途逐项核对，不能全文替换。
- 稳定身份为：展示名 `CC Gateway`、包/二进制 `cc-gateway`、Rust lib `cc_gateway_lib`、Tauri `identifier` 为 `io.github.loveyless.ccgateway`、Deep Link 为 `ccgateway://`、默认数据目录为 `~/.cc-gateway`。
- 首次发布可保持 updater 停用；OS 代码签名与 Tauri updater 签名是两套边界，不能相互替代。重新启用 updater 前必须使用维护版自己的密钥、manifest 和下载 endpoint。

## 并存与兼容边界

以下 Gateway 标识共同构成安装和自有数据隔离，修改任一项都要同步检查配置、测试、文档和打包流程：

- `CC Gateway`、`cc-gateway`、`cc_gateway_lib`
- `io.github.loveyless.ccgateway`、`ccgateway://`
- `~/.cc-gateway`、`cc-gateway.db`、`cc-gateway.log`
- 默认代理端口 `15722`、远程同步根目录 `cc-gateway-sync`
- 自动启动名称、托盘 ID、Claude Desktop profile ID、Codex 代理所有权标记和模型目录文件名

以下上游或历史值有明确兼容用途，不能因为改名全文替换：

- `cc-switch-webdav-sync` 是远程 manifest 的历史协议格式；默认远程根目录已经隔离。
- Codex 历史会话中的 `ccswitch` provider ID、代理响应中的历史协议前缀和数据库字段/函数名可能承担兼容语义。
- 第三方 URL 中的 `cc-switch`、`ccswitch`、`CC-SWITCH` 可能是外部推广参数或优惠码。
- `CHANGELOG.md`、`docs/release-notes/`、上游链接和署名属于历史记录。
- `-- CC Switch SQLite 导出` 只用于兼容用户显式导入上游备份，不得扩展为启动时读取 `~/.cc-switch`。

Gateway 与上游仍共享 `~/.claude`、`~/.codex` 等 Agent live 配置。两个应用可以并装和双开，但不得宣称它们能同时接管同一个 Agent；涉及 Provider、代理、MCP、Prompt 或 Skill 写入时必须考虑最后写入者覆盖和所有权误判。完整边界见 `FORK.md`。

当前 updater 已禁用，Tauri 配置不包含上游公钥或 endpoint，Release/R2 workflow 默认关闭。只有在维护版拥有自己的更新密钥、发布清单、下载地址并完成跨平台验证后才能重新启用。

## 代码区域速查

- `src/`：React 渲染层、页面组件、查询/API 封装、配置预设和多语言。
- `src-tauri/src/`：Tauri commands、Rust service、代理、数据库、配置读写、系统集成。
- `src/config/`：各 Agent/CLI 的 Provider 预设。这里常含第三方官网、推广参数和模型默认值，修改前区分功能配置与商业链接。
- `tests/`：前端 Vitest 测试。
- `src-tauri/tests/`：Rust 集成测试；Rust 单元测试也分布在 `src-tauri/src/`。
- `.github/workflows/`：CI、跨平台发布和 R2 同步。发布 workflow 依赖签名、Apple 公证和 R2 secrets，不能仅靠本地构建成功推断可发布。

## 验证命令

仓库的 CI 当前执行：

```bash
pnpm install --frozen-lockfile
pnpm typecheck
pnpm format:check
pnpm test:unit

cargo fmt --check --manifest-path src-tauri/Cargo.toml
cargo clippy --manifest-path src-tauri/Cargo.toml -- -D warnings
cargo test --manifest-path src-tauri/Cargo.toml
```

构建渲染层可使用 `pnpm run build:renderer`；完整桌面包使用 `pnpm build`，但需要对应平台的 Tauri 系统依赖和发布凭据。若本机缺少依赖或工具链，必须在交付中明确写出“未验证”的命令和最小补证路径。

## 维护策略

- 先建立可重复的基线，再做品牌、功能或发布链路改造。
- 与上游同步时，保留一个明确的 upstream remote 或等价记录，并记录同步基线、冲突处理和本维护版差异。
- 每个变更保持单一目的；用户可见行为、数据格式和发布身份的变更应拆成可独立验证的提交。
- 不把 README、示例、Mock 或本地构建结果当作运行时验收；涉及真实用户路径时，必须按实际平台和入口验证。
