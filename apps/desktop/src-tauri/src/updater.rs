//! 自动更新：对接自建的 Basic 版本更新服务（https://ashu180.cn）。
//!
//! 流程：`GET /api/version/info` 取最新版本 → 比较整数版本号 → 下载安装包
//! （支持 Range 续传，最多 3 次）→ 校验 MD5 与 Ed25519 签名 → 静默运行 NSIS 安装器并退出。
//!
//! 替代原来的 tauri-plugin-updater（GitHub Releases + latest.json）。
//! 签名用独立的 Ed25519 密钥：发布机对安装包原始字节签名，公钥内置在这里。
//! 没有签名或验签失败的包一律拒绝安装。

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use tokio::io::AsyncWriteExt;
use tracing::{info, warn};

use crate::dto::UpdateDto;

const HOST: &str = "https://ashu180.cn";
/// 项目 KEY（公开接口用，不是机密）
const PROJECT_UUID: &str = "af61462d-d513-4ef5-9064-f7619550f615";
/// 更新包签名公钥（Ed25519，32 字节 hex）。私钥只在 GitHub Secrets 里。
const SIGNING_PUBKEY: &str = "cfa37b5526d2f7bde6d0b58100c96a568a0827ae782675b7fed079f6fa2024dd";
/// 启动后多久做第一次检测（别和启动抢资源）
pub const FIRST_CHECK_DELAY: Duration = Duration::from_secs(15);
/// 之后的检测间隔
pub const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 3600);

/// 服务端统一响应外壳。
#[derive(Debug, Deserialize)]
struct Envelope<T> {
    code: i64,
    #[serde(default)]
    message: String,
    data: Option<T>,
}

/// `/api/version/info` 的 data。
#[derive(Debug, Clone, Deserialize)]
pub struct RemoteInfo {
    pub version: u32,
    #[serde(default)]
    pub file_name: String,
    #[serde(default)]
    pub file_size: u64,
    #[serde(default)]
    pub update: bool,
    #[serde(default)]
    pub md5: String,
    #[serde(default)]
    pub status: i32,
    #[serde(default)]
    pub notice: String,
    #[serde(default)]
    pub download_url: String,
    #[serde(default)]
    pub signature: String,
}

/// semver → 服务端的整数版本号：major*10000 + minor*100 + patch。
/// 必须与 `scripts/publish_update.py` 的 `version_code` 一致。
pub fn version_code(semver: &str) -> Option<u32> {
    let core = semver.trim_start_matches('v').split(['-', '+']).next()?;
    let mut it = core.split('.').map(|p| p.parse::<u32>().ok());
    let (major, minor, patch) = (it.next()??, it.next()??, it.next()??);
    if it.next().is_some() || minor > 99 || patch > 99 {
        return None;
    }
    Some(major * 10000 + minor * 100 + patch)
}

/// 整数版本号 → 展示用的 x.y.z。
pub fn version_text(code: u32) -> String {
    format!("{}.{}.{}", code / 10000, code / 100 % 100, code % 100)
}

/// 当前程序版本。
pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// 根据服务端信息判断有没有可用更新。
pub fn evaluate(remote: &RemoteInfo, current: &str) -> UpdateDto {
    let local = version_code(current).unwrap_or(0);
    let has_file = !remote.download_url.is_empty() || !remote.file_name.is_empty();
    // status=1：这个版本被发布方禁用，不提供给客户端
    let disabled = remote.status == 1;
    let available = !disabled && has_file && remote.version > local;
    UpdateDto {
        current_version: current.to_string(),
        latest_version: (remote.version > 0).then(|| version_text(remote.version)),
        available,
        force: available && remote.update,
        disabled,
        notice: remote.notice.clone(),
        file_size: remote.file_size,
    }
}

fn client() -> Result<reqwest::Client> {
    // reqwest 用 rustls-no-provider：显式装 ring，避免引入需要 C 编译器的 aws-lc
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
    reqwest::Client::builder()
        .user_agent(concat!("FolderSync/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
        .build()
        .context("创建 HTTP 客户端失败")
}

/// 取服务端最新版本信息。失败时把服务端 message 原样带出来。
pub async fn fetch_info() -> Result<RemoteInfo> {
    let url = format!("{HOST}/api/version/info?uuid={PROJECT_UUID}");
    let resp = client()?
        .get(&url)
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .context("连接更新服务失败")?;
    let status = resp.status();
    let env: Envelope<RemoteInfo> = resp.json().await.context("更新服务返回格式错误")?;
    if !status.is_success() || env.code != 200 {
        bail!(
            "{}",
            if env.message.is_empty() {
                status.to_string()
            } else {
                env.message
            }
        );
    }
    env.data.ok_or_else(|| anyhow!("更新服务未返回版本信息"))
}

/// 检测更新。
pub async fn check() -> Result<UpdateDto> {
    Ok(evaluate(&fetch_info().await?, current_version()))
}

/// 校验下载结果：MD5 必须与服务端一致，Ed25519 签名必须能用内置公钥验过。
pub fn verify(bytes: &[u8], md5_hex: &str, signature_hex: &str) -> Result<()> {
    use md5::{Digest, Md5};
    let got = hex::encode(Md5::digest(bytes));
    if !md5_hex.is_empty() && !got.eq_ignore_ascii_case(md5_hex) {
        bail!("MD5 校验失败（期望 {md5_hex}，实际 {got}）");
    }
    verify_signature(bytes, signature_hex, SIGNING_PUBKEY)
}

fn verify_signature(bytes: &[u8], signature_hex: &str, pubkey_hex: &str) -> Result<()> {
    if signature_hex.is_empty() {
        bail!("更新包没有签名，拒绝安装");
    }
    let pk: [u8; 32] = hex::decode(pubkey_hex)?
        .try_into()
        .map_err(|_| anyhow!("内置公钥长度错误"))?;
    let sig: [u8; 64] = hex::decode(signature_hex)
        .context("签名不是合法 hex")?
        .try_into()
        .map_err(|_| anyhow!("签名长度错误（应为 64 字节）"))?;
    let key = ed25519_dalek::VerifyingKey::from_bytes(&pk).context("内置公钥无效")?;
    key.verify_strict(bytes, &ed25519_dalek::Signature::from_bytes(&sig))
        .map_err(|_| anyhow!("签名校验失败，安装包可能被篡改，已拒绝安装"))
}

/// 下载到 `dest`：已有部分内容时用 Range 续传。`on_progress(已下载, 总大小)`。
async fn fetch_to(
    http: &reqwest::Client,
    url: &str,
    dest: &Path,
    total: u64,
    on_progress: &(dyn Fn(u64, u64) + Send + Sync),
) -> Result<()> {
    let mut have = tokio::fs::metadata(dest)
        .await
        .map(|m| m.len())
        .unwrap_or(0);
    if total > 0 && have >= total {
        // 已经下完（或比预期还大，交给校验判断）
        return Ok(());
    }
    let mut req = http.get(url).timeout(Duration::from_secs(600));
    if have > 0 {
        req = req.header(reqwest::header::RANGE, format!("bytes={have}-"));
    }
    let mut resp = req.send().await.context("下载失败")?;
    let status = resp.status();
    if !status.is_success() {
        bail!("下载失败：HTTP {status}");
    }
    // 服务端不支持 Range（返回 200 而不是 206）就从头写
    let resume = status == reqwest::StatusCode::PARTIAL_CONTENT;
    if !resume {
        have = 0;
    }
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(resume)
        .truncate(!resume)
        .open(dest)
        .await
        .with_context(|| format!("写入 {} 失败", dest.display()))?;
    while let Some(chunk) = resp.chunk().await.context("下载中断")? {
        file.write_all(&chunk).await?;
        have += chunk.len() as u64;
        on_progress(have, total);
    }
    file.flush().await?;
    Ok(())
}

/// 下载并校验安装包，返回本地路径。网络/MD5 失败最多重试 3 次；验签失败立即放弃。
pub async fn download_verified(
    info: &RemoteInfo,
    on_progress: &(dyn Fn(u64, u64) + Send + Sync),
) -> Result<PathBuf> {
    if info.signature.is_empty() {
        bail!("更新包没有签名，拒绝安装");
    }
    // 文件名来自服务端：只取最后一段，防路径穿越；必须是 .exe
    let name = Path::new(&info.file_name)
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| n.to_ascii_lowercase().ends_with(".exe"))
        .ok_or_else(|| anyhow!("更新包文件名不合法: {}", info.file_name))?;
    let dir = std::env::temp_dir().join("foldersync-update");
    tokio::fs::create_dir_all(&dir).await?;
    let dest = dir.join(name);

    let http = client()?;
    let fallback = format!("{HOST}/api/version/download?uuid={PROJECT_UUID}");
    let mut last_err = anyhow!("未开始下载");
    for attempt in 1..=3 {
        // download_url 是 1 小时有效的预签名链接；重试时改走统一下载接口拿新链接
        let url = if attempt == 1 && !info.download_url.is_empty() {
            info.download_url.as_str()
        } else {
            fallback.as_str()
        };
        if let Err(e) = fetch_to(&http, url, &dest, info.file_size, on_progress).await {
            warn!(attempt, "下载更新包失败: {e:#}");
            last_err = e;
            continue;
        }
        let bytes = tokio::fs::read(&dest).await?;
        match verify(&bytes, &info.md5, &info.signature) {
            Ok(()) => return Ok(dest),
            Err(e) => {
                // 内容不对：删掉重下，不能在坏文件上续传
                let _ = tokio::fs::remove_file(&dest).await;
                if e.to_string().contains("签名") {
                    return Err(e);
                }
                warn!(attempt, "更新包校验失败: {e:#}");
                last_err = e;
            }
        }
    }
    Err(last_err.context("下载更新包失败（已重试 3 次）"))
}

/// 运行 NSIS 安装器（被动模式：只显示进度条，装完自动重启应用）。调用方随后应退出进程。
///
/// 参数与 tauri-plugin-updater 的 passive 模式一致：`/P` 被动、`/R` 装完重启、
/// `/UPDATE` 告诉安装器这是升级（会先结束正在运行的旧进程）。
pub fn launch_installer(path: &Path) -> Result<()> {
    info!(path = %path.display(), "启动更新安装器");
    std::process::Command::new(path)
        .args(["/P", "/R", "/UPDATE"])
        .spawn()
        .with_context(|| format!("启动安装器失败: {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(version: u32, status: i32, update: bool) -> RemoteInfo {
        RemoteInfo {
            version,
            file_name: "FolderSync_0.2.5_x64-setup.exe".into(),
            file_size: 1,
            update,
            md5: String::new(),
            status,
            notice: "n".into(),
            download_url: "https://x".into(),
            signature: "s".into(),
        }
    }

    #[test]
    fn version_code_roundtrip() {
        assert_eq!(version_code("0.2.5"), Some(205));
        assert_eq!(version_code("v1.10.3"), Some(11003));
        assert_eq!(version_code("0.2.5-beta.1"), Some(205));
        assert_eq!(version_code("0.2"), None);
        assert_eq!(version_code("0.100.0"), None);
        assert_eq!(version_text(205), "0.2.5");
        assert_eq!(version_text(11003), "1.10.3");
        // 单调：任何更大的 semver 编码后也更大
        assert!(version_code("0.3.0") > version_code("0.2.99"));
        assert!(version_code("1.0.0") > version_code("0.99.99"));
    }

    #[test]
    fn evaluate_rules() {
        let e = evaluate(&info(205, 0, false), "0.2.4");
        assert!(e.available && !e.force);
        assert_eq!(e.latest_version.as_deref(), Some("0.2.5"));
        // 同版本 / 旧版本：不提示
        assert!(!evaluate(&info(205, 0, false), "0.2.5").available);
        assert!(!evaluate(&info(204, 0, false), "0.2.5").available);
        // 强制更新
        assert!(evaluate(&info(205, 0, true), "0.2.4").force);
        // 被禁用的版本不提供
        let d = evaluate(&info(205, 1, true), "0.2.4");
        assert!(!d.available && !d.force && d.disabled);
        // 服务端还没上传过文件（version=0）
        let mut empty = info(0, 0, false);
        empty.file_name.clear();
        empty.download_url.clear();
        let z = evaluate(&empty, "0.2.4");
        assert!(!z.available && z.latest_version.is_none());
    }

    /// 与发布脚本（Python cryptography）互通的固定向量：
    /// seed = 32 个 0x01，消息 b"foldersync"，由 `Ed25519PrivateKey.sign` 生成。
    #[test]
    fn signature_matches_python_cryptography() {
        let pk = "8a88e3dd7409f195fd52db2d3cba5d72ca6709bf1d94121bf3748801b40f6f5c";
        let sig = "09de7a5ca83dcf953ddc07e04ff7a8c14ec6289d7e124f88a29afa0da4a34b21\
                   4bd1fc0761da60e5ddf584ce660e4cd9b6f0e3913791c4bc6f03e7f7f27ae902";
        verify_signature(b"foldersync", sig, pk).expect("Python 签名应能通过验签");
        // 内容被改一个字节 / 没有签名：都必须拒绝
        assert!(verify_signature(b"foldersynC", sig, pk).is_err());
        assert!(verify_signature(b"foldersync", "", pk).is_err());
    }

    /// 联网测试：确认真实服务端的响应能被解析。
    /// `cargo test -p foldersync-desktop updater -- --ignored`
    #[tokio::test]
    #[ignore]
    async fn live_info_parses() {
        let info = fetch_info().await.expect("info 接口应可用");
        let dto = evaluate(&info, current_version());
        println!("remote version={} dto={dto:?}", info.version);
    }

    #[test]
    fn md5_mismatch_rejected_before_signature() {
        let e = verify(b"abc", "00000000000000000000000000000000", "aa").unwrap_err();
        assert!(e.to_string().contains("MD5"));
    }
}
