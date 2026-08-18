//! Skills 服务层
//!
//! v3.10.0+ 统一管理架构：
//! - SSOT（单一事实源）：`~/.cc-gateway/skills/`
//! - 本机导入到 SSOT，按需同步到各应用目录
//! - 数据库存储安装记录和启用状态

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::app_config::{AppType, InstalledSkill, SkillApps, UnmanagedSkill};
use crate::config::get_app_config_dir;
use crate::database::Database;
use crate::error::format_skill_error;

// ========== 数据结构 ==========

/// Skill 同步方式
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SyncMethod {
    /// 自动选择：优先 symlink，失败时回退到 copy
    #[default]
    Auto,
    /// 符号链接（推荐，节省磁盘空间）
    Symlink,
    /// 文件复制（兼容模式）
    Copy,
}

/// Skill 存储位置（SSOT 目录选择）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SkillStorageLocation {
    /// CC Gateway 管理目录 (~/.cc-gateway/skills/)
    #[default]
    CcSwitch,
    /// Agent Skills 统一标准目录 (~/.agents/skills/)
    Unified,
}


/// 仓库配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillRepo {
    /// GitHub 用户/组织名
    pub owner: String,
    /// 仓库名称
    pub name: String,
    /// 分支 (默认 "main")
    pub branch: String,
    /// 是否启用
    pub enabled: bool,
}

/// 技能安装状态（旧版兼容）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillState {
    /// 是否已安装
    pub installed: bool,
    /// 安装时间
    #[serde(rename = "installedAt")]
    pub installed_at: DateTime<Utc>,
}

/// 持久化存储结构（仓库配置）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillStore {
    /// directory -> 安装状态（旧版兼容，新版不使用）
    pub skills: HashMap<String, SkillState>,
    /// 仓库列表
    pub repos: Vec<SkillRepo>,
}

impl Default for SkillStore {
    fn default() -> Self {
        SkillStore {
            skills: HashMap::new(),
            repos: vec![
                SkillRepo {
                    owner: "anthropics".to_string(),
                    name: "skills".to_string(),
                    branch: "main".to_string(),
                    enabled: true,
                },
                SkillRepo {
                    owner: "ComposioHQ".to_string(),
                    name: "awesome-claude-skills".to_string(),
                    branch: "master".to_string(),
                    enabled: true,
                },
                SkillRepo {
                    owner: "cexll".to_string(),
                    name: "myclaude".to_string(),
                    branch: "master".to_string(),
                    enabled: true,
                },
                SkillRepo {
                    owner: "JimLiu".to_string(),
                    name: "baoyu-skills".to_string(),
                    branch: "main".to_string(),
                    enabled: true,
                },
            ],
        }
    }
}

/// Skill 卸载结果
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillUninstallResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backup_path: Option<String>,
}


/// Skill 存储位置迁移结果
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrationResult {
    pub migrated_count: usize,
    pub skipped_count: usize,
    pub errors: Vec<String>,
}


#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillBackupEntry {
    pub backup_id: String,
    pub backup_path: String,
    pub created_at: i64,
    pub skill: InstalledSkill,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SkillBackupMetadata {
    skill: InstalledSkill,
    backup_created_at: i64,
    source_path: String,
}

const SKILL_BACKUP_RETAIN_COUNT: usize = 20;


/// 技能元数据 (从 SKILL.md 解析)
#[derive(Debug, Clone, Deserialize)]
pub struct SkillMetadata {
    pub name: Option<String>,
    pub description: Option<String>,
}

/// 导入已有 Skill 时，前端显式提交的启用应用选择
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportSkillSelection {
    pub directory: String,
    #[serde(default)]
    pub apps: SkillApps,
}

#[derive(Debug, Clone, Deserialize)]
struct LegacySkillMigrationRow {
    directory: String,
    app_type: String,
}

// ========== ~/.agents/ lock 文件解析 ==========

/// `~/.agents/.skill-lock.json` 文件结构
#[derive(Deserialize)]
struct AgentsLockFile {
    skills: HashMap<String, AgentsLockSkill>,
}

/// lock 文件中单个 skill 的信息
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AgentsLockSkill {
    source: Option<String>,
    source_type: Option<String>,
    source_url: Option<String>,
    skill_path: Option<String>,
    branch: Option<String>,
    source_branch: Option<String>,
}

#[derive(Debug, Clone)]
struct LockRepoInfo {
    owner: String,
    repo: String,
    skill_path: Option<String>,
    branch: Option<String>,
}

fn normalize_optional_branch(branch: Option<String>) -> Option<String> {
    branch.and_then(|b| {
        let trimmed = b.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn parse_branch_from_source_url(source_url: Option<&str>) -> Option<String> {
    let source_url = source_url?;
    let source_url = source_url.trim();
    if source_url.is_empty() {
        return None;
    }

    // 支持 https://github.com/owner/repo/tree/<branch>/...
    if let Some((_, after_tree)) = source_url.split_once("/tree/") {
        let branch = after_tree
            .split('/')
            .next()
            .map(str::trim)
            .filter(|s| !s.is_empty())?;
        return Some(branch.to_string());
    }

    // 支持 URL fragment: ...git#branch
    if let Some((_, fragment)) = source_url.split_once('#') {
        let branch = fragment
            .split('&')
            .next()
            .map(str::trim)
            .filter(|s| !s.is_empty())?;
        return Some(branch.to_string());
    }

    // 支持 query: ...?branch=xxx / ?ref=xxx
    if let Some((_, query)) = source_url.split_once('?') {
        for pair in query.split('&') {
            let Some((key, value)) = pair.split_once('=') else {
                continue;
            };
            if matches!(key, "branch" | "ref") {
                let branch = value.trim();
                if !branch.is_empty() {
                    return Some(branch.to_string());
                }
            }
        }
    }

    None
}

/// 获取 `~/.agents/skills/` 目录（存在时返回）
fn get_agents_skills_dir() -> Option<PathBuf> {
    let dir = crate::config::get_home_dir().join(".agents").join("skills");
    dir.exists().then_some(dir)
}

/// 解析 `~/.agents/.skill-lock.json`，返回 skill_name -> 仓库信息
fn parse_agents_lock() -> HashMap<String, LockRepoInfo> {
    let path = crate::config::get_home_dir()
        .join(".agents")
        .join(".skill-lock.json");
    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => {
            if e.kind() == std::io::ErrorKind::NotFound {
                log::debug!("未找到 agents lock 文件: {}", path.display());
            } else {
                log::warn!("读取 agents lock 文件失败 ({}): {}", path.display(), e);
            }
            return HashMap::new();
        }
    };
    let lock: AgentsLockFile = match serde_json::from_str(&content) {
        Ok(l) => l,
        Err(e) => {
            log::warn!("解析 agents lock 文件失败 ({}): {}", path.display(), e);
            return HashMap::new();
        }
    };
    let parsed: HashMap<String, LockRepoInfo> = lock
        .skills
        .into_iter()
        .filter_map(|(name, skill)| {
            let source = skill.source?;
            if skill.source_type.as_deref() != Some("github") {
                return None;
            }
            let (owner, repo) = source.split_once('/')?;
            let branch = normalize_optional_branch(skill.branch)
                .or_else(|| normalize_optional_branch(skill.source_branch))
                .or_else(|| parse_branch_from_source_url(skill.source_url.as_deref()));
            Some((
                name,
                LockRepoInfo {
                    owner: owner.to_string(),
                    repo: repo.to_string(),
                    skill_path: skill.skill_path,
                    branch,
                },
            ))
        })
        .collect();
    log::info!(
        "agents lock 文件解析完成，共识别 {} 个 github skill",
        parsed.len()
    );
    parsed
}

// ========== SkillService ==========

pub struct SkillService;

impl Default for SkillService {
    fn default() -> Self {
        Self::new()
    }
}

impl SkillService {
    pub fn new() -> Self {
        Self
    }

    /// 构建 Skill 文档 URL（指向仓库中的 SKILL.md 文件）
    ///
    /// 坐标不合法时返回 None：这个值会存进 `readme_url`，前端「查看文档」用
    /// `openExternal` 直接打开，恶意 branch 能把它指到 github.com 上的任意路径。
    fn build_skill_doc_url(
        owner: &str,
        repo: &str,
        branch: &str,
        doc_path: &str,
    ) -> Option<String> {
        if Self::validate_repo_ref(owner, repo, branch).is_err() {
            log::warn!("跳过非法仓库坐标的文档链接: {owner}/{repo}@{branch}");
            return None;
        }
        Some(format!(
            "https://github.com/{owner}/{repo}/blob/{branch}/{doc_path}"
        ))
    }

    /// 从旧 readme_url 中提取仓库内文档路径，兼容 `blob`/`tree` 两种格式
    fn extract_doc_path_from_url(url: &str) -> Option<String> {
        let marker = if url.contains("/blob/") {
            "/blob/"
        } else if url.contains("/tree/") {
            "/tree/"
        } else {
            return None;
        };

        let (_, tail) = url.split_once(marker)?;
        let (_, path) = tail.split_once('/')?;
        if path.is_empty() {
            return None;
        }
        Some(path.to_string())
    }

    // ========== 路径管理 ==========

    /// 获取 SSOT 目录（根据设置返回 ~/.cc-gateway/skills/ 或 ~/.agents/skills/）
    pub fn get_ssot_dir() -> Result<PathBuf> {
        let location = crate::settings::get_skill_storage_location();
        let dir = match location {
            SkillStorageLocation::CcSwitch => get_app_config_dir().join("skills"),
            SkillStorageLocation::Unified => {
                crate::config::get_home_dir().join(".agents").join("skills")
            }
        };
        fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    /// 获取 Skill 卸载备份目录（~/.cc-gateway/skill-backups/）
    fn get_backup_dir() -> Result<PathBuf> {
        let dir = get_app_config_dir().join("skill-backups");
        fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    /// 获取应用的 skills 目录
    pub fn get_app_skills_dir(app: &AppType) -> Result<PathBuf> {
        app.ensure_supported()?;
        // 目录覆盖：优先使用用户在 settings.json 中配置的 override 目录
        match app {
            AppType::Claude => {
                if let Some(custom) = crate::settings::get_claude_override_dir() {
                    return Ok(custom.join("skills"));
                }
            }
            AppType::ClaudeDesktop => {}
            AppType::Codex => {
                if let Some(custom) = crate::settings::get_codex_override_dir() {
                    return Ok(custom.join("skills"));
                }
            }
            AppType::GrokBuild => {
                if let Some(custom) = crate::settings::get_grok_override_dir() {
                    return Ok(custom.join("skills"));
                }
            }
            AppType::Gemini | AppType::OpenCode | AppType::OpenClaw | AppType::Hermes => {
                unreachable!("retired app rejected above")
            }
        }

        // 默认路径：回退到用户主目录下的标准位置。
        // 必须走 get_home_dir()（可被 CC_GATEWAY_TEST_HOME 覆盖）：Windows 上 dirs::home_dir()
        // 走 Known Folder API，测试无法隔离真实用户目录。
        let home = crate::config::get_home_dir();

        Ok(match app {
            AppType::Claude => home.join(".claude").join("skills"),
            AppType::ClaudeDesktop => home.join(".claude-desktop").join("skills"),
            AppType::Codex => home.join(".codex").join("skills"),
            AppType::GrokBuild => home.join(".grok").join("skills"),
            AppType::Gemini | AppType::OpenCode | AppType::OpenClaw | AppType::Hermes => {
                unreachable!("retired app rejected above")
            }
        })
    }

    // ========== 统一管理方法 ==========

    /// 获取所有已安装的 Skills
    pub fn get_all_installed(db: &Arc<Database>) -> Result<Vec<InstalledSkill>> {
        let skills = db.get_all_installed_skills()?;
        Ok(skills.into_values().collect())
    }


    /// 卸载 Skill
    ///
    /// 流程：
    /// 1. 从所有应用目录删除
    /// 2. 从 SSOT 删除
    /// 3. 从数据库删除
    pub fn uninstall(db: &Arc<Database>, id: &str) -> Result<SkillUninstallResult> {
        // 获取 skill 信息
        let skill = db
            .get_installed_skill(id)?
            .ok_or_else(|| anyhow!("Skill not found: {id}"))?;

        // DB 行可能被同步导入污染（远端快照 raw SQL 直接灌库，绕过安装期校验），
        // 也可能是 v3.11.0 引入 sanitize_install_name 之前留下的存量脏值
        // （当年扫描不过滤点开头目录，`.github/SKILL.md` 会存成 `.github`）。
        //
        // 守卫失败时**跳过全部文件系统操作、但仍删除 DB 行**：`db.delete_skill`
        // 全项目只有这一处调用且未暴露为命令，若在此直接返回 Err，用户就再也无法
        // 从界面删掉这条记录，只能手改 SQLite。安全目标是「不碰危险路径」，
        // 不是「把用户锁在坏状态里」。
        let backup_path = match Self::require_valid_directory(&skill.directory) {
            Ok(directory) => {
                let backup_path = Self::create_uninstall_backup(&skill)?
                    .map(|path| path.to_string_lossy().to_string());

                // 从所有应用目录删除
                for app in AppType::all() {
                    let _ = Self::remove_from_app(&directory, &app);
                }

                // 从 SSOT 删除
                let ssot_dir = Self::get_ssot_dir()?;
                let skill_path = ssot_dir.join(&directory);
                if skill_path.exists() {
                    fs::remove_dir_all(&skill_path)?;
                }
                backup_path
            }
            Err(err) => {
                log::warn!(
                    "Skill {id} 的 directory 非法（{:?}），跳过文件清理，仅删除数据库记录: {err}",
                    skill.directory
                );
                None
            }
        };

        // 从数据库删除
        db.delete_skill(id)?;

        log::info!(
            "Skill {} 卸载成功{}",
            skill.name,
            backup_path
                .as_deref()
                .map(|path| format!(", backup: {path}"))
                .unwrap_or_default()
        );

        Ok(SkillUninstallResult { backup_path })
    }

    // ========== 更新检测 ==========

    /// 计算目录内容的 SHA-256 哈希
    ///
    /// 递归遍历目录下所有非隐藏文件，按相对路径字典序排列，
    /// 将 "相对路径\0内容\0" 逐文件 feed 给同一个 hasher。
    pub fn compute_dir_hash(dir: &Path) -> Result<String> {
        use sha2::{Digest, Sha256};

        let mut files: Vec<PathBuf> = Vec::new();
        Self::collect_files_for_hash(dir, dir, &mut files)?;
        files.sort();

        let mut hasher = Sha256::new();
        for file_path in &files {
            let relative = file_path.strip_prefix(dir).unwrap_or(file_path);
            let rel_str = relative.to_string_lossy().replace('\\', "/");
            hasher.update(rel_str.as_bytes());
            hasher.update(b"\0");
            let content = fs::read(file_path)
                .with_context(|| format!("读取文件失败: {}", file_path.display()))?;
            hasher.update(&content);
            hasher.update(b"\0");
        }

        Ok(format!("{:x}", hasher.finalize()))
    }

    /// 递归收集目录下所有非隐藏文件
    #[allow(clippy::only_used_in_recursion)]
    fn collect_files_for_hash(base: &Path, current: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
        let entries = fs::read_dir(current)
            .with_context(|| format!("读取目录失败: {}", current.display()))?;
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            let path = entry.path();
            if path.is_dir() {
                Self::collect_files_for_hash(base, &path, files)?;
            } else {
                files.push(path);
            }
        }
        Ok(())
    }


    /// 迁移 Skill 存储位置（在两个 SSOT 目录间移动文件）
    ///
    /// 安全策略：先移文件，后改设置。中途崩溃时设置仍指向旧目录。
    pub fn migrate_storage(
        db: &Arc<Database>,
        target: SkillStorageLocation,
    ) -> Result<MigrationResult> {
        let current = crate::settings::get_skill_storage_location();
        if current == target {
            return Ok(MigrationResult {
                migrated_count: 0,
                skipped_count: 0,
                errors: vec![],
            });
        }

        // 1. 解析旧目录和新目录（不改设置）
        let old_dir = Self::get_ssot_dir()?;
        let new_dir = match target {
            SkillStorageLocation::CcSwitch => get_app_config_dir().join("skills"),
            SkillStorageLocation::Unified => {
                crate::config::get_home_dir().join(".agents").join("skills")
            }
        };
        fs::create_dir_all(&new_dir)?;

        // 2. 逐个移动 skill 目录
        let skills = db.get_all_installed_skills()?;
        let mut result = MigrationResult {
            migrated_count: 0,
            skipped_count: 0,
            errors: vec![],
        };

        for skill in skills.values() {
            // 下面是 rename 与 remove_dir_all，脏 directory 可把任意目录搬走或删掉。
            // 软失败：本函数已有 errors 收集通道，记一条继续处理其余 skill，
            // 不要整体中断——用户只是在切换存储位置。
            let directory = match Self::require_valid_directory(&skill.directory) {
                Ok(directory) => directory,
                Err(err) => {
                    result
                        .errors
                        .push(format!("{}: {err}", skill.directory.escape_debug()));
                    continue;
                }
            };
            let src = old_dir.join(&directory);
            let dst = new_dir.join(&directory);

            if !src.exists() {
                result.skipped_count += 1;
                continue;
            }
            if dst.exists() {
                result.skipped_count += 1;
                continue;
            }

            // 优先 rename（同文件系统原子操作），失败则 copy+delete
            match fs::rename(&src, &dst) {
                Ok(()) => result.migrated_count += 1,
                Err(_) => match Self::copy_dir_recursive(&src, &dst) {
                    Ok(()) => {
                        let _ = fs::remove_dir_all(&src);
                        result.migrated_count += 1;
                    }
                    Err(e) => {
                        result.errors.push(format!("{}: {e}", skill.directory));
                    }
                },
            }
        }

        // 3. 文件移动完成后才持久化设置
        crate::settings::set_skill_storage_location(target)?;

        // 4. 刷新所有应用目录的 symlink（指向新 SSOT）
        for app in AppType::all() {
            let _ = Self::sync_to_app(db, &app);
        }

        log::info!(
            "Skill 存储迁移完成: {} 迁移, {} 跳过, {} 错误",
            result.migrated_count,
            result.skipped_count,
            result.errors.len()
        );

        Ok(result)
    }

    pub fn list_backups() -> Result<Vec<SkillBackupEntry>> {
        let backup_dir = Self::get_backup_dir()?;
        let mut entries = Vec::new();

        for entry in fs::read_dir(&backup_dir)? {
            let entry = match entry {
                Ok(entry) => entry,
                Err(err) => {
                    log::warn!("读取 Skill 备份目录项失败: {err}");
                    continue;
                }
            };
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }

            match Self::read_backup_metadata(&path) {
                Ok(metadata) => entries.push(SkillBackupEntry {
                    backup_id: entry.file_name().to_string_lossy().to_string(),
                    backup_path: path.to_string_lossy().to_string(),
                    created_at: metadata.backup_created_at,
                    skill: metadata.skill,
                }),
                Err(err) => {
                    log::warn!("解析 Skill 备份失败 {}: {err:#}", path.display());
                }
            }
        }

        entries.sort_by_key(|entry| std::cmp::Reverse(entry.created_at));
        Ok(entries)
    }

    pub fn delete_backup(backup_id: &str) -> Result<()> {
        let backup_path = Self::backup_path_for_id(backup_id)?;
        let metadata = fs::symlink_metadata(&backup_path)
            .with_context(|| format!("failed to access {}", backup_path.display()))?;

        if !metadata.is_dir() {
            return Err(anyhow!(
                "Skill backup is not a directory: {}",
                backup_path.display()
            ));
        }

        fs::remove_dir_all(&backup_path)
            .with_context(|| format!("failed to delete {}", backup_path.display()))?;

        log::info!("Skill 备份已删除: {}", backup_path.display());
        Ok(())
    }

    pub fn restore_from_backup(
        db: &Arc<Database>,
        backup_id: &str,
        current_app: &AppType,
    ) -> Result<InstalledSkill> {
        let backup_path = Self::backup_path_for_id(backup_id)?;
        let metadata = Self::read_backup_metadata(&backup_path)?;
        let backup_skill_dir = backup_path.join("skill");
        if !backup_skill_dir.join("SKILL.md").exists() {
            return Err(anyhow!(
                "Skill backup is invalid or missing SKILL.md: {}",
                backup_path.display()
            ));
        }

        let existing_skills = db.get_all_installed_skills()?;
        if existing_skills.contains_key(&metadata.skill.id)
            || existing_skills.values().any(|skill| {
                skill
                    .directory
                    .eq_ignore_ascii_case(&metadata.skill.directory)
            })
        {
            return Err(anyhow!(
                "Skill already exists, please uninstall the current one first: {}",
                metadata.skill.directory
            ));
        }

        // meta.json 是文件内容（可能来自手工放置或不可信备份），directory 此前
        // 未经任何校验就直接 join——可穿越出 SSOT 目录写任意位置。必须先校验。
        let directory = Self::require_valid_directory(&metadata.skill.directory)?;

        let ssot_dir = Self::get_ssot_dir()?;
        let restore_path = ssot_dir.join(&directory);
        if restore_path.exists() || Self::is_symlink(&restore_path) {
            return Err(anyhow!(
                "Restore target already exists: {}",
                restore_path.display()
            ));
        }

        let mut restored_skill = metadata.skill;
        restored_skill.directory = directory;
        restored_skill.installed_at = Utc::now().timestamp();
        restored_skill.apps = SkillApps::only(current_app);
        restored_skill.updated_at = 0;

        Self::copy_dir_recursive(&backup_skill_dir, &restore_path)?;

        // 重新计算内容哈希
        restored_skill.content_hash = Self::compute_dir_hash(&restore_path).ok();

        if let Err(err) = db.save_skill(&restored_skill) {
            let _ = fs::remove_dir_all(&restore_path);
            return Err(err.into());
        }

        if !restored_skill.apps.is_empty() {
            if let Err(err) = Self::sync_to_app_dir(&restored_skill.directory, current_app) {
                let _ = db.delete_skill(&restored_skill.id);
                let _ = fs::remove_dir_all(&restore_path);
                return Err(err);
            }
        }

        log::info!(
            "Skill {} 已从备份恢复到 {}",
            restored_skill.name,
            restore_path.display()
        );

        Ok(restored_skill)
    }

    /// 切换应用启用状态
    ///
    /// 启用：复制到应用目录
    /// 禁用：从应用目录删除
    pub fn toggle_app(db: &Arc<Database>, id: &str, app: &AppType, enabled: bool) -> Result<()> {
        // 获取当前 skill
        let mut skill = db
            .get_installed_skill(id)?
            .ok_or_else(|| anyhow!("Skill not found: {id}"))?;

        // 更新状态
        skill.apps.set_enabled_for(app, enabled);

        // 同步文件
        if enabled {
            Self::sync_to_app_dir(&skill.directory, app)?;
        } else {
            Self::remove_from_app(&skill.directory, app)?;
        }

        // 更新数据库
        db.update_skill_apps(id, &skill.apps)?;

        log::info!("Skill {} 的 {:?} 状态已更新为 {}", skill.name, app, enabled);

        Ok(())
    }

    /// 扫描未管理的 Skills
    ///
    /// 扫描各应用目录，找出未被 CC Gateway 管理的 Skills
    pub fn scan_unmanaged(db: &Arc<Database>) -> Result<Vec<UnmanagedSkill>> {
        let managed_skills = db.get_all_installed_skills()?;
        let managed_dirs: HashSet<String> = managed_skills
            .values()
            .map(|s| s.directory.clone())
            .collect();

        // 收集所有待扫描的目录及其来源标签
        let mut scan_sources: Vec<(PathBuf, String)> = Vec::new();
        for app in AppType::all() {
            if let Ok(d) = Self::get_app_skills_dir(&app) {
                scan_sources.push((d, app.as_str().to_string()));
            }
        }
        if let Some(agents_dir) = get_agents_skills_dir() {
            scan_sources.push((agents_dir, "agents".to_string()));
        }
        if let Ok(ssot_dir) = Self::get_ssot_dir() {
            scan_sources.push((ssot_dir, "cc-gateway".to_string()));
        }

        let mut unmanaged: HashMap<String, UnmanagedSkill> = HashMap::new();

        for (scan_dir, label) in &scan_sources {
            let entries = match fs::read_dir(scan_dir) {
                Ok(e) => e,
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                let dir_name = entry.file_name().to_string_lossy().to_string();
                if dir_name.starts_with('.') || managed_dirs.contains(&dir_name) {
                    continue;
                }

                let skill_md = path.join("SKILL.md");
                if !skill_md.exists() {
                    continue;
                }
                let (name, description) = Self::read_skill_name_desc(&skill_md, &dir_name);

                unmanaged
                    .entry(dir_name.clone())
                    .and_modify(|s| s.found_in.push(label.clone()))
                    .or_insert(UnmanagedSkill {
                        directory: dir_name,
                        name,
                        description,
                        found_in: vec![label.clone()],
                        path: path.display().to_string(),
                    });
            }
        }

        Ok(unmanaged.into_values().collect())
    }

    /// 从应用目录导入 Skills
    ///
    /// 将未管理的 Skills 导入到 CC Gateway 统一管理
    pub fn import_from_apps(
        db: &Arc<Database>,
        imports: Vec<ImportSkillSelection>,
    ) -> Result<Vec<InstalledSkill>> {
        let ssot_dir = Self::get_ssot_dir()?;
        let agents_lock = parse_agents_lock();
        let mut imported = Vec::new();

        // 将 lock 文件中发现的仓库保存到 skill_repos
        save_repos_from_lock(
            db,
            &agents_lock,
            imports.iter().map(|selection| selection.directory.as_str()),
        );

        // 收集所有候选搜索目录
        let mut search_sources: Vec<(PathBuf, String)> = Vec::new();
        for app in AppType::all() {
            if let Ok(d) = Self::get_app_skills_dir(&app) {
                search_sources.push((d, app.as_str().to_string()));
            }
        }
        if let Some(agents_dir) = get_agents_skills_dir() {
            search_sources.push((agents_dir, "agents".to_string()));
        }
        search_sources.push((ssot_dir.clone(), "cc-gateway".to_string()));

        for selection in imports {
            // selection.directory 由前端 IPC 直接传入、此前全程无校验，而它既被
            // 用来探测源目录、又作为 copy_dir_recursive 的目标、最后还原样入库。
            // 在入口处拒掉，同时切断「脏值 sink」和「脏值来源」两条线。
            let dir_name = match Self::require_valid_directory(&selection.directory) {
                Ok(dir_name) => dir_name,
                Err(err) => {
                    log::warn!("跳过导入：{err}");
                    continue;
                }
            };
            // 在所有候选目录中查找
            let mut source_path: Option<PathBuf> = None;

            for (base, label) in &search_sources {
                let skill_path = base.join(&dir_name);
                if skill_path.exists() {
                    if source_path.is_none() {
                        source_path = Some(skill_path);
                    }
                    log::debug!("Skill '{dir_name}' found in source '{label}'");
                }
            }

            let source = match source_path {
                Some(p) => p,
                None => continue,
            };
            if !source.join("SKILL.md").exists() {
                log::warn!(
                    "Skip importing '{}' because source '{}' has no SKILL.md",
                    dir_name,
                    source.display()
                );
                continue;
            }

            // 复制到 SSOT
            let dest = ssot_dir.join(&dir_name);
            if !dest.exists() {
                Self::copy_dir_recursive(&source, &dest)?;
            }

            // 解析元数据
            let skill_md = dest.join("SKILL.md");
            let (name, description) = Self::read_skill_name_desc(&skill_md, &dir_name);

            // 启用状态仅信任用户本次显式选择，不再根据“在哪些位置找到”自动推断。
            let apps = selection.apps;

            // 从 lock 文件提取仓库信息
            let (id, repo_owner, repo_name, repo_branch, readme_url) =
                build_repo_info_from_lock(&agents_lock, &dir_name);

            // 计算内容哈希
            let ssot_skill_dir = ssot_dir.join(&dir_name);
            let content_hash = Self::compute_dir_hash(&ssot_skill_dir).ok();

            // 创建记录
            let skill = InstalledSkill {
                id,
                name,
                description,
                directory: dir_name,
                repo_owner,
                repo_name,
                repo_branch,
                readme_url,
                apps,
                installed_at: chrono::Utc::now().timestamp(),
                content_hash,
                updated_at: 0,
            };

            // 保存到数据库
            db.save_skill(&skill)?;

            imported.push(skill);
        }

        log::info!("成功导入 {} 个 Skills", imported.len());

        Ok(imported)
    }

    // ========== 文件同步方法 ==========

    /// 创建符号链接（跨平台）
    ///
    /// - Unix: 使用 std::os::unix::fs::symlink
    /// - Windows: 使用 std::os::windows::fs::symlink_dir
    #[cfg(unix)]
    fn create_symlink(src: &Path, dest: &Path) -> Result<()> {
        std::os::unix::fs::symlink(src, dest)
            .with_context(|| format!("创建符号链接失败: {} -> {}", src.display(), dest.display()))
    }

    #[cfg(windows)]
    fn create_symlink(src: &Path, dest: &Path) -> Result<()> {
        std::os::windows::fs::symlink_dir(src, dest)
            .with_context(|| format!("创建符号链接失败: {} -> {}", src.display(), dest.display()))
    }

    /// 检查路径是否为符号链接
    fn is_symlink(path: &Path) -> bool {
        path.symlink_metadata()
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
    }

    /// 获取当前同步方式配置
    fn get_sync_method() -> SyncMethod {
        crate::settings::get_skill_sync_method()
    }

    /// 同步 Skill 到应用目录（使用 symlink 或 copy）
    ///
    /// 根据配置和平台选择最佳同步方式：
    /// - Auto: 优先尝试 symlink，失败时回退到 copy
    /// - Symlink: 仅使用 symlink
    /// - Copy: 仅使用文件复制
    pub fn sync_to_app_dir(directory: &str, app: &AppType) -> Result<()> {
        if matches!(app, AppType::ClaudeDesktop) {
            return Ok(());
        }

        // directory 可能来自被污染的 DB 行（如同步导入的远端快照），join 前必须校验。
        let directory = Self::require_valid_directory(directory)?;

        let ssot_dir = Self::get_ssot_dir()?;
        let source = ssot_dir.join(&directory);

        Self::validate_sync_source_dir(&source, &directory)?;

        let app_dir = Self::get_app_skills_dir(app)?;
        fs::create_dir_all(&app_dir)?;

        let dest = app_dir.join(&directory);

        let sync_method = Self::get_sync_method();

        match sync_method {
            SyncMethod::Auto => {
                if dest.exists() && !Self::is_symlink(&dest) {
                    Self::replace_dest_with_copy(&source, &dest, &directory)?;
                    log::debug!("Skill {directory} 已通过复制同步到 {app:?}");
                    return Ok(());
                }

                if Self::is_symlink(&dest) {
                    Self::remove_path(&dest)?;
                }

                // 优先尝试 symlink
                match Self::create_symlink(&source, &dest) {
                    Ok(()) => {
                        log::debug!("Skill {directory} 已通过 symlink 同步到 {app:?}");
                        return Ok(());
                    }
                    Err(err) => {
                        log::warn!(
                            "Symlink 创建失败，将回退到文件复制: {} -> {}. 错误: {err:#}",
                            source.display(),
                            dest.display()
                        );
                    }
                }
                // Fallback 到 copy
                Self::replace_dest_with_copy(&source, &dest, &directory)?;
                log::debug!("Skill {directory} 已通过复制同步到 {app:?}");
            }
            SyncMethod::Symlink => {
                if dest.exists() || Self::is_symlink(&dest) {
                    Self::remove_path(&dest)?;
                }
                Self::create_symlink(&source, &dest)?;
                log::debug!("Skill {directory} 已通过 symlink 同步到 {app:?}");
            }
            SyncMethod::Copy => {
                Self::replace_dest_with_copy(&source, &dest, &directory)?;
                log::debug!("Skill {directory} 已通过复制同步到 {app:?}");
            }
        }

        Ok(())
    }

    /// 复制 Skill 到应用目录（保留用于向后兼容）
    #[deprecated(note = "请使用 sync_to_app_dir() 代替")]
    pub fn copy_to_app(directory: &str, app: &AppType) -> Result<()> {
        Self::sync_to_app_dir(directory, app)
    }

    /// 删除路径（支持 symlink 和真实目录）
    fn remove_path(path: &Path) -> Result<()> {
        if Self::is_symlink(path) {
            // 符号链接：仅删除链接本身，不影响源文件
            #[cfg(unix)]
            fs::remove_file(path)?;
            #[cfg(windows)]
            fs::remove_dir(path)?; // Windows 的目录 symlink 需要用 remove_dir
        } else if path.is_dir() {
            // 真实目录：递归删除
            fs::remove_dir_all(path)?;
        } else if path.exists() {
            // 普通文件
            fs::remove_file(path)?;
        }
        Ok(())
    }

    fn validate_sync_source_dir(source: &Path, directory: &str) -> Result<()> {
        if !source.is_dir() {
            return Err(anyhow!("Skill 不存在于 SSOT: {directory}"));
        }

        let manifest = source.join("SKILL.md");
        if !manifest.is_file() {
            return Err(anyhow!(
                "Skill 源目录缺少 SKILL.md，拒绝同步以避免覆盖目标目录: {}",
                source.display()
            ));
        }

        Ok(())
    }

    fn replace_dest_with_copy(source: &Path, dest: &Path, directory: &str) -> Result<()> {
        Self::validate_sync_source_dir(source, directory)?;

        let parent = dest
            .parent()
            .ok_or_else(|| anyhow!("Invalid skill destination: {}", dest.display()))?;
        fs::create_dir_all(parent)?;

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let tmp_name = Self::sanitize_backup_segment(directory);
        let tmp = parent.join(format!(".{tmp_name}.tmp-{}-{nonce}", std::process::id()));

        if tmp.exists() || Self::is_symlink(&tmp) {
            Self::remove_path(&tmp)?;
        }

        let copy_result = Self::copy_dir_recursive(source, &tmp);
        if let Err(err) = copy_result {
            let _ = Self::remove_path(&tmp);
            return Err(err);
        }

        if dest.exists() || Self::is_symlink(dest) {
            Self::remove_path(dest)?;
        }

        fs::rename(&tmp, dest).with_context(|| {
            let _ = Self::remove_path(&tmp);
            format!(
                "替换 Skill 目录失败: {} -> {}",
                tmp.display(),
                dest.display()
            )
        })?;

        Ok(())
    }

    /// 判断路径是否为指向 SSOT 目录内的符号链接。
    fn is_symlink_to_ssot(path: &Path, ssot_dir: &Path) -> bool {
        if !Self::is_symlink(path) {
            return false;
        }

        let Ok(target) = fs::read_link(path) else {
            return false;
        };

        if target.is_absolute() && target.starts_with(ssot_dir) {
            return true;
        }

        let resolved = path
            .parent()
            .map(|parent| parent.join(&target))
            .unwrap_or(target.clone());

        let canonical_ssot = ssot_dir
            .canonicalize()
            .unwrap_or_else(|_| ssot_dir.to_path_buf());
        let canonical_target = resolved.canonicalize().unwrap_or(resolved);

        canonical_target.starts_with(&canonical_ssot)
    }

    /// 从应用目录删除 Skill（支持 symlink 和真实目录）
    pub fn remove_from_app(directory: &str, app: &AppType) -> Result<()> {
        if matches!(app, AppType::ClaudeDesktop) {
            return Ok(());
        }

        // directory 可能来自被污染的 DB 行（如同步导入的远端快照），
        // 这里执行的是删除操作，join 前必须校验，防止任意目录删除。
        let directory = Self::require_valid_directory(directory)?;

        let app_dir = Self::get_app_skills_dir(app)?;
        let skill_path = app_dir.join(&directory);

        if skill_path.exists() || Self::is_symlink(&skill_path) {
            Self::remove_path(&skill_path)?;
            log::debug!("Skill {directory} 已从 {app:?} 删除");
        }

        Ok(())
    }

    /// 同步所有已启用的 Skills 到指定应用
    pub fn sync_to_app(db: &Arc<Database>, app: &AppType) -> Result<()> {
        if matches!(app, AppType::ClaudeDesktop) {
            return Ok(());
        }

        let skills = db.get_all_installed_skills()?;
        let ssot_dir = Self::get_ssot_dir()?;
        let app_dir = Self::get_app_skills_dir(app)?;

        let indexed_skills: HashMap<String, &InstalledSkill> = skills
            .values()
            .map(|skill| (skill.directory.to_lowercase(), skill))
            .collect();

        if app_dir.exists() {
            for entry in fs::read_dir(&app_dir)? {
                let entry = entry?;
                let path = entry.path();
                let dir_name = entry.file_name().to_string_lossy().to_string();

                if dir_name.starts_with('.') {
                    continue;
                }

                if let Some(skill) = indexed_skills.get(&dir_name.to_lowercase()) {
                    if !skill.apps.is_enabled_for(app) {
                        Self::remove_path(&path)?;
                    }
                    continue;
                }

                if Self::is_symlink_to_ssot(&path, &ssot_dir) {
                    Self::remove_path(&path)?;
                }
            }
        }

        for skill in skills.values() {
            if skill.apps.is_enabled_for(app) {
                // 逐条容错而非 `?` 传播：本函数在切换供应商时被调用，一条脏
                // directory（存量点开头目录、或同步导入灌进来的行）不得让整个
                // 应用的 skill 同步全部失效。
                if let Err(err) = Self::sync_to_app_dir(&skill.directory, app) {
                    log::warn!(
                        "同步 skill {} 到 {app:?} 失败，跳过该条: {err}",
                        skill.directory
                    );
                }
            }
        }

        Ok(())
    }


    /// 静态方法：解析技能元数据
    fn parse_skill_metadata_static(path: &Path) -> Result<SkillMetadata> {
        let content = fs::read_to_string(path)?;
        let content = content.trim_start_matches('\u{feff}');

        let parts: Vec<&str> = content.splitn(3, "---").collect();
        if parts.len() < 3 {
            return Ok(SkillMetadata {
                name: None,
                description: None,
            });
        }

        let front_matter = parts[1].trim();
        let meta: SkillMetadata = serde_yaml::from_str(front_matter).unwrap_or(SkillMetadata {
            name: None,
            description: None,
        });

        Ok(meta)
    }

    /// 从 SKILL.md 读取名称和描述，不存在则用目录名兜底
    fn read_skill_name_desc(skill_md: &Path, fallback_name: &str) -> (String, Option<String>) {
        if skill_md.exists() {
            match Self::parse_skill_metadata_static(skill_md) {
                Ok(meta) => (
                    meta.name.unwrap_or_else(|| fallback_name.to_string()),
                    meta.description,
                ),
                Err(_) => (fallback_name.to_string(), None),
            }
        } else {
            (fallback_name.to_string(), None)
        }
    }

    /// 校验并规范化技能源路径（允许多级目录），拒绝路径穿越和绝对路径

    /// 校验并规范化安装目录名（最终落盘目录名，仅单段）
    fn sanitize_install_name(raw: &str) -> Option<String> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return None;
        }

        // 显式拒绝两种分隔符，不能依赖 components() 的平台语义：
        // `\` 在 Linux/macOS 上不是分隔符，会被当成合法单段名放行，
        // 但同一个值同步/还原到 Windows 上就变成了嵌套路径。
        if trimmed.contains('/') || trimmed.contains('\\') {
            return None;
        }

        let path = Path::new(trimmed);
        let mut components = path.components();
        match (components.next(), components.next()) {
            (Some(Component::Normal(name)), None) => {
                let normalized = name.to_string_lossy().trim().to_string();
                if normalized.is_empty()
                    || normalized == "."
                    || normalized == ".."
                    || normalized.starts_with('.')
                {
                    None
                } else {
                    Some(normalized)
                }
            }
            _ => None,
        }
    }

    /// 校验来自 DB 行 / 备份 meta.json 等外部来源的 directory 字段。
    ///
    /// 存储值按构造本应是单段安装名（见 sanitize_install_name），但有两个入口
    /// 会绕过安装期校验：同步导入的远端快照直接灌库（raw SQL），以及手工放置 /
    /// 不可信备份里的 meta.json。任何把它 join 进文件系统路径的使用点（尤其是
    /// remove_dir_all 这类删除操作）必须先过这道校验，拒绝路径穿越。
    ///
    /// 只校验、不归一化：`sanitize_install_name` 会 `trim()`，若拿它的返回值替换
    /// 原值，磁盘上真实带空格的目录名就再也 join 不中。所以这里要求归一化结果与
    /// 原值逐字相同，否则一律视为非法。
    fn require_valid_directory(directory: &str) -> Result<String> {
        match Self::sanitize_install_name(directory) {
            Some(normalized) if normalized == directory => Ok(normalized),
            _ => Err(anyhow!(
                "Invalid skill directory (possible path traversal): {directory:?}"
            )),
        }
    }

    /// GitHub 账号名（user / org login）。
    ///
    /// 只放行 ASCII 字母数字与 `-`。这比 GitHub 自身的规则更严，但该字段会被拼进
    /// 下载 URL，任何 `/`、`.`、`%`、`\` 都可能改写请求落点（见 validate_repo_ref）。
    fn is_valid_github_owner(owner: &str) -> bool {
        !owner.is_empty()
            && owner.len() <= 39
            && owner.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    }

    /// GitHub 仓库名。允许 `.` `-` `_`，但整体不能是 `.` 或 `..`。
    fn is_valid_github_repo_name(name: &str) -> bool {
        !name.is_empty()
            && name.len() <= 100
            && name != "."
            && name != ".."
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
    }

    /// git 分支名。
    ///
    /// 分支名合法含 `/`（`feature/x`），所以不能整体禁掉分隔符——按段做白名单。
    /// 逐段 `!starts_with('.')` 比整体 `contains("..")` 更稳：它同时挡掉 `a/./b`、
    /// `a/.../b` 这类变形。除 `git check-ref-format` 的规则外还额外禁掉 `#` 与 `%`：
    /// 前者会把 URL 后半截变成 fragment，后者可用百分号编码绕过字符检查。
    fn is_valid_git_branch(branch: &str) -> bool {
        // 空串和 "HEAD" 都是 `download_repo` 的哨兵，语义都是「用仓库默认分支」：
        // 分支候选表对两者一视同仁地跳过，改试 main / master，所以它们**永远不会
        // 被拼进 URL**，也就没有可校验的攻击面。空串必须放行——`skill_repos` 的
        // 存量行可以是空 branch（建表默认值是 'main'，但不禁止空串），前端两处
        // `repo.branch || "main"` 就是照着这个前提写的。把它当非法会让那些仓库
        // 在 download_repo 第一行就报 INVALID_REPO_REF，技能面板直接列不出来。
        if branch.is_empty() || branch.eq_ignore_ascii_case("HEAD") {
            return true;
        }
        if branch.len() > 255 {
            return false;
        }
        if branch.starts_with('/') || branch.ends_with('/') || branch.contains("//") {
            return false;
        }
        if branch.contains("@{") {
            return false;
        }
        // `is_ascii_control()` 的范围是 U+0000..=U+001F **加上** U+007F DELETE，
        // 所以不需要另外再点名 DEL。
        if branch
            .chars()
            .any(|c| c.is_ascii_control() || " ~^:?*[\\#%".contains(c))
        {
            return false;
        }
        branch.split('/').all(|segment| {
            !segment.is_empty()
                && !segment.starts_with('.')
                && !segment.ends_with('.')
                && !segment.ends_with(".lock")
        })
    }

    /// 校验一组仓库坐标，用于任何会被拼进 github.com URL 的地方。
    ///
    /// 动机：`download_repo` 把 owner/name/branch 直接 format 进
    /// `https://github.com/{owner}/{name}/archive/refs/heads/{branch}.zip`，而 URL
    /// 解析会消解点段——branch 写成 `../../../releases/download/v1/evil` 时，落点变成
    /// 该仓库的 **release asset**，即攻击者可上传的任意字节。归档内容一旦可控，
    /// 解压路径校验就成了唯一防线，所以这一层必须堵死。
    pub(crate) fn validate_repo_ref(owner: &str, name: &str, branch: &str) -> Result<()> {
        if !Self::is_valid_github_owner(owner) || !Self::is_valid_github_repo_name(name) {
            return Err(anyhow!(format_skill_error(
                "INVALID_REPO_REF",
                &[("owner", owner), ("name", name)],
                Some("checkRepoUrl"),
            )));
        }
        if !Self::is_valid_git_branch(branch) {
            return Err(anyhow!(format_skill_error(
                "INVALID_REPO_REF",
                &[("owner", owner), ("name", name), ("branch", branch)],
                Some("checkRepoUrl"),
            )));
        }
        Ok(())
    }

    /// 出口断言：URL 拼好后再确认它确实指向预期的 github.com 路径。
    ///
    /// 这是纵深防御——即便上面的字符集校验将来漏了某种变形（百分号编码、新的
    /// 分隔符语义等），这里也能拦住落点被改写的请求。
    /// 递归复制目录
    fn copy_dir_recursive(src: &Path, dest: &Path) -> Result<()> {
        fs::create_dir_all(dest)?;

        for entry in fs::read_dir(src)? {
            let entry = entry?;
            let path = entry.path();
            let dest_path = dest.join(entry.file_name());

            if path.is_dir() {
                Self::copy_dir_recursive(&path, &dest_path)?;
            } else {
                fs::copy(&path, &dest_path)?;
            }
        }

        Ok(())
    }

    fn resolve_uninstall_backup_source(skill: &InstalledSkill) -> Result<Option<PathBuf>> {
        // 返回值会被整目录复制进 ~/.cc-gateway/skill-backups/ 并由 get_skill_backups
        // 在界面上列出——脏 directory 在这里等于任意文件读取 + 外泄通道。
        let directory = Self::require_valid_directory(&skill.directory)?;

        let ssot_path = Self::get_ssot_dir()?.join(&directory);
        if ssot_path.is_dir() {
            return Ok(Some(ssot_path));
        }

        for app in AppType::all() {
            let app_dir = match Self::get_app_skills_dir(&app) {
                Ok(dir) => dir,
                Err(_) => continue,
            };
            let candidate = app_dir.join(&directory);
            if candidate.is_dir() {
                return Ok(Some(candidate));
            }
        }

        Ok(None)
    }

    fn sanitize_backup_segment(segment: &str) -> String {
        let sanitized = segment
            .chars()
            .map(|c| match c {
                'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' => c,
                _ => '-',
            })
            .collect::<String>()
            .trim_matches('-')
            .to_string();

        if sanitized.is_empty() {
            "skill".to_string()
        } else {
            sanitized
        }
    }

    fn cleanup_old_skill_backups(dir: &Path) -> Result<()> {
        let mut entries = fs::read_dir(dir)?
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| {
                let metadata = entry.metadata().ok()?;
                if !metadata.is_dir() {
                    return None;
                }
                Some((entry.path(), metadata.modified().ok()))
            })
            .collect::<Vec<_>>();

        if entries.len() <= SKILL_BACKUP_RETAIN_COUNT {
            return Ok(());
        }

        entries.sort_by_key(|(_, modified)| *modified);
        let remove_count = entries.len().saturating_sub(SKILL_BACKUP_RETAIN_COUNT);

        for (path, _) in entries.into_iter().take(remove_count) {
            fs::remove_dir_all(&path)?;
        }

        Ok(())
    }

    fn backup_path_for_id(backup_id: &str) -> Result<PathBuf> {
        if backup_id.contains("..")
            || backup_id.contains('/')
            || backup_id.contains('\\')
            || backup_id.trim().is_empty()
        {
            return Err(anyhow!("Invalid backup id: {backup_id}"));
        }

        Ok(Self::get_backup_dir()?.join(backup_id))
    }

    fn read_backup_metadata(backup_path: &Path) -> Result<SkillBackupMetadata> {
        let metadata_path = backup_path.join("meta.json");
        let content = fs::read_to_string(&metadata_path)
            .with_context(|| format!("failed to read {}", metadata_path.display()))?;
        serde_json::from_str(&content)
            .with_context(|| format!("failed to parse {}", metadata_path.display()))
    }

    fn create_uninstall_backup(skill: &InstalledSkill) -> Result<Option<PathBuf>> {
        let Some(source_path) = Self::resolve_uninstall_backup_source(skill)? else {
            log::warn!(
                "Skill {} 卸载前未找到可备份的目录，将跳过备份",
                skill.directory
            );
            return Ok(None);
        };

        let backup_root = Self::get_backup_dir()?;
        let timestamp = Utc::now().format("%Y%m%d_%H%M%S");
        let slug = Self::sanitize_backup_segment(&skill.directory);
        let mut backup_path = backup_root.join(format!("{timestamp}_{slug}"));
        let mut counter = 1;
        while backup_path.exists() {
            backup_path = backup_root.join(format!("{timestamp}_{slug}_{counter}"));
            counter += 1;
        }

        let write_backup = || -> Result<()> {
            let skill_backup_dir = backup_path.join("skill");
            Self::copy_dir_recursive(&source_path, &skill_backup_dir)?;

            let metadata = SkillBackupMetadata {
                skill: skill.clone(),
                backup_created_at: Utc::now().timestamp(),
                source_path: source_path.to_string_lossy().to_string(),
            };
            let metadata_path = backup_path.join("meta.json");
            let metadata_json = serde_json::to_string_pretty(&metadata)
                .context("failed to serialize skill backup metadata")?;
            fs::write(&metadata_path, metadata_json)
                .with_context(|| format!("failed to write {}", metadata_path.display()))?;
            Ok(())
        };

        if let Err(err) = write_backup() {
            let _ = fs::remove_dir_all(&backup_path);
            return Err(err);
        }

        if let Err(err) = Self::cleanup_old_skill_backups(&backup_root) {
            log::warn!("清理旧 Skill 备份失败: {err:#}");
        }

        log::info!(
            "Skill {} 已在卸载前备份到 {}",
            skill.name,
            backup_path.display()
        );

        Ok(Some(backup_path))
    }

}

// ========== 迁移支持 ==========

/// 从 lock 文件信息构建 skill 的 ID、仓库字段和 readme URL
///
/// 返回 (id, repo_owner, repo_name, repo_branch, readme_url)
fn build_repo_info_from_lock(
    lock: &HashMap<String, LockRepoInfo>,
    dir_name: &str,
) -> (
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
) {
    match lock.get(dir_name) {
        Some(info) => {
            let branch = info.branch.clone();
            let url_branch = branch.clone().unwrap_or_else(|| "HEAD".to_string());
            // 优先使用 lock 文件中的 skillPath，否则回退到 dir_name/SKILL.md
            let fallback = format!("{dir_name}/SKILL.md");
            let doc_path = info.skill_path.as_deref().unwrap_or(&fallback);
            let url =
                SkillService::build_skill_doc_url(&info.owner, &info.repo, &url_branch, doc_path);
            (
                format!("{}/{}:{dir_name}", info.owner, info.repo),
                Some(info.owner.clone()),
                Some(info.repo.clone()),
                branch,
                url,
            )
        }
        None => (format!("local:{dir_name}"), None, None, None, None),
    }
}

/// 将 lock 文件中发现的仓库保存到 skill_repos（去重）
fn save_repos_from_lock(
    db: &Arc<Database>,
    lock: &HashMap<String, LockRepoInfo>,
    directories: impl Iterator<Item = impl AsRef<str>>,
) {
    let existing_repos: HashSet<(String, String)> = db
        .get_skill_repos()
        .unwrap_or_default()
        .into_iter()
        .map(|r| (r.owner, r.name))
        .collect();
    let mut added = HashSet::new();

    for dir_name in directories {
        if let Some(info) = lock.get(dir_name.as_ref()) {
            let key = (info.owner.clone(), info.repo.clone());
            if !existing_repos.contains(&key) && added.insert(key) {
                let skill_repo = SkillRepo {
                    owner: info.owner.clone(),
                    name: info.repo.clone(),
                    // 未知分支时使用 HEAD 语义，后续下载会回退到 main/master。
                    branch: info.branch.clone().unwrap_or_else(|| "HEAD".to_string()),
                    enabled: true,
                };
                // lock 文件由外部 agents CLI 写入，owner/repo/branch 均未经校验，
                // 且 branch 是从 `/tree/`、fragment、`?ref=` 里抠出来的裸串。
                if SkillService::validate_repo_ref(
                    &skill_repo.owner,
                    &skill_repo.name,
                    &skill_repo.branch,
                )
                .is_err()
                {
                    log::warn!(
                        "跳过 agents lock 中坐标非法的仓库: {}/{}@{}",
                        skill_repo.owner,
                        skill_repo.name,
                        skill_repo.branch
                    );
                    continue;
                }
                if let Err(e) = db.save_skill_repo(&skill_repo) {
                    log::warn!("保存 skill 仓库 {}/{} 失败: {}", info.owner, info.repo, e);
                } else {
                    log::info!(
                        "从 agents lock 文件发现并添加仓库: {}/{} ({})",
                        info.owner,
                        info.repo,
                        skill_repo.branch
                    );
                }
            }
        }
    }
}

/// 首次启动迁移：扫描应用目录，重建数据库
pub fn migrate_skills_to_ssot(db: &Arc<Database>) -> Result<usize> {
    let ssot_dir = SkillService::get_ssot_dir()?;
    let agents_lock = parse_agents_lock();
    let snapshot: Vec<LegacySkillMigrationRow> =
        match db.get_setting("skills_ssot_migration_snapshot")? {
            Some(value) if !value.trim().is_empty() => match serde_json::from_str(&value) {
                Ok(rows) => rows,
                Err(err) => {
                    log::warn!("解析 skills 迁移快照失败，将回退到文件系统扫描: {err}");
                    Vec::new()
                }
            },
            _ => Vec::new(),
        };

    let has_snapshot = !snapshot.is_empty();
    let mut discovered: HashMap<String, SkillApps> = HashMap::new();

    if has_snapshot {
        for row in &snapshot {
            // snapshot 存在 settings 表里，而 settings 在同步范围内、可被远端快照
            // 覆盖。下面 discovered 的每个 key 都会被 join 成路径并写回 skills 表，
            // 所以脏值必须在进入 discovered 之前就滤掉。
            if SkillService::require_valid_directory(&row.directory).is_err() {
                log::warn!("跳过 SSOT 迁移快照中非法的 directory: {:?}", row.directory);
                continue;
            }
            if let Ok(app) = row.app_type.parse::<AppType>() {
                discovered
                    .entry(row.directory.clone())
                    .or_default()
                    .set_enabled_for(&app, true);
            }
        }
    }

    // 扫描各应用目录
    for app in AppType::all() {
        let app_dir = match SkillService::get_app_skills_dir(&app) {
            Ok(d) => d,
            Err(_) => continue,
        };

        let entries = match fs::read_dir(&app_dir) {
            Ok(e) => e,
            Err(_) => continue,
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }

            let dir_name = entry.file_name().to_string_lossy().to_string();
            if dir_name.starts_with('.') {
                continue;
            }
            if !path.join("SKILL.md").exists() {
                continue;
            }
            if has_snapshot && !discovered.contains_key(&dir_name) {
                continue;
            }

            // 复制到 SSOT（如果不存在）
            let ssot_path = ssot_dir.join(&dir_name);
            if !ssot_path.exists() {
                SkillService::copy_dir_recursive(&path, &ssot_path)?;
            }

            if !has_snapshot {
                discovered
                    .entry(dir_name)
                    .or_default()
                    .set_enabled_for(&app, true);
            }
        }
    }

    // 重建数据库
    db.clear_skills()?;

    // 将 lock 文件中发现的仓库保存到 skill_repos
    save_repos_from_lock(db, &agents_lock, discovered.keys());

    let mut count = 0;
    for (directory, apps) in discovered {
        let ssot_path = ssot_dir.join(&directory);
        let skill_md = ssot_path.join("SKILL.md");

        let (name, description) = SkillService::read_skill_name_desc(&skill_md, &directory);

        let (id, repo_owner, repo_name, repo_branch, readme_url) =
            build_repo_info_from_lock(&agents_lock, &directory);

        let content_hash = SkillService::compute_dir_hash(&ssot_path).ok();

        let skill = InstalledSkill {
            id,
            name,
            description,
            directory,
            repo_owner,
            repo_name,
            repo_branch,
            readme_url,
            apps,
            installed_at: chrono::Utc::now().timestamp(),
            content_hash,
            updated_at: 0,
        };

        db.save_skill(&skill)?;
        count += 1;
    }

    let _ = db.set_setting("skills_ssot_migration_snapshot", "");

    log::info!("Skills 迁移完成，共 {count} 个");

    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn validate_repo_ref_accepts_real_world_coordinates() {
        // 合法分支名允许 `/`，不能因为防穿越就把它们一起禁掉
        for branch in [
            "main",
            "master",
            "HEAD",
            "feature/new-thing",
            "release/v1.2.3",
            "fix-123",
            "user.name/topic",
        ] {
            assert!(
                SkillService::validate_repo_ref("farion1231", "cc-switch", branch).is_ok(),
                "must accept branch: {branch:?}"
            );
        }
        assert!(SkillService::validate_repo_ref("a", "b.c_d-e", "main").is_ok());
    }

    #[test]
    fn validate_repo_ref_accepts_the_empty_branch_sentinel() {
        // 空 branch 与 "HEAD" 在 download_repo 里是同一个哨兵：分支候选表跳过
        // 两者，改试 main / master，所以它们从不进 URL。校验若把空串当非法，
        // 存量 skill_repos 行（建表默认 'main'，但空串没被禁）会在 download_repo
        // 第一行就 INVALID_REPO_REF，整个技能面板列不出东西——前端两处
        // `repo.branch || "main"` 正是照着"空串可用"写的。
        assert!(
            SkillService::validate_repo_ref("farion1231", "cc-switch", "").is_ok(),
            "the empty-branch sentinel must stay usable"
        );
    }

    #[test]
    fn validate_repo_ref_rejects_url_hijacking_branches() {
        // 这是核心用例：branch 被拼进 archive URL，URL 解析会消解点段，
        // 落点会从 /archive/refs/heads/ 改写成攻击者可上传的 release asset。
        for branch in [
            "../../../releases/download/v1/evil",
            "..",
            "../x",
            "a/../../b",
            "a/./b",
            "..\\..\\releases\\download\\v1\\evil",
            "/leading",
            "trailing/",
            "double//slash",
            "with space",
            "frag#ment",
            "pct%2e%2e",
            "ref@{0}",
            "seg.lock",
            ".hidden/x",
        ] {
            assert!(
                SkillService::validate_repo_ref("owner", "repo", branch).is_err(),
                "must reject branch: {branch:?}"
            );
        }
        for (owner, name) in [
            ("..", "repo"),
            ("own/er", "repo"),
            ("owner", ".."),
            ("owner", "re/po"),
            ("owner", "re po"),
            ("", "repo"),
            ("owner", ""),
        ] {
            assert!(
                SkillService::validate_repo_ref(owner, name, "main").is_err(),
                "must reject coordinates: {owner:?}/{name:?}"
            );
        }
    }

    #[test]
    fn build_skill_doc_url_drops_illegal_coordinates() {
        assert_eq!(
            SkillService::build_skill_doc_url("owner", "repo", "main", "a/SKILL.md").as_deref(),
            Some("https://github.com/owner/repo/blob/main/a/SKILL.md")
        );
        // readme_url 会被前端 openExternal 直接打开，非法坐标不得产出链接
        assert!(
            SkillService::build_skill_doc_url("owner", "repo", "../../../issues", "x").is_none()
        );
    }

    #[test]
    fn write_skill(dir: &Path, name: &str) {
        fs::create_dir_all(dir).expect("create skill dir");
        fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: Test skill\n---\n"),
        )
        .expect("write SKILL.md");
    }

    /// CC_GATEWAY_TEST_HOME 隔离守卫（serial 测试间互斥由 #[serial] 保证，
    /// 守卫只负责在测试结束后恢复原值）。
    struct TestHomeGuard(Option<std::ffi::OsString>);
    impl TestHomeGuard {
        fn set(home: &Path) -> Self {
            let guard = Self(std::env::var_os("CC_GATEWAY_TEST_HOME"));
            std::env::set_var("CC_GATEWAY_TEST_HOME", home);
            guard
        }
    }
    impl Drop for TestHomeGuard {
        fn drop(&mut self) {
            match self.0.take() {
                Some(value) => std::env::set_var("CC_GATEWAY_TEST_HOME", value),
                None => std::env::remove_var("CC_GATEWAY_TEST_HOME"),
            }
        }
    }

    fn poisoned_skill(id: &str, directory: &str) -> InstalledSkill {
        InstalledSkill {
            id: id.to_string(),
            name: "poisoned".to_string(),
            description: None,
            directory: directory.to_string(),
            repo_owner: None,
            repo_name: None,
            repo_branch: None,
            readme_url: None,
            apps: SkillApps::default(),
            installed_at: 0,
            content_hash: None,
            updated_at: 0,
        }
    }

    #[test]
    fn require_valid_directory_accepts_single_segment_names_only() {
        assert_eq!(
            SkillService::require_valid_directory("my-skill").expect("valid name"),
            "my-skill"
        );
        for bad in [
            "..",
            "../..",
            "../../etc",
            "a/b",
            "a\\b",
            "",
            ".hidden",
            "C:\\evil",
            "/etc",
        ] {
            assert!(
                SkillService::require_valid_directory(bad).is_err(),
                "must reject: {bad:?}"
            );
        }
    }

    #[test]
    fn restore_from_backup_rejects_traversal_directory_in_metadata() {
        let temp = tempdir().expect("tempdir");
        let _guard = TestHomeGuard::set(temp.path());

        // 手工放置一个备份：meta.json 里的 directory 指向 SSOT 之外。
        // SSOT 位于 {home}/.cc-gateway/skills，"../../pwned-restore" 若生效会写到 {home}/pwned-restore。
        let backup_id = "20260727_120000_evil";
        let backup_dir = SkillService::get_backup_dir()
            .expect("backup dir")
            .join(backup_id);
        write_skill(&backup_dir.join("skill"), "evil");
        let metadata = SkillBackupMetadata {
            skill: poisoned_skill("owner/repo:evil", "../../pwned-restore"),
            backup_created_at: 0,
            source_path: "x".to_string(),
        };
        fs::write(
            backup_dir.join("meta.json"),
            serde_json::to_string_pretty(&metadata).expect("serialize metadata"),
        )
        .expect("write meta.json");

        let db = std::sync::Arc::new(Database::memory().expect("memory db"));
        let result = SkillService::restore_from_backup(&db, backup_id, &AppType::Claude);

        assert!(
            result.is_err(),
            "restore must reject a traversal directory from meta.json"
        );
        assert!(
            !temp.path().join("pwned-restore").exists(),
            "restore must not write outside the SSOT dir"
        );
    }

    #[test]
    #[serial_test::serial]
    fn remove_from_app_rejects_traversal_directory() {
        let temp = tempdir().expect("tempdir");
        let _guard = TestHomeGuard::set(temp.path());

        // 受害目录与 app skills 目录都先建好，保证未修复时代码真的能删到它：
        // app_dir = {home}/.claude/skills，"../../victim-remove" 解析为 {home}/victim-remove。
        let victim = temp.path().join("victim-remove");
        fs::create_dir_all(&victim).expect("create victim dir");
        fs::create_dir_all(temp.path().join(".claude").join("skills")).expect("create app dir");

        let result = SkillService::remove_from_app("../../victim-remove", &AppType::Claude);

        assert!(result.is_err(), "remove_from_app must reject traversal");
        assert!(victim.exists(), "victim directory must not be deleted");
    }

    #[test]
    #[serial_test::serial]
    fn uninstall_rejects_traversal_directory_from_db_row() {
        let temp = tempdir().expect("tempdir");
        let _guard = TestHomeGuard::set(temp.path());

        // 模拟历史数据库中遗留的脏数据：directory 含路径穿越（save_skill 不校验）。
        // SSOT = {home}/.cc-gateway/skills，
        // "../../victim-uninstall" 解析为 {home}/victim-uninstall。
        let victim = temp.path().join("victim-uninstall");
        fs::create_dir_all(&victim).expect("create victim dir");

        let db = std::sync::Arc::new(Database::memory().expect("memory db"));
        let skill = poisoned_skill("owner/repo:evil", "../../victim-uninstall");
        db.save_skill(&skill).expect("seed poisoned row");

        let result = SkillService::uninstall(&db, &skill.id);

        // 危险的文件系统操作必须被跳过……
        assert!(victim.exists(), "victim directory must not be deleted");
        // ……但记录本身必须能删掉。db.delete_skill 全项目只有 uninstall 一处调用
        // 且未暴露为命令，若这里返回 Err，脏行就永远无法从界面清除。
        assert!(
            result.is_ok(),
            "uninstall must still succeed so the poisoned row can be removed: {result:?}"
        );
        assert!(
            db.get_installed_skill(&skill.id)
                .expect("query skill")
                .is_none(),
            "poisoned row must be deleted from the database"
        );
    }

    #[test]
    #[serial_test::serial]
    fn migrate_storage_skips_bad_rows_without_moving_foreign_dirs() {
        /// skill_storage_location 存在进程级全局 settings_store 里，migrate_storage
        /// 成功后会改写它。#[serial] 只保证互斥、不负责还原，必须自己复位，
        /// 否则后续测试的 get_ssot_dir() 会解析到另一个位置。
        struct StorageLocationGuard(SkillStorageLocation);
        impl Drop for StorageLocationGuard {
            fn drop(&mut self) {
                let _ = crate::settings::set_skill_storage_location(self.0);
            }
        }
        let _location_guard = StorageLocationGuard(crate::settings::get_skill_storage_location());

        let temp = tempdir().expect("tempdir");
        let _guard = TestHomeGuard::set(temp.path());

        // migrate_storage 会 fs::rename / remove_dir_all，脏 directory 能把
        // SSOT 之外的任意目录搬走或删掉。
        let victim = temp.path().join("victim-migrate");
        fs::create_dir_all(&victim).expect("create victim dir");

        let db = std::sync::Arc::new(Database::memory().expect("memory db"));
        let skill = poisoned_skill("owner/repo:evil", "../../victim-migrate");
        db.save_skill(&skill).expect("seed poisoned row");

        // 必须迁到与当前不同的位置，否则函数在 current == target 处直接短路返回
        let result = SkillService::migrate_storage(&db, SkillStorageLocation::Unified)
            .expect("migration must not abort");

        assert!(victim.exists(), "foreign directory must not be moved away");
        assert_eq!(
            result.migrated_count, 0,
            "poisoned row must not count as migrated"
        );
        assert!(
            !result.errors.is_empty(),
            "the skipped row must be reported through the errors channel"
        );
    }

    #[test]
    #[serial_test::serial]
    fn uninstall_backup_source_rejects_traversal_directory() {
        let temp = tempdir().expect("tempdir");
        let _guard = TestHomeGuard::set(temp.path());

        // 备份源会被整目录复制进 skill-backups 并在界面列出 → 任意文件外泄面。
        let secrets = temp.path().join("secrets");
        fs::create_dir_all(&secrets).expect("create secrets dir");
        fs::write(secrets.join("id_rsa"), b"PRIVATE").expect("write secret");

        let skill = poisoned_skill("owner/repo:evil", "../../secrets");
        let result = SkillService::resolve_uninstall_backup_source(&skill);

        assert!(
            result.is_err(),
            "backup source must reject a traversal directory"
        );
    }

    #[test]
    #[serial_test::serial]
    fn sync_to_app_skips_bad_rows_instead_of_aborting_the_whole_app() {
        let temp = tempdir().expect("tempdir");
        let _guard = TestHomeGuard::set(temp.path());

        let ssot_dir = SkillService::get_ssot_dir().expect("ssot dir");
        write_skill(&ssot_dir.join("good-skill"), "good");

        let db = std::sync::Arc::new(Database::memory().expect("memory db"));
        // 一条脏行 + 一条正常行。脏行来自同步导入/存量数据，不得连累正常行——
        // sync_to_app 在切换供应商时触发，整体中断会让所有 skill 一起失效。
        //
        // 名字刻意让脏行排前面：查询是 `ORDER BY name ASC`，脏行必须先被处理，
        // 否则「未修复时会中断」这个前提不成立，测试就成了摆设。
        let mut bad = poisoned_skill("owner/repo:bad", "../../escape-sync");
        bad.name = "a-poisoned".to_string();
        bad.apps = SkillApps::only(&AppType::Claude);
        db.save_skill(&bad).expect("seed poisoned row");

        let mut good = poisoned_skill("owner/repo:good", "good-skill");
        good.name = "z-healthy".to_string();
        good.apps = SkillApps::only(&AppType::Claude);
        db.save_skill(&good).expect("seed good row");

        SkillService::sync_to_app(&db, &AppType::Claude).expect("sync must not abort");

        let app_dir = SkillService::get_app_skills_dir(&AppType::Claude).expect("app dir");
        assert!(
            app_dir.join("good-skill").exists(),
            "the healthy skill must still be synced despite the poisoned row"
        );
    }

    #[test]
    // serial：与 backup/deeplink 等同样读写进程级 CC_GATEWAY_TEST_HOME 的测试互斥，
    // EnvGuard 只负责恢复不提供互斥。
    #[serial_test::serial]
    fn get_app_skills_dir_honors_test_home_override() {
        // 回归：曾直呼 dirs::home_dir() 绕过 CC_GATEWAY_TEST_HOME——Unix 上碰巧跟 $HOME
        // 一致所以测试能过，Windows 上 dirs 走 Known Folder API，测试隔离整体失效
        // （tests/skill_sync.rs 扫到 runner 真实用户目录）。
        struct EnvGuard(Option<std::ffi::OsString>);
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(value) => std::env::set_var("CC_GATEWAY_TEST_HOME", value),
                    None => std::env::remove_var("CC_GATEWAY_TEST_HOME"),
                }
            }
        }
        let temp = tempdir().expect("tempdir");
        let _guard = EnvGuard(std::env::var_os("CC_GATEWAY_TEST_HOME"));
        std::env::set_var("CC_GATEWAY_TEST_HOME", temp.path());

        let dir =
            SkillService::get_app_skills_dir(&AppType::Claude).expect("resolve claude skills dir");
        assert!(
            dir.starts_with(temp.path()),
            "skills dir must live under the overridden test home, got {}",
            dir.display()
        );
    }

    #[test]
    fn replace_dest_with_copy_rejects_empty_source_without_touching_existing_dest() {
        let temp = tempdir().expect("tempdir");
        let source = temp.path().join("source-skill");
        let dest = temp.path().join("app-skills").join("source-skill");
        fs::create_dir_all(&source).expect("create empty source");
        write_skill(&dest, "Existing Skill");

        let err = SkillService::replace_dest_with_copy(&source, &dest, "source-skill")
            .expect_err("empty source should not replace existing app skill");

        assert!(
            err.to_string().contains("SKILL.md"),
            "unexpected error: {err:#}"
        );
        assert!(
            dest.join("SKILL.md").is_file(),
            "existing destination skill should be preserved"
        );
    }

    #[test]
}
