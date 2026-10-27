//! 协议服务端（桌面端）：处理对端接入、配对请求、索引增量。
//!
//! 与 CLI 的 `SyncServer` 同构，但多两件事：
//! 1. 把待确认的配对请求推给前端（`pairing://request` 事件）等用户点确认；
//! 2. 应用对端增量后推 `sync://activity` 事件。
//!
//! 提示词 5.3 要求事件驱动界面，不能让用户手动点刷新。

use std::sync::Arc;

use fs_sync_net::frame;
use fs_sync_net::message::{Message, PairingRequest, PairingResponse, error_code};
use iroh::endpoint::Connection;
use iroh::protocol::{AcceptError, ProtocolHandler};
use tracing::{debug, info, warn};

use crate::dto::{ActivityDto, ActivityKind, PendingPairingDto};
use crate::state::AppState;

/// 桌面端协议处理器。
#[derive(Clone)]
pub struct DesktopServer {
    pub state: Arc<AppState>,
    /// 前端事件发送器
    pub events: EventSink,
}

/// 事件发送器：把后端事件转成 Tauri 事件。
///
/// 抽成 trait 对象是为了能在没有 AppHandle 的测试里构造一个空实现。
pub type EventSink = Arc<dyn Fn(&str, serde_json::Value) + Send + Sync>;

impl std::fmt::Debug for DesktopServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DesktopServer").finish_non_exhaustive()
    }
}

impl ProtocolHandler for DesktopServer {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        let peer = conn.remote_id();
        let trusted = self.state.trust.is_trusted(&peer).unwrap_or(false);
        let conn_type = fs_sync_net::conn::classify_conn(&conn)
            .map(|c| c.to_string())
            .unwrap_or_else(|| "未知".into());

        let mut bi = conn.accept_bi().await?;

        // 长连接循环：双向实时同步要求一条连接上持续收发
        loop {
            let msg = match frame::read_frame(&mut bi.1).await {
                Ok(m) => m,
                Err(e) => {
                    debug!("对端连接结束: {e}");
                    // 标记离线
                    self.state.conns.lock().await.remove(&peer.to_string());
                    return Ok(());
                }
            };

            match msg {
                Message::Hello { device_name, .. } => {
                    if !trusted {
                        warn!(%device_name, "未信任设备尝试接入，已拒绝");
                        conn.close(1u32.into(), b"not trusted");
                        return Ok(());
                    }
                    info!(device = %device_name, conn_type = %conn_type, "已信任设备接入");
                    // 记录连接方式，前端据此显示"直连/中继"
                    let kind = match fs_sync_net::conn::classify_conn(&conn) {
                        Some(fs_sync_net::conn::ConnType::Direct) => crate::dto::ConnKind::Direct,
                        Some(fs_sync_net::conn::ConnType::Relay) => crate::dto::ConnKind::Relay,
                        Some(fs_sync_net::conn::ConnType::Tailscale) => {
                            crate::dto::ConnKind::Tailscale
                        }
                        None => crate::dto::ConnKind::Unknown,
                    };
                    self.state
                        .conns
                        .lock()
                        .await
                        .insert(peer.to_string(), (kind, None));
                    frame::write_frame(
                        &mut bi.0,
                        &Message::Hello {
                            protocol_version: fs_sync_net::message::PROTOCOL_VERSION,
                            device_name: self.state.device_name.clone(),
                            folders: Vec::new(),
                        },
                    )
                    .await
                    .map_err(|e| {
                        AcceptError::from_boxed(Box::new(std::io::Error::other(e.to_string())))
                    })?;
                }

                Message::PairingRequest(req) => {
                    self.handle_pairing(&mut bi.0, peer, req).await?;
                }

                Message::Index { folder_id, entries }
                | Message::IndexUpdate { folder_id, entries } => {
                    if !trusted {
                        conn.close(1u32.into(), b"not trusted");
                        return Ok(());
                    }
                    let count = entries.len();
                    let path_hint = entries.first().map(|e| e.path.clone()).unwrap_or_default();

                    let rt = {
                        let guard = self.state.runtimes.lock().await;
                        guard.get(&folder_id).cloned()
                    };
                    let Some(rt) = rt else {
                        // 未知文件夹：告诉对端而不是静默丢弃
                        frame::write_frame(
                            &mut bi.0,
                            &Message::Error {
                                code: error_code::FOLDER_UNKNOWN,
                                message: format!("未知文件夹: {folder_id}"),
                            },
                        )
                        .await
                        .ok();
                        continue;
                    };

                    match rt
                        .handle_remote_update(self.state.endpoint.clone(), peer, entries)
                        .await
                    {
                        Ok(()) => {
                            info!(count, folder = %folder_id, "已应用对端增量");
                            // 推活动事件（提示词 5.3：sync://activity）
                            emit_activity(
                                &self.events,
                                ActivityKind::Download,
                                &folder_id,
                                &path_hint,
                                &format!("收到 {count} 个文件变更"),
                            );
                        }
                        Err(e) => {
                            warn!("应用对端增量失败: {e:#}");
                            emit_activity(
                                &self.events,
                                ActivityKind::Error,
                                &folder_id,
                                &path_hint,
                                &format!("同步失败: {e}"),
                            );
                        }
                    }
                }

                Message::Ping => {
                    frame::write_frame(&mut bi.0, &Message::Pong).await.ok();
                }
                Message::Pong => {}
                other => {
                    debug!("忽略消息: {other:?}");
                }
            }
        }
    }
}

impl DesktopServer {
    /// 处理配对请求：把请求挂起并推给前端，等用户点确认。
    async fn handle_pairing(
        &self,
        send: &mut iroh::endpoint::SendStream,
        peer: iroh::EndpointId,
        req: PairingRequest,
    ) -> Result<(), AcceptError> {
        // 简单唯一 id：纳秒时间戳（同一进程内不会碰撞）
        let request_id = format!(
            "{:x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let expires_at = now_secs() + 120;
        let fp = fs_sync_daemon::fingerprint(&peer);

        // 挂起等待用户决定（和 CLI 的 auto-confirm 走同一套 PairingManager）
        let state = self.state.clone();
        let token = req.token.clone();
        let device_name = req.device_name.clone();
        let rid = request_id.clone();
        let peer_name = req.device_name.clone();

        // 先告知前端有配对请求
        self.state.pending_pairings.lock().await.insert(
            request_id.clone(),
            (req.device_name.clone(), fp.clone(), expires_at),
        );
        (self.events)(
            "pairing://request",
            serde_json::to_value(PendingPairingDto {
                request_id: request_id.clone(),
                device_name: req.device_name.clone(),
                fingerprint: fp.clone(),
                expires_at,
            })
            .unwrap_or(serde_json::Value::Null),
        );

        // 等用户决定（轮询 pending 表，最长 120 秒）
        let deadline = now_secs() + 120;
        let accepted = loop {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            if !state.pending_pairings.lock().await.contains_key(&rid) {
                // 用户已处理（confirm_pairing 会移除并写入 trust）
                break state.trust.is_trusted(&peer).unwrap_or(false);
            }
            if now_secs() > deadline {
                break false;
            }
        };
        state.pending_pairings.lock().await.remove(&rid);

        // 真正完成配对（消费 token + 写信任名单）
        if accepted {
            match state
                .pairing
                .on_pairing_request(
                    peer,
                    &token,
                    &device_name,
                    std::time::Duration::from_secs(1),
                )
                .await
            {
                Ok(true) => info!(%peer_name, "配对完成"),
                Ok(false) => warn!(%peer_name, "配对 token 校验失败"),
                Err(e) => warn!("配对失败: {e}"),
            }
        }

        frame::write_frame(
            send,
            &Message::PairingResponse(PairingResponse {
                accepted,
                device_name: state.device_name.clone(),
            }),
        )
        .await
        .map_err(|e| AcceptError::from_boxed(Box::new(std::io::Error::other(e.to_string()))))?;
        Ok(())
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 推一条活动事件给前端。
pub fn emit_activity(
    events: &EventSink,
    kind: ActivityKind,
    folder_id: &str,
    path: &str,
    message: &str,
) {
    let dto = ActivityDto {
        id: 0,
        at: now_secs(),
        kind,
        folder_id: folder_id.to_string(),
        path: path.to_string(),
        size: 0,
        message: message.to_string(),
    };
    events(
        "sync://activity",
        serde_json::to_value(dto).unwrap_or(serde_json::Value::Null),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_secs_is_plausible() {
        let t = now_secs();
        // 2020-01-01 之后
        assert!(t > 1_577_836_800);
    }

    #[test]
    fn emit_activity_serializes_kind_as_snake_case() {
        let captured = Arc::new(std::sync::Mutex::new(
            Vec::<(String, serde_json::Value)>::new(),
        ));
        let cap = captured.clone();
        let sink: EventSink = Arc::new(move |name, payload| {
            cap.lock().unwrap().push((name.to_string(), payload));
        });

        emit_activity(&sink, ActivityKind::Conflict, "f1", "a.txt", "冲突");

        let got = captured.lock().unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, "sync://activity");
        assert_eq!(got[0].1["kind"], "conflict");
        assert_eq!(got[0].1["path"], "a.txt");
    }
}
