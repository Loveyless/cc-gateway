//! 项目 Profile 命令桩
//!
//! 项目切换已下线。命令仍注册，避免旧前端调用直接丢 IPC。
//! 列表恒为空，写入/应用一律拒绝。`profiles` 表只作历史兼容，不再有 DAO。

use serde::Serialize;
use tauri::State;

use crate::error::AppError;
use crate::store::AppState;

fn profile_retired() -> String {
    AppError::localized(
        "profile.retired",
        "项目切换已永久停止",
        "Project switching has been permanently discontinued",
    )
    .to_string()
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileDto {
    pub id: String,
    pub name: String,
    pub payload: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<i64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CurrentProfileIds {
    pub claude: Option<String>,
    pub claude_desktop: Option<String>,
    pub codex: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfilesResponse {
    pub profiles: Vec<ProfileDto>,
    pub current_ids: CurrentProfileIds,
}

#[tauri::command]
pub fn list_profiles(_state: State<'_, AppState>) -> Result<ProfilesResponse, String> {
    Ok(ProfilesResponse {
        profiles: Vec::new(),
        current_ids: CurrentProfileIds {
            claude: None,
            claude_desktop: None,
            codex: None,
        },
    })
}

#[tauri::command]
pub fn create_profile(
    _state: State<'_, AppState>,
    _name: String,
    _scope: String,
) -> Result<ProfileDto, String> {
    Err(profile_retired())
}

#[tauri::command]
pub fn update_profile(
    _state: State<'_, AppState>,
    _id: String,
    _name: Option<String>,
    _resnapshot: Option<bool>,
    _scope: Option<String>,
) -> Result<ProfileDto, String> {
    Err(profile_retired())
}

#[tauri::command]
pub fn delete_profile(_state: State<'_, AppState>, _id: String) -> Result<(), String> {
    Err(profile_retired())
}

#[tauri::command]
pub fn clear_current_profile(_state: State<'_, AppState>, _scope: String) -> Result<(), String> {
    Err(profile_retired())
}

#[tauri::command]
pub fn apply_profile(
    _app: tauri::AppHandle,
    _state: State<'_, AppState>,
    _id: String,
    _scope: String,
) -> Result<Vec<String>, String> {
    Err(profile_retired())
}
