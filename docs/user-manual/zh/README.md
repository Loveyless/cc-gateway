# CC Gateway 用户手册

> Claude Code / Claude Desktop / Codex / Grok Build 全方位辅助工具

当前维护版运行时只管理上述四个工具。Gemini CLI、OpenCode、OpenClaw、Hermes、会话管理器、套餐额度查询、云同步和 Skills 市场已下线；下列带“已下线”的章节仅作历史说明。

## 目录结构

```
📚 CC Gateway 用户手册
│
├── 1. 快速入门
│   ├── 1.1 软件介绍
│   ├── 1.2 安装指南
│   ├── 1.3 界面概览
│   ├── 1.4 快速上手
│   └── 1.5 个性化配置
│
├── 2. 供应商管理
│   ├── 2.1 添加供应商
│   ├── 2.2 切换供应商
│   ├── 2.3 编辑供应商
│   ├── 2.4 排序与复制
│   ├── 2.5 用量查询（已下线）
│   └── 2.6 Claude Desktop
│
├── 3. 扩展功能
│   ├── 3.1 MCP 服务器管理
│   ├── 3.2 Prompts 提示词管理
│   ├── 3.3 Skills 技能管理
│   ├── 3.4 会话管理器（已下线）
│   └── 3.5 工作区文件与每日记忆（已下线）
│
├── 4. 代理与高可用
│   ├── 4.1 代理服务
│   ├── 4.2 应用接管
│   ├── 4.3 故障转移（已下线）
│   ├── 4.4 用量统计
│   └── 4.5 模型检查（已下线）
│
└── 5. 常见问题
    ├── 5.1 配置文件说明
    ├── 5.2 FAQ
    ├── 5.3 深度链接协议
    └── 5.4 环境变量冲突
```

## 文件列表

### 1. 快速入门

| 文件 | 内容 |
|------|------|
| [1.1-introduction.md](./1-getting-started/1.1-introduction.md) | 软件介绍、核心功能、支持平台 |
| [1.2-installation.md](./1-getting-started/1.2-installation.md) | Windows/macOS/Linux 安装指南 |
| [1.3-interface.md](./1-getting-started/1.3-interface.md) | 界面布局、导航栏、供应商卡片说明 |
| [1.4-quickstart.md](./1-getting-started/1.4-quickstart.md) | 5 分钟快速上手教程 |
| [1.5-settings.md](./1-getting-started/1.5-settings.md) | 语言、主题、目录配置 |

### 2. 供应商管理

| 文件 | 内容 |
|------|------|
| [2.1-add.md](./2-providers/2.1-add.md) | 使用预设、自定义配置、统一供应商 |
| [2.2-switch.md](./2-providers/2.2-switch.md) | 主界面切换、托盘切换、生效方式 |
| [2.3-edit.md](./2-providers/2.3-edit.md) | 编辑配置、修改 API Key、回填机制 |
| [2.4-sort-duplicate.md](./2-providers/2.4-sort-duplicate.md) | 拖拽排序、复制供应商、删除 |
| [2.5-usage-query.md](./2-providers/2.5-usage-query.md) | 已下线：套餐额度 / Usage Script |
| [2.6-claude-desktop.md](./2-providers/2.6-claude-desktop.md) | Claude Desktop 第三方供应商、直连与模型映射 |

### 3. 扩展功能

| 文件 | 内容 |
|------|------|
| [3.1-mcp.md](./3-extensions/3.1-mcp.md) | MCP 协议、添加服务器、应用绑定 |
| [3.2-prompts.md](./3-extensions/3.2-prompts.md) | 创建预设、激活切换、智能回填 |
| [3.3-skills.md](./3-extensions/3.3-skills.md) | 本机导入、按应用开关、投影到应用目录 |
| [3.4-sessions.md](./3-extensions/3.4-sessions.md) | 已下线：会话管理器 |

### 4. 代理与高可用

| 文件 | 内容 |
|------|------|
| [4.1-service.md](./4-proxy/4.1-service.md) | 启动代理、配置项、运行状态 |
| [4.2-routing.md](./4-proxy/4.2-routing.md) | 应用路由、配置修改、状态指示 |
| [4.3-failover.md](./4-proxy/4.3-failover.md) | 已下线：故障转移队列 |
| [4.4-usage.md](./4-proxy/4.4-usage.md) | 用量统计、趋势图表、定价配置 |
| [4.5-model-test.md](./4-proxy/4.5-model-test.md) | 已下线：连通性 / 流式检测 |

### 5. 常见问题

| 文件 | 内容 |
|------|------|
| [5.1-config-files.md](./5-faq/5.1-config-files.md) | CC Gateway 存储、CLI 配置文件格式 |
| [5.2-questions.md](./5-faq/5.2-questions.md) | 常见问题解答 |
| [5.3-deeplink.md](./5-faq/5.3-deeplink.md) | 深度链接协议、生成和使用方法 |
| [5.4-env-conflict.md](./5-faq/5.4-env-conflict.md) | 环境变量冲突检测与处理 |

## 快速链接

- **新用户**：从 [1.1 软件介绍](./1-getting-started/1.1-introduction.md) 开始
- **安装问题**：查看 [1.2 安装指南](./1-getting-started/1.2-installation.md)
- **配置供应商**：查看 [2.1 添加供应商](./2-providers/2.1-add.md)
- **使用 Claude Desktop**：查看 [2.6 Claude Desktop](./2-providers/2.6-claude-desktop.md)
- **使用代理**：查看 [4.1 代理服务](./4-proxy/4.1-service.md)
- **遇到问题**：查看 [5.2 FAQ](./5-faq/5.2-questions.md)

## 版本信息

- 文档版本：维护版当前边界
- 最后更新：2026-08-18
- 适用于本仓库当前维护版（四工具：Claude Code / Claude Desktop / Codex / Grok Build）

### 当前维护版边界

- 运行时应用：Claude Code、Claude Desktop、Codex、Grok Build
- Skills：只支持本机导入、开关和投影，不再提供 GitHub / ZIP / skills.sh
- 已下线：Hermes、会话管理器、套餐额度 / Usage Script、故障转移 UI、连通性检查、云同步

## 贡献

欢迎提交 Issue 或 PR 改进文档：

- [GitHub Issues](https://github.com/Loveyless/cc-gateway/issues)
- [GitHub Repository](https://github.com/Loveyless/cc-gateway)
