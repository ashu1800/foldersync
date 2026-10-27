//! 设置持久化（JSON 文件，简单够用）。
//!
//! 用 JSON 而非 SQLite：设置量小、需要人眼可读、便于用户手改排障。

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};

use crate::dto::{SettingsDto, SettingsPatch};

/// 设置存储：内存副本 + 原子写盘。
pub struct SettingsStore {
    path: PathBuf,
    inner: Mutex<SettingsDto>,
}

impl SettingsStore {
    /// 从磁盘加载（文件不存在则用默认值）。
    pub fn load(path: &Path) -> Result<Self> {
        let dto = match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                tracing::warn!("设置文件损坏，改用默认值: {e}");
                SettingsDto::default()
            }),
            Err(_) => SettingsDto::default(),
        };
        Ok(Self {
            path: path.to_path_buf(),
            inner: Mutex::new(dto),
        })
    }

    /// 当前设置快照。
    pub fn get(&self) -> SettingsDto {
        self.inner.lock().unwrap().clone()
    }

    /// 应用补丁并立即落盘。
    pub fn update(&self, patch: SettingsPatch) -> Result<SettingsDto> {
        let mut guard = self.inner.lock().unwrap();
        apply_patch(&mut guard, patch);
        let dto = guard.clone();
        drop(guard);
        self.persist(&dto)?;
        Ok(dto)
    }

    /// 原子写盘（先写临时文件再 rename，避免断电留半截文件）。
    fn persist(&self, dto: &SettingsDto) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let json = serde_json::to_string_pretty(dto)?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, json).context("写设置临时文件失败")?;
        std::fs::rename(&tmp, &self.path).context("替换设置文件失败")?;
        Ok(())
    }
}

/// 只覆盖补丁里出现的字段。
fn apply_patch(s: &mut SettingsDto, p: SettingsPatch) {
    if let Some(v) = p.device_name {
        s.device_name = v;
    }
    if let Some(v) = p.auto_start {
        s.auto_start = v;
    }
    if let Some(v) = p.close_to_tray {
        s.close_to_tray = v;
    }
    if let Some(v) = p.upload_limit_kbps {
        s.upload_limit_kbps = v;
    }
    if let Some(v) = p.download_limit_kbps {
        s.download_limit_kbps = v;
    }
    // 双层 Option：外层 None = 不改；Some(None) = 清空
    if let Some(v) = p.relay_url {
        s.relay_url = v;
    }
    if let Some(v) = p.network_mode {
        s.network_mode = v;
    }
    if let Some(v) = p.listen_port {
        s.listen_port = v;
    }
    if let Some(v) = p.default_ignore {
        s.default_ignore = v;
    }
    if let Some(v) = p.log_level {
        s.log_level = v;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patch_only_touches_provided_fields() {
        let mut s = SettingsDto {
            upload_limit_kbps: 500,
            device_name: "我的电脑".into(),
            ..Default::default()
        };

        apply_patch(
            &mut s,
            SettingsPatch {
                download_limit_kbps: Some(200),
                ..Default::default()
            },
        );

        assert_eq!(s.download_limit_kbps, 200);
        assert_eq!(s.upload_limit_kbps, 500, "未提供的字段不应被清空");
        assert_eq!(s.device_name, "我的电脑");
    }

    #[test]
    fn explicit_null_clears_optional_fields() {
        let mut s = SettingsDto {
            relay_url: Some("https://relay.example".into()),
            listen_port: Some(4433),
            ..Default::default()
        };

        apply_patch(
            &mut s,
            SettingsPatch {
                relay_url: Some(None),
                listen_port: Some(None),
                ..Default::default()
            },
        );

        assert_eq!(s.relay_url, None);
        assert_eq!(s.listen_port, None);
    }

    #[test]
    fn persists_and_reloads() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");

        let store = SettingsStore::load(&path).unwrap();
        store
            .update(SettingsPatch {
                device_name: Some("测试机".into()),
                upload_limit_kbps: Some(1024),
                ..Default::default()
            })
            .unwrap();

        let reloaded = SettingsStore::load(&path).unwrap();
        let s = reloaded.get();
        assert_eq!(s.device_name, "测试机");
        assert_eq!(s.upload_limit_kbps, 1024);
    }

    #[test]
    fn corrupted_file_falls_back_to_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        std::fs::write(&path, b"{ this is not json").unwrap();

        let store = SettingsStore::load(&path).unwrap();
        assert_eq!(store.get().log_level, "info", "损坏时用默认值而非崩溃");
    }
}
