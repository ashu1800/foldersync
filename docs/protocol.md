# FolderSync 协议 v1

- ALPN：`/foldersync/1`
- 帧：4 字节大端长度前缀 + postcard 编码消息体
- 所有消息携带 `protocol_version: u16`（当前 1）

## 消息

```rust
enum Message {
    Hello { protocol_version, device_name, folders: Vec<FolderSummary> },
    Index { folder_id, entries: Vec<FileEntry> },
    IndexUpdate { folder_id, entries },
    Request { folder_id, path, hash },
    FolderInvite { .. },
    FolderInviteResponse { .. },
    Ping,
    Pong,
    Error { code, message },
}
```

（M2 实施时随代码定稿字段，本文档保持与代码同步。）
