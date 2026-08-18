//! Usage script validation for legacy provider metadata.

use crate::error::AppError;
use crate::provider::UsageScript;

/// Validate leftover UsageScript configuration (boundary checks).
pub(crate) fn validate_usage_script(script: &UsageScript) -> Result<(), AppError> {
    if let Some(interval) = script.auto_query_interval {
        if interval > 1440 {
            return Err(AppError::localized(
                "usage_script.interval_too_large",
                format!("自动查询间隔不能超过 1440 分钟（24小时），当前值: {interval}"),
                format!(
                    "Auto query interval cannot exceed 1440 minutes (24 hours), current: {interval}"
                ),
            ));
        }
    }

    Ok(())
}
