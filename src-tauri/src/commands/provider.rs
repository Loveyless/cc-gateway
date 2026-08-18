use indexmap::IndexMap;
use tauri::{Emitter, Manager, State};

use crate::app_config::AppType;
use crate::commands::copilot::CopilotAuthState;
use crate::commands::xai_oauth::XaiOAuthState;
use crate::error::AppError;
use crate::provider::{ClaudeDesktopMode, Provider};
use crate::services::{
    EndpointLatency, ProviderService, ProviderSortUpdate, SpeedtestService, SwitchResult,
};
use crate::store::AppState;
use std::str::FromStr;

// 常量定义
const TEMPLATE_TYPE_GITHUB_COPILOT: &str = "github_copilot";
const TEMPLATE_TYPE_TOKEN_PLAN: &str = "token_plan";
const TEMPLATE_TYPE_BALANCE: &str = "balance";
const TEMPLATE_TYPE_OFFICIAL_SUBSCRIPTION: &str = "official_subscription";
const COPILOT_UNIT_PREMIUM: &str = "requests";

/// 获取所有供应商
#[tauri::command]
pub fn get_providers(
    state: State<'_, AppState>,
    app: String,
) -> Result<IndexMap<String, Provider>, String> {
    let app_type = AppType::from_str(&app).map_err(|e| e.to_string())?;
    ProviderService::list(state.inner(), app_type).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn get_current_provider(state: State<'_, AppState>, app: String) -> Result<String, String> {
    let app_type = AppType::from_str(&app).map_err(|e| e.to_string())?;
    ProviderService::current(state.inner(), app_type).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn add_provider(
    state: State<'_, AppState>,
    app: String,
    provider: Provider,
    #[allow(non_snake_case)] addToLive: Option<bool>,
) -> Result<bool, String> {
    let app_type = AppType::from_str(&app).map_err(|e| e.to_string())?;
    ProviderService::add(state.inner(), app_type, provider, addToLive.unwrap_or(true))
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn update_provider(
    state: State<'_, AppState>,
    app: String,
    provider: Provider,
    #[allow(non_snake_case)] originalId: Option<String>,
) -> Result<bool, String> {
    let app_type = AppType::from_str(&app).map_err(|e| e.to_string())?;
    ProviderService::update(state.inner(), app_type, originalId.as_deref(), provider)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn delete_provider(
    state: State<'_, AppState>,
    app: String,
    id: String,
) -> Result<bool, String> {
    let app_type = AppType::from_str(&app).map_err(|e| e.to_string())?;
    ProviderService::delete(state.inner(), app_type, &id)
        .map(|_| true)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn remove_provider_from_live_config(
    state: tauri::State<'_, AppState>,
    app: String,
    id: String,
) -> Result<bool, String> {
    let app_type = AppType::from_str(&app).map_err(|e| e.to_string())?;
    ProviderService::remove_from_live_config(state.inner(), app_type, &id)
        .map(|_| true)
        .map_err(|e| e.to_string())
}

fn switch_provider_internal(
    state: &AppState,
    app_type: AppType,
    id: &str,
) -> Result<SwitchResult, AppError> {
    ProviderService::switch(state, app_type, id)
}

#[cfg_attr(not(feature = "test-hooks"), doc(hidden))]
pub fn switch_provider_test_hook(
    state: &AppState,
    app_type: AppType,
    id: &str,
) -> Result<SwitchResult, AppError> {
    switch_provider_internal(state, app_type, id)
}

#[tauri::command]
pub async fn switch_provider(
    app_handle: tauri::AppHandle,
    app: String,
    id: String,
) -> Result<SwitchResult, String> {
    let app_type = AppType::from_str(&app).map_err(|e| e.to_string())?;
    tauri::async_runtime::spawn_blocking(move || {
        let state = app_handle
            .try_state::<AppState>()
            .ok_or_else(|| "应用状态不可用".to_string())?;
        switch_provider_internal(state.inner(), app_type, &id).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| format!("供应商切换任务执行失败: {e}"))?
}

fn import_default_config_internal(state: &AppState, app_type: AppType) -> Result<bool, AppError> {
    if matches!(app_type, AppType::GrokBuild) {
        // 官方登录态（live 语法合法且无自定义模型表）+ 用户手动导入：
        // 导入的正确结果是让 Grok Official 成为当前供应商，而非报错。
        // 只挂在命令层 = 只有手动动作可达；启动自动导入走 service 层、
        // 官方态照旧报错静默跳过，删掉的官方条目不会被重启复活
        //（全项目惯例：启动自动导入只产出 default，从不产出官方条目）。
        if let Ok(settings) = crate::grok_config::read_grok_live_settings() {
            let config = settings
                .get("config")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            if crate::grok_config::is_official_live_config(config) {
                state.db.ensure_official_seed_by_id(
                    crate::database::GROKBUILD_OFFICIAL_PROVIDER_ID,
                    AppType::GrokBuild,
                )?;
                state.db.set_current_provider(
                    app_type.as_str(),
                    crate::database::GROKBUILD_OFFICIAL_PROVIDER_ID,
                )?;
                crate::settings::set_current_provider(
                    &app_type,
                    Some(crate::database::GROKBUILD_OFFICIAL_PROVIDER_ID),
                )?;
                return Ok(true);
            }
        }

        // Safety net: 与 claude-desktop 导入同语义 —— 用户主动点导入是"重新
        // 整理该表"的隐式信号，把官方入口补回来。覆盖导入必然失败的场景
        //（live 文件缺失 / TOML 语法错误 / 残缺的自定义配置），避免
        // "报错 + 空列表"死胡同。失败只 warn，不影响导入主流程。
        if let Err(e) = state.db.ensure_official_seed_by_id(
            crate::database::GROKBUILD_OFFICIAL_PROVIDER_ID,
            AppType::GrokBuild,
        ) {
            log::warn!("Failed to ensure grokbuild-official seed during import: {e}");
        }
    }

    let imported = ProviderService::import_default_config(state, app_type.clone())?;

    if imported {
        // Extract common config snippet (mirrors old startup logic in lib.rs)
        if state
            .db
            .should_auto_extract_config_snippet(app_type.as_str())?
        {
            match ProviderService::extract_common_config_snippet(state, app_type.clone()) {
                Ok(snippet) if !snippet.is_empty() && snippet != "{}" => {
                    let _ = state
                        .db
                        .set_config_snippet(app_type.as_str(), Some(snippet));
                    let _ = state
                        .db
                        .set_config_snippet_cleared(app_type.as_str(), false);
                }
                _ => {}
            }
        }

        ProviderService::migrate_legacy_common_config_usage_if_needed(state, app_type.clone())?;
    }

    Ok(imported)
}

#[cfg_attr(not(feature = "test-hooks"), doc(hidden))]
pub fn import_default_config_test_hook(
    state: &AppState,
    app_type: AppType,
) -> Result<bool, AppError> {
    import_default_config_internal(state, app_type)
}

#[tauri::command]
pub fn import_default_config(state: State<'_, AppState>, app: String) -> Result<bool, String> {
    let app_type = AppType::from_str(&app).map_err(|e| e.to_string())?;
    import_default_config_internal(&state, app_type).map_err(Into::into)
}

#[tauri::command]
pub async fn get_claude_desktop_status(
    state: State<'_, AppState>,
) -> Result<crate::claude_desktop_config::ClaudeDesktopStatus, String> {
    let proxy_running = state.proxy_service.is_running().await;
    crate::claude_desktop_config::get_status(state.db.as_ref(), proxy_running)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn get_claude_desktop_default_routes(
) -> Vec<crate::claude_desktop_config::ClaudeDesktopDefaultRoute> {
    crate::claude_desktop_config::default_proxy_routes()
}

#[tauri::command]
pub fn import_claude_desktop_providers_from_claude(
    state: State<'_, AppState>,
) -> Result<usize, String> {
    let claude_providers = state
        .db
        .get_all_providers(AppType::Claude.as_str())
        .map_err(|e| e.to_string())?;
    let existing_ids = state
        .db
        .get_provider_ids(AppType::ClaudeDesktop.as_str())
        .map_err(|e| e.to_string())?;

    let mut imported = 0usize;
    for provider in claude_providers.values() {
        if existing_ids.contains(&provider.id) {
            continue;
        }

        let mut desktop_provider = provider.clone();
        desktop_provider.in_failover_queue = false;
        let meta = desktop_provider.meta.get_or_insert_with(Default::default);

        if crate::claude_desktop_config::is_compatible_direct_provider(provider)
            && claude_provider_models_are_claude_safe(provider)
        {
            meta.claude_desktop_mode = Some(ClaudeDesktopMode::Direct);
        } else if let Some(routes) = suggested_claude_desktop_routes(provider) {
            meta.claude_desktop_mode = Some(ClaudeDesktopMode::Proxy);
            meta.claude_desktop_model_routes = routes;
        } else {
            continue;
        }

        state
            .db
            .save_provider(AppType::ClaudeDesktop.as_str(), &desktop_provider)
            .map_err(|e| e.to_string())?;
        imported += 1;
    }

    // Safety net: 用户可能手动删除过 claude-desktop-official seed。
    // 用户主动点 import 是"重新整理 ClaudeDesktop 表"的隐式信号，把官方入口补回来。
    // 失败只 warn，不影响 imported 主流程；imported 计数语义保持纯净。
    if let Err(e) = state.db.ensure_official_seed_by_id(
        crate::database::CLAUDE_DESKTOP_OFFICIAL_PROVIDER_ID,
        AppType::ClaudeDesktop,
    ) {
        log::warn!("Failed to ensure claude-desktop-official seed during import: {e}");
    }

    Ok(imported)
}

#[tauri::command]
pub fn ensure_claude_desktop_official_provider(state: State<'_, AppState>) -> Result<bool, String> {
    state
        .db
        .ensure_official_seed_by_id(
            crate::database::CLAUDE_DESKTOP_OFFICIAL_PROVIDER_ID,
            AppType::ClaudeDesktop,
        )
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn ensure_codex_official_provider(state: State<'_, AppState>) -> Result<bool, String> {
    state
        .db
        .ensure_official_seed_by_id(crate::database::CODEX_OFFICIAL_PROVIDER_ID, AppType::Codex)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn ensure_grokbuild_official_provider(state: State<'_, AppState>) -> Result<bool, String> {
    state
        .db
        .ensure_official_seed_by_id(
            crate::database::GROKBUILD_OFFICIAL_PROVIDER_ID,
            AppType::GrokBuild,
        )
        .map_err(|e| e.to_string())
}

fn claude_provider_models_are_claude_safe(provider: &Provider) -> bool {
    let Some(env) = provider
        .settings_config
        .get("env")
        .and_then(|value| value.as_object())
    else {
        return true;
    };

    [
        "ANTHROPIC_MODEL",
        "ANTHROPIC_DEFAULT_HAIKU_MODEL",
        "ANTHROPIC_DEFAULT_SONNET_MODEL",
        "ANTHROPIC_DEFAULT_OPUS_MODEL",
    ]
    .into_iter()
    .filter_map(|key| env.get(key).and_then(|value| value.as_str()))
    .map(str::trim)
    .filter(|value| !value.is_empty())
    .all(crate::claude_desktop_config::is_claude_safe_model_id)
}

pub(crate) fn suggested_claude_desktop_routes(
    provider: &Provider,
) -> Option<std::collections::HashMap<String, crate::provider::ClaudeDesktopModelRoute>> {
    let env = provider
        .settings_config
        .get("env")
        .and_then(|value| value.as_object())?;
    let mut routes = std::collections::HashMap::new();
    let supports_1m_default = !matches!(
        provider
            .meta
            .as_ref()
            .and_then(|meta| meta.provider_type.as_deref()),
        Some("github_copilot") | Some("codex_oauth") | Some("xai_oauth")
    );

    fn add_route(
        routes: &mut std::collections::HashMap<String, crate::provider::ClaudeDesktopModelRoute>,
        env: &serde_json::Map<String, serde_json::Value>,
        route_key: &str,
        env_key: &str,
        supports_1m_default: bool,
    ) {
        let Some(raw_model) = env
            .get(env_key)
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            return;
        };

        // Claude 端 env 值可能带 [1M] 后缀；Claude Desktop schema 不接受后缀，
        // 改用 supports1m 字段表达 1M 能力。在 import 边界做单向翻译。
        let marker = crate::claude_desktop_config::ONE_M_CONTEXT_MARKER.as_bytes();
        let raw_bytes = raw_model.as_bytes();
        let has_1m_marker = raw_bytes.len() >= marker.len()
            && raw_bytes[raw_bytes.len() - marker.len()..].eq_ignore_ascii_case(marker);
        let stripped_model: &str = if has_1m_marker {
            raw_model[..raw_model.len() - marker.len()].trim_end()
        } else {
            raw_model
        };
        if stripped_model.is_empty() {
            return;
        }
        let effective_supports_1m = supports_1m_default || has_1m_marker;
        let explicit_label_override = env
            .get(&format!("{env_key}_NAME"))
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        let label_override = explicit_label_override.clone().or_else(|| {
            (!crate::claude_desktop_config::is_claude_safe_model_id(stripped_model))
                .then(|| stripped_model.to_string())
        });

        // 何时覆盖既有 label_override：原本为空 / 这次来的是 explicit _NAME /
        // 既有值只是 stripped_model 派生的占位（被 explicit 或更具体的值挤掉）。
        let should_overwrite = |existing: Option<&str>| {
            existing.is_none()
                || explicit_label_override.is_some()
                || existing == Some(stripped_model)
        };

        let merge_into = |existing: &mut crate::provider::ClaudeDesktopModelRoute| {
            let merged = existing.supports_1m.unwrap_or(false) || effective_supports_1m;
            existing.supports_1m = Some(merged);
            if should_overwrite(existing.label_override.as_deref()) {
                existing.label_override = label_override.clone();
            }
        };

        if let Some(existing) = routes
            .values_mut()
            .find(|existing| existing.model == stripped_model)
        {
            merge_into(existing);
            return;
        }

        routes
            .entry(route_key.to_string())
            .and_modify(merge_into)
            .or_insert_with(|| crate::provider::ClaudeDesktopModelRoute {
                model: stripped_model.to_string(),
                label_override,
                supports_1m: Some(effective_supports_1m),
            });
    }

    for spec in crate::claude_desktop_config::DEFAULT_PROXY_ROUTES {
        add_route(
            &mut routes,
            env,
            spec.route_id,
            spec.env_key,
            supports_1m_default,
        );
    }

    // 三个 default env_key 全空时用 ANTHROPIC_MODEL 派生兜底路由。
    if routes.is_empty() {
        let primary_route = crate::claude_desktop_config::DEFAULT_PROXY_ROUTES[0].route_id;
        add_route(
            &mut routes,
            env,
            primary_route,
            "ANTHROPIC_MODEL",
            supports_1m_default,
        );
    }

    (!routes.is_empty()).then_some(routes)
}

#[tauri::command]
pub fn read_live_provider_settings(app: String) -> Result<serde_json::Value, String> {
    let app_type = AppType::from_str(&app).map_err(|e| e.to_string())?;
    ProviderService::read_live_settings(app_type).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn test_api_endpoints(
    urls: Vec<String>,
    #[allow(non_snake_case)] timeoutSecs: Option<u64>,
) -> Result<Vec<EndpointLatency>, String> {
    SpeedtestService::test_endpoints(urls, timeoutSecs)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn get_custom_endpoints(
    state: State<'_, AppState>,
    app: String,
    #[allow(non_snake_case)] providerId: String,
) -> Result<Vec<crate::settings::CustomEndpoint>, String> {
    let app_type = AppType::from_str(&app).map_err(|e| e.to_string())?;
    ProviderService::get_custom_endpoints(state.inner(), app_type, &providerId)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn add_custom_endpoint(
    state: State<'_, AppState>,
    app: String,
    #[allow(non_snake_case)] providerId: String,
    url: String,
) -> Result<(), String> {
    let app_type = AppType::from_str(&app).map_err(|e| e.to_string())?;
    ProviderService::add_custom_endpoint(state.inner(), app_type, &providerId, url)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn remove_custom_endpoint(
    state: State<'_, AppState>,
    app: String,
    #[allow(non_snake_case)] providerId: String,
    url: String,
) -> Result<(), String> {
    let app_type = AppType::from_str(&app).map_err(|e| e.to_string())?;
    ProviderService::remove_custom_endpoint(state.inner(), app_type, &providerId, url)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn update_endpoint_last_used(
    state: State<'_, AppState>,
    app: String,
    #[allow(non_snake_case)] providerId: String,
    url: String,
) -> Result<(), String> {
    let app_type = AppType::from_str(&app).map_err(|e| e.to_string())?;
    ProviderService::update_endpoint_last_used(state.inner(), app_type, &providerId, url)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn update_providers_sort_order(
    state: State<'_, AppState>,
    app: String,
    updates: Vec<ProviderSortUpdate>,
) -> Result<bool, String> {
    let app_type = AppType::from_str(&app).map_err(|e| e.to_string())?;
    ProviderService::update_sort_order(state.inner(), app_type, updates).map_err(|e| e.to_string())
}

use crate::provider::UniversalProvider;
use std::collections::HashMap;
use tauri::AppHandle;

#[derive(Clone, serde::Serialize)]
pub struct UniversalProviderSyncedEvent {
    pub action: String,
    pub id: String,
}

fn emit_universal_provider_synced(app: &AppHandle, action: &str, id: &str) {
    let _ = app.emit(
        "universal-provider-synced",
        UniversalProviderSyncedEvent {
            action: action.to_string(),
            id: id.to_string(),
        },
    );
}

#[tauri::command]
pub fn get_universal_providers(
    state: State<'_, AppState>,
) -> Result<HashMap<String, UniversalProvider>, String> {
    ProviderService::list_universal(state.inner()).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn get_universal_provider(
    state: State<'_, AppState>,
    id: String,
) -> Result<Option<UniversalProvider>, String> {
    ProviderService::get_universal(state.inner(), &id).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn upsert_universal_provider(
    app: AppHandle,
    state: State<'_, AppState>,
    provider: UniversalProvider,
) -> Result<bool, String> {
    let id = provider.id.clone();
    let result =
        ProviderService::upsert_universal(state.inner(), provider).map_err(|e| e.to_string())?;

    emit_universal_provider_synced(&app, "upsert", &id);

    Ok(result)
}

#[tauri::command]
pub fn delete_universal_provider(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
) -> Result<bool, String> {
    let result =
        ProviderService::delete_universal(state.inner(), &id).map_err(|e| e.to_string())?;

    emit_universal_provider_synced(&app, "delete", &id);

    Ok(result)
}

#[tauri::command]
pub fn sync_universal_provider(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
) -> Result<bool, String> {
    let result =
        ProviderService::sync_universal_to_apps(state.inner(), &id).map_err(|e| e.to_string())?;

    emit_universal_provider_synced(&app, "sync", &id);

    Ok(result)
}

// ============================================================================
// ============================================================================

#[cfg(test)]
mod import_claude_desktop_tests {
    use super::suggested_claude_desktop_routes;
    use crate::provider::{Provider, ProviderMeta};
    use serde_json::json;

    fn make_provider(env: serde_json::Value, provider_type: Option<&str>) -> Provider {
        let mut p = Provider::with_id(
            "test-claude".to_string(),
            "Test".to_string(),
            json!({ "env": env }),
            None,
        );
        if let Some(pt) = provider_type {
            p.meta = Some(ProviderMeta {
                provider_type: Some(pt.to_string()),
                ..ProviderMeta::default()
            });
        }
        p
    }

    #[test]
    fn route_strips_1m_suffix_and_sets_supports_1m() {
        let p = make_provider(
            json!({
                "ANTHROPIC_DEFAULT_SONNET_MODEL": "claude-sonnet-4-5-20250929[1M]",
            }),
            None,
        );
        let routes = suggested_claude_desktop_routes(&p).expect("routes built");
        let r = routes.get("claude-sonnet-5").expect("sonnet route present");
        assert_eq!(r.model, "claude-sonnet-4-5-20250929");
        assert!(
            !r.model.to_ascii_lowercase().contains("[1m]"),
            "model must not contain [1m] suffix"
        );
        assert_eq!(r.label_override, None);
        assert_eq!(r.supports_1m, Some(true));
    }

    #[test]
    fn route_preserves_model_without_suffix() {
        let p = make_provider(
            json!({
                "ANTHROPIC_DEFAULT_SONNET_MODEL": "kimi-k2",
            }),
            None,
        );
        let routes = suggested_claude_desktop_routes(&p).expect("routes built");
        let r = routes.get("claude-sonnet-5").expect("sonnet route present");
        assert_eq!(r.model, "kimi-k2");
        assert_eq!(r.label_override.as_deref(), Some("kimi-k2"));
        // 默认 provider_type 缺省 → supports_1m_default = true
        assert_eq!(r.supports_1m, Some(true));
    }

    #[test]
    fn route_uses_claude_code_model_name_as_label_override() {
        let p = make_provider(
            json!({
                "ANTHROPIC_DEFAULT_SONNET_MODEL": "kimi-k2",
                "ANTHROPIC_DEFAULT_SONNET_MODEL_NAME": "Kimi K2",
            }),
            None,
        );
        let routes = suggested_claude_desktop_routes(&p).expect("routes built");
        let r = routes.get("claude-sonnet-5").expect("sonnet route present");
        assert_eq!(r.model, "kimi-k2");
        assert_eq!(r.label_override.as_deref(), Some("Kimi K2"));
    }

    #[test]
    fn route_1m_suffix_overrides_provider_type_default() {
        // github_copilot 默认 supports_1m_default = false，但 [1M] 后缀应强制 true
        let p = make_provider(
            json!({
                "ANTHROPIC_DEFAULT_SONNET_MODEL": "gpt-5-codex[1M]",
            }),
            Some("github_copilot"),
        );
        let routes = suggested_claude_desktop_routes(&p).expect("routes built");
        let r = routes.get("claude-sonnet-5").expect("sonnet route present");
        assert_eq!(r.model, "gpt-5-codex");
        assert_eq!(r.label_override.as_deref(), Some("gpt-5-codex"));
        assert_eq!(r.supports_1m, Some(true));
    }

    #[test]
    fn route_github_copilot_without_suffix_keeps_false() {
        let p = make_provider(
            json!({
                "ANTHROPIC_DEFAULT_SONNET_MODEL": "gpt-5-codex",
            }),
            Some("github_copilot"),
        );
        let routes = suggested_claude_desktop_routes(&p).expect("routes built");
        let r = routes.get("claude-sonnet-5").expect("sonnet route present");
        assert_eq!(r.model, "gpt-5-codex");
        assert_eq!(r.label_override.as_deref(), Some("gpt-5-codex"));
        assert_eq!(r.supports_1m, Some(false));
    }

    #[test]
    fn same_upstream_across_three_aliases_merges_to_one_route() {
        let p = make_provider(
            json!({
                "ANTHROPIC_DEFAULT_SONNET_MODEL": "MiniMax-M2",
                "ANTHROPIC_DEFAULT_OPUS_MODEL": "MiniMax-M2",
                "ANTHROPIC_DEFAULT_HAIKU_MODEL": "MiniMax-M2",
            }),
            None,
        );
        let routes = suggested_claude_desktop_routes(&p).expect("routes built");
        assert_eq!(routes.len(), 1, "three aliases → one merged route");
        let r = routes.get("claude-sonnet-5").expect("merged route present");
        assert_eq!(r.model, "MiniMax-M2");
        assert_eq!(r.label_override.as_deref(), Some("MiniMax-M2"));
    }

    #[test]
    fn same_upstream_with_partial_1m_marker_takes_or_aggregation() {
        // sonnet 带 [1M]，opus/haiku 不带 → 合并后 supports_1m == Some(true)
        let p = make_provider(
            json!({
                "ANTHROPIC_DEFAULT_SONNET_MODEL": "MiniMax-M2[1M]",
                "ANTHROPIC_DEFAULT_OPUS_MODEL": "MiniMax-M2",
                "ANTHROPIC_DEFAULT_HAIKU_MODEL": "MiniMax-M2",
            }),
            None,
        );
        let routes = suggested_claude_desktop_routes(&p).expect("routes built");
        assert_eq!(routes.len(), 1);
        let r = routes.get("claude-sonnet-5").expect("merged route present");
        assert_eq!(r.supports_1m, Some(true));
    }

    #[test]
    fn different_upstream_models_produce_separate_routes() {
        let p = make_provider(
            json!({
                "ANTHROPIC_DEFAULT_SONNET_MODEL": "GLM-4.6",
                "ANTHROPIC_DEFAULT_OPUS_MODEL": "GLM-4-Air",
                "ANTHROPIC_DEFAULT_HAIKU_MODEL": "GLM-4-Flash",
            }),
            None,
        );
        let routes = suggested_claude_desktop_routes(&p).expect("routes built");
        assert_eq!(routes.len(), 3);
        assert_eq!(routes.get("claude-sonnet-5").unwrap().model, "GLM-4.6");
        assert_eq!(routes.get("claude-opus-5").unwrap().model, "GLM-4-Air");
        assert_eq!(routes.get("claude-haiku-4-5").unwrap().model, "GLM-4-Flash");
        assert_eq!(
            routes
                .get("claude-sonnet-5")
                .unwrap()
                .label_override
                .as_deref(),
            Some("GLM-4.6")
        );
    }

    #[test]
    fn anthropic_model_fallback_only_triggers_when_empty() {
        // 三个 default env_key 都不填，仅 ANTHROPIC_MODEL
        let p = make_provider(
            json!({
                "ANTHROPIC_MODEL": "kimi-k2",
            }),
            None,
        );
        let routes = suggested_claude_desktop_routes(&p).expect("routes built");
        assert_eq!(routes.len(), 1);
        let r = routes
            .get("claude-sonnet-5")
            .expect("fallback route present");
        assert_eq!(r.model, "kimi-k2");
        assert_eq!(r.label_override.as_deref(), Some("kimi-k2"));
    }

    #[test]
    fn existing_claude_prefix_not_duplicated() {
        let p = make_provider(
            json!({
                "ANTHROPIC_DEFAULT_SONNET_MODEL": "claude-sonnet-4-5-20250929",
            }),
            None,
        );
        let routes = suggested_claude_desktop_routes(&p).expect("routes built");
        assert!(routes.contains_key("claude-sonnet-5"));
        assert!(!routes.contains_key("claude-claude-sonnet-4-5-20250929"));
        assert_eq!(
            routes.get("claude-sonnet-5").expect("route").label_override,
            None
        );
    }
}
