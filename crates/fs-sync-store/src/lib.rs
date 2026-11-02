//! fs-sync-store：SQLite 持久化层（WAL 模式）。
//!
//! 表：devices（信任列表）、folders（同步文件夹）、entries（文件索引与墓碑）、
//! activity（活动日志）。所有操作走 `Store`（内部 `Mutex<Connection>`），线程安全。

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use fs_sync_core::FileEntry;
use rusqlite::Connection;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("数据库错误: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("JSON 编解码错误: {0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// 已配对设备。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Device {
    /// SSH 公钥 SHA256 指纹（设备 ID）
    pub id: String,
    pub name: String,
    /// 配对时间（Unix 秒）
    pub paired_at: i64,
    /// 对端 `authorized_keys` 公钥
    pub ssh_public_key: String,
    /// 对端 SSH 服务端主机公钥
    pub host_key: String,
    /// SSH 登录用户名（应用专用）
    pub username: String,
    /// Tailscale 地址（IP / MagicDNS）
    pub addresses: Vec<String>,
    /// SSH 服务端端口
    pub port: u16,
    /// 中继地址
    pub relay_url: Option<String>,
    /// 中继房间 ID
    pub relay_room: Option<String>,
    /// 中继房间口令
    pub relay_secret: Option<String>,
}

/// 同步文件夹。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Folder {
    pub id: String,
    pub name: String,
    pub local_path: PathBuf,
    /// 是否暂停同步
    pub paused: bool,
    /// 墓碑保留天数
    pub tombstone_days: i64,
    pub created_at: i64,
    /// 分享给哪些设备（空 = 所有已配对设备）
    pub shared_with: Vec<String>,
}

/// 活动日志条目。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Activity {
    pub id: i64,
    /// unix 毫秒
    pub at: i64,
    /// upload / download / delete / conflict / error
    pub kind: String,
    pub folder_id: String,
    pub path: String,
    /// 文件字节数（删除/信息类为 0）
    pub size: i64,
    pub detail: String,
}

/// 待用户确认的文件夹共享邀请。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PendingFolderInvite {
    pub folder_id: String,
    pub folder_name: String,
    /// 邀请方设备名（展示用）
    pub from_device: String,
    /// 收到时间（Unix 秒）
    pub received_at: i64,
}

/// SQLite 存储。Clone 便宜（内部 Arc）。
#[derive(Clone)]
pub struct Store {
    conn: std::sync::Arc<Mutex<Connection>>,
}

impl Store {
    /// 打开（或创建）数据库并跑迁移。
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        // pair 与 serve 是两个独立进程共享同一库文件；并发写时默认 0 等待
        // 会直接报 SQLITE_BUSY。给 5 秒让锁等待重试。
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Self::migrate(&conn)?;
        Ok(Self {
            conn: std::sync::Arc::new(Mutex::new(conn)),
        })
    }

    /// 内存库（测试用）。
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::migrate(&conn)?;
        Ok(Self {
            conn: std::sync::Arc::new(Mutex::new(conn)),
        })
    }

    fn migrate(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS devices (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                paired_at INTEGER NOT NULL,
                ssh_public_key TEXT NOT NULL DEFAULT '',
                host_key TEXT NOT NULL DEFAULT '',
                username TEXT NOT NULL DEFAULT 'foldersync',
                addresses TEXT NOT NULL DEFAULT '[]',
                port INTEGER NOT NULL DEFAULT 2222,
                relay_url TEXT,
                relay_room TEXT,
                relay_secret TEXT
            );
            CREATE TABLE IF NOT EXISTS folders (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                local_path TEXT NOT NULL,
                paused INTEGER NOT NULL DEFAULT 0,
                tombstone_days INTEGER NOT NULL DEFAULT 30,
                created_at INTEGER NOT NULL,
                shared_with TEXT NOT NULL DEFAULT '[]'
            );
            CREATE TABLE IF NOT EXISTS entries (
                folder_id TEXT NOT NULL,
                path TEXT NOT NULL,
                size INTEGER NOT NULL,
                mtime INTEGER NOT NULL,
                hash TEXT NOT NULL,
                is_dir INTEGER NOT NULL,
                deleted INTEGER NOT NULL,
                version_json TEXT NOT NULL,
                modified_by TEXT NOT NULL,
                deleted_at INTEGER NOT NULL DEFAULT 0,
                mode INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (folder_id, path)
            );
            CREATE INDEX IF NOT EXISTS idx_entries_folder ON entries(folder_id);
            CREATE TABLE IF NOT EXISTS activity (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                at INTEGER NOT NULL,
                kind TEXT NOT NULL,
                folder_id TEXT NOT NULL,
                path TEXT NOT NULL,
                size INTEGER NOT NULL DEFAULT 0,
                detail TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS pairing_tokens (
                token TEXT PRIMARY KEY,
                expires_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS declined_folder_invites (
                folder_id TEXT PRIMARY KEY,
                declined_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS pending_folder_invites (
                folder_id TEXT PRIMARY KEY,
                folder_name TEXT NOT NULL,
                from_device TEXT NOT NULL,
                received_at INTEGER NOT NULL
            );
            "#,
        )?;
        // 老版本（iroh）数据库升级：补 SSH 相关列。SQLite 没有 ADD COLUMN IF NOT EXISTS，
        // 先查 PRAGMA 再逐个补，失败只可能是已存在。
        for (name, ddl) in [
            ("ssh_public_key", "TEXT NOT NULL DEFAULT ''"),
            ("host_key", "TEXT NOT NULL DEFAULT ''"),
            ("username", "TEXT NOT NULL DEFAULT 'foldersync'"),
            ("addresses", "TEXT NOT NULL DEFAULT '[]'"),
            ("port", "INTEGER NOT NULL DEFAULT 2222"),
            ("relay_url", "TEXT"),
            ("relay_room", "TEXT"),
            ("relay_secret", "TEXT"),
        ] {
            ensure_column(conn, "devices", name, ddl)?;
        }
        // 活动条目补文件大小列（旧库升级）
        ensure_column(conn, "activity", "size", "INTEGER NOT NULL DEFAULT 0")?;
        // 文件夹补共享设备列表（旧库升级，空表示所有设备）
        ensure_column(conn, "folders", "shared_with", "TEXT NOT NULL DEFAULT '[]'")?;
        // 墓碑记录删除时间（旧库升级）：旧墓碑按 0 处理，由首次 upsert 刷新
        ensure_column(conn, "entries", "deleted_at", "INTEGER NOT NULL DEFAULT 0")?;
        ensure_column(conn, "entries", "mode", "INTEGER NOT NULL DEFAULT 0")?;
        Ok(())
    }

    // ---- devices ----

    pub fn upsert_device(&self, d: &Device) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO devices
                (id, name, paired_at, ssh_public_key, host_key, username, addresses, port, relay_url, relay_room, relay_secret)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(id) DO UPDATE SET
                name = ?2, paired_at = ?3, ssh_public_key = ?4, host_key = ?5,
                username = ?6, addresses = ?7, port = ?8,
                relay_url = ?9, relay_room = ?10, relay_secret = ?11",
            rusqlite::params![
                d.id,
                d.name,
                d.paired_at,
                d.ssh_public_key,
                d.host_key,
                d.username,
                serde_json::to_string(&d.addresses)?,
                d.port,
                d.relay_url,
                d.relay_room,
                d.relay_secret
            ],
        )?;
        Ok(())
    }

    pub fn list_devices(&self) -> Result<Vec<Device>> {
        let conn = self.lock();
        let mut st = conn.prepare(
            "SELECT id, name, paired_at, ssh_public_key, host_key, username, addresses, port,
                    relay_url, relay_room, relay_secret
             FROM devices ORDER BY name",
        )?;
        let rows = st
            .query_map([], |r| {
                let addresses_json: Option<String> = r.get(6)?;
                Ok(Device {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    paired_at: r.get(2)?,
                    // 旧 iroh 设备行这些列是 NULL：给安全默认值，让 UI 能列出并删除
                    ssh_public_key: r.get::<_, Option<String>>(3)?.unwrap_or_default(),
                    host_key: r.get::<_, Option<String>>(4)?.unwrap_or_default(),
                    username: r
                        .get::<_, Option<String>>(5)?
                        .unwrap_or_else(|| "foldersync".into()),
                    addresses: addresses_json
                        .and_then(|j| serde_json::from_str(&j).ok())
                        .unwrap_or_default(),
                    port: r
                        .get::<_, Option<i64>>(7)?
                        .map(|p| p as u16)
                        .unwrap_or(2222),
                    relay_url: r.get(8)?,
                    relay_room: r.get(9)?,
                    relay_secret: r.get(10)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn remove_device(&self, id: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute("DELETE FROM devices WHERE id = ?1", [id])?;
        Ok(())
    }

    // ---- folders ----

    pub fn add_folder(&self, f: &Folder) -> Result<()> {
        self.add_folder_inner(f, true)
    }

    /// 接受对端邀请建文件夹：不能在并发删除后清掉用户刚写入的 decline 标记。
    pub fn add_folder_from_invite(&self, f: &Folder) -> Result<()> {
        self.add_folder_inner(f, false)
    }

    fn add_folder_inner(&self, f: &Folder, clear_declined: bool) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO folders (id, name, local_path, paused, tombstone_days, created_at, shared_with)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                f.id,
                f.name,
                f.local_path.to_string_lossy(),
                f.paused as i64,
                f.tombstone_days,
                f.created_at,
                serde_json::to_string(&f.shared_with)?
            ],
        )?;
        if clear_declined {
            // 用户主动重新添加同名文件夹时，撤销之前的邀请拒绝标记。
            conn.execute(
                "DELETE FROM declined_folder_invites WHERE folder_id = ?1",
                [&f.id],
            )?;
        }
        Ok(())
    }

    pub fn list_folders(&self) -> Result<Vec<Folder>> {
        let conn = self.lock();
        let mut st = conn.prepare(
            "SELECT id, name, local_path, paused, tombstone_days, created_at, shared_with
             FROM folders ORDER BY created_at",
        )?;
        let rows = st
            .query_map([], |r| {
                let shared: Option<String> = r.get(6)?;
                Ok(Folder {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    local_path: PathBuf::from(r.get::<_, String>(2)?),
                    paused: r.get::<_, i64>(3)? != 0,
                    tombstone_days: r.get(4)?,
                    created_at: r.get(5)?,
                    shared_with: match shared {
                        Some(s) => serde_json::from_str(&s).map_err(|e| {
                            rusqlite::Error::FromSqlConversionFailure(
                                6,
                                rusqlite::types::Type::Text,
                                Box::new(e),
                            )
                        })?,
                        // NULL = 旧数据，等价于“所有已配对设备”
                        None => Vec::new(),
                    },
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn set_folder_paused(&self, id: &str, paused: bool) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE folders SET paused = ?2 WHERE id = ?1",
            rusqlite::params![id, paused as i64],
        )?;
        Ok(())
    }

    /// 更新文件夹本地路径（外置盘换盘符后重新定位）。
    pub fn set_folder_path(&self, id: &str, path: &std::path::Path) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE folders SET local_path = ?2 WHERE id = ?1",
            rusqlite::params![id, path.to_string_lossy()],
        )?;
        Ok(())
    }

    /// 更新文件夹的共享设备列表（空 = 所有已配对设备）。
    pub fn set_folder_shared_with(&self, id: &str, device_ids: &[String]) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE folders SET shared_with = ?2 WHERE id = ?1",
            rusqlite::params![id, serde_json::to_string(device_ids)?],
        )?;
        Ok(())
    }

    pub fn remove_folder(&self, id: &str) -> Result<()> {
        let conn = self.lock();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        conn.execute(
            "INSERT OR REPLACE INTO declined_folder_invites (folder_id, declined_at) VALUES (?1, ?2)",
            rusqlite::params![id, now],
        )?;
        conn.execute("DELETE FROM entries WHERE folder_id = ?1", [id])?;
        conn.execute("DELETE FROM folders WHERE id = ?1", [id])?;
        Ok(())
    }

    /// 事务回滚/竞态失败时删除邀请记录，但不写“用户已拒绝”标记。
    pub fn discard_folder_record(&self, id: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute("DELETE FROM entries WHERE folder_id = ?1", [id])?;
        conn.execute("DELETE FROM folders WHERE id = ?1", [id])?;
        Ok(())
    }

    /// 记录"用户已拒绝该文件夹邀请"标记（不删任何数据）。
    pub fn upsert_declined_folder_invite(&self, id: &str) -> Result<()> {
        let conn = self.lock();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        conn.execute(
            "INSERT OR REPLACE INTO declined_folder_invites (folder_id, declined_at) VALUES (?1, ?2)",
            rusqlite::params![id, now],
        )?;
        Ok(())
    }

    /// 已在本机明确移除的文件夹，不再接受同 id 的自动邀请。
    pub fn is_folder_invite_declined(&self, id: &str) -> Result<bool> {
        let conn = self.lock();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM declined_folder_invites WHERE folder_id = ?1",
            [id],
            |r| r.get(0),
        )?;
        Ok(count > 0)
    }

    pub fn clear_folder_invite_declined(&self, id: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "DELETE FROM declined_folder_invites WHERE folder_id = ?1",
            [id],
        )?;
        Ok(())
    }

    // ---- pending_folder_invites（待用户确认的共享邀请）----

    /// 记录一条待确认邀请（同 id 覆盖刷新时间）。
    pub fn upsert_pending_folder_invite(&self, inv: &PendingFolderInvite) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO pending_folder_invites (folder_id, folder_name, from_device, received_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(folder_id) DO UPDATE SET
                folder_name = ?2, from_device = ?3, received_at = ?4",
            rusqlite::params![
                inv.folder_id,
                inv.folder_name,
                inv.from_device,
                inv.received_at
            ],
        )?;
        Ok(())
    }

    /// 全部待确认邀请（按时间正序）。
    pub fn list_pending_folder_invites(&self) -> Result<Vec<PendingFolderInvite>> {
        let conn = self.lock();
        let mut st = conn.prepare(
            "SELECT folder_id, folder_name, from_device, received_at
             FROM pending_folder_invites ORDER BY received_at",
        )?;
        let rows = st
            .query_map([], |r| {
                Ok(PendingFolderInvite {
                    folder_id: r.get(0)?,
                    folder_name: r.get(1)?,
                    from_device: r.get(2)?,
                    received_at: r.get(3)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 取一条；不存在返回 None（已被处理/已清）。
    pub fn get_pending_folder_invite(&self, id: &str) -> Result<Option<PendingFolderInvite>> {
        let conn = self.lock();
        let mut st = conn.prepare(
            "SELECT folder_id, folder_name, from_device, received_at
             FROM pending_folder_invites WHERE folder_id = ?1",
        )?;
        let mut rows = st.query_map([id], |r| {
            Ok(PendingFolderInvite {
                folder_id: r.get(0)?,
                folder_name: r.get(1)?,
                from_device: r.get(2)?,
                received_at: r.get(3)?,
            })
        })?;
        match rows.next() {
            Some(row) => Ok(Some(row?)),
            None => Ok(None),
        }
    }

    /// 处理完成（接受或拒绝）后删除。
    pub fn remove_pending_folder_invite(&self, id: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "DELETE FROM pending_folder_invites WHERE folder_id = ?1",
            [id],
        )?;
        Ok(())
    }

    // ---- entries（文件索引 + 墓碑）----

    /// 全量替换某文件夹的索引（初始扫描 / 兜底扫描后调用）。
    pub fn replace_entries(&self, folder_id: &str, entries: &[FileEntry]) -> Result<()> {
        let conn = self.lock();
        let tx = conn.unchecked_transaction()?;
        tx.execute("DELETE FROM entries WHERE folder_id = ?1", [folder_id])?;
        for e in entries {
            insert_entry(&tx, folder_id, e)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// 更新单条（upsert）。
    pub fn upsert_entry(&self, folder_id: &str, e: &FileEntry) -> Result<()> {
        let conn = self.lock();
        insert_entry(&conn, folder_id, e)?;
        Ok(())
    }

    /// 删除单条索引记录（墓碑过期清理用）。
    pub fn delete_entry(&self, folder_id: &str, path: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "DELETE FROM entries WHERE folder_id = ?1 AND path = ?2",
            rusqlite::params![folder_id, path],
        )?;
        Ok(())
    }

    /// 过期的墓碑路径（deleted_at 早于 cutoff）。deleted_at = 0 的是旧版数据，
    /// 保守起见不过期，等下一次墓碑刷新补上时间戳。
    pub fn list_expired_tombstones(&self, folder_id: &str, cutoff: i64) -> Result<Vec<String>> {
        let conn = self.lock();
        let mut st = conn.prepare(
            "SELECT path FROM entries
             WHERE folder_id = ?1 AND deleted = 1 AND deleted_at > 0 AND deleted_at < ?2",
        )?;
        let rows = st
            .query_map([folder_id, &cutoff.to_string()], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 只统计存活条目，避免概览页为拿文件数反序列化整棵索引。
    pub fn count_live_entries(&self, folder_id: &str) -> Result<u32> {
        let conn = self.lock();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM entries WHERE folder_id = ?1 AND deleted = 0",
            [folder_id],
            |r| r.get(0),
        )?;
        Ok(count.max(0) as u32)
    }

    pub fn list_entries(&self, folder_id: &str) -> Result<Vec<FileEntry>> {
        let conn = self.lock();
        let mut st = conn.prepare(
            "SELECT path, size, mtime, hash, is_dir, deleted, version_json, modified_by, deleted_at, mode
             FROM entries WHERE folder_id = ?1 ORDER BY path",
        )?;
        let rows = st
            .query_map([folder_id], row_to_entry)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 列出某前缀下的**直接子项**（文件树浏览用）。
    ///
    /// 之前 get_folder_tree 每次点击都 list_entries 全表载入内存再过滤，
    /// 几万文件 = 每点一个目录全表扫。SQL 条件把过滤下推给 SQLite 的
    /// 主键索引（folder_id, path），只取需要的那一层。
    pub fn list_entries_under(&self, folder_id: &str, prefix: &str) -> Result<Vec<FileEntry>> {
        let conn = self.lock();
        // 空前缀：顶层条目 = path 不含 '/'（INSTR 找不到分隔符返回 0）。
        // 非空前缀：`prefix/xxx`（LIKE 走索引前缀），且 tail（prefix 之后剩余）不含 '/'。
        // SUBSTR 是 1-based：path = "pre/xxx"，prefix = "pre" → tail 从 len+2 开始。
        if prefix.is_empty() {
            let sql = "SELECT path, size, mtime, hash, is_dir, deleted, version_json, modified_by, deleted_at, mode
                 FROM entries WHERE folder_id = ?1 AND deleted = 0 AND INSTR(path, '/') = 0
                 ORDER BY path";
            let mut st = conn.prepare(sql)?;
            let rows = st
                .query_map([folder_id], row_to_entry)?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            return Ok(rows);
        }
        let pattern = format!("{prefix}/%");
        let tail_offset = (prefix.len() + 2) as i64;
        let sql = "SELECT path, size, mtime, hash, is_dir, deleted, version_json, modified_by, deleted_at, mode
                 FROM entries
                 WHERE folder_id = ?1 AND deleted = 0
                   AND path LIKE ?2
                   AND INSTR(SUBSTR(path, ?3), '/') = 0
                 ORDER BY path";
        let mut st = conn.prepare(sql)?;
        let rows = st
            .query_map(
                rusqlite::params![folder_id, pattern, tail_offset],
                row_to_entry,
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    // ---- activity ----

    pub fn log_activity(
        &self,
        kind: &str,
        folder_id: &str,
        path: &str,
        detail: &str,
    ) -> Result<()> {
        self.log_activity_sized(kind, folder_id, path, 0, detail)
    }

    /// 记录活动，带文件字节数（列表页展示「大小」列）。
    pub fn log_activity_sized(
        &self,
        kind: &str,
        folder_id: &str,
        path: &str,
        size: i64,
        detail: &str,
    ) -> Result<()> {
        let conn = self.lock();
        let at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        conn.execute(
            "INSERT INTO activity (at, kind, folder_id, path, size, detail) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![at, kind, folder_id, path, size, detail],
        )?;
        Ok(())
    }

    /// 按时间倒序列出活动（limit 上限防止一次拉爆）。
    pub fn list_activity(&self, folder_id: Option<&str>, limit: u32) -> Result<Vec<Activity>> {
        self.list_activity_filtered(folder_id, None, limit, None)
    }

    /// 活动列表（文件夹 + 类型 + 游标过滤）。
    pub fn list_activity_filtered(
        &self,
        folder_id: Option<&str>,
        kind: Option<&str>,
        limit: u32,
        before_id: Option<i64>,
    ) -> Result<Vec<Activity>> {
        let conn = self.lock();
        let mut st = conn.prepare(
            "SELECT id, at, kind, folder_id, path, size, detail FROM activity
             WHERE (?1 IS NULL OR folder_id = ?1)
               AND (?2 IS NULL OR kind = ?2)
               AND (?3 IS NULL OR id < ?3)
             ORDER BY id DESC LIMIT ?4",
        )?;
        let rows = st
            .query_map(
                rusqlite::params![folder_id, kind, before_id, limit.clamp(1, 500)],
                |r| {
                    Ok(Activity {
                        id: r.get(0)?,
                        at: r.get(1)?,
                        kind: r.get(2)?,
                        folder_id: r.get(3)?,
                        path: r.get(4)?,
                        size: r.get(5)?,
                        detail: r.get(6)?,
                    })
                },
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 只保留最近 `max_rows` 条活动，防止数据库无限增长。
    pub fn prune_activity(&self, max_rows: i64) -> Result<usize> {
        let conn = self.lock();
        let deleted = conn.execute(
            "DELETE FROM activity WHERE id NOT IN (
                SELECT id FROM activity ORDER BY id DESC LIMIT ?1
             )",
            [max_rows.max(1)],
        )?;
        Ok(deleted)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().expect("store 锁中毒")
    }

    // ---- pairing tokens（跨进程共享，pair 与 serve 各是独立进程）----

    /// 记录一个待使用的配对 token。
    pub fn add_pairing_token(&self, token: &str, expires_at: i64) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT OR REPLACE INTO pairing_tokens (token, expires_at) VALUES (?1, ?2)",
            rusqlite::params![token, expires_at],
        )?;
        Ok(())
    }

    /// 一次性消费 token：存在且未过期则删除并返回 true。
    /// 只校验不消费（SSH 密码认证阶段用；真正消费在配对请求处理时）。
    pub fn validate_pairing_token(&self, token: &str, now: i64) -> Result<bool> {
        let conn = self.lock();
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pairing_tokens WHERE token = ?1 AND expires_at >= ?2",
            rusqlite::params![token, now],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    pub fn consume_pairing_token(&self, token: &str, now: i64) -> Result<bool> {
        let conn = self.lock();
        let n = conn.execute(
            "DELETE FROM pairing_tokens WHERE token = ?1 AND expires_at >= ?2",
            rusqlite::params![token, now],
        )?;
        // 顺带清理过期 token
        let _ = conn.execute(
            "DELETE FROM pairing_tokens WHERE expires_at < ?1",
            rusqlite::params![now],
        );
        Ok(n > 0)
    }

    /// 当前未过期的 token 数量（测试/诊断用）。
    pub fn count_pairing_tokens(&self, now: i64) -> Result<u32> {
        let conn = self.lock();
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pairing_tokens WHERE expires_at >= ?1",
            rusqlite::params![now],
            |r| r.get(0),
        )?;
        Ok(n as u32)
    }
}

fn ensure_column(conn: &Connection, table: &str, name: &str, ddl: &str) -> Result<()> {
    let mut st = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let exists = st
        .query_map([], |r| r.get::<_, String>(1))?
        .filter_map(|r| r.ok())
        .any(|c| c == name);
    drop(st);
    if !exists {
        conn.execute(&format!("ALTER TABLE {table} ADD COLUMN {name} {ddl}"), [])?;
    }
    Ok(())
}

fn insert_entry(conn: &Connection, folder_id: &str, e: &FileEntry) -> Result<()> {
    let version_json = serde_json::to_string(&e.version)?;
    // deleted_at：墓碑的删除时间（Unix 秒）。由 SQL 自动维护，调用方无需关心：
    // - 置为墓碑（活 → deleted）：记当前时间；
    // - 已是墓碑：保留原删除时间（版本递增的墓碑刷新不能把时间洗掉，否则 30 天保留期永远从最后一次广播算起）；
    // - 复活/普通条目：清 0。
    conn.execute(
        "INSERT INTO entries (folder_id, path, size, mtime, hash, is_dir, deleted, version_json, modified_by, deleted_at, mode)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9,
                 CASE WHEN ?7 = 1
                      THEN CASE WHEN EXISTS (SELECT 1 FROM entries WHERE folder_id = ?1 AND path = ?2 AND deleted = 1)
                                THEN (SELECT deleted_at FROM entries WHERE folder_id = ?1 AND path = ?2)
                                ELSE (SELECT strftime('%s','now'))
                           END
                      ELSE 0 END,
                 ?10)
         ON CONFLICT(folder_id, path) DO UPDATE SET
            size = ?3, mtime = ?4, hash = ?5, is_dir = ?6,
            deleted = ?7, version_json = ?8, modified_by = ?9, mode = ?10,
            deleted_at = CASE WHEN ?7 = 1
                              THEN CASE WHEN entries.deleted = 1 THEN entries.deleted_at
                                        ELSE (SELECT strftime('%s','now'))
                                   END
                              ELSE 0 END",
        rusqlite::params![
            folder_id,
            e.path,
            e.size,
            e.mtime,
            e.hash,
            e.is_dir as i64,
            e.deleted as i64,
            version_json,
            e.modified_by,
            e.mode
        ],
    )?;
    Ok(())
}

fn row_to_entry(r: &rusqlite::Row<'_>) -> std::result::Result<FileEntry, rusqlite::Error> {
    let version_json: String = r.get(6)?;
    let version: fs_sync_core::VersionVector = serde_json::from_str(&version_json)
        .map_err(|_| rusqlite::Error::InvalidColumnName("version_json".into()))?;
    Ok(FileEntry {
        path: r.get(0)?,
        size: r.get(1)?,
        mtime: r.get(2)?,
        hash: r.get(3)?,
        is_dir: r.get::<_, i64>(4)? != 0,
        deleted: r.get::<_, i64>(5)? != 0,
        version,
        modified_by: r.get(7)?,
        mode: r.get::<_, i64>(10).unwrap_or(0) as u32,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_entry(path: &str, dev: &str) -> FileEntry {
        let mut e = FileEntry::new(path, 42, 1000, "cafe".repeat(8), false, dev);
        e.version.increment(dev);
        e
    }

    fn sample_device(id: &str, name: &str) -> Device {
        Device {
            id: id.into(),
            name: name.into(),
            paired_at: 100,
            ssh_public_key: "ssh-ed25519 AAAA".into(),
            host_key: "ssh-ed25519 BBBB".into(),
            username: "foldersync".into(),
            addresses: vec!["100.64.0.2".into()],
            port: 2222,
            relay_url: Some("tcp://relay:9443".into()),
            relay_room: Some("room".into()),
            relay_secret: Some("secret".into()),
        }
    }

    #[test]
    fn device_crud() {
        let s = Store::open_in_memory().unwrap();
        s.upsert_device(&sample_device(&"a".repeat(64), "home-pc"))
            .unwrap();
        let devs = s.list_devices().unwrap();
        assert_eq!(devs.len(), 1);
        assert_eq!(devs[0].name, "home-pc");
        assert_eq!(devs[0].addresses, vec!["100.64.0.2".to_string()]);
        assert_eq!(devs[0].port, 2222);

        // 重命名 = upsert
        s.upsert_device(&sample_device(&"a".repeat(64), "renamed"))
            .unwrap();
        assert_eq!(s.list_devices().unwrap()[0].name, "renamed");

        s.remove_device(&"a".repeat(64)).unwrap();
        assert!(s.list_devices().unwrap().is_empty());
    }

    #[test]
    fn folder_crud_and_pause() {
        let s = Store::open_in_memory().unwrap();
        s.add_folder(&Folder {
            id: "f1".into(),
            name: "work".into(),
            local_path: PathBuf::from("E:\\work"),
            paused: false,
            tombstone_days: 30,
            created_at: 1,
            shared_with: vec!["dev-b".into()],
        })
        .unwrap();
        s.set_folder_paused("f1", true).unwrap();
        let fs = s.list_folders().unwrap();
        assert!(fs[0].paused);
        assert_eq!(fs[0].shared_with, vec!["dev-b".to_string()]);
        s.remove_folder("f1").unwrap();
        assert!(s.list_folders().unwrap().is_empty());
        assert!(s.is_folder_invite_declined("f1").unwrap());
    }

    #[test]
    fn pending_folder_invite_crud() {
        let s = Store::open_in_memory().unwrap();
        let inv = PendingFolderInvite {
            folder_id: "f9".into(),
            folder_name: "共享".into(),
            from_device: "ashu_work".into(),
            received_at: 100,
        };
        s.upsert_pending_folder_invite(&inv).unwrap();
        assert_eq!(s.list_pending_folder_invites().unwrap(), vec![inv.clone()]);
        assert_eq!(
            s.get_pending_folder_invite("f9").unwrap(),
            Some(inv.clone())
        );
        assert_eq!(s.get_pending_folder_invite("nope").unwrap(), None);

        // 同 id 覆盖：不产生重复行
        let mut upd = inv.clone();
        upd.received_at = 200;
        s.upsert_pending_folder_invite(&upd).unwrap();
        assert_eq!(s.list_pending_folder_invites().unwrap().len(), 1);
        assert_eq!(
            s.get_pending_folder_invite("f9")
                .unwrap()
                .unwrap()
                .received_at,
            200
        );

        s.remove_pending_folder_invite("f9").unwrap();
        assert!(s.list_pending_folder_invites().unwrap().is_empty());
        // 重复删除幂等
        s.remove_pending_folder_invite("f9").unwrap();
    }

    #[test]
    fn entries_roundtrip_with_version_vector() {
        let s = Store::open_in_memory().unwrap();
        let e1 = sample_entry("a.txt", "devA");
        let mut e2 = sample_entry("b/c.txt", "devB");
        e2.version.increment("devA"); // 多设备版本向量

        s.replace_entries("f1", &[e1.clone(), e2.clone()]).unwrap();
        let got = s.list_entries("f1").unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(s.count_live_entries("f1").unwrap(), 2);
        assert_eq!(got[0], e1);
        assert_eq!(got[1], e2);

        // 墓碑
        let mut t = sample_entry("old.txt", "devA");
        t.deleted = true;
        s.upsert_entry("f1", &t).unwrap();
        let got = s.list_entries("f1").unwrap();
        assert_eq!(got.len(), 3);
        assert_eq!(s.count_live_entries("f1").unwrap(), 2);
        assert!(got.iter().find(|e| e.path == "old.txt").unwrap().deleted);
    }

    #[test]
    fn replace_entries_clears_old() {
        let s = Store::open_in_memory().unwrap();
        s.replace_entries("f1", &[sample_entry("a.txt", "A")])
            .unwrap();
        s.replace_entries("f1", &[sample_entry("b.txt", "A")])
            .unwrap();
        let got = s.list_entries("f1").unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].path, "b.txt");
    }

    /// 文件树前缀查询：只取该层直接子项，不混入深层、不混入墓碑、不混入其他文件夹。
    #[test]
    fn list_entries_under_returns_direct_children_only() {
        let s = Store::open_in_memory().unwrap();
        s.replace_entries(
            "f1",
            &[
                sample_entry("top.txt", "A"),
                sample_entry("docs", "A"),
                sample_entry("docs/readme.md", "A"),
                sample_entry("docs/sub/deep.txt", "A"),
                sample_entry("docs2/other.txt", "A"),
            ],
        )
        .unwrap();
        // 墓碑不出现在树里
        let mut t = sample_entry("gone.txt", "A");
        t.deleted = true;
        s.upsert_entry("f1", &t).unwrap();
        // 其他文件夹不串
        s.replace_entries("f2", &[sample_entry("docs/alien.txt", "A")])
            .unwrap();

        let top = s.list_entries_under("f1", "").unwrap();
        let mut names: Vec<&str> = top.iter().map(|e| e.path.as_str()).collect();
        names.sort();
        assert_eq!(names, vec!["docs", "top.txt"], "顶层不应含 docs2/deep/gone");

        let docs = s.list_entries_under("f1", "docs").unwrap();
        let mut names: Vec<&str> = docs.iter().map(|e| e.path.as_str()).collect();
        names.sort();
        assert_eq!(
            names,
            vec!["docs/readme.md"],
            "docs 层只有直接子项，不含 sub/deep 与 docs2"
        );
    }

    /// 墓碑的 deleted_at 必须记录删除时间，且墓碑版本刷新（递增重广播）不能把它洗掉。
    /// 以前 GC 按文件 mtime 过期：删一个 31 天未动的文件，墓碑 10 分钟内被清，
    /// 对端离线回来后文件复活。现在 GC 按 deleted_at。
    #[test]
    fn tombstone_records_delete_time_and_survives_resurrection_updates() {
        let s = Store::open_in_memory().unwrap();

        // 新墓碑：deleted_at ≈ now
        let mut t = sample_entry("old.txt", "A");
        t.deleted = true;
        s.upsert_entry("f1", &t).unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let expired_fresh = s.list_expired_tombstones("f1", now - 60).unwrap();
        assert!(
            expired_fresh.is_empty(),
            "刚删除的墓碑 60 秒内不应过期: {expired_fresh:?}"
        );

        // 墓碑版本刷新（对端确认时递增重广播）：deleted_at 必须保留
        t.version.increment("A");
        s.upsert_entry("f1", &t).unwrap();
        let still_fresh = s.list_expired_tombstones("f1", now - 60).unwrap();
        assert!(still_fresh.is_empty(), "墓碑刷新不能把 deleted_at 洗掉");

        // 复活（同名新文件）：deleted_at 清 0，不再出现在过期列表
        let revived = sample_entry("old.txt", "A");
        s.upsert_entry("f1", &revived).unwrap();
        assert!(
            s.list_expired_tombstones("f1", now + 86400 * 365)
                .unwrap()
                .is_empty()
        );

        // 再次删除后，大 cutoff 应过期
        let mut t2 = sample_entry("old.txt", "A");
        t2.deleted = true;
        s.upsert_entry("f1", &t2).unwrap();
        let expired = s.list_expired_tombstones("f1", now + 86400 * 365).unwrap();
        assert_eq!(expired, vec!["old.txt".to_string()]);
    }

    /// 旧版库（无 deleted_at 列）升级后：旧墓碑 deleted_at=0，永不过期（保守），
    /// 待下一次墓碑刷新补上时间戳。
    #[test]
    fn legacy_tombstone_without_deleted_at_never_expires() {
        let s = Store::open_in_memory().unwrap();
        let mut t = sample_entry("legacy.txt", "A");
        t.deleted = true;
        s.upsert_entry("f1", &t).unwrap();
        // 直接把 deleted_at 改成 0，模拟旧版数据
        s.lock()
            .execute(
                "UPDATE entries SET deleted_at = 0 WHERE path = 'legacy.txt'",
                [],
            )
            .unwrap();
        let far_future = i64::MAX / 2;
        assert!(
            s.list_expired_tombstones("f1", far_future)
                .unwrap()
                .is_empty(),
            "deleted_at=0 的旧墓碑必须保守不过期"
        );
    }

    #[test]
    fn activity_log_and_filter() {
        let s = Store::open_in_memory().unwrap();
        s.log_activity_sized("upload", "f1", "a.txt", 1024, "上传完成")
            .unwrap();
        s.log_activity("download", "f2", "b.txt", "2048B").unwrap();
        assert_eq!(s.list_activity(None, 10).unwrap().len(), 2);
        let only_f1 = s.list_activity(Some("f1"), 10).unwrap();
        assert_eq!(only_f1.len(), 1);
        assert_eq!(only_f1[0].kind, "upload");
        assert_eq!(only_f1[0].size, 1024);
        // 倒序：最新在前
        assert_eq!(s.list_activity(None, 1).unwrap()[0].path, "b.txt");
        // 类型过滤
        let only_upload = s
            .list_activity_filtered(None, Some("upload"), 10, None)
            .unwrap();
        assert_eq!(only_upload.len(), 1);
        assert_eq!(only_upload[0].path, "a.txt");
        // 游标分页：取到第二新的条目
        let newest = s.list_activity(None, 1).unwrap()[0].id;
        let older = s
            .list_activity_filtered(None, None, 10, Some(newest))
            .unwrap();
        assert_eq!(older.len(), 1);
        assert_eq!(older[0].path, "a.txt");
        // pruning 只保留最近 N 条
        assert_eq!(s.prune_activity(1).unwrap(), 1);
        let left = s.list_activity(None, 10).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].path, "b.txt");
    }

    #[test]
    fn invalid_shared_with_fails_closed() {
        let s = Store::open_in_memory().unwrap();
        s.add_folder(&Folder {
            id: "f1".into(),
            name: "F".into(),
            local_path: std::path::PathBuf::from("C:/f1"),
            paused: false,
            tombstone_days: 30,
            created_at: 1,
            shared_with: vec!["devA".into()],
        })
        .unwrap();
        s.lock()
            .execute(
                "UPDATE folders SET shared_with = '{broken' WHERE id = 'f1'",
                [],
            )
            .unwrap();
        assert!(
            s.list_folders().is_err(),
            "损坏的共享列表必须报错，不能静默变成“全部设备”"
        );
    }

    #[test]
    fn pairing_tokens_are_single_use() {
        let s = Store::open_in_memory().unwrap();
        s.add_pairing_token("tok1", 1000).unwrap();
        assert_eq!(s.count_pairing_tokens(999).unwrap(), 1);

        // 有效期内首次消费成功
        assert!(s.consume_pairing_token("tok1", 999).unwrap());
        // 再次消费失败（一次性）
        assert!(!s.consume_pairing_token("tok1", 999).unwrap());
        assert_eq!(s.count_pairing_tokens(999).unwrap(), 0);
    }

    #[test]
    fn expired_pairing_token_rejected() {
        let s = Store::open_in_memory().unwrap();
        s.add_pairing_token("tok2", 1000).unwrap();
        // now > expires_at
        assert!(!s.consume_pairing_token("tok2", 1001).unwrap());
    }

    #[test]
    fn wal_mode_enabled() {
        let tmp = tempfile::tempdir().unwrap();
        let s = Store::open(&tmp.path().join("db.sqlite3")).unwrap();
        let mode: String = s
            .lock()
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal");
    }
}
