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
    pub detail: String,
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
                created_at INTEGER NOT NULL
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
                PRIMARY KEY (folder_id, path)
            );
            CREATE INDEX IF NOT EXISTS idx_entries_folder ON entries(folder_id);
            CREATE TABLE IF NOT EXISTS activity (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                at INTEGER NOT NULL,
                kind TEXT NOT NULL,
                folder_id TEXT NOT NULL,
                path TEXT NOT NULL,
                detail TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS pairing_tokens (
                token TEXT PRIMARY KEY,
                expires_at INTEGER NOT NULL
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
        let conn = self.lock();
        conn.execute(
            "INSERT INTO folders (id, name, local_path, paused, tombstone_days, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                f.id,
                f.name,
                f.local_path.to_string_lossy(),
                f.paused as i64,
                f.tombstone_days,
                f.created_at
            ],
        )?;
        Ok(())
    }

    pub fn list_folders(&self) -> Result<Vec<Folder>> {
        let conn = self.lock();
        let mut st = conn.prepare(
            "SELECT id, name, local_path, paused, tombstone_days, created_at
             FROM folders ORDER BY created_at",
        )?;
        let rows = st
            .query_map([], |r| {
                Ok(Folder {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    local_path: PathBuf::from(r.get::<_, String>(2)?),
                    paused: r.get::<_, i64>(3)? != 0,
                    tombstone_days: r.get(4)?,
                    created_at: r.get(5)?,
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

    pub fn remove_folder(&self, id: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute("DELETE FROM entries WHERE folder_id = ?1", [id])?;
        conn.execute("DELETE FROM folders WHERE id = ?1", [id])?;
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

    pub fn list_entries(&self, folder_id: &str) -> Result<Vec<FileEntry>> {
        let conn = self.lock();
        let mut st = conn.prepare(
            "SELECT path, size, mtime, hash, is_dir, deleted, version_json, modified_by
             FROM entries WHERE folder_id = ?1 ORDER BY path",
        )?;
        let rows = st
            .query_map([folder_id], row_to_entry)?
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
        let conn = self.lock();
        let at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        conn.execute(
            "INSERT INTO activity (at, kind, folder_id, path, detail) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![at, kind, folder_id, path, detail],
        )?;
        Ok(())
    }

    /// 按时间倒序列出活动（limit 上限防止一次拉爆）。
    pub fn list_activity(&self, folder_id: Option<&str>, limit: u32) -> Result<Vec<Activity>> {
        let conn = self.lock();
        let mut st = conn.prepare(
            "SELECT id, at, kind, folder_id, path, detail FROM activity
             WHERE (?1 IS NULL OR folder_id = ?1)
             ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = st
            .query_map(rusqlite::params![folder_id, limit], |r| {
                Ok(Activity {
                    id: r.get(0)?,
                    at: r.get(1)?,
                    kind: r.get(2)?,
                    folder_id: r.get(3)?,
                    path: r.get(4)?,
                    detail: r.get(5)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
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
    conn.execute(
        "INSERT INTO entries (folder_id, path, size, mtime, hash, is_dir, deleted, version_json, modified_by)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(folder_id, path) DO UPDATE SET
            size = ?3, mtime = ?4, hash = ?5, is_dir = ?6,
            deleted = ?7, version_json = ?8, modified_by = ?9",
        rusqlite::params![
            folder_id,
            e.path,
            e.size,
            e.mtime,
            e.hash,
            e.is_dir as i64,
            e.deleted as i64,
            version_json,
            e.modified_by
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
        })
        .unwrap();
        s.set_folder_paused("f1", true).unwrap();
        let fs = s.list_folders().unwrap();
        assert!(fs[0].paused);
        s.remove_folder("f1").unwrap();
        assert!(s.list_folders().unwrap().is_empty());
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
        assert_eq!(got[0], e1);
        assert_eq!(got[1], e2);

        // 墓碑
        let mut t = sample_entry("old.txt", "devA");
        t.deleted = true;
        s.upsert_entry("f1", &t).unwrap();
        let got = s.list_entries("f1").unwrap();
        assert_eq!(got.len(), 3);
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

    #[test]
    fn activity_log_and_filter() {
        let s = Store::open_in_memory().unwrap();
        s.log_activity("upload", "f1", "a.txt", "1024B").unwrap();
        s.log_activity("download", "f2", "b.txt", "2048B").unwrap();
        assert_eq!(s.list_activity(None, 10).unwrap().len(), 2);
        let only_f1 = s.list_activity(Some("f1"), 10).unwrap();
        assert_eq!(only_f1.len(), 1);
        assert_eq!(only_f1[0].kind, "upload");
        // 倒序：最新在前
        assert_eq!(s.list_activity(None, 1).unwrap()[0].path, "b.txt");
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
