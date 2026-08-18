//! Skill import from deep link
//!
//! GitHub / marketplace skill install via URL is no longer supported.

use super::DeepLinkImportRequest;
use crate::error::AppError;
use crate::store::AppState;

/// Import a skill from deep link request
pub fn import_skill_from_deeplink(
    _state: &AppState,
    request: DeepLinkImportRequest,
) -> Result<String, AppError> {
    if request.resource != "skill" {
        return Err(AppError::InvalidInput(format!(
            "Expected skill resource, got '{}'",
            request.resource
        )));
    }

    Err(AppError::localized(
        "skills.marketplace_retired",
        "技能市场与仓库安装已下线，请从本机已有技能目录导入。",
        "Skill marketplace and repository install have been discontinued. Import skills from local app directories instead.",
    ))
}
