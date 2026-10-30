//! 前后端共享 DTO。
//!
//! 这些类型同时用 `serde`（前端 JSON）与 `ts-rs`（生成 TypeScript 定义），
//! 避免两端手写类型漂移（提示词 5.3）。

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// 统一错误返回。
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export_to = "../../src/lib/bindings/")]
pub struct ErrorDto {
    /// 机器可读的错误码（前端据此做本地化）
    pub code: String,
    /// 人类可读的描述（已本地化）
    pub message: String,
}

impl ErrorDto {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            message: message.into(),
        }
    }
}

impl From<anyhow::Error> for ErrorDto {
    fn from(e: anyhow::Error) -> Self {
        Self::new("internal", format!("{e:#}"))
    }
}

impl From<fs_sync_store::StoreError> for ErrorDto {
    fn from(e: fs_sync_store::StoreError) -> Self {
        Self::new("store", e.to_string())
    }
}

pub type CmdResult<T> = Result<T, ErrorDto>;

/// 全局总状态（概览页顶部）。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, TS, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[ts(export_to = "../../src/lib/bindings/")]
pub enum OverallStatus {
    /// 全部已同步
    Synced,
    /// 正在同步
    Syncing,
    /// 有错误
    Error,
    /// 已暂停
    Paused,
}

/// 连接方式。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, TS, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[ts(export_to = "../../src/lib/bindings/")]
pub enum ConnKind {
    /// 经中继服务器
    Relay,
    /// 经 Tailscale 的 SSH/SFTP
    Tailscale,
    /// 未知/未连接
    Unknown,
}

/// 设备信息。
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export_to = "../../src/lib/bindings/")]
pub struct DeviceDto {
    /// SSH 公钥指纹（设备 ID）
    pub id: String,
    /// 设备显示名
    pub name: String,
    /// 公钥指纹（分组便于人工核对，如 `abcd-ef01-...`）
    pub fingerprint: String,
    /// 是否在线
    pub online: bool,
    /// 连接方式
    pub conn: ConnKind,
    /// 往返延迟（毫秒），未知为 None
    #[ts(type = "number | null")]
    pub rtt_ms: Option<u64>,
    /// Tailscale 地址（第一个）
    pub address: Option<String>,
    /// SSH 端口
    pub port: u16,
    /// 是否已配置中继房间
    pub relay_ready: bool,
}

/// 文件夹状态。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, TS, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[ts(export_to = "../../src/lib/bindings/")]
pub enum FolderStatus {
    /// 空闲（已同步）
    Idle,
    /// 扫描中
    Scanning,
    /// 同步中
    Syncing,
    /// 已暂停
    Paused,
    /// 出错
    Error,
}

/// 文件夹信息。
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export_to = "../../src/lib/bindings/")]
pub struct FolderDto {
    pub id: String,
    pub name: String,
    /// 本地路径
    pub path: String,
    pub status: FolderStatus,
    /// 是否暂停
    pub paused: bool,
    /// 最近同步时间（Unix 秒），从未同步为 None
    #[ts(type = "number | null")]
    pub last_sync_at: Option<i64>,
    /// 待处理文件数
    pub pending: u32,
    /// 已同步文件数
    pub file_count: u32,
    /// 分享给的设备 id 列表
    pub shared_with: Vec<String>,
}

/// 概览数据。
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export_to = "../../src/lib/bindings/")]
pub struct OverviewDto {
    pub status: OverallStatus,
    /// 本机设备名
    pub device_name: String,
    /// 本机 SSH 设备 ID
    pub device_id: String,
    /// 本机指纹
    pub fingerprint: String,
    /// Tailscale 状态描述（如 `ashu.tail7ebaff.ts.net (100.81.188.46)`），未启用为 None
    pub tailscale: Option<String>,
    /// 在线设备数
    pub online_devices: u32,
    /// 同步中的文件数（用于进度显示）
    pub syncing_files: u32,
    /// 剩余待同步文件数
    pub remaining_files: u32,
    /// 错误数
    pub error_count: u32,
}

/// 活动条目类型。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, TS, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[ts(export_to = "../../src/lib/bindings/")]
pub enum ActivityKind {
    Upload,
    Download,
    Delete,
    Conflict,
    Error,
    Info,
}

/// 活动条目。
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export_to = "../../src/lib/bindings/")]
pub struct ActivityDto {
    /// 自增 id（游标分页用）
    #[ts(type = "number")]
    pub id: i64,
    /// Unix 秒
    #[ts(type = "number")]
    pub at: i64,
    pub kind: ActivityKind,
    pub folder_id: String,
    /// 相对路径
    pub path: String,
    /// 文件大小（字节）
    #[ts(type = "number")]
    pub size: i64,
    /// 描述文本
    pub message: String,
}

/// 活动查询过滤。
#[derive(Debug, Clone, Default, Serialize, Deserialize, TS)]
#[ts(export_to = "../../src/lib/bindings/", optional_fields)]
pub struct ActivityFilter {
    #[ts(optional)]
    pub folder_id: Option<String>,
    #[ts(optional)]
    pub kind: Option<ActivityKind>,
    /// 返回条数上限
    #[ts(optional)]
    pub limit: Option<u32>,
    /// 游标：返回 id 小于此值的条目（向下翻页）
    #[ts(optional, type = "number | null")]
    pub before_id: Option<i64>,
}

/// 冲突文件。
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export_to = "../../src/lib/bindings/")]
pub struct ConflictDto {
    /// 冲突副本的相对路径
    pub id: String,
    pub folder_id: String,
    /// 原文件路径
    pub original_path: String,
    /// 冲突副本路径
    pub conflict_path: String,
    /// 副本大小
    #[ts(type = "number")]
    pub conflict_size: i64,
    /// 原始大小（本地不存在则为 None）
    #[ts(type = "number | null")]
    pub original_size: Option<i64>,
    /// 副本修改时间（Unix 秒）
    #[ts(type = "number")]
    pub conflict_mtime: i64,
    /// 原始修改时间
    #[ts(type = "number | null")]
    pub original_mtime: Option<i64>,
    /// 副本来源设备名（从文件名解析）
    pub source_device: String,
}

/// 冲突解决方式。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, TS, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[ts(export_to = "../../src/lib/bindings/")]
pub enum ConflictResolution {
    /// 保留冲突副本（覆盖原文件）
    KeepConflict,
    /// 保留原版本（删除副本）
    KeepOriginal,
    /// 两者都保留（副本改名留档）
    KeepBoth,
}

/// 文件树节点（文件夹详情页只读浏览）。
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export_to = "../../src/lib/bindings/")]
pub struct TreeNodeDto {
    /// 相对路径
    pub path: String,
    /// 文件名
    pub name: String,
    pub is_dir: bool,
    /// 字节数
    #[ts(type = "number")]
    pub size: i64,
    /// Unix 秒
    #[ts(type = "number")]
    pub mtime: i64,
    /// 同步状态
    pub state: FileState,
    /// 是否冲突副本
    pub is_conflict: bool,
}

/// 单文件同步状态。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, TS, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[ts(export_to = "../../src/lib/bindings/")]
pub enum FileState {
    /// 已同步
    Synced,
    /// 待上传
    PendingUpload,
    /// 待下载
    PendingDownload,
    /// 同步中
    InProgress,
    /// 已忽略
    Ignored,
    /// 已删除（墓碑）
    Deleted,
    /// 出错
    Failed,
}

/// 配对码信息。
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export_to = "../../src/lib/bindings/")]
pub struct PairingCodeDto {
    /// 可直接分享的配对码字符串
    pub code: String,
    /// 过期时间（Unix 秒）
    #[ts(type = "number")]
    pub expires_at: i64,
    /// 剩余秒数
    #[ts(type = "number")]
    pub ttl_seconds: i64,
    /// 二维码内容（与 code 相同，单独给出便于前端渲染）
    pub qr_content: String,
}

/// 待确认的配对请求。
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export_to = "../../src/lib/bindings/")]
pub struct PendingPairingDto {
    pub request_id: String,
    pub device_name: String,
    pub fingerprint: String,
    /// 过期时间（Unix 秒）
    #[ts(type = "number")]
    pub expires_at: i64,
}

/// 收到的共享邀请。
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export_to = "../../src/lib/bindings/")]
pub struct InviteDto {
    pub invite_id: String,
    pub folder_id: String,
    pub folder_name: String,
    pub from_device: String,
    /// 建议的本地路径
    pub suggested_path: String,
    /// 文件数
    pub file_count: u32,
    /// 总字节数
    #[ts(type = "number")]
    pub total_size: i64,
}

/// 忽略规则配置。
#[derive(Debug, Clone, Default, Serialize, Deserialize, TS)]
#[ts(export_to = "../../src/lib/bindings/")]
pub struct IgnoreRulesDto {
    /// 是否启用内置默认规则
    pub use_defaults: bool,
    /// 用户自定义规则（.gitignore 语法，每行一条）
    pub custom: Vec<String>,
    /// 生效的内置规则（只读回显，让用户知道默认忽略了什么）
    pub defaults: Vec<String>,
}

/// 网络模式。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, TS, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
#[ts(export_to = "../../src/lib/bindings/")]
pub enum NetworkMode {
    /// 自动：Tailscale 可用就走 SFTP 直连，否则走中继
    #[default]
    Auto,
    /// 仅 Tailscale（SSH/SFTP 直连）
    Tailscale,
    /// 仅中继服务器
    Relay,
}

impl From<NetworkMode> for fs_sync_net::conn::NetworkMode {
    fn from(m: NetworkMode) -> Self {
        match m {
            NetworkMode::Auto => Self::Auto,
            NetworkMode::Tailscale => Self::Tailscale,
            NetworkMode::Relay => Self::Relay,
        }
    }
}

/// 本机网络/SSH 环境状态（首次引导用）。
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export_to = "../../src/lib/bindings/")]
pub struct EnvStatusDto {
    pub ssh_ready: bool,
    pub ssh_port: u16,
    pub firewall_ok: bool,
    pub tailscale_installed: bool,
    pub tailscale_running: bool,
    pub tailscale_ips: Vec<String>,
    pub tailscale_hostname: Option<String>,
    pub relay_configured: bool,
    pub relay_reachable: bool,
    /// 至少有一种可用传输方式
    pub ready: bool,
    /// `install_tailscale` / `tailscale_login` / `setup_relay` / `check_relay` / `repair_ssh` / `repair_firewall`
    pub suggestions: Vec<String>,
    pub notes: Vec<String>,
}

impl From<fs_sync_net::env::EnvStatus> for EnvStatusDto {
    fn from(s: fs_sync_net::env::EnvStatus) -> Self {
        Self {
            ssh_ready: s.ssh_ready,
            ssh_port: s.ssh_port,
            firewall_ok: s.firewall_ok,
            tailscale_installed: s.tailscale.installed,
            tailscale_running: s.tailscale.running,
            tailscale_ips: s.tailscale.ips,
            tailscale_hostname: s.tailscale.hostname,
            relay_configured: s.relay_configured,
            relay_reachable: s.relay_reachable,
            ready: s.ready,
            suggestions: s.suggestions,
            notes: s.notes,
        }
    }
}

/// 应用设置。
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(default)]
#[ts(export_to = "../../src/lib/bindings/")]
pub struct SettingsDto {
    pub device_name: String,
    /// 开机自启
    pub auto_start: bool,
    /// 关闭窗口时最小化到托盘
    pub close_to_tray: bool,
    /// 上传限速（KB/s），0 = 不限
    pub upload_limit_kbps: u32,
    /// 下载限速（KB/s），0 = 不限
    pub download_limit_kbps: u32,
    /// 中继服务器地址（`tcp://IP:9443`），None = 未配置
    pub relay_url: Option<String>,
    pub network_mode: NetworkMode,
    /// 本机 SSH 服务端口
    pub ssh_port: u16,
    /// 全局默认忽略规则（新建文件夹时套用）
    pub default_ignore: Vec<String>,
    /// 日志级别：error/warn/info/debug/trace
    pub log_level: String,
}

impl Default for SettingsDto {
    fn default() -> Self {
        Self {
            device_name: "".into(),
            auto_start: false,
            close_to_tray: true,
            upload_limit_kbps: 0,
            download_limit_kbps: 0,
            // 官方自建中继；用户可在设置页清空或改成自己的服务器
            relay_url: Some(fs_sync_net::DEFAULT_RELAY_URL.to_string()),
            network_mode: NetworkMode::Auto,
            ssh_port: fs_sync_net::env::default_ssh_port(),
            default_ignore: Vec::new(),
            log_level: "info".into(),
        }
    }
}

/// 设置补丁（只更新提供的字段）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, TS)]
#[ts(export_to = "../../src/lib/bindings/", optional_fields)]
pub struct SettingsPatch {
    pub device_name: Option<String>,
    pub auto_start: Option<bool>,
    pub close_to_tray: Option<bool>,
    pub upload_limit_kbps: Option<u32>,
    pub download_limit_kbps: Option<u32>,
    #[serde(default, with = "double_option")]
    #[ts(optional, type = "string | null")]
    pub relay_url: Option<Option<String>>,
    pub network_mode: Option<NetworkMode>,
    pub ssh_port: Option<u16>,
    pub default_ignore: Option<Vec<String>>,
    pub log_level: Option<String>,
}

/// `Option<Option<T>>` 的 serde 适配：区分「未提供」与「显式设为 null」。
mod double_option {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<T, S>(value: &Option<Option<T>>, s: S) -> Result<S::Ok, S::Error>
    where
        T: Serialize,
        S: Serializer,
    {
        match value {
            Some(inner) => inner.serialize(s),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, T, D>(d: D) -> Result<Option<Option<T>>, D::Error>
    where
        T: Deserialize<'de>,
        D: Deserializer<'de>,
    {
        Option::<T>::deserialize(d).map(Some)
    }
}

/// 新建文件夹请求。
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export_to = "../../src/lib/bindings/")]
pub struct AddFolderRequest {
    pub path: String,
    /// 分享给哪些设备
    pub device_ids: Vec<String>,
    pub ignore: IgnoreRulesDto,
}

/// 单文件进度事件（`sync://progress`，每文件每 200ms 至多一次）。
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export_to = "../../src/lib/bindings/")]
pub struct ProgressDto {
    pub folder_id: String,
    pub path: String,
    /// 已传输字节
    #[ts(type = "number")]
    pub done: u64,
    /// 总字节
    #[ts(type = "number")]
    pub total: u64,
    pub direction: Direction,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, TS, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[ts(export_to = "../../src/lib/bindings/")]
pub enum Direction {
    Upload,
    Download,
}

/// 设备状态事件（`device://status`）。
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export_to = "../../src/lib/bindings/")]
pub struct DeviceStatusEvent {
    pub device: DeviceDto,
}

/// 文件夹状态事件（`folder://status`）。
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export_to = "../../src/lib/bindings/")]
pub struct FolderStatusEvent {
    pub folder: FolderDto,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_dto_serializes_to_code_and_message() {
        let e = ErrorDto::new("not_found", "找不到文件夹");
        let json = serde_json::to_string(&e).unwrap();
        assert!(json.contains("\"code\":\"not_found\""));
        assert!(json.contains("找不到文件夹"));
    }

    #[test]
    fn overall_status_uses_snake_case() {
        let json = serde_json::to_string(&OverallStatus::Syncing).unwrap();
        assert_eq!(json, "\"syncing\"");
    }

    #[test]
    fn conn_kind_variants_are_stable() {
        // 前端依赖这些字符串，改动即破坏兼容
        assert_eq!(
            serde_json::to_string(&ConnKind::Relay).unwrap(),
            "\"relay\""
        );
        assert_eq!(
            serde_json::to_string(&ConnKind::Tailscale).unwrap(),
            "\"tailscale\""
        );
    }

    #[test]
    fn settings_patch_distinguishes_absent_and_null() {
        // 只给 upload_limit：其他字段必须是 None（不覆盖已有值）
        let patch: SettingsPatch = serde_json::from_str(r#"{"upload_limit_kbps": 100}"#).unwrap();
        assert_eq!(patch.upload_limit_kbps, Some(100));
        assert!(patch.device_name.is_none());
        assert!(patch.relay_url.is_none(), "未提供时不覆盖");

        // 显式 null：要求清空 relay_url
        let patch: SettingsPatch = serde_json::from_str(r#"{"relay_url": null}"#).unwrap();
        assert_eq!(patch.relay_url, Some(None), "显式 null 应清空");
    }

    #[test]
    fn settings_defaults_are_sane() {
        let s = SettingsDto::default();
        assert_eq!(s.upload_limit_kbps, 0, "0 表示不限速");
        assert!(s.close_to_tray, "默认关闭到托盘");
        assert_eq!(s.log_level, "info");
        assert_eq!(s.network_mode, NetworkMode::Auto);
    }
}
