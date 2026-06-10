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
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;

/// 临时文件快照：持有快照文件和对应的 SQLite 连接。
///
/// `conn` 先于 `_temp` 声明，确保 drop 顺序为先关闭 SQLite 句柄，再删除临时文件。
/// 在 Windows 上 SQLite 持有独占锁，若反序 drop 会导致 NamedTempFile::drop 静默失败，临时文件泄漏。
pub(crate) struct DbSnapshot {
    conn: Connection,
    _temp: NamedTempFile,
}

impl DbSnapshot {
    pub(crate) fn conn(&self) -> &Connection {
        &self.conn
    }
}

const CC_SWITCH_SQL_EXPORT_HEADER: &str = "-- CC Switch SQLite 导出";

/// Tables whose data rows are skipped when exporting for WebDAV sync.
const SYNC_SKIP_TABLES: &[&str] = &[
    "proxy_request_logs",
    "stream_check_logs",
    "provider_health",
    "proxy_live_backup",
    "usage_daily_rollups",
];

/// Tables whose local data is preserved (restored from local snapshot) during WebDAV import.
/// Excludes ephemeral tables like provider_health that can safely rebuild at runtime.
const SYNC_PRESERVE_TABLES: &[&str] = &[
    "proxy_request_logs",
    "stream_check_logs",
    "proxy_live_backup",
    "usage_daily_rollups",
];

/// A database backup entry for the UI
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupEntry {
    pub filename: String,
    pub size_bytes: u64,
    pub created_at: String, // ISO 8601
}

impl Database {
    /// 导出为 SQLite 兼容的 SQL 文本（内存缓冲，完整导出）。
    ///
    /// **仅用于小型数据库或测试**。大型导出应使用 [`export_sql`] 流式路径。
    pub fn export_sql_string(&self) -> Result<String, AppError> {
        let snapshot = self.snapshot_to_file()?;
        let mut buf: Vec<u8> = Vec::new();
        Self::dump_sql_to_writer(snapshot.conn(), &[], &mut buf)?;
        String::from_utf8(buf)
            .map_err(|e| AppError::Database(format!("SQL dump 包含无效 UTF-8: {e}")))
    }

    /// Export SQL for sync (WebDAV), skipping local-only tables' data.
    ///
    /// **仅用于小型数据库或测试**。F1-B 将使该路径完全流式化。
    pub fn export_sql_string_for_sync(&self) -> Result<String, AppError> {
        let snapshot = self.snapshot_to_file()?;
        let mut buf: Vec<u8> = Vec::new();
        Self::dump_sql_to_writer(snapshot.conn(), SYNC_SKIP_TABLES, &mut buf)?;
        String::from_utf8(buf)
            .map_err(|e| AppError::Database(format!("SQL dump 包含无效 UTF-8: {e}")))
    }

    /// Export sync SQL directly to a writer (streaming, zero copy-into-Vec).
    ///
    /// Skips the same `SYNC_SKIP_TABLES` as [`export_sql_string_for_sync`].
    /// The byte format is identical — this is the F1-B streaming path.
    pub fn export_sql_to_writer_for_sync<W: Write>(&self, w: &mut W) -> Result<(), AppError> {
        let snapshot = self.snapshot_to_file()?;
        Self::dump_sql_to_writer(snapshot.conn(), SYNC_SKIP_TABLES, w)
    }

    /// 导出为 SQLite 兼容的 SQL 文本，流式写入目标文件（原子替换）。
    ///
    /// 全程无 O(DB 大小) 内存分配：快照写临时文件，dump 直接流式写入，
    /// 最后 persist 原子替换目标路径。临时文件由 NamedTempFile RAII 管理，
    /// 任何错误路径均自动清理，不泄漏 .tmp 文件。
    pub fn export_sql(&self, target_path: &Path) -> Result<(), AppError> {
        let parent = target_path
            .parent()
            .ok_or_else(|| AppError::Config("无效的导出路径".to_string()))?;

        fs::create_dir_all(parent).map_err(|e| AppError::io(parent, e))?;

        // 在目标目录创建临时文件，保证 persist 是同一挂载点内的原子操作；
        // NamedTempFile 在任何提前返回时自动删除临时文件。
        let mut tmp = NamedTempFile::new_in(parent).map_err(|e| AppError::IoContext {
            context: "创建导出临时文件失败".to_string(),
            source: e,
        })?;
        // 提前捕获路径，避免 flush 错误闭包与可变借用冲突。
        let tmp_path_for_err = tmp.path().to_path_buf();

        {
            let mut writer = BufWriter::new(tmp.as_file_mut());
            let snapshot = self.snapshot_to_file()?;
            Self::dump_sql_to_writer(snapshot.conn(), &[], &mut writer)?;
            writer
                .flush()
                .map_err(|e| AppError::io(&tmp_path_for_err, e))?;
        }

        #[cfg(windows)]
        if target_path.exists() {
            fs::remove_file(target_path).map_err(|e| AppError::IoContext {
                context: format!("覆盖前删除目标文件失败: {}", target_path.display()),
                source: e,
            })?;
        }

        tmp.persist(target_path).map_err(|e| AppError::IoContext {
            context: format!("原子替换导出文件失败: {}", target_path.display()),
            source: e.error,
        })?;

        Ok(())
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
        self.import_sql_string_inner(sql_raw, &[])
    }

    /// Import SQL generated for sync, then restore local-only tables from the
    /// current device snapshot before replacing the main database.
    pub(crate) fn import_sql_string_for_sync(&self, sql_raw: &str) -> Result<String, AppError> {
        self.import_sql_string_inner(sql_raw, SYNC_PRESERVE_TABLES)
    }

    fn import_sql_string_inner(
        &self,
        sql_raw: &str,
        preserve_tables: &[&str],
    ) -> Result<String, AppError> {
        let sql_content = sql_raw.trim_start_matches('\u{feff}');
        Self::validate_cc_switch_sql_export(sql_content)?;

        // 导入前备份现有数据库
        let backup_path = self.backup_database_file()?;

        let local_snapshot = if preserve_tables.is_empty() {
            None
        } else {
            Some(self.snapshot_to_file()?)
        };

        // 在临时数据库执行导入，确保失败不会污染主库
        let temp_file = NamedTempFile::new().map_err(|e| AppError::IoContext {
            context: "创建临时数据库文件失败".to_string(),
            source: e,
        })?;
        let temp_path = temp_file.path().to_path_buf();
        let temp_conn =
            Connection::open(&temp_path).map_err(|e| AppError::Database(e.to_string()))?;

        temp_conn
            .execute_batch(sql_content)
            .map_err(|e| AppError::Database(format!("执行 SQL 导入失败: {e}")))?;

        // 补齐缺失表/索引并进行基础校验
        Self::create_tables_on_conn(&temp_conn)?;
        Self::apply_schema_migrations_on_conn(&temp_conn)?;
        Self::validate_basic_state(&temp_conn)?;
        if let Some(local_snapshot) = local_snapshot.as_ref() {
            Self::restore_tables(local_snapshot.conn(), &temp_conn, preserve_tables)?;
        }

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

    /// 创建临时文件快照以避免长时间持有数据库锁。
    ///
    /// 快照写入临时文件（而非内存），不产生 O(DB 大小) 的 RAM 分配。
    /// 返回的 [`DbSnapshot`] 持有临时文件和连接的所有权，两者同时释放。
    pub(crate) fn snapshot_to_file(&self) -> Result<DbSnapshot, AppError> {
        let temp = NamedTempFile::new().map_err(|e| AppError::IoContext {
            context: "创建快照临时文件失败".to_string(),
            source: e,
        })?;
        let temp_path = temp.path().to_path_buf();

        {
            let src = lock_conn!(self.conn);
            let mut dest =
                Connection::open(&temp_path).map_err(|e| AppError::Database(e.to_string()))?;
            let backup =
                Backup::new(&src, &mut dest).map_err(|e| AppError::Database(e.to_string()))?;
            backup
                .step(-1)
                .map_err(|e| AppError::Database(e.to_string()))?;
        }

        let conn = Connection::open(&temp_path).map_err(|e| AppError::Database(e.to_string()))?;
        Ok(DbSnapshot { _temp: temp, conn })
    }

    fn validate_cc_switch_sql_export(sql: &str) -> Result<(), AppError> {
        let trimmed = sql.trim_start();
        if trimmed.starts_with(CC_SWITCH_SQL_EXPORT_HEADER) {
            return Ok(());
        }

        Err(AppError::localized(
            "backup.sql.invalid_format",
            "仅支持导入由 CC Switch 导出的 SQL 备份文件。",
            "Only SQL backups exported by CC Switch are supported.",
        ))
    }

    fn restore_tables(
        source_conn: &Connection,
        target_conn: &Connection,
        tables: &[&str],
    ) -> Result<(), AppError> {
        for table in tables {
            if !Self::table_exists(source_conn, table)? || !Self::table_exists(target_conn, table)?
            {
                continue;
            }

            let columns = Self::get_table_columns(source_conn, table)?;
            if columns.is_empty() {
                continue;
            }

            target_conn
                .execute(&format!("DELETE FROM \"{table}\""), [])
                .map_err(|e| AppError::Database(format!("清空表 {table} 失败: {e}")))?;

            let placeholders = (1..=columns.len())
                .map(|idx| format!("?{idx}"))
                .collect::<Vec<_>>()
                .join(", ");
            let cols = columns
                .iter()
                .map(|column| format!("\"{column}\""))
                .collect::<Vec<_>>()
                .join(", ");
            let insert_sql = format!("INSERT INTO \"{table}\" ({cols}) VALUES ({placeholders})");

            let mut stmt = source_conn
                .prepare(&format!("SELECT * FROM \"{table}\""))
                .map_err(|e| AppError::Database(format!("读取表 {table} 失败: {e}")))?;
            let mut rows = stmt
                .query([])
                .map_err(|e| AppError::Database(format!("查询表 {table} 数据失败: {e}")))?;

            while let Some(row) = rows.next().map_err(|e| AppError::Database(e.to_string()))? {
                let mut values = Vec::with_capacity(columns.len());
                for idx in 0..columns.len() {
                    values.push(
                        row.get::<_, rusqlite::types::Value>(idx)
                            .map_err(|e| AppError::Database(e.to_string()))?,
                    );
                }

                target_conn
                    .execute(&insert_sql, rusqlite::params_from_iter(values.iter()))
                    .map_err(|e| AppError::Database(format!("恢复表 {table} 数据失败: {e}")))?;
            }
        }

        Ok(())
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
    pub fn backup_database_file(&self) -> Result<Option<PathBuf>, AppError> {
        let db_path = get_app_config_dir().join("cc-switch.db");
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

    /// 将数据库 dump 为 SQL 文本，流式写入 `w`。
    ///
    /// 不在内存中积累整个 dump 字符串；每条 INSERT / schema 行写完后立即
    /// 传给 writer，内存峰值仅为单行缓冲。字节格式与旧版 `dump_sql` 完全一致。
    fn dump_sql_to_writer<W: Write>(
        conn: &Connection,
        skip_tables: &[&str],
        w: &mut W,
    ) -> Result<(), AppError> {
        let map_io = |e: std::io::Error| AppError::IoContext {
            context: "写入 SQL dump 失败".to_string(),
            source: e,
        };

        let timestamp = Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
        let user_version: i64 = conn
            .query_row("PRAGMA user_version;", [], |row| row.get(0))
            .map_err(|e| AppError::Database(e.to_string()))?;

        write!(
            w,
            "-- CC Switch SQLite 导出\n-- 生成时间: {timestamp}\n-- user_version: {user_version}\n"
        )
        .map_err(map_io)?;
        w.write_all(b"PRAGMA foreign_keys=OFF;\n").map_err(map_io)?;
        writeln!(w, "PRAGMA user_version={user_version};").map_err(map_io)?;
        w.write_all(b"BEGIN TRANSACTION;\n").map_err(map_io)?;

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

            w.write_all(sql.as_bytes()).map_err(map_io)?;
            w.write_all(b";\n").map_err(map_io)?;

            if obj_type == "table" && !name.starts_with("sqlite_") {
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

            let cols = columns
                .iter()
                .map(|c| format!("\"{c}\""))
                .collect::<Vec<_>>()
                .join(", ");

            let mut stmt = conn
                .prepare(&format!("SELECT * FROM \"{table}\""))
                .map_err(|e| AppError::Database(e.to_string()))?;
            let mut rows = stmt
                .query([])
                .map_err(|e| AppError::Database(e.to_string()))?;

            while let Some(row) = rows.next().map_err(|e| AppError::Database(e.to_string()))? {
                let mut values = Vec::with_capacity(columns.len());
                for idx in 0..columns.len() {
                    let value = row
                        .get_ref(idx)
                        .map_err(|e| AppError::Database(e.to_string()))?;
                    values.push(Self::format_sql_value(value)?);
                }

                writeln!(
                    w,
                    "INSERT INTO \"{table}\" ({cols}) VALUES ({});",
                    values.join(", ")
                )
                .map_err(map_io)?;
            }
        }

        w.write_all(b"COMMIT;\nPRAGMA foreign_keys=ON;\n")
            .map_err(map_io)?;
        Ok(())
    }

    /// 获取表的列名列表
    fn get_table_columns(conn: &Connection, table: &str) -> Result<Vec<String>, AppError> {
        let mut stmt = conn
            .prepare(&format!("PRAGMA table_info(\"{table}\")"))
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
    use super::{Database, CC_SWITCH_SQL_EXPORT_HEADER};
    use crate::error::AppError;
    use crate::settings::{update_settings, AppSettings};
    use serial_test::serial;

    // ── F1-A: 流式导出测试 ─────────────────────────────────────────────────

    /// 完整往返：seed DB → export_sql 到文件 → import_sql 到新 DB → 行一致
    #[test]
    fn export_sql_file_roundtrip() -> Result<(), AppError> {
        let src = Database::memory()?;
        {
            let conn = crate::database::lock_conn!(src.conn);
            conn.execute(
                "INSERT INTO providers (id, app_type, name, settings_config, meta)
                 VALUES ('p-rt', 'claude', 'Roundtrip Provider', '{}', '{}')",
                [],
            )?;
            conn.execute(
                "INSERT INTO mcp_servers (id, name, server_config)
                 VALUES ('m-rt', 'RT MCP', '{}')",
                [],
            )?;
        }

        let tmp_dir = tempfile::tempdir().expect("tempdir");
        let export_path = tmp_dir.path().join("export.sql");

        src.export_sql(&export_path)?;
        assert!(export_path.exists(), "export file should exist");

        let dst = Database::memory()?;
        dst.import_sql(&export_path)?;

        let (providers, mcps): (i64, i64) = {
            let conn = crate::database::lock_conn!(dst.conn);
            let p = conn.query_row(
                "SELECT COUNT(*) FROM providers WHERE id = 'p-rt'",
                [],
                |r| r.get(0),
            )?;
            let m = conn.query_row(
                "SELECT COUNT(*) FROM mcp_servers WHERE id = 'm-rt'",
                [],
                |r| r.get(0),
            )?;
            (p, m)
        };
        assert_eq!(providers, 1, "provider row should round-trip");
        assert_eq!(mcps, 1, "mcp_server row should round-trip");

        Ok(())
    }

    /// 原子覆盖合约：对同一路径二次导出时，目标文件内容以第二次导出的数据为准。
    /// 验证 export_sql 的 persist 路径在目标已存在时能正确替换。
    #[test]
    fn export_sql_overwrites_existing_file() -> Result<(), AppError> {
        let tmp_dir = tempfile::tempdir().expect("tempdir");
        let export_path = tmp_dir.path().join("overwrite_test.sql");

        // 第一次导出：只有 provider p-ow1
        let db1 = Database::memory()?;
        {
            let conn = crate::database::lock_conn!(db1.conn);
            conn.execute(
                "INSERT INTO providers (id, app_type, name, settings_config, meta)
                 VALUES ('p-ow1', 'claude', 'OW Provider 1', '{}', '{}')",
                [],
            )?;
        }
        db1.export_sql(&export_path)?;
        assert!(
            export_path.exists(),
            "export file should exist after first export"
        );

        // 第二次导出：有 provider p-ow1 和 p-ow2 两行
        let db2 = Database::memory()?;
        {
            let conn = crate::database::lock_conn!(db2.conn);
            conn.execute(
                "INSERT INTO providers (id, app_type, name, settings_config, meta)
                 VALUES ('p-ow1', 'claude', 'OW Provider 1', '{}', '{}')",
                [],
            )?;
            conn.execute(
                "INSERT INTO providers (id, app_type, name, settings_config, meta)
                 VALUES ('p-ow2', 'claude', 'OW Provider 2', '{}', '{}')",
                [],
            )?;
        }
        db2.export_sql(&export_path)?;

        // 导入覆盖后的文件，验证行数为 2（第二次导出的内容）
        let dst = Database::memory()?;
        dst.import_sql(&export_path)?;
        let provider_count: i64 = {
            let conn = crate::database::lock_conn!(dst.conn);
            conn.query_row(
                "SELECT COUNT(*) FROM providers WHERE id IN ('p-ow1','p-ow2')",
                [],
                |r| r.get(0),
            )?
        };
        assert_eq!(
            provider_count, 2,
            "导出文件应被第二次导出原子覆盖，包含 2 行 provider"
        );

        Ok(())
    }

    /// dump 幂等性：同一 DB 两次导出字节相同（时间戳外的结构体积稳定）
    /// 严格验证：输出以 CC_SWITCH_SQL_EXPORT_HEADER 开头，且通过 validate 函数
    #[test]
    fn export_sql_string_starts_with_header_and_validates() -> Result<(), AppError> {
        let db = Database::memory()?;
        {
            let conn = crate::database::lock_conn!(db.conn);
            conn.execute(
                "INSERT INTO providers (id, app_type, name, settings_config, meta)
                 VALUES ('p-hdr', 'claude', 'Header Provider', '{}', '{}')",
                [],
            )?;
        }

        let dump = db.export_sql_string()?;
        assert!(
            dump.trim_start().starts_with(CC_SWITCH_SQL_EXPORT_HEADER),
            "dump should start with header, got: {}",
            &dump[..dump.len().min(80)]
        );

        // validate_cc_switch_sql_export 非 pub(crate)，通过 import_sql_string 间接验证；
        // import_sql_string 返回 Ok 即可证明头部校验通过。
        let dst = Database::memory()?;
        dst.import_sql_string(&dump)?;

        Ok(())
    }

    /// skip_tables 生效验证：SYNC_SKIP_TABLES 中的数据不出现在 sync dump 中，
    /// 但在完整 dump 中存在
    #[test]
    fn skip_tables_honored_in_sync_dump() -> Result<(), AppError> {
        let db = Database::memory()?;
        {
            let conn = crate::database::lock_conn!(db.conn);
            // sync_skip_tables のうち proxy_request_logs にデータを入れる
            conn.execute(
                "INSERT INTO providers (id, app_type, name, settings_config, meta)
                 VALUES ('p-skip', 'claude', 'Skip Provider', '{}', '{}')",
                [],
            )?;
            conn.execute(
                "INSERT INTO proxy_request_logs (
                    request_id, provider_id, app_type, model,
                    input_tokens, output_tokens, total_cost_usd,
                    latency_ms, status_code, created_at
                ) VALUES ('req-skip', 'p-skip', 'claude', 'claude-3', 10, 5, '0.001', 50, 200, 1000)",
                [],
            )?;
        }

        // sync dump should NOT contain the log row
        let sync_dump = db.export_sql_string_for_sync()?;
        assert!(
            !sync_dump.contains("req-skip"),
            "sync dump must not contain proxy_request_logs rows"
        );

        // full dump SHOULD contain the log row
        let full_dump = db.export_sql_string()?;
        assert!(
            full_dump.contains("req-skip"),
            "full dump must contain proxy_request_logs rows"
        );

        // 验证 SYNC_SKIP_TABLES 全部不在 sync dump 数据中
        // （表定义仍然会出现，只是行数据不出现）
        // 我们用 import 后验证行数来确认
        let dst = Database::memory()?;
        dst.import_sql_string(&sync_dump)?;
        let log_count: i64 = {
            let conn = crate::database::lock_conn!(dst.conn);
            conn.query_row("SELECT COUNT(*) FROM proxy_request_logs", [], |r| r.get(0))?
        };
        assert_eq!(log_count, 0, "imported sync dump should have 0 log rows");

        Ok(())
    }

    /// 相同数据 dump 两次，结构相同（时间戳不同但行内容完全一致）。
    /// 通过去掉生成时间行后比较其余内容来验证 dump 确定性。
    #[test]
    fn dump_determinism_excluding_timestamp() -> Result<(), AppError> {
        let db = Database::memory()?;
        {
            let conn = crate::database::lock_conn!(db.conn);
            conn.execute(
                "INSERT INTO providers (id, app_type, name, settings_config, meta)
                 VALUES ('p-det', 'claude', 'Det Provider', '{}', '{}')",
                [],
            )?;
        }

        let strip_ts = |s: &str| -> String {
            s.lines()
                .filter(|l| !l.starts_with("-- 生成时间:"))
                .collect::<Vec<_>>()
                .join("\n")
        };

        let dump1 = strip_ts(&db.export_sql_string()?);
        let dump2 = strip_ts(&db.export_sql_string()?);
        assert_eq!(
            dump1, dump2,
            "dumps of same DB should be identical (modulo timestamp)"
        );

        Ok(())
    }

    #[test]
    fn sync_import_preserves_local_only_tables() -> Result<(), AppError> {
        let remote_db = Database::memory()?;
        {
            let conn = crate::database::lock_conn!(remote_db.conn);
            conn.execute(
                "INSERT INTO providers (id, app_type, name, settings_config, meta)
                 VALUES ('remote-provider', 'claude', 'Remote Provider', '{}', '{}')",
                [],
            )?;
        }
        let remote_sql = remote_db.export_sql_string_for_sync()?;

        let local_db = Database::memory()?;
        {
            let conn = crate::database::lock_conn!(local_db.conn);
            conn.execute(
                "INSERT INTO providers (id, app_type, name, settings_config, meta)
                 VALUES ('local-provider', 'claude', 'Local Provider', '{}', '{}')",
                [],
            )?;
            conn.execute(
                "INSERT INTO proxy_request_logs (
                    request_id, provider_id, app_type, model,
                    input_tokens, output_tokens, total_cost_usd,
                    latency_ms, status_code, created_at
                ) VALUES ('req-1', 'local-provider', 'claude', 'claude-3', 100, 50, '0.01', 120, 200, 1000)",
                [],
            )?;
            conn.execute(
                "INSERT INTO usage_daily_rollups (
                    date, app_type, provider_id, model, request_count, success_count,
                    input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens,
                    total_cost_usd, avg_latency_ms
                ) VALUES ('2026-03-01', 'claude', 'local-provider', 'claude-3', 7, 7, 700, 350, 0, 0, '0.07', 120)",
                [],
            )?;
            conn.execute(
                "INSERT INTO stream_check_logs (
                    provider_id, provider_name, app_type, status, success, message,
                    response_time_ms, http_status, model_used, retry_count, tested_at
                ) VALUES ('local-provider', 'Local Provider', 'claude', 'operational', 1, 'ok', 42, 200, 'claude-3', 0, 1000)",
                [],
            )?;
        }

        local_db.import_sql_string_for_sync(&remote_sql)?;

        let remote_provider_exists: i64 = {
            let conn = crate::database::lock_conn!(local_db.conn);
            conn.query_row(
                "SELECT COUNT(*) FROM providers WHERE id = 'remote-provider' AND app_type = 'claude'",
                [],
                |row| row.get(0),
            )?
        };
        assert_eq!(
            remote_provider_exists, 1,
            "remote config should be imported"
        );

        let (request_logs, rollups, stream_logs): (i64, i64, i64) = {
            let conn = crate::database::lock_conn!(local_db.conn);
            let request_logs =
                conn.query_row("SELECT COUNT(*) FROM proxy_request_logs", [], |row| {
                    row.get(0)
                })?;
            let rollups =
                conn.query_row("SELECT COUNT(*) FROM usage_daily_rollups", [], |row| {
                    row.get(0)
                })?;
            let stream_logs =
                conn.query_row("SELECT COUNT(*) FROM stream_check_logs", [], |row| {
                    row.get(0)
                })?;
            (request_logs, rollups, stream_logs)
        };
        assert_eq!(request_logs, 1, "local request logs should be preserved");
        assert_eq!(rollups, 1, "local rollups should be preserved");
        assert_eq!(
            stream_logs, 1,
            "local stream check logs should be preserved"
        );

        Ok(())
    }

    #[test]
    #[serial]
    fn periodic_maintenance_runs_even_when_auto_backup_disabled() -> Result<(), AppError> {
        let old_test_home = std::env::var_os("CC_SWITCH_TEST_HOME");
        let test_home =
            std::env::temp_dir().join("cc-switch-periodic-maintenance-backup-disabled-test");
        let _ = std::fs::remove_dir_all(&test_home);
        std::fs::create_dir_all(&test_home).expect("create test home");
        std::env::set_var("CC_SWITCH_TEST_HOME", &test_home);

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

        match old_test_home {
            Some(value) => std::env::set_var("CC_SWITCH_TEST_HOME", value),
            None => std::env::remove_var("CC_SWITCH_TEST_HOME"),
        }

        Ok(())
    }

    // ── F1-B: 流式 sync 导出测试 ───────────────────────────────────────────

    /// export_sql_to_writer_for_sync 的字节输出与 export_sql_string_for_sync 完全相同
    /// （时间戳行除外），证明 wire format 兼容性不变。
    #[test]
    fn export_sql_to_writer_for_sync_matches_string_variant() -> Result<(), AppError> {
        let db = Database::memory()?;
        {
            let conn = crate::database::lock_conn!(db.conn);
            conn.execute(
                "INSERT INTO providers (id, app_type, name, settings_config, meta)
                 VALUES ('p-f1b', 'claude', 'F1B Provider', '{}', '{}')",
                [],
            )?;
        }

        // string variant (old path)
        let string_dump = db.export_sql_string_for_sync()?;

        // writer variant (new streaming path)
        let mut buf: Vec<u8> = Vec::new();
        db.export_sql_to_writer_for_sync(&mut buf)?;
        let writer_dump = String::from_utf8(buf).expect("writer dump should be valid UTF-8");

        // Strip timestamp line before comparing — timestamps differ between calls.
        let strip_ts = |s: &str| -> String {
            s.lines()
                .filter(|l| !l.starts_with("-- 生成时间:"))
                .collect::<Vec<_>>()
                .join("\n")
        };

        assert_eq!(
            strip_ts(&string_dump),
            strip_ts(&writer_dump),
            "streaming writer variant must produce identical content to string variant"
        );
        Ok(())
    }

    /// export_sql_to_writer_for_sync skips SYNC_SKIP_TABLES 数据行 but writes their schema.
    #[test]
    fn export_sql_to_writer_for_sync_skips_skip_tables() -> Result<(), AppError> {
        let db = Database::memory()?;
        {
            let conn = crate::database::lock_conn!(db.conn);
            conn.execute(
                "INSERT INTO providers (id, app_type, name, settings_config, meta)
                 VALUES ('p-f1b2', 'claude', 'F1B2 Provider', '{}', '{}')",
                [],
            )?;
            conn.execute(
                "INSERT INTO proxy_request_logs (
                    request_id, provider_id, app_type, model,
                    input_tokens, output_tokens, total_cost_usd,
                    latency_ms, status_code, created_at
                ) VALUES ('req-f1b', 'p-f1b2', 'claude', 'claude-3', 1, 1, '0.0', 10, 200, 1)",
                [],
            )?;
        }

        let mut buf: Vec<u8> = Vec::new();
        db.export_sql_to_writer_for_sync(&mut buf)?;
        let dump = String::from_utf8(buf).expect("valid UTF-8");

        assert!(
            !dump.contains("req-f1b"),
            "streaming sync dump must not contain proxy_request_logs rows"
        );
        assert!(
            dump.starts_with(CC_SWITCH_SQL_EXPORT_HEADER),
            "streaming sync dump must start with CC Switch export header"
        );

        // Confirm the output is importable
        let dst = Database::memory()?;
        dst.import_sql_string(&dump)?;
        let count: i64 = {
            let conn = crate::database::lock_conn!(dst.conn);
            conn.query_row("SELECT COUNT(*) FROM proxy_request_logs", [], |r| r.get(0))?
        };
        assert_eq!(
            count, 0,
            "imported streaming sync dump should have 0 log rows"
        );

        Ok(())
    }
}
