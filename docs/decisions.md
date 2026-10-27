# 设计决策

## 2026-10-27 版本选型

- iroh 1.4.1：1.x 起类型全面更名（EndpointId/EndpointAddr），presets 引入。选最新稳定版并锁定。
- iroh-blobs 0.103.1：0.103.0 及更早多个版本被 yank，0.103.1 是当前 max_stable_version，rpc 特性基于 noq 与 iroh 1.4 对齐。
- 双端 `conn.closed().await` 会互等死锁：约定发起方（dial 端）读完直接 drop，接收方（accept 端）等 closed。此坑已在 M0 实测发现并验证。
- Windows 远端跑常驻进程：SSH 会话结束会带走 `start /b` 子进程，用 `schtasks /SC ONCE` 启动可存活，测试脚本用此法。

## 2026-10-27 配对 token 必须持久化（M2 实测发现）

现象：`foldersync pair` 生成的配对码，对端 `join` 时被拒"token 无效"。

根因：pair 与 serve 是两个独立进程，token 原本存在 pair 进程内存里，serve 进程看不到。这不是测试问题而是架构缺陷——GUI 里"生成配对码"和"处理连接"也可能在不同上下文。

决策：token 落 SQLite `pairing_tokens` 表（`expires_at` 索引），`count/consume` 由 store 层保证原子性与一次性。回归测试 `token_shared_across_pairing_manager_instances` 守住此行为。

## 2026-10-27 CLI 无 UI 的配对确认

服务端收到配对请求后挂起等待用户确认（120s 超时自动拒绝）。CLI 无交互界面，提供 `serve --auto-confirm` 供调试；桌面端 M5 用 `pairing://request` 事件交给界面确认。这是有意保留的口子，不是安全弱化——auto-confirm 只在显式传参时启用。

## 2026-10-27 FsStore 必须单实例共享（M3 实测发现）

现象：daemon 测试挂起超过 60 秒不结束，cargo 看似"卡死"（实际 CPU 只跑了 0.7 秒）。

根因：`FsStore` 每个实例都会启动独立的后台 actor，多个实例争抢同一个 redb 数据库文件时互相阻塞。测试里 `engine()` 建一个、`apply_pulled` 内部再 open 一个、`ensure_blob` 又 open 一个，三重争抢。

决策：`SyncEngine` 持有 `Arc<FsStore>` 作为字段，**所有操作复用同一实例**，不再按需 `open_store`。修复后测试 0.07 秒跑完。这是"打开存储"与"持有存储"的语义区别——后者才正确。

## 2026-10-27 blob 目录按进程角色隔离（M3 实测发现）

现象：`foldersync serve` 运行时，另开 `foldersync sync` 会卡死。

根因：两个进程使用同一个 `blobs` 目录 → 又是 redb 跨进程争锁。

决策：服务端用 `<data>/blobs`，一次性客户端用 `<data>/blobs-client`。代价是两边各存一份内容缓存，换来进程隔离（这也是 Syncthing 之类的常见做法：文件夹索引与块缓存分离）。M4 起改为常驻单进程架构后，可回归单一目录。

## 2026-10-27 单向同步的方向语义（M3 实测修正）

`run_sync` 的视角是"本地需要做什么"：`Push` = 对端需要我们的内容，`Pull` = 我们要拉对端的内容。

M3 测试暴露出：本端算完 `Push` 就退出，对端来不及回拉 → `Unable to download`。修复两点：

1. `sync` 命令也挂载 `iroh_blobs::ALPN` 服务端（否则对端无法拉取）。
2. 推送方向结束后保持在线等对端拉完（`conn.closed()` + 60s 超时兜底）。

另修：目录条目（`is_dir`）不能走 blob 拉取路径（会产生"hash 无效"错误），直接建目录。
## 2026-10-27 M4 双向实时同步：四个实测踩出来的坑

M4 用了两轮真机调试才通（家 ↔ 公司）。四个 bug 全部是靠日志定位的，记在这里避免重犯：

### 1. 初始扫描必须同时把内容放进 blob 存储

`initial_scan` 只建索引不存内容 → 对端拉到条目却 `Unable to download`。索引和内容是两个东西，只登记不算同步。

### 2. tokio::select! 的分支一旦模式不匹配就被永久禁用

`Some(entry) = recv_local_change(...)` 在队列空时返回 `None`，该分支当场失效，后续本地变更要等下一次 `read_frame` 或 20 秒保活才可能推送（实测删除延迟 20 秒）。修法：等待函数内部 loop 到有数据为止，**永不返回 None**。

### 3. 服务端 handler 不能处理一条消息就收工

`SyncServer::accept` 原本读完一条 Index 就 `conn.close("sync done")`，客户端长连接立刻被踢，每秒重连一次。改成消息循环，只在真正出错或对端断开时退出。

### 4. 冲突处理必须合并版本向量并递增

`apply_conflict` 后只把远端原版本写进索引 → 下一轮 diff 仍判 Concurrent → **每秒生成一个新副本**（实测目录里堆了 6 份 sync-conflict）。修法：冲突落盘后写回 `merge(win, lose).increment(me)`，双端收敛到同一个版本。回归测试 `conflict_converges_and_does_not_repeat` 锁死这个行为。

### 冲突副本命名

副本用**设备名**（`DESKTOP-T7CO944`）而非 EndpointId：64 位 hex 会让文件名长达 100+ 字符。`SyncSession` 持 `device_name` 字段专门为此。

## 2026-10-27 folder_id 在 CLI 阶段用文件夹名派生

正式模型（提示词 4.3）是创建方生成 UUID、经 `FolderInvite` 下发。CLI 阶段没有邀请流程，用文件夹名哈希（`folder-<blake3(name)[..16]>`）让两端同名文件夹自然配对。

代价：改文件夹名等于换一个文件夹。M5 接入邀请流程后改为 UUID，这一层可以直接删。

`run_sync` 的视角是"本地需要做什么"：`Push` = 对端需要我们的内容，`Pull` = 我们要拉对端的内容。

M3 测试暴露出：本端算完 `Push` 就退出，对端来不及回拉 → `Unable to download`。

修复两点：
1. `sync` 命令也挂载 `iroh_blobs::ALPN` 服务端（否则对端无法拉取）。
2. 推送方向结束后保持在线等对端拉完（`conn.closed()` + 60s 超时兜底），而不是立即 `endpoint.close()`。

另修：目录条目（`is_dir`）不能再走 blob 拉取路径（会产生"hash 无效"错误），直接建目录。
## 2026-10-27 M5 桌面端：三个关键取舍

### 用 ts-rs 而不是 specta

两者都能生成 TS 类型。选 ts-rs 因为：命令行式导出（`cargo test` 里跑），CI 里 diff 一下就知道有没有漂移；specta 需要跟 tauri-specta 生态绑定，多一层依赖。代价是 ts-rs 生成的枚举默认不带 `export` 前缀、`u64` 默认映射成 `bigint`（JSON 传不过去），这两点在 `bindings.rs` / `dto.rs` 里各修了一处。

### 类型生成放在 `cargo test` 里

`export_bindings` 是个 `#[test]`。好处：CI 的 `cargo test` 顺带保证类型定义仍可生成，改了 Rust 却忘了重新生成会被 CI 的 `git diff --exit-code` 抓住。坏处：跑测试会写文件——但这正是我们想要的副作用。

### 事件用轮询 + 推送混合

`device://status` 这类事件由后端主动推，但在线状态和 RTT 在 iroh 内部是变化的、事件推不全，所以前端另有 5 秒一次的低频兜底拉取。只推不轮不完整，只轮不推不实时。

## 2026-10-27 M6 限速是有意的近似实现

限速做的是「传输后结算」：传完一批数据，按大小睡相应时间。不是 iroh 内部的精确整形。

为什么这么做：真正精确的限速要包一层带令牌桶的 `AsyncRead`/`AsyncWrite` 再塞给 iroh，改动面大、容易引入死锁（iroh 内部对流的读写有超时假设）。当前实现对用户感知的「限速了」是准确的，对网络统计不精确。

升级路径：若用户抱怨限速不准，在 `limits.rs` 里加流包装器即可，调用点（`lib.rs` 两处 `.consume()`）不用改。

## 2026-10-27 诊断包刻意不含的内容

`DiagnosticReport` 收集版本、设备 id/指纹、Tailscale 地址、设置、索引统计、日志尾巴。

**不含**：文件内容、配对 token、私钥。日志里可能有完整路径（排障需要），这一点在文档里向用户说明。
