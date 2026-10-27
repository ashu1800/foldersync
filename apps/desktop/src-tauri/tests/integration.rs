//! 后端集成测试：验证命令层的核心逻辑（不启动 Tauri 窗口）。
//!
//! 这里不测 `#[tauri::command]` 包装（需要 AppHandle），而是测它们依赖的
//! `AppState` 与运行时组合是否真的能扫、能同步、能算状态。

use std::sync::Arc;

use fs_sync_store::Folder;

use foldersync_desktop_lib::dto::{OverallStatus, SettingsPatch};
use foldersync_desktop_lib::settings::SettingsStore;
use foldersync_desktop_lib::state::AppState;

/// 建一个隔离的 AppState（独立临时目录 + 独立端点）。
async fn state() -> (Arc<AppState>, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let st = AppState::init(tmp.path().to_path_buf()).await.unwrap();
    (st, tmp)
}

#[tokio::test]
async fn init_creates_identity_and_store() {
    let tmp = tempfile::tempdir().unwrap();
    let st = AppState::init(tmp.path().to_path_buf()).await.unwrap();

    assert!(tmp.path().join("secret.key").exists(), "应创建设备身份");
    assert!(tmp.path().join("state.sqlite3").exists(), "应创建数据库");
    assert!(!st.device_name.is_empty(), "应有默认设备名");
    assert!(!st.endpoint_id.to_string().is_empty());
}

#[tokio::test]
async fn start_folder_indexes_existing_files() {
    let (st, tmp) = state().await;
    let dir = tmp.path().join("shared");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.txt"), b"hello").unwrap();
    std::fs::write(dir.join("b.txt"), b"world").unwrap();

    let rt = st.start_folder("f-test".into(), dir.clone()).await.unwrap();

    let snap = rt.session.snapshot().await;
    assert_eq!(snap.len(), 2, "应索引两个文件");
    assert!(snap.contains_key("a.txt"));
    assert!(snap.contains_key("b.txt"));
}

#[tokio::test]
async fn devices_empty_on_fresh_state() {
    let (st, _t) = state().await;
    assert!(st.devices().await.is_empty(), "新设备应无已配对设备");
}

#[tokio::test]
async fn folders_reflect_store_and_runtime() {
    let (st, tmp) = state().await;
    let dir = tmp.path().join("docs");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("x.md"), b"# hi").unwrap();

    st.store
        .add_folder(&Folder {
            id: "f-docs".into(),
            name: "docs".into(),
            local_path: dir.clone(),
            paused: false,
            tombstone_days: 30,
            created_at: 0,
        })
        .unwrap();
    st.start_folder("f-docs".into(), dir).await.unwrap();

    let folders = st.folders().await;
    assert_eq!(folders.len(), 1);
    assert_eq!(folders[0].name, "docs");
    assert!(!folders[0].paused);
    assert_eq!(folders[0].file_count, 1, "应统计非删除条目");
}

#[tokio::test]
async fn paused_folder_reports_paused_status() {
    let (st, tmp) = state().await;
    let dir = tmp.path().join("p");
    std::fs::create_dir_all(&dir).unwrap();
    st.store
        .add_folder(&Folder {
            id: "f-p".into(),
            name: "p".into(),
            local_path: dir,
            paused: true,
            tombstone_days: 30,
            created_at: 0,
        })
        .unwrap();

    let folders = st.folders().await;
    assert_eq!(
        folders[0].status,
        foldersync_desktop_lib::dto::FolderStatus::Paused
    );
    assert!(folders[0].paused);
}

#[tokio::test]
async fn settings_roundtrip_through_app_state() {
    let (st, _t) = state().await;

    let next = st
        .update_settings(SettingsPatch {
            device_name: Some("笔记本".into()),
            upload_limit_kbps: Some(2048),
            ..Default::default()
        })
        .unwrap();

    assert_eq!(next.device_name, "笔记本");
    assert_eq!(next.upload_limit_kbps, 2048);
    assert_eq!(st.settings_dto().device_name, "笔记本");
}

#[tokio::test]
async fn stop_folder_removes_runtime() {
    let (st, tmp) = state().await;
    let dir = tmp.path().join("s");
    std::fs::create_dir_all(&dir).unwrap();
    st.start_folder("f-s".into(), dir).await.unwrap();
    assert_eq!(st.runtimes.lock().await.len(), 1);

    st.stop_folder("f-s").await;
    assert!(st.runtimes.lock().await.is_empty());
}

#[tokio::test]
async fn overall_status_synced_when_no_work() {
    let (st, _t) = state().await;
    // 没有文件夹、没有待同步 → 应为空但不算出错
    let folders = st.folders().await;
    let remaining: u32 = folders.iter().map(|f| f.pending).sum();
    assert_eq!(remaining, 0);

    // 模拟前端算出来的状态
    let status = if folders
        .iter()
        .any(|f| f.status == foldersync_desktop_lib::dto::FolderStatus::Error)
    {
        OverallStatus::Error
    } else if remaining > 0 {
        OverallStatus::Syncing
    } else {
        OverallStatus::Synced
    };
    assert_eq!(status, OverallStatus::Synced);
}

#[test]
fn settings_store_independent_of_app_state() {
    let tmp = tempfile::tempdir().unwrap();
    let s = SettingsStore::load(&tmp.path().join("s.json")).unwrap();
    assert_eq!(s.get().log_level, "info");
}
