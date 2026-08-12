// 前端统一使用 AppId 作为应用标识（与后端命令参数 `app` 一致）
export type AppId =
  | "claude"
  | "claude-desktop"
  | "codex"
  | "gemini"
  | "grokbuild"
  | "opencode"
  | "openclaw" // legacy serialized data only; not a supported runtime app
  | "hermes";

/** Applications that remain supported by current UI and runtime actions. */
export type RuntimeAppId = Exclude<AppId, "openclaw">;
