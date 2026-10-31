# FolderSync 协议 v2（SSH + SFTP）

当前协议版本：`PROTOCOL_VERSION = 3`（定义见 `crates/fs-sync-net/src/message.rs`）。
v1 的 iroh / QUIC 协议已弃用；v2 与 v3 的枚举尾部新增 `IndexChunk`，跨版本连接会由 `Hello` 握手拒绝。

## 1. 传输层

- SSH：默认 **TCP 2222**，使用 `russh`；每台设备既监听（服务端）也主动连接（客户端）。
- 直连路径：Tailscale 地址（`100.64.0.0/10` 或 MagicDNS 名）。
- 中继路径：客户端主动出站连接 `foldersync-relay` 的 **TCP 9443**。
- SSH 登录名：`foldersync`（应用专用，不映射系统账号）。
- 认证：
  - 已信任设备：SSH 公钥认证；公钥的 SHA256 指纹（设备 ID）必须在信任列表中。
  - 配对阶段：SSH 密码认证，密码为配对码中的一次性 token。
- SSH 子系统：
  - `foldersync`：已信任设备的控制消息长连接。
  - `sftp`：文件传输，服务端沙箱根目录为 `<data>/share`。
  - `foldersync-pair`：配对请求/响应。

## 2. 帧格式

控制消息使用长度前缀帧：

| 字段 | 类型 | 说明 |
|------|------|------|
| `length` | `u32`（大端） | postcard 消息体的字节数 |
| `body` | `bytes` | postcard 编码的 `Message` |

- 单帧上限 `MAX_FRAME = 64 MiB`：索引可能很大，但必须有界，防恶意对端；超大索引按 4000 条分页为 `IndexChunk`。
- 帧本身不带版本字段。`Hello` 消息携带 `protocol_version: u16`（当前为 3），双方必须匹配；中继模式的 `index.json` 顶层也带同名的 `protocol_version`。
- 所有消息定义以 `crates/fs-sync-net/src/message.rs` 为准。

## 3. 消息

```rust
enum Message {
    Hello {
        protocol_version: u16,
        device_name: String,
        folders: Vec<FolderSummary>,
    },
    Index {
        folder_id: String,
        entries: Vec<FileEntry>,
    },
    IndexUpdate {
        folder_id: String,
        entries: Vec<FileEntry>,
    },
    PairingRequest(PairingRequest),
    PairingResponse(PairingResponse),
    FolderInvite(FolderInvite),
    FolderInviteResponse(FolderInviteResponse),
    Ping,
    Pong,
    Error {
        code: u32,
        message: String,
    },
    IndexChunk {
        folder_id: String,
        entries: Vec<FileEntry>,
        chunk_index: u32,
        done: bool,
    },
}
```

| 消息 | 方向/用途 | 关键字段 |
|------|-----------|----------|
| `Hello` | 连接建立后双方交换设备名与文件夹摘要 | `protocol_version`、`device_name`、`folders: Vec<FolderSummary>` |
| `Index` | 全量索引交换 | `folder_id`、`entries: Vec<FileEntry>` |
| `IndexUpdate` | 增量索引；文件内容不放在消息里，走 SFTP 按 `hash` 拉取 | `folder_id`、`entries` |
| `PairingRequest` | 配对子系统：B → A | `token`、`peer: PeerInfo` |
| `PairingResponse` | 配对子系统：A → B | `accepted`、`peer: PeerInfo`（拒绝时为空） |
| `FolderInvite` | 文件夹共享邀请 | `folder_id`、`folder_name`、`from_name` |
| `FolderInviteResponse` | 邀请响应 | `folder_id`、`accepted`、`reason` |
| `IndexChunk` | 全量索引分页；`chunk_index` 从 0 连续递增，`done` 标记最后一块 | `folder_id`、`entries`、`chunk_index`、`done` |
| `Ping` / `Pong` | 保活 | 无 |
| `Error` | 协议错误 | `code`、`message` |

### 3.1 PeerInfo

```rust
struct PeerInfo {
    device_id: String,       // SSH 公钥 SHA256 指纹（设备 ID）
    device_name: String,     // 设备显示名
    ssh_public_key: String,  // authorized_keys 一行格式
    host_key: String,       // SSH 服务端主机公钥（首次连接时固定校验）
    username: String,        // SSH 登录名（固定 foldersync）
    port: u16,               // SSH 服务端端口（默认 2222）
    addrs: Vec<String>,      // Tailscale IP / MagicDNS 名，可为空
    relay: Option<String>,   // 中继地址 tcp://host:port
    relay_room: Option<String>,   // 中继房间 ID（配对成功后由发起方生成）
    relay_secret: Option<String>, // 中继房间口令
}
```

### 3.2 配对结构

```rust
struct PairingCode {
    peer: PeerInfo,     // 发起方的完整连接信息
    token: String,      // 一次性 token（16 字节 hex）
    expires_at: u64,    // 过期时间（Unix 秒，默认 +300 秒）
}

struct PairingRequest {
    token: String,
    peer: PeerInfo,     // 接收方（加入方）的完整连接信息
}

struct PairingResponse {
    accepted: bool,
    peer: PeerInfo,     // 接受时为发起方信息（含正式中继房间）；拒绝时为空
}

struct FolderSummary {
    folder_id: String,
    name: String,
}
```

`FileEntry`（来自 `fs-sync-core`）包含：`path`、`size`、`mtime`、`hash`（blake3 hex，目录为空串）、`is_dir`、`deleted`、`version`（版本向量）、`modified_by`。

## 4. 配对流程

### 4.1 SSH 直连配对

1. 设备 A 生成 `PairingCode`，编码为：
   `foldersync:<base64url(JSON), no padding>`。
   默认 5 分钟有效，token 一次性。
2. 设备 B 解码配对码，依次尝试 `peer.addrs` 中的 Tailscale 地址，连接 `peer.port`，用 `peer.username` + `token` 做 SSH 密码认证。
3. B 请求 `foldersync-pair` 子系统，发送 `Message::PairingRequest { token, peer: B 的 PeerInfo }`。
4. A 校验 token 与 B 的 SSH 公钥一致性，等待用户确认（120 秒超时即拒绝），然后返回 `Message::PairingResponse`。
5. 双方把对端 `PeerInfo`（SSH 公钥、主机公钥、地址、中继房间）写入信任列表；之后所有连接使用 SSH 公钥认证。

### 4.2 中继配对

1. 直连地址全部失败（或没有 Tailscale）时，B 使用配对码里的 `peer.relay` 或本地配置的中继地址。
2. 配对房间：
   - 房间 ID：`blake3(token)` 的 hex；
   - 房间口令：`token`。
3. B 连接中继房间，写 `pairing/request.json`；轮询 `pairing/response.json`（1 秒一次，130 秒超时）。
4. A 侧的中继配对监听器读取请求并向用户确认，写回响应。
5. `PairingResponse` 中带 A 生成的正式房间 `relay_room` / `relay_secret`，后续同步使用该房间。

## 5. 文件传输

- SFTP 根目录为 `<data>/share`；对端看到的是以 `/` 为根的沙箱，不能使用绝对路径或 `..`，不支持符号链接。
- Blob 路径：`blobs/<blake3 hex>`。索引条目的 `hash` 就是文件名。
- 下载流程：
  1. 通过 SFTP 读到本地临时文件；
  2. 重新计算 blake3，与期望 `hash` 比对；
  3. 校验通过后原子 rename 进本地 blob 库。
- 目录条目（`is_dir = true`）不传输文件内容，直接创建目录。
- 中继模式下不使用 `foldersync` 控制帧：双方通过 SFTP 交换 JSON 文件：
  - `devices/<device-id>/index.json`
  - `devices/<device-id>/blobs/<blake3>`
  - 每个设备只写自己的目录，读对端目录；索引原子写（临时文件 + rename）。

中继索引 JSON：

```json
{
  "protocol_version": 3,
  "device_id": "SHA256:...",
  "device_name": "home-pc",
  "folders": [
    {
      "folder_id": "folder-...",
      "folder_name": "work",
      "entries": []
    }
  ]
}
```

## 6. 错误码

| 常量 | 值 | 含义 |
|------|----|------|
| `PROTOCOL_MISMATCH` | 1 | 协议版本不匹配 |
| `BAD_TOKEN` | 2 | 配对 token 无效、已使用或已过期 |
| `NOT_TRUSTED` | 3 | 设备公钥不在信任列表 |
| `FOLDER_UNKNOWN` | 4 | 未知的 `folder_id` |
| `INTERNAL` | 5 | 内部错误 |

## 7. 版本历史

- **v1**：iroh 1.4.1 + iroh-blobs 0.103.1，ALPN `/foldersync/1`，QUIC/UDP。已弃用，不再维护。
- **v2**：SSH（russh 0.65）+ SFTP（russh-sftp 3.0.1），Tailscale 直连 / 自建中继。当前版本。
