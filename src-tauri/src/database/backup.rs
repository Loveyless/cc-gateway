//! 数据库备份和恢复
//!
//! 提供 SQL 导出/导入和二进制快照备份功能。

use super::{lock_conn, Database};
use crate::config::get_app_config_dir;
use crate::error::AppError;
use chrono::{Local, Utc};
use rusqlite::backup::Backup;
use rusqlite::types::ValueRef;
use rusqlite::Connection;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;

const CC_GATEWAY_SQL_EXPORT_HEADER: &str = "-- CC Gateway SQLite 导出";
const LEGACY_CC_SWITCH_SQL_EXPORT_HEADER: &str = "-- CC Switch SQLite 导出";

/// Bound combined INSERT batches while still amortizing statement parsing.
/// A row larger than this cap is emitted alone because it cannot be split.
const INSERT_BATCH_MAX_ROWS: usize = 200;
const INSERT_BATCH_MAX_BYTES: usize = 1024 * 1024;

/// `dump_sql` 会写出的 PRAGMA。其余 PRAGMA 一律拒绝——`temp_store_directory`
/// 能把临时文件重定向到任意目录，`writable_schema` 能绕过 schema 完整性检查。
const IMPORT_ALLOWED_PRAGMAS: &[&str] = &["foreign_keys", "user_version"];

/// 执行外部 SQL 期间的 authorizer：拒绝一切能**离开临时数据库文件**的动作。
///
/// 头部校验（`validate_cc_switch_sql_export`）只比较一个注释前缀，任何人都能在
/// 合法前缀后面接着写别的语句。`ATTACH DATABASE '/path/x.db'` 的副作用发生在
/// `validate_basic_state` 之前，导入即使最终失败，文件也已经被创建；而 `settings`
/// SQL 导入接受用户选择的备份文件，所以这条路径的输入不可信。
///
/// 为什么是 authorizer 而不是「扫描 ATTACH 关键字」：字符串扫描会被 `/*x*/ATTACH`、
/// 大小写、换行绕过，还漏掉 `VACUUM INTO`。authorizer 在 prepare 阶段按**解析结果**
/// 回调，绕不过语法层。
///
/// 为什么是「拒绝越界动作」而不是「只放行 dump_sql 的语句」：这段 SQL 跑在
/// `NamedTempFile` 建的一次性库上，而那个库的全部内容本来就由这份 SQL 决定。
/// 因此 `DELETE` / `DROP` / `UPDATE` 给不了攻击者任何新东西——**唯一有意义的边界
/// 是那个临时文件本身**。按 dump_sql 的产物做严格白名单只会带来误伤风险（用户
/// 库里出现一种没预料到的对象就恢复不了备份），却不多挡任何攻击。
///
/// 越界动作是实测出来的，不是推断的：
/// - `ATTACH DATABASE 'x'`、`VACUUM INTO 'x'`、裸 `VACUUM` **三者都**报
///   `AuthAction::Attach`，所以拒 `Attach` 一条即可覆盖
/// - 文件后端的虚拟表模块（`csvfile`、`zipfile` 等）能读写任意路径 → 拒 vtable
/// - `Unknown` 是 rusqlite 对未识别动作码的兜底 → 未知即拒，将来 SQLite 新增的
///   跨文件语句会默认落进这里，不依赖有人记得回来补名单
fn import_authorizer(context: rusqlite::hooks::AuthContext<'_>) -> rusqlite::hooks::Authorization {
    use rusqlite::hooks::{AuthAction, Authorization};

    let escapes_temp_db = match context.action {
        AuthAction::Attach { .. } | AuthAction::Detach { .. } => true,
        AuthAction::CreateVtable { .. } | AuthAction::DropVtable { .. } => true,
        AuthAction::Unknown { .. } => true,
        AuthAction::Pragma { pragma_name, .. } => !IMPORT_ALLOWED_PRAGMAS
            .iter()
            .any(|allowed| pragma_name.eq_ignore_ascii_case(allowed)),
        _ => false,
    };

    if escapes_temp_db {
        // SQLite 只会回一句 "not authorized"，不记日志就无从知道是哪条语句被拦。
        log::warn!("SQL 导入拒绝了越界语句: {:?}", context.action);
        Authorization::Deny
    } else {
        Authorization::Allow
    }
}

/// A database backup entry for the UI
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupEntry {
    pub filename: String,
    pub size_bytes: u64,
    pub created_at: String, // ISO 8601
}

impl Database {
    /// 导出为 SQLite 兼容的 SQL 文本（内存字符串，完整导出）
    pub fn export_sql_string(&self) -> Result<String, AppError> {
        let snapshot = self.snapshot_to_memory()?;
        Self::dump_sql(&snapshot, &[])
    }

    /// 导出为 SQLite 兼容的 SQL 文本
    pub fn export_sql(&self, target_path: &Path) -> Result<(), AppError> {
        let dump = self.export_sql_string()?;

        if let Some(parent) = target_path.parent() {
            fs::create_dir_all(parent).map_err(|e| AppError::io(parent, e))?;
        }

        crate::config::atomic_write(target_path, dump.as_bytes())
    }

    /// 从 SQL 文件导入，返回生成的备份 ID（若无备份则为空字符串）
    pub fn import_sql(&self, source_path: &Path) -> Result<String, AppError> {
        if !source_path.exists() {
            return Err(AppError::InvalidInput(format!(
                "SQL 文件不存在: {}",
                source_path.display()
            )));
        }

        let sql_raw = fs::read_to_string(source_path).map_err(|e| AppError::io(source_path, e))?;
        let sql_content = sql_raw.trim_start_matches('\u{feff}');
        self.import_sql_string(sql_content)
    }

    /// 从 SQL 字符串导入，返回生成的备份 ID（若无备份则为空字符串）
    pub fn import_sql_string(&self, sql_raw: &str) -> Result<String, AppError> {
        self.import_sql_string_inner(sql_raw)
    }

    fn import_sql_string_inner(&self, sql_raw: &str) -> Result<String, AppError> {
        let sql_content = sql_raw.trim_start_matches('\u{feff}');
        Self::validate_cc_switch_sql_export(sql_content)?;

        // 导入前备份现有数据库
        let backup_path = self.backup_database_file()?;

        // 在临时数据库执行导入，确保失败不会污染主库
        let temp_file = NamedTempFile::new().map_err(|e| AppError::IoContext {
            context: "创建临时数据库文件失败".to_string(),
            source: e,
        })?;
        let temp_path = temp_file.path().to_path_buf();
        let temp_conn =
            Connection::open(&temp_path).map_err(|e| AppError::Database(e.to_string()))?;

        // authorizer 只覆盖外部 SQL，执行完立刻摘掉：紧随其后的
        // `create_tables_on_conn` / `apply_schema_migrations_on_conn` 是本程序自己的
        // schema 维护语句，不属于需要设防的输入，没必要让它们也过一遍守卫。
        temp_conn.authorizer(Some(import_authorizer));
        let batch_result = temp_conn.execute_batch(sql_content);
        temp_conn.authorizer(
            None::<fn(rusqlite::hooks::AuthContext<'_>) -> rusqlite::hooks::Authorization>,
        );
        batch_result.map_err(|e| AppError::Database(format!("执行 SQL 导入失败: {e}")))?;

        // 补齐缺失表/索引并进行基础校验
        Self::create_tables_on_conn(&temp_conn)?;
        Self::apply_schema_migrations_on_conn(&temp_conn)?;
        Self::validate_basic_state(&temp_conn)?;

        // 使用 Backup 将临时库原子写回主库
        {
            let mut main_conn = lock_conn!(self.conn);
            let backup = Backup::new(&temp_conn, &mut main_conn)
                .map_err(|e| AppError::Database(e.to_string()))?;
            backup
                .step(-1)
                .map_err(|e| AppError::Database(e.to_string()))?;
        }

        let backup_id = backup_path
            .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().to_string()))
            .unwrap_or_default();

        Ok(backup_id)
    }

    /// 创建内存快照以避免长时间持有数据库锁
    pub(crate) fn snapshot_to_memory(&self) -> Result<Connection, AppError> {
        let conn = lock_conn!(self.conn);
        let mut snapshot =
            Connection::open_in_memory().map_err(|e| AppError::Database(e.to_string()))?;

        {
            let backup =
                Backup::new(&conn, &mut snapshot).map_err(|e| AppError::Database(e.to_string()))?;
            backup
                .step(-1)
                .map_err(|e| AppError::Database(e.to_string()))?;
        }

        Ok(snapshot)
    }

    fn validate_cc_switch_sql_export(sql: &str) -> Result<(), AppError> {
        let trimmed = sql.trim_start();
        if trimmed.starts_with(CC_GATEWAY_SQL_EXPORT_HEADER)
            || trimmed.starts_with(LEGACY_CC_SWITCH_SQL_EXPORT_HEADER)
        {
            return Ok(());
        }

        Err(AppError::localized(
            "backup.sql.invalid_format",
            "仅支持导入由 CC Gateway 或上游 CC Switch 导出的 SQL 备份文件。",
            "Only SQL backups exported by CC Gateway or upstream CC Switch are supported.",
        ))
    }

    /// Periodic backup: create a new backup if the latest one is older than the configured interval
    pub(crate) fn periodic_backup_if_needed(&self) -> Result<(), AppError> {
        let interval_hours = crate::settings::effective_backup_interval_hours();
        if interval_hours > 0 {
            let backup_dir = get_app_config_dir().join("backups");
            if !backup_dir.exists() {
                self.backup_database_file()?;
            } else {
                let latest = fs::read_dir(&backup_dir).ok().and_then(|entries| {
                    entries
                        .filter_map(|e| e.ok())
                        .filter(|e| e.path().extension().map(|ext| ext == "db").unwrap_or(false))
                        .filter_map(|e| e.metadata().ok().and_then(|m| m.modified().ok()))
                        .max()
                });

                let interval_secs = u64::from(interval_hours) * 3600;
                let needs_backup = match latest {
                    None => true,
                    Some(last_modified) => {
                        last_modified.elapsed().unwrap_or_default()
                            > std::time::Duration::from_secs(interval_secs)
                    }
                };

                if needs_backup {
                    log::info!(
                        "Periodic backup: latest backup is older than {interval_hours} hours, creating new backup"
                    );
                    self.backup_database_file()?;
                }
            }
        }

        // Periodic maintenance is always enabled, regardless of auto-backup settings.
        let mut reclaimed_rows = 0u64;
        match self.cleanup_old_stream_check_logs(7) {
            Ok(deleted) => {
                reclaimed_rows += deleted;
            }
            Err(e) => {
                log::warn!("Periodic stream_check_logs cleanup failed: {e}");
            }
        }
        match self.rollup_and_prune(30) {
            Ok(deleted) => {
                reclaimed_rows += deleted;
            }
            Err(e) => {
                log::warn!("Periodic rollup_and_prune failed: {e}");
            }
        }
        if reclaimed_rows > 0 {
            let conn = lock_conn!(self.conn);
            if let Err(e) = conn.execute_batch("PRAGMA incremental_vacuum;") {
                log::warn!("Periodic incremental vacuum failed: {e}");
            }
        }

        Ok(())
    }

    /// 生成一致性快照备份，返回备份文件路径（不存在主库时返回 None）
    pub(crate) fn backup_database_file(&self) -> Result<Option<PathBuf>, AppError> {
        let db_path = get_app_config_dir().join("cc-gateway.db");
        if !db_path.exists() {
            return Ok(None);
        }

        let backup_dir = db_path
            .parent()
            .ok_or_else(|| AppError::Config("无效的数据库路径".to_string()))?
            .join("backups");

        fs::create_dir_all(&backup_dir).map_err(|e| AppError::io(&backup_dir, e))?;

        let base_id = format!("db_backup_{}", Local::now().format("%Y%m%d_%H%M%S"));
        let mut backup_id = base_id.clone();
        let mut backup_path = backup_dir.join(format!("{backup_id}.db"));
        let mut counter = 1;
        while backup_path.exists() {
            backup_id = format!("{base_id}_{counter}");
            backup_path = backup_dir.join(format!("{backup_id}.db"));
            counter += 1;
        }

        {
            let conn = lock_conn!(self.conn);
            let mut dest_conn =
                Connection::open(&backup_path).map_err(|e| AppError::Database(e.to_string()))?;
            let backup = Backup::new(&conn, &mut dest_conn)
                .map_err(|e| AppError::Database(e.to_string()))?;
            backup
                .step(-1)
                .map_err(|e| AppError::Database(e.to_string()))?;
        }

        Self::cleanup_db_backups(&backup_dir)?;
        Ok(Some(backup_path))
    }

    /// 清理旧的数据库备份，保留最新的 N 个
    fn cleanup_db_backups(dir: &Path) -> Result<(), AppError> {
        let retain = crate::settings::effective_backup_retain_count();
        let entries = match fs::read_dir(dir) {
            Ok(iter) => iter
                .filter_map(|entry| entry.ok())
                .filter(|entry| {
                    entry
                        .path()
                        .extension()
                        .map(|ext| ext == "db")
                        .unwrap_or(false)
                })
                .collect::<Vec<_>>(),
            Err(_) => return Ok(()),
        };

        if entries.len() <= retain {
            return Ok(());
        }

        let remove_count = entries.len().saturating_sub(retain);
        let mut sorted = entries;
        sorted.sort_by_key(|entry| entry.metadata().and_then(|m| m.modified()).ok());

        for entry in sorted.into_iter().take(remove_count) {
            if let Err(err) = fs::remove_file(entry.path()) {
                log::warn!("删除旧数据库备份失败 {}: {}", entry.path().display(), err);
            }
        }
        Ok(())
    }

    /// 基础状态校验
    fn validate_basic_state(conn: &Connection) -> Result<(), AppError> {
        let provider_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM providers", [], |row| row.get(0))
            .map_err(|e| AppError::Database(e.to_string()))?;
        let mcp_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM mcp_servers", [], |row| row.get(0))
            .map_err(|e| AppError::Database(e.to_string()))?;

        if provider_count == 0 && mcp_count == 0 {
            return Err(AppError::Config(
                "导入的 SQL 未包含有效的供应商或 MCP 数据".to_string(),
            ));
        }
        Ok(())
    }

    /// 导出数据库为 SQL 文本
    fn dump_sql(conn: &Connection, skip_tables: &[&str]) -> Result<String, AppError> {
        let mut output = String::new();
        let timestamp = Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
        let user_version: i64 = conn
            .query_row("PRAGMA user_version;", [], |row| row.get(0))
            .unwrap_or(0);

        output.push_str(&format!(
            "{CC_GATEWAY_SQL_EXPORT_HEADER}\n-- 生成时间: {timestamp}\n-- user_version: {user_version}\n"
        ));
        output.push_str("PRAGMA foreign_keys=OFF;\n");
        output.push_str(&format!("PRAGMA user_version={user_version};\n"));
        output.push_str("BEGIN TRANSACTION;\n");

        // 导出 schema
        let mut stmt = conn
            .prepare(
                "SELECT type, name, tbl_name, sql
                 FROM sqlite_master
                 WHERE sql NOT NULL AND type IN ('table','index','trigger','view')
                 ORDER BY type='table' DESC, name",
            )
            .map_err(|e| AppError::Database(e.to_string()))?;

        let mut tables = Vec::new();
        let mut triggers = Vec::new();
        let mut rows = stmt
            .query([])
            .map_err(|e| AppError::Database(e.to_string()))?;
        while let Some(row) = rows.next().map_err(|e| AppError::Database(e.to_string()))? {
            let obj_type: String = row.get(0).map_err(|e| AppError::Database(e.to_string()))?;
            let name: String = row.get(1).map_err(|e| AppError::Database(e.to_string()))?;
            let sql: String = row.get(3).map_err(|e| AppError::Database(e.to_string()))?;

            // 跳过 SQLite 内部对象（如 sqlite_sequence）
            if name.starts_with("sqlite_") {
                continue;
            }

            if obj_type == "trigger" {
                triggers.push(sql);
                continue;
            }

            output.push_str(&sql);
            output.push_str(";\n");
            if obj_type == "table" {
                tables.push(name);
            }
        }

        // 导出数据
        for table in tables {
            if skip_tables.iter().any(|t| *t == table) {
                continue;
            }
            let columns = Self::get_table_columns(conn, &table)?;
            if columns.is_empty() {
                continue;
            }

            // 每行一条 INSERT 是导入慢的根源：恢复侧要为每条语句单独
            // 解析/准备/收尾，2 万行实测 21 秒（内存库上一样慢，说明是
            // 纯 CPU 而非 I/O）。合并成多行 VALUES 后同样数据 <100ms。
            // SQLite 从 3.7.11（2012）起支持多行 VALUES，且导入侧是通用
            // execute_batch，新旧两种格式都能读——向后兼容无忧。
            let quoted_table = Self::quote_identifier(&table);
            let quoted_columns = columns
                .iter()
                .map(|column| Self::quote_identifier(column))
                .collect::<Vec<_>>()
                .join(", ");
            let insert_prefix = format!("INSERT INTO {quoted_table} ({quoted_columns}) VALUES ");

            let mut stmt = conn
                .prepare(&format!("SELECT {quoted_columns} FROM {quoted_table}"))
                .map_err(|e| AppError::Database(e.to_string()))?;
            let mut rows = stmt
                .query([])
                .map_err(|e| AppError::Database(e.to_string()))?;

            let mut pending_rows = 0usize;
            let mut batch = String::new();
            while let Some(row) = rows.next().map_err(|e| AppError::Database(e.to_string()))? {
                let mut values = Vec::with_capacity(columns.len());
                for idx in 0..columns.len() {
                    let value = row
                        .get_ref(idx)
                        .map_err(|e| AppError::Database(e.to_string()))?;
                    values.push(Self::format_sql_value(value)?);
                }

                let row_sql = format!("({})", values.join(", "));
                let separator_bytes = usize::from(pending_rows > 0);
                if pending_rows > 0
                    && batch.len() + separator_bytes + row_sql.len() + 2 > INSERT_BATCH_MAX_BYTES
                {
                    batch.push_str(";\n");
                    output.push_str(&batch);
                    pending_rows = 0;
                }

                if pending_rows == 0 {
                    batch.clear();
                    batch.push_str(&insert_prefix);
                } else {
                    batch.push(',');
                }
                batch.push_str(&row_sql);
                pending_rows += 1;

                if pending_rows >= INSERT_BATCH_MAX_ROWS {
                    batch.push_str(";\n");
                    output.push_str(&batch);
                    pending_rows = 0;
                }
            }
            if pending_rows > 0 {
                batch.push_str(";\n");
                output.push_str(&batch);
            }
        }

        // Triggers must be created after loading table data so they cannot
        // change dump rows or abandon the remainder of a multi-row INSERT.
        for sql in triggers {
            output.push_str(&sql);
            output.push_str(";\n");
        }

        output.push_str("COMMIT;\nPRAGMA foreign_keys=ON;\n");
        Ok(output)
    }

    fn quote_identifier(identifier: &str) -> String {
        format!("\"{}\"", identifier.replace('"', "\"\""))
    }

    /// 获取表的列名列表
    fn get_table_columns(conn: &Connection, table: &str) -> Result<Vec<String>, AppError> {
        let quoted_table = Self::quote_identifier(table);
        let mut stmt = conn
            .prepare(&format!("PRAGMA table_info({quoted_table})"))
            .map_err(|e| AppError::Database(e.to_string()))?;
        let iter = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .map_err(|e| AppError::Database(e.to_string()))?;

        let mut columns = Vec::new();
        for col in iter {
            columns.push(col.map_err(|e| AppError::Database(e.to_string()))?);
        }
        Ok(columns)
    }

    /// 格式化 SQL 值
    fn format_sql_value(value: ValueRef<'_>) -> Result<String, AppError> {
        match value {
            ValueRef::Null => Ok("NULL".to_string()),
            ValueRef::Integer(i) => Ok(i.to_string()),
            ValueRef::Real(f) => Ok(f.to_string()),
            ValueRef::Text(t) => {
                let text = std::str::from_utf8(t)
                    .map_err(|e| AppError::Database(format!("文本字段不是有效的 UTF-8: {e}")))?;
                let escaped = text.replace('\'', "''");
                Ok(format!("'{escaped}'"))
            }
            ValueRef::Blob(bytes) => {
                let mut s = String::from("X'");
                for b in bytes {
                    use std::fmt::Write;
                    let _ = write!(&mut s, "{b:02X}");
                }
                s.push('\'');
                Ok(s)
            }
        }
    }

    /// List all database backup files, sorted by creation time (newest first)
    pub fn list_backups() -> Result<Vec<BackupEntry>, AppError> {
        let backup_dir = get_app_config_dir().join("backups");
        if !backup_dir.exists() {
            return Ok(vec![]);
        }

        let mut entries: Vec<BackupEntry> = fs::read_dir(&backup_dir)
            .map_err(|e| AppError::io(&backup_dir, e))?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().map(|ext| ext == "db").unwrap_or(false))
            .filter_map(|e| {
                let metadata = e.metadata().ok()?;
                let filename = e.file_name().to_string_lossy().to_string();
                let size_bytes = metadata.len();
                let created_at = metadata
                    .modified()
                    .ok()
                    .map(|t| {
                        let dt: chrono::DateTime<Utc> = t.into();
                        dt.to_rfc3339()
                    })
                    .unwrap_or_default();
                Some(BackupEntry {
                    filename,
                    size_bytes,
                    created_at,
                })
            })
            .collect();

        // Sort by created_at descending (newest first)
        entries.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(entries)
    }

    /// Restore database from a backup file. Returns the safety backup ID.
    pub fn restore_from_backup(&self, filename: &str) -> Result<String, AppError> {
        // Security: validate filename to prevent path traversal
        if filename.contains("..")
            || filename.contains('/')
            || filename.contains('\\')
            || !filename.ends_with(".db")
        {
            return Err(AppError::InvalidInput(
                "Invalid backup filename".to_string(),
            ));
        }

        let backup_dir = get_app_config_dir().join("backups");
        let backup_path = backup_dir.join(filename);

        if !backup_path.exists() {
            return Err(AppError::InvalidInput(format!(
                "Backup file not found: {filename}"
            )));
        }

        // Step 1: Create safety backup of current database
        let safety_backup = self.backup_database_file()?;
        let safety_id = safety_backup
            .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().to_string()))
            .unwrap_or_default();

        // Step 2: Open the backup file and restore it to the main database
        let source_conn =
            Connection::open(&backup_path).map_err(|e| AppError::Database(e.to_string()))?;

        {
            let mut main_conn = lock_conn!(self.conn);
            let backup = Backup::new(&source_conn, &mut main_conn)
                .map_err(|e| AppError::Database(e.to_string()))?;
            backup
                .step(-1)
                .map_err(|e| AppError::Database(e.to_string()))?;
        }

        // Step 3: Run schema migrations (backup may be from an older version)
        self.create_tables()?;
        self.apply_schema_migrations()?;
        self.ensure_model_pricing_seeded()?;

        log::info!("Database restored from backup: {filename}, safety backup: {safety_id}");
        Ok(safety_id)
    }

    /// Rename a backup file. Returns the new filename.
    pub fn rename_backup(old_filename: &str, new_name: &str) -> Result<String, AppError> {
        // Validate old filename (path traversal + .db suffix)
        if old_filename.contains("..")
            || old_filename.contains('/')
            || old_filename.contains('\\')
            || !old_filename.ends_with(".db")
        {
            return Err(AppError::InvalidInput(
                "Invalid backup filename".to_string(),
            ));
        }

        // Clean new name
        let trimmed = new_name.trim();
        if trimmed.is_empty() {
            return Err(AppError::InvalidInput(
                "New name cannot be empty".to_string(),
            ));
        }

        // Length limit (without .db suffix)
        let name_part = trimmed.strip_suffix(".db").unwrap_or(trimmed);
        if name_part.len() > 100 {
            return Err(AppError::InvalidInput(
                "Name too long (max 100 characters)".to_string(),
            ));
        }

        // Prevent path traversal in new name
        if name_part.contains("..")
            || name_part.contains('/')
            || name_part.contains('\\')
            || name_part.contains('\0')
        {
            return Err(AppError::InvalidInput(
                "Invalid characters in new name".to_string(),
            ));
        }

        let new_filename = format!("{name_part}.db");

        let backup_dir = get_app_config_dir().join("backups");
        let old_path = backup_dir.join(old_filename);
        let new_path = backup_dir.join(&new_filename);

        if !old_path.exists() {
            return Err(AppError::InvalidInput(format!(
                "Backup file not found: {old_filename}"
            )));
        }

        if new_path.exists() {
            return Err(AppError::InvalidInput(format!(
                "A backup named '{new_filename}' already exists"
            )));
        }

        fs::rename(&old_path, &new_path).map_err(|e| AppError::io(&old_path, e))?;
        log::info!("Renamed backup: {old_filename} -> {new_filename}");
        Ok(new_filename)
    }

    /// Delete a backup file permanently.
    pub fn delete_backup(filename: &str) -> Result<(), AppError> {
        // Validate filename (path traversal + .db suffix)
        if filename.contains("..")
            || filename.contains('/')
            || filename.contains('\\')
            || !filename.ends_with(".db")
        {
            return Err(AppError::InvalidInput(
                "Invalid backup filename".to_string(),
            ));
        }

        let backup_path = get_app_config_dir().join("backups").join(filename);
        if !backup_path.exists() {
            return Err(AppError::InvalidInput(format!(
                "Backup file not found: {filename}"
            )));
        }

        fs::remove_file(&backup_path).map_err(|e| AppError::io(&backup_path, e))?;
        log::info!("Deleted backup: {filename}");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::Database;
    use crate::error::AppError;
    use crate::settings::{update_settings, AppSettings};
    use rusqlite::Connection;
    use serial_test::serial;

    struct TestHomeGuard {
        previous_test_home: Option<std::ffi::OsString>,
        temp_dir: tempfile::TempDir,
    }

    impl TestHomeGuard {
        fn new() -> Self {
            let temp_dir = tempfile::tempdir().expect("create isolated test home");
            let previous_test_home = std::env::var_os("CC_GATEWAY_TEST_HOME");
            std::env::set_var("CC_GATEWAY_TEST_HOME", temp_dir.path());
            // Prevent the Windows legacy-HOME fallback without mutating HOME:
            // an existing default DB keeps get_app_config_dir() anchored under
            // CC_GATEWAY_TEST_HOME and makes import exercise its safety backup.
            let config_dir = temp_dir.path().join(".cc-gateway");
            std::fs::create_dir_all(&config_dir).expect("create isolated config directory");
            std::fs::File::create(config_dir.join("cc-gateway.db"))
                .expect("create isolated database sentinel");
            let guard = Self {
                previous_test_home,
                temp_dir,
            };
            let resolved = crate::config::get_app_config_dir();
            assert!(
                resolved.starts_with(guard.temp_dir.path()),
                "isolated test home resolved outside its temp directory: {}",
                resolved.display()
            );
            guard
        }

        fn path(&self) -> &std::path::Path {
            self.temp_dir.path()
        }
    }

    impl Drop for TestHomeGuard {
        fn drop(&mut self) {
            match self.previous_test_home.as_ref() {
                Some(previous) => std::env::set_var("CC_GATEWAY_TEST_HOME", previous),
                None => std::env::remove_var("CC_GATEWAY_TEST_HOME"),
            }
        }
    }

    #[test]
    #[serial]
    fn import_rejects_cross_file_statements_and_leaves_no_file_behind() -> Result<(), AppError> {
        let test_home = TestHomeGuard::new();
        // `VACUUM INTO` 是关键字扫描方案最容易漏的一条：它不含 "ATTACH" 字样，
        // 却和 ATTACH 一样落到 `AuthAction::Attach`（实测），因此同一条规则挡住两者。
        let cases: [(&str, &str); 2] = [
            ("attach", "ATTACH DATABASE '{path}' AS evil;"),
            ("vacuum-into", "VACUUM INTO '{path}';"),
        ];

        for (label, template) in cases {
            let target = test_home
                .path()
                .join(format!("cc-gateway-authorizer-{label}.sqlite"));

            // 合法的导出头 + 越界语句。头部校验只比前缀，这份输入过得了它，
            // 真正拦下来的必须是 authorizer。
            let malicious = format!(
                "{}\n{}\n",
                super::CC_GATEWAY_SQL_EXPORT_HEADER,
                template.replace("{path}", &target.to_string_lossy().replace('\'', "''"))
            );

            let db = Database::memory()?;
            let result = db.import_sql_string(&malicious);

            let error = result.expect_err("越界 SQL 必须被拒绝");
            assert!(
                error.to_string().to_ascii_lowercase().contains("authoriz"),
                "{label} 必须由 authorizer 拒绝，实际错误: {error}"
            );
            // 光报错不够：文件创建发生在 prepare 之后、`validate_basic_state` 之前，
            // 守卫若失效，即便导入整体失败，文件也已经躺在磁盘上了。
            assert!(
                !target.exists(),
                "被拒绝的 {label} 不得在磁盘上留下文件: {}",
                target.display()
            );
        }
        Ok(())
    }

    #[test]
    #[serial]
    fn import_still_accepts_a_genuine_export() -> Result<(), AppError> {
        let _test_home = TestHomeGuard::new();
        // 白名单收得紧，必须有一条回归防线证明它没误伤自家导出格式——
        // 这条测试红了就说明 dump_sql 写出了白名单没覆盖的语句。
        let source = Database::memory()?;
        {
            let conn = crate::database::lock_conn!(source.conn);
            conn.execute(
                "INSERT INTO providers (id, app_type, name, settings_config, meta)
                 VALUES ('p1', 'claude', 'Provider One', '{}', '{}')",
                [],
            )?;
        }
        let exported = source.export_sql_string()?;

        let target = Database::memory()?;
        target.import_sql_string(&exported)?;

        let conn = crate::database::lock_conn!(target.conn);
        let name: String = conn.query_row(
            "SELECT name FROM providers WHERE id = 'p1' AND app_type = 'claude'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(name, "Provider One");
        Ok(())
    }

    #[test]
    #[serial]
    fn sql_file_api_round_trips_existing_export_behavior() -> Result<(), AppError> {
        let test_home = TestHomeGuard::new();
        let source = Database::memory()?;
        {
            let conn = crate::database::lock_conn!(source.conn);
            conn.execute_batch(
                "INSERT INTO providers (id, app_type, name, settings_config, meta)
                 VALUES ('file-provider', 'claude', 'File Provider', '{}', '{}');
                 INSERT INTO proxy_request_logs (
                     request_id, provider_id, app_type, model,
                     input_tokens, output_tokens, total_cost_usd,
                     latency_ms, status_code, created_at
                 ) VALUES ('file-request', 'file-provider', 'claude', 'claude-file', 5, 3, '0', 10, 200, 1);",
            )?;
        }

        let backup_path = test_home.path().join("round-trip.sql");
        source.export_sql(&backup_path)?;

        let target = Database::memory()?;
        {
            let conn = crate::database::lock_conn!(target.conn);
            conn.execute(
                "INSERT INTO providers (id, app_type, name, settings_config, meta)
                 VALUES ('target-sentinel', 'claude', 'Must Be Replaced', '{}', '{}')",
                [],
            )?;
        }
        target.import_sql(&backup_path)?;

        let conn = crate::database::lock_conn!(target.conn);
        let providers = conn
            .prepare("SELECT id FROM providers ORDER BY id")?
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(providers, vec!["file-provider"]);
        let request_exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM proxy_request_logs WHERE request_id = 'file-request')",
            [],
            |row| row.get(0),
        )?;
        assert!(request_exists, "文件 API 必须完整恢复导出数据");
        Ok(())
    }

    #[test]
    #[serial]
    fn failed_sql_import_keeps_the_existing_database_unchanged() -> Result<(), AppError> {
        let _test_home = TestHomeGuard::new();
        let target = Database::memory()?;
        {
            let conn = crate::database::lock_conn!(target.conn);
            conn.execute(
                "INSERT INTO providers (id, app_type, name, settings_config, meta)
                 VALUES ('sentinel', 'claude', 'Existing Provider', '{}', '{}')",
                [],
            )?;
        }

        let invalid_sql = format!(
            "{}\nBEGIN TRANSACTION;\nCREATE TABLE partial (id INTEGER);\nTHIS IS NOT SQL;\n",
            super::CC_GATEWAY_SQL_EXPORT_HEADER
        );
        assert!(target.import_sql_string(&invalid_sql).is_err());

        let conn = crate::database::lock_conn!(target.conn);
        let provider: (i64, String, String) = conn.query_row(
            "SELECT COUNT(*), MIN(id), MIN(name) FROM providers",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        assert_eq!(provider, (1, "sentinel".into(), "Existing Provider".into()));
        let partial_exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'partial')",
            [],
            |row| row.get(0),
        )?;
        assert!(!partial_exists, "失败导入的临时对象不得进入主库");
        Ok(())
    }

    #[test]
    #[serial]
    fn import_accepts_upstream_header_and_legacy_single_row_exports() -> Result<(), AppError> {
        let _test_home = TestHomeGuard::new();
        // This schema is copied from the v3.8.3 tag. Its data statements use
        // the historical one-row-per-INSERT format and omit all newer columns.
        let legacy = format!(
            "{}\nPRAGMA foreign_keys=OFF;\nPRAGMA user_version=1;\nBEGIN TRANSACTION;\n{}
             INSERT INTO providers (
                 id, app_type, name, settings_config, meta, is_current
             ) VALUES (
                 'legacy-provider', 'claude', 'Legacy Provider',
                 '{{\"anthropicApiKey\":\"sk-old\"}}', '{{}}', 1
             );
             INSERT INTO skills (key, installed, installed_at)
             VALUES ('claude:legacy-skill', 1, 1700000000);
             COMMIT;\nPRAGMA foreign_keys=ON;\n",
            super::LEGACY_CC_SWITCH_SQL_EXPORT_HEADER,
            crate::database::tests::V3_8_SCHEMA_V1_SQL,
        );

        let target = Database::memory()?;
        target.import_sql_string(&legacy)?;

        let conn = crate::database::lock_conn!(target.conn);
        let user_version: i32 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        assert_eq!(user_version, crate::database::SCHEMA_VERSION);
        let provider: (String, String) = conn.query_row(
            "SELECT name, settings_config FROM providers WHERE id = 'legacy-provider'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!(
            provider,
            (
                "Legacy Provider".into(),
                "{\"anthropicApiKey\":\"sk-old\"}".into()
            )
        );
        let cost_multiplier: String = conn.query_row(
            "SELECT cost_multiplier FROM providers WHERE id = 'legacy-provider'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(cost_multiplier, "1.0");
        let skill_snapshot: String = conn.query_row(
            "SELECT value FROM settings WHERE key = 'skills_ssot_migration_snapshot'",
            [],
            |row| row.get(0),
        )?;
        assert!(
            skill_snapshot.contains("legacy-skill"),
            "重建 skills 表时必须保留旧数据迁移快照"
        );
        Ok(())
    }

    #[test]
    #[serial]
    fn dump_sql_batches_rows_into_multi_row_inserts() -> Result<(), AppError> {
        let _test_home = TestHomeGuard::new();
        // 每行一条 INSERT 是导入慢的根源（恢复侧逐条解析，2 万行实测 21s）。
        // 这条测试钉死批量格式：450 行必须合并成 ceil(450/200) = 3 条语句。
        // 一旦退回到逐行导出，这里立刻变红。
        let db = Database::memory()?;
        {
            let conn = crate::database::lock_conn!(db.conn);
            for i in 0..450 {
                conn.execute(
                    "INSERT INTO providers (id, app_type, name, settings_config, meta)
                     VALUES (?1, 'claude', 'p', '{}', '{}')",
                    [format!("p{i}")],
                )?;
            }
        }

        let sql = db.export_sql_string()?;
        let insert_count = sql.matches("INSERT INTO \"providers\"").count();
        assert_eq!(
            insert_count, 3,
            "450 行应合并为 3 条多行 INSERT（每批 200 行），实际 {insert_count} 条"
        );

        let target = Database::memory()?;
        target.import_sql_string(&sql)?;
        let conn = crate::database::lock_conn!(target.conn);
        let row_count: i64 =
            conn.query_row("SELECT COUNT(*) FROM providers", [], |row| row.get(0))?;
        assert_eq!(row_count, 450, "批次边界不得漏行或重复行");
        for boundary in [0, 199, 200, 399, 400, 449] {
            let exists: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM providers WHERE id = ?1)",
                [format!("p{boundary}")],
                |row| row.get(0),
            )?;
            assert!(exists, "批次边界行 p{boundary} 必须完整恢复");
        }
        Ok(())
    }

    #[test]
    fn dump_sql_splits_large_rows_by_statement_bytes() -> Result<(), AppError> {
        let source = Connection::open_in_memory()?;
        source.execute(
            "CREATE TABLE large_rows (id INTEGER PRIMARY KEY, payload TEXT NOT NULL)",
            [],
        )?;

        // Each row fits below the byte cap, while any pair exceeds it.
        let payload = "x".repeat(super::INSERT_BATCH_MAX_BYTES / 2 + 1024);
        for id in 1..=3 {
            source.execute(
                "INSERT INTO large_rows (id, payload) VALUES (?1, ?2)",
                rusqlite::params![id, payload],
            )?;
        }

        let sql = Database::dump_sql(&source, &[])?;
        let inserts = sql
            .lines()
            .filter(|line| line.starts_with("INSERT INTO \"large_rows\""))
            .collect::<Vec<_>>();
        assert_eq!(inserts.len(), 3, "超大字段应按 SQL 字节数提前切批");
        assert!(
            inserts
                .iter()
                .all(|statement| statement.len() <= super::INSERT_BATCH_MAX_BYTES),
            "每条可独立容纳的 INSERT 都应保持在字节上限内"
        );

        let target = Connection::open_in_memory()?;
        target.execute_batch(&sql)?;
        let (count, min_len, max_len): (i64, i64, i64) = target.query_row(
            "SELECT COUNT(*), MIN(length(payload)), MAX(length(payload)) FROM large_rows",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        assert_eq!(count, 3);
        assert_eq!(min_len, payload.len() as i64);
        assert_eq!(max_len, payload.len() as i64);
        Ok(())
    }

    #[test]
    fn dump_sql_round_trips_generated_columns_and_quoted_identifiers() -> Result<(), AppError> {
        let source = Connection::open_in_memory()?;
        source.execute_batch(
            r#"
            CREATE TABLE "generated""values" (
                "a" TEXT NOT NULL,
                "computed" TEXT GENERATED ALWAYS AS ("a" || '-generated') STORED,
                "b""tail" TEXT NOT NULL
            );
            INSERT INTO "generated""values" ("a", "b""tail")
            VALUES ('source', 'ordinary-tail');
            "#,
        )?;

        let sql = Database::dump_sql(&source, &[])?;
        assert!(sql.contains("INSERT INTO \"generated\"\"values\" (\"a\", \"b\"\"tail\") VALUES"));

        let target = Connection::open_in_memory()?;
        target.execute_batch(&sql)?;
        let values: (String, String, String) = target.query_row(
            "SELECT \"a\", \"computed\", \"b\"\"tail\" FROM \"generated\"\"values\"",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        assert_eq!(
            values,
            (
                "source".to_string(),
                "source-generated".to_string(),
                "ordinary-tail".to_string()
            )
        );
        Ok(())
    }

    #[test]
    fn dump_sql_loads_rows_before_creating_triggers() -> Result<(), AppError> {
        let source = Connection::open_in_memory()?;
        source.execute_batch(
            "CREATE TABLE triggered_rows (seq INTEGER PRIMARY KEY);
             INSERT INTO triggered_rows VALUES (1), (2), (3);
             CREATE TRIGGER ignore_second_row
             BEFORE INSERT ON triggered_rows
             WHEN NEW.seq = 2
             BEGIN
                 SELECT RAISE(IGNORE);
             END;",
        )?;

        let sql = Database::dump_sql(&source, &[])?;
        let data_pos = sql.find("INSERT INTO \"triggered_rows\"").unwrap();
        let trigger_pos = sql.find("CREATE TRIGGER ignore_second_row").unwrap();
        assert!(data_pos < trigger_pos, "触发器必须在数据恢复完成后创建");

        let target = Connection::open_in_memory()?;
        target.execute_batch(&sql)?;
        let rows = target
            .prepare("SELECT seq FROM triggered_rows ORDER BY seq")?
            .query_map([], |row| row.get::<_, i64>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(rows, vec![1, 2, 3]);
        let trigger_exists: bool = target.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'trigger' AND name = 'ignore_second_row')",
            [],
            |row| row.get(0),
        )?;
        assert!(trigger_exists, "触发器本身仍必须随备份恢复");
        Ok(())
    }

    #[test]
    fn dump_sql_preserves_indexes_and_views() -> Result<(), AppError> {
        let source = Connection::open_in_memory()?;
        source.execute_batch(
            "CREATE TABLE indexed_rows (id INTEGER PRIMARY KEY, value TEXT NOT NULL);
             CREATE UNIQUE INDEX indexed_rows_value_idx ON indexed_rows(value);
             CREATE VIEW indexed_rows_view AS
                 SELECT id, value FROM indexed_rows WHERE value LIKE 'kept%';
             CREATE TRIGGER a_insert_indexed_rows_view
             INSTEAD OF INSERT ON indexed_rows_view
             BEGIN
                 INSERT INTO indexed_rows (id, value) VALUES (NEW.id, NEW.value);
             END;
             INSERT INTO indexed_rows VALUES (1, 'kept-value'), (2, 'hidden-value');",
        )?;

        let sql = Database::dump_sql(&source, &[])?;
        let target = Connection::open_in_memory()?;
        target.execute_batch(&sql)?;

        for (object_type, object_name) in [
            ("index", "indexed_rows_value_idx"),
            ("view", "indexed_rows_view"),
            ("trigger", "a_insert_indexed_rows_view"),
        ] {
            let exists: bool = target.query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM sqlite_master WHERE type = ?1 AND name = ?2
                )",
                [object_type, object_name],
                |row| row.get(0),
            )?;
            assert!(exists, "{object_type} {object_name} 必须随 SQL dump 恢复");
        }

        target.execute(
            "INSERT INTO indexed_rows_view (id, value) VALUES (3, 'kept-via-trigger')",
            [],
        )?;
        let view_rows = target
            .prepare("SELECT id, value FROM indexed_rows_view ORDER BY id")?
            .query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(
            view_rows,
            vec![
                (1, "kept-value".to_string()),
                (3, "kept-via-trigger".to_string()),
            ]
        );
        Ok(())
    }

    #[test]
    #[serial]
    fn multi_row_dump_round_trips_special_values() -> Result<(), AppError> {
        let _test_home = TestHomeGuard::new();
        // 多行 VALUES 的转义面比单行宽：单引号、换行、英文逗号（列分隔符）、
        // 中文、emoji、BLOB、NULL——任何一个处理错都会让整批语法崩掉或数据变形。
        let source = Database::memory()?;
        {
            let conn = crate::database::lock_conn!(source.conn);
            conn.execute(
                "INSERT INTO providers (id, app_type, name, settings_config, meta)
                 VALUES ('special', 'claude', ?1, ?2, '{}')",
                rusqlite::params!["O'Brien,\n第二行 \"quoted\" 😀", "{\"key\": \"it's, ok\"}"],
            )?;
            conn.execute(
                "INSERT INTO providers (id, app_type, name, settings_config, meta)
                 VALUES ('with-blob', 'claude', 'blob', X'00FF10', '{}')",
                [],
            )?;
            conn.execute(
                "INSERT INTO providers (id, app_type, name, settings_config, meta, category)
                 VALUES ('with-null', 'claude', 'nullcat', '{}', '{}', NULL)",
                [],
            )?;
        }

        let sql = source.export_sql_string()?;
        let target = Database::memory()?;
        target.import_sql_string(&sql)?;

        let conn = crate::database::lock_conn!(target.conn);
        let name: String = conn.query_row(
            "SELECT name FROM providers WHERE id = 'special'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(name, "O'Brien,\n第二行 \"quoted\" 😀");
        let cfg: String = conn.query_row(
            "SELECT settings_config FROM providers WHERE id = 'special'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(cfg, "{\"key\": \"it's, ok\"}");

        let blob_type: String = conn.query_row(
            "SELECT typeof(settings_config) FROM providers WHERE id = 'with-blob'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(blob_type, "blob", "BLOB 存储类型必须在往返后保留");
        let blob: Vec<u8> = conn.query_row(
            "SELECT settings_config FROM providers WHERE id = 'with-blob'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(blob, vec![0x00, 0xFF, 0x10]);

        let category: Option<String> = conn.query_row(
            "SELECT category FROM providers WHERE id = 'with-null'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(category, None, "NULL 必须在往返后保留");
        Ok(())
    }

    #[test]
    #[serial]
    fn periodic_maintenance_runs_even_when_auto_backup_disabled() -> Result<(), AppError> {
        let _test_home = TestHomeGuard::new();

        let settings = AppSettings {
            backup_interval_hours: Some(0),
            ..AppSettings::default()
        };
        update_settings(settings).expect("disable auto backup");

        let db = Database::memory()?;
        let now = chrono::Utc::now().timestamp();
        let old_ts = now - 40 * 86400;
        let old_stream_ts = now - 8 * 86400;

        {
            let conn = crate::database::lock_conn!(db.conn);
            conn.execute(
                "INSERT INTO proxy_request_logs (
                    request_id, provider_id, app_type, model,
                    input_tokens, output_tokens, total_cost_usd,
                    latency_ms, status_code, created_at
                ) VALUES ('old-req', 'p1', 'claude', 'claude-3', 100, 50, '0.01', 100, 200, ?1)",
                [old_ts],
            )?;
            conn.execute(
                "INSERT INTO stream_check_logs (
                    provider_id, provider_name, app_type, status, success, message,
                    response_time_ms, http_status, model_used, retry_count, tested_at
                ) VALUES ('p1', 'Provider 1', 'claude', 'operational', 1, 'ok', 42, 200, 'claude-3', 0, ?1)",
                [old_stream_ts],
            )?;
        }

        db.periodic_backup_if_needed()?;

        let (remaining_request_logs, stream_logs, rollups): (i64, i64, i64) = {
            let conn = crate::database::lock_conn!(db.conn);
            let remaining_request_logs =
                conn.query_row("SELECT COUNT(*) FROM proxy_request_logs", [], |row| {
                    row.get(0)
                })?;
            let stream_logs =
                conn.query_row("SELECT COUNT(*) FROM stream_check_logs", [], |row| {
                    row.get(0)
                })?;
            let rollups =
                conn.query_row("SELECT COUNT(*) FROM usage_daily_rollups", [], |row| {
                    row.get(0)
                })?;
            (remaining_request_logs, stream_logs, rollups)
        };

        assert_eq!(
            remaining_request_logs, 0,
            "old request logs should still be pruned when auto backup is disabled"
        );
        assert_eq!(
            stream_logs, 0,
            "old stream check logs should still be pruned when auto backup is disabled"
        );
        assert_eq!(rollups, 1, "old request logs should be rolled up");

        Ok(())
    }

    /// 性能基准（不是回归测试）：用接近重度代理用户的行数测量
    /// 完整 SQL 导出和本地文件导入的耗时与产物大小。
    ///
    /// 手动运行：`cargo test --lib perf_backup -- --ignored --nocapture`
    #[test]
    #[ignore = "perf harness, run explicitly"]
    #[serial]
    fn perf_backup_export_import_paths() -> Result<(), AppError> {
        use std::time::Instant;

        const LOG_ROWS: usize = 20_000;
        const STREAM_ROWS: usize = 5_000;
        const ROLLUP_ROWS: usize = 1_000;

        let _test_home = TestHomeGuard::new();

        fn populate(
            db: &Database,
            log_rows: usize,
            stream_rows: usize,
            rollup_rows: usize,
        ) -> Result<(), AppError> {
            let mut conn = crate::database::lock_conn!(db.conn);
            let tx = conn.transaction()?;
            for i in 0..50 {
                tx.execute(
                    "INSERT INTO providers (id, app_type, name, settings_config, meta)
                     VALUES (?1, 'claude', ?2, '{}', '{}')",
                    rusqlite::params![format!("p{i}"), format!("Provider {i}")],
                )?;
            }
            for i in 0..log_rows {
                tx.execute(
                    "INSERT INTO proxy_request_logs (
                        request_id, provider_id, app_type, model,
                        input_tokens, output_tokens, total_cost_usd,
                        latency_ms, status_code, created_at
                    ) VALUES (?1, 'p1', 'claude', 'claude-3', 100, 50, '0.01', 120, 200, 1000)",
                    [format!("req-{i}")],
                )?;
            }
            for i in 0..stream_rows {
                tx.execute(
                    "INSERT INTO stream_check_logs (
                        provider_id, provider_name, app_type, status, success, message,
                        response_time_ms, http_status, model_used, retry_count, tested_at
                    ) VALUES ('p1', 'Provider 1', 'claude', 'operational', 1, 'ok', 42, 200, 'claude-3', 0, ?1)",
                    [1000i64 + i as i64],
                )?;
            }
            for i in 0..rollup_rows {
                // (date, app_type, provider_id, model, request_model, pricing_model)
                // 上有 UNIQUE 约束，日期必须逐行唯一。
                let date = format!(
                    "{:04}-{:02}-{:02}",
                    2025 + i / 336,
                    i / 28 % 12 + 1,
                    i % 28 + 1
                );
                tx.execute(
                    "INSERT INTO usage_daily_rollups (
                        date, app_type, provider_id, model, request_count, success_count,
                        input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens,
                        total_cost_usd, avg_latency_ms
                    ) VALUES (?1, 'claude', 'p1', 'claude-3', 7, 7, 700, 350, 0, 0, '0.07', 120)",
                    [date],
                )?;
            }
            tx.commit()?;
            Ok(())
        }

        let source = Database::memory()?;
        populate(&source, LOG_ROWS, STREAM_ROWS, ROLLUP_ROWS)?;

        let t = Instant::now();
        let full_sql = source.export_sql_string()?;
        println!(
            "export_sql_string (full): {:?}, {} bytes",
            t.elapsed(),
            full_sql.len()
        );

        let t = Instant::now();
        let import_target = Database::memory()?;
        import_target.import_sql_string(&full_sql)?;
        println!("import_sql_string (local file path): {:?}", t.elapsed());
        {
            let conn = crate::database::lock_conn!(import_target.conn);
            let counts: (i64, i64, i64, i64) = conn.query_row(
                "SELECT
                    (SELECT COUNT(*) FROM providers),
                    (SELECT COUNT(*) FROM proxy_request_logs),
                    (SELECT COUNT(*) FROM stream_check_logs),
                    (SELECT COUNT(*) FROM usage_daily_rollups)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )?;
            assert_eq!(
                counts,
                (50, LOG_ROWS as i64, STREAM_ROWS as i64, ROLLUP_ROWS as i64)
            );
        }

        Ok(())
    }

    /// 分阶段拆解 import_sql_string 的耗时，定位慢在哪一步。
    ///
    /// 手动运行：`cargo test --lib perf_import_phases -- --ignored --nocapture`
    #[test]
    #[ignore = "perf diagnostic, run explicitly"]
    fn perf_import_phases() -> Result<(), AppError> {
        use rusqlite::Connection;
        use std::time::Instant;
        use tempfile::NamedTempFile;

        const LOG_ROWS: usize = 20_000;

        let source = Database::memory()?;
        {
            let mut conn = crate::database::lock_conn!(source.conn);
            let tx = conn.transaction()?;
            for i in 0..50 {
                tx.execute(
                    "INSERT INTO providers (id, app_type, name, settings_config, meta)
                     VALUES (?1, 'claude', ?2, '{}', '{}')",
                    rusqlite::params![format!("p{i}"), format!("Provider {i}")],
                )?;
            }
            for i in 0..LOG_ROWS {
                tx.execute(
                    "INSERT INTO proxy_request_logs (
                        request_id, provider_id, app_type, model,
                        input_tokens, output_tokens, total_cost_usd,
                        latency_ms, status_code, created_at
                    ) VALUES (?1, 'p1', 'claude', 'claude-3', 100, 50, '0.01', 120, 200, 1000)",
                    [format!("req-{i}")],
                )?;
            }
            tx.commit()?;
        }
        let sql = source.export_sql_string()?;
        println!("payload: {} bytes, {LOG_ROWS} log rows", sql.len());

        let temp_file = NamedTempFile::new().expect("temp file");
        let temp_conn = Connection::open(temp_file.path()).expect("open temp conn");

        let t = Instant::now();
        temp_conn
            .execute_batch(&sql)
            .expect("execute_batch should succeed");
        println!("phase execute_batch: {:?}", t.elapsed());

        let t = Instant::now();
        Database::create_tables_on_conn(&temp_conn)?;
        Database::apply_schema_migrations_on_conn(&temp_conn)?;
        println!("phase schema+migrations: {:?}", t.elapsed());

        let t = Instant::now();
        let target = Database::memory()?;
        {
            let mut main_conn = crate::database::lock_conn!(target.conn);
            let backup =
                rusqlite::backup::Backup::new(&temp_conn, &mut main_conn).expect("backup init");
            backup.step(-1).expect("backup step");
        }
        println!("phase backup-to-main: {:?}", t.elapsed());

        // 对照组：同样的语句但临时库关掉 journal / synchronous。
        let temp_file2 = NamedTempFile::new().expect("temp file 2");
        let temp_conn2 = Connection::open(temp_file2.path()).expect("open temp conn 2");
        temp_conn2
            .execute_batch("PRAGMA journal_mode=MEMORY; PRAGMA synchronous=OFF;")
            .expect("pragmas");
        let t = Instant::now();
        temp_conn2
            .execute_batch(&sql)
            .expect("execute_batch should succeed");
        println!(
            "phase execute_batch (journal=MEMORY, sync=OFF): {:?}",
            t.elapsed()
        );

        // 对照组 B：同一份脚本跑在内存库上，区分“纯 CPU/解析”还是“文件 I/O”。
        let mem_conn = Connection::open_in_memory().expect("open mem conn");
        let t = Instant::now();
        mem_conn
            .execute_batch(&sql)
            .expect("execute_batch mem should succeed");
        println!("phase execute_batch (in-memory): {:?}", t.elapsed());

        // 对照组 C：同样的数据改成多行 VALUES（每 200 行一条 INSERT），
        // 验证“每行一条语句”的解析开销占比。
        let mut batched = String::from("PRAGMA foreign_keys=OFF;\nBEGIN TRANSACTION;\n");
        batched.push_str(
            "CREATE TABLE bench_logs (
                request_id TEXT, provider_id TEXT, app_type TEXT, model TEXT,
                input_tokens INTEGER, output_tokens INTEGER, total_cost_usd TEXT,
                latency_ms INTEGER, status_code INTEGER, created_at INTEGER
            );\n",
        );
        const BATCH: usize = 200;
        for chunk_start in (0..LOG_ROWS).step_by(BATCH) {
            batched.push_str("INSERT INTO bench_logs VALUES ");
            for i in chunk_start..(chunk_start + BATCH).min(LOG_ROWS) {
                if i > chunk_start {
                    batched.push(',');
                }
                batched.push_str(&format!(
                    "('req-{i}','p1','claude','claude-3',100,50,'0.01',120,200,1000)"
                ));
            }
            batched.push_str(";\n");
        }
        batched.push_str("COMMIT;\n");
        let mem_conn2 = Connection::open_in_memory().expect("open mem conn 2");
        let t = Instant::now();
        mem_conn2
            .execute_batch(&batched)
            .expect("batched should succeed");
        println!(
            "phase execute_batch (in-memory, multi-row VALUES x{BATCH}): {:?}",
            t.elapsed()
        );

        Ok(())
    }
}
