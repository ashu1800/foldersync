//! 帧读写：在 QUIC 流上读写长度前缀消息。

use anyhow::{Context, Result};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::message::{MAX_FRAME, Message, decode_frame, encode_frame};

/// 写一帧（长度前缀 + 体），并 flush。
pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, msg: &Message) -> Result<()> {
    let buf = encode_frame(msg)?;
    w.write_all(&buf).await.context("写帧失败")?;
    w.flush().await.context("flush 失败")?;
    Ok(())
}

/// 读一帧。内部先读 4 字节长度，再按长度读体。
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<Message> {
    let mut head = [0u8; 4];
    r.read_exact(&mut head)
        .await
        .context("读帧头失败（对端关闭？）")?;
    let len = u32::from_be_bytes(head) as usize;
    anyhow::ensure!(len <= MAX_FRAME, "帧超长: {len}");
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await.context("读帧体失败")?;
    let mut full = Vec::with_capacity(4 + len);
    full.extend_from_slice(&head);
    full.extend_from_slice(&body);
    decode_frame(&full).context("解码消息失败")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::Message;

    #[tokio::test]
    async fn roundtrip_over_duplex() {
        // 用 tokio 双工内存流模拟
        let (mut c1, mut c2) = tokio::io::duplex(1024);
        let msg = Message::Ping;
        write_frame(&mut c1, &msg).await.unwrap();
        let got = read_frame(&mut c2).await.unwrap();
        assert_eq!(got, Message::Ping);
    }

    #[tokio::test]
    async fn two_frames_in_sequence() {
        let (mut c1, mut c2) = tokio::io::duplex(4096);
        write_frame(&mut c1, &Message::Ping).await.unwrap();
        write_frame(&mut c1, &Message::Pong).await.unwrap();
        assert_eq!(read_frame(&mut c2).await.unwrap(), Message::Ping);
        assert_eq!(read_frame(&mut c2).await.unwrap(), Message::Pong);
    }
}
