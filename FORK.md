# CC Gateway 维护版边界

本仓库是 [Loveyless/cc-gateway](https://github.com/Loveyless/cc-gateway)，基于 [farion1231/cc-switch](https://github.com/farion1231/cc-switch) 的 MIT 许可代码维护。

## 当前状态

- 本维护版采用“独立并存”策略，不覆盖或接管已安装的上游 CC Switch。
- Gemini CLI、OpenCode、OpenClaw、Hermes、项目切换、会话管理器、套餐额度/Usage Script 和云同步已从运行时下线。旧数据库/设置字段仍可反序列化，但不会再读写这些应用的 live 配置。
- Skills 只保留本机导入、开关和投影；GitHub 发现、自定义仓库和 ZIP 安装已下线。
- 应用内 updater 已停用，Release workflow 默认关闭。未启用仓库变量 `CC_GATEWAY_RELEASE_ENABLED=true` 前，不应通过打 tag 发布桌面包。
- GitHub Funding 和上游商业赞助内容已移除；需要由维护者确认收款主体和披露方式后才能重新配置。
- 内置供应商预设仍保留部分上游推广 URL 和优惠参数以避免破坏既有入口；界面会明确标注其继承自上游，不代表 CC Gateway 建立了合作关系，也不保证优惠有效。首发前必须决定保留并披露，还是移除这些推广参数。
- 云同步默认远程根目录 `cc-gateway-sync` 仅作历史隔离记录；当前版本不再提供 WebDAV/S3 同步。

独立运行身份如下：

| 边界 | CC Gateway | 上游 CC Switch |
| --- | --- | --- |
| 展示名 | `CC Gateway` | `CC Switch` |
| Rust/可执行文件 | `cc-gateway` / `cc_gateway_lib` | `cc-switch` / `cc_switch_lib` |
| Tauri identifier | `io.github.loveyless.ccgateway` | `com.ccswitch.desktop` |
| Deep Link | `ccgateway://` | `ccswitch://` |
| 默认数据目录 | `~/.cc-gateway` | `~/.cc-switch` |
| 默认数据库/日志 | `cc-gateway.db` / `cc-gateway.log` | `cc-switch.db` / `cc-switch.log` |
| 默认代理端口 | `15722` | `15721` |
| 默认远程同步根目录 | `cc-gateway-sync` | `cc-switch-sync` |

CC Gateway 不会在启动时读取或迁移 `~/.cc-switch`，Windows 旧版 `HOME/.cc-switch` 回退也已移除。上游 SQL 备份仅在用户主动选择“导入”时兼容，导入前会按现有安全流程备份 Gateway 数据。

## 并存限制

安装身份和应用自有数据已隔离，但两个应用仍会操作同一组外部 Agent live 配置，例如 `~/.claude`、`~/.claude.json`、`~/.codex` 和 `~/.config/grok`。因此：

- 可以同时安装和启动两个应用，但不要让它们同时接管同一个 Agent。
- Provider 切换、代理接管、MCP/Prompt/Skill 投影都遵循“最后写入者生效”，不能把两个应用同时运行理解为 Agent 配置也完全隔离。
- Gateway 使用独立的 Claude Desktop profile ID、Codex 代理所有权标记和模型目录文件名，避免原版按自己的标记误清理 Gateway 状态；这不能消除两个应用写同一个 live 配置文件的根本竞争。
- 完全运行时隔离需要为每个 Agent 提供独立配置目录并贯穿启动命令，不属于当前安装身份改造。

## 发布前置条件

在第一次对外发布前，必须完成并验证：

1. 确认首个维护版版本号和 tag 规则，避免与上游版本号造成“官方升级包”的误解。
2. 替换或书面确认应用图标、DMG 背景等品牌资产的授权与识别度；安装身份独立不代表视觉品牌已完成。
3. 配置 Apple Developer ID、Notary 凭据并验证 macOS `.app`/`.dmg` 签名与公证；为 Windows 增加并验证 Authenticode 签名，不能把 Tauri updater 签名当作系统代码签名。
4. 在 macOS、Windows、Linux 各完成一次干净安装、与上游并装、双开、Deep Link、自动启动、卸载、数据保留和回滚验证；Windows 需覆盖 x64/ARM64，macOS 需覆盖 Intel/Apple Silicon 产物。
5. 验证显式导入上游 SQL 备份不会自动读取或改写 `~/.cc-switch`。
6. 检查 Release 资产命名、哈希/签名、下载说明、Support/Security、隐私说明和第三方链接披露，再将 GitHub Release 从 prerelease 调整为正式发布。

首次发布可以继续禁用应用内 updater，不需要 Tauri updater signing key。只有决定重新启用自动更新时，才生成 CC Gateway 自己的 updater key、公钥、`latest.json` 和下载 endpoint；不得复用 `dl.ccswitch.io`、上游 manifest 或上游私钥。

## 上游同步

本地维护仓库已配置独立的 `upstream` remote。其他克隆可用以下命令建立同样的边界：

```bash
git remote add upstream git@github.com:farion1231/cc-switch.git
git fetch upstream main
```

同步前先记录当前维护版基线和工作区状态，比较 `main...upstream/main`，再通过可审查的合并提交处理。不要在脏工作区直接合并，也不要把 `upstream` 当作维护版发布目标。上游历史 release notes 中的链接和署名用于记录历史来源，不应在没有必要时批量改写。
