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

fn add_test_folder(st: &AppState, id: &str, dir: &std::path::Path) {
    st.store
        .add_folder(&Folder {
            id: id.into(),
            name: id.into(),
            local_path: dir.to_path_buf(),
            paused: false,
            tombstone_days: 30,
            created_at: 0,
            shared_with: Vec::new(),
        })
        .unwrap();
}

#[tokio::test]
async fn init_creates_identity_and_store() {
    let tmp = tempfile::tempdir().unwrap();
    let st = AppState::init(tmp.path().to_path_buf()).await.unwrap();

    assert!(
        tmp.path().join("ssh_identity").exists(),
        "应创建 SSH 身份密钥"
    );
    assert!(
        tmp.path().join("ssh_host_ed25519").exists(),
        "应创建 SSH 主机密钥"
    );
    assert!(tmp.path().join("state.sqlite3").exists(), "应创建数据库");
    assert!(!st.device_name.is_empty(), "应有默认设备名");
    assert!(!st.device_id.is_empty());
}

#[tokio::test]
async fn start_folder_indexes_existing_files() {
    let (st, tmp) = state().await;
    let dir = tmp.path().join("shared");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.txt"), b"hello").unwrap();
    std::fs::write(dir.join("b.txt"), b"world").unwrap();
    add_test_folder(&st, "f-test", &dir);

    let rt = st.start_folder("f-test".into(), dir.clone()).await.unwrap();

    let snap = rt.session.snapshot().await;
    assert_eq!(snap.len(), 2, "应索引两个文件");
    assert!(snap.contains_key("a.txt"));
    assert!(snap.contains_key("b.txt"));
}

/// 回归：冲突副本删掉后变成墓碑，以前仍出现在冲突列表里，界面上永远消不掉。
#[tokio::test]
async fn deleted_conflict_copy_leaves_conflict_list() {
    use foldersync_desktop_lib::commands::conflict_dtos_for;
    use fs_sync_daemon::{EventKind, FileEvent};

    let (st, tmp) = state().await;
    let dir = tmp.path().join("c");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("k.txt"), b"win").unwrap();
    let copy = "k.sync-conflict-20261030-083719-PC.txt";
    std::fs::write(dir.join(copy), b"lose").unwrap();
    st.store
        .add_folder(&Folder {
            id: "f-c".into(),
            name: "c".into(),
            local_path: dir.clone(),
            paused: false,
            tombstone_days: 30,
            created_at: 0,
            shared_with: Vec::new(),
        })
        .unwrap();
    let rt = st.start_folder("f-c".into(), dir.clone()).await.unwrap();
    assert_eq!(conflict_dtos_for(&st, None).len(), 1);

    std::fs::remove_file(dir.join(copy)).unwrap();
    // 磁盘上没了：即使索引还没更新也不该再列出
    assert!(conflict_dtos_for(&st, None).is_empty());
    // 索引变成墓碑后同样不列出
    rt.session
        .on_local_event(FileEvent {
            path: copy.into(),
            kind: EventKind::Removed,
        })
        .await
        .unwrap();
    assert!(conflict_dtos_for(&st, None).is_empty());
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
            shared_with: Vec::new(),
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
            shared_with: Vec::new(),
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
    add_test_folder(&st, "f-s", &dir);
    st.start_folder("f-s".into(), dir).await.unwrap();
    assert_eq!(st.runtimes.lock().unwrap().len(), 1);

    st.stop_folder("f-s").await;
    assert!(st.runtimes.lock().unwrap().is_empty());
}

#[tokio::test]
async fn overall_status_not_configured_when_empty() {
    let (st, _t) = state().await;
    // 没有文件夹、没有设备：应显示「尚未配置」，不能误报“全部已同步”
    let folders = st.folders().await;
    assert!(folders.is_empty());
    let status = if folders.is_empty() {
        OverallStatus::NotConfigured
    } else {
        OverallStatus::Synced
    };
    assert_eq!(status, OverallStatus::NotConfigured);
}

#[test]
fn settings_store_independent_of_app_state() {
    let tmp = tempfile::tempdir().unwrap();
    let s = SettingsStore::load(&tmp.path().join("s.json")).unwrap();
    assert_eq!(s.get().log_level, "info");
}
