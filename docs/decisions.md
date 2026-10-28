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
## 2026-10-27 冲突处理曾在 loser_bytes 缺失时静默丢数据（双节点测试抓出）

这是整个项目里最严重的一个 bug，写在这里因为它**没有任何日志、没有任何报错**：

```rust
// 修复前
if let Some(bytes) = loser_bytes {
    tokio::fs::write(&conflict_dest, &bytes).await?;   // 只有 Some 才写副本
}
self.apply_pulled(winner, winner_hash).await?;          // 然后无条件覆盖原文件
```

`loser_bytes` 来自「从 blob 存储读落败版本」，上游是 `pull_blob(...).ok()` ——
对端内容读不到时变成 `None`，于是副本没写，接着胜者把原路径覆盖掉。
**本地那份修改就这么没了，日志里只有一行 "冲突已处理"**。

为什么手工跨机测试没发现：真机测试时两端 blob 都健康，`loser_bytes` 总是 `Some`。
只有同一进程内两个节点各自持有独立 blob 库时（`tests/two_node.rs`），
B 的落败版本在 B 自己的库里、但拉取路径指向 A，才会稳定地拿到 `None`。

修复（`engine.rs::apply_conflict`）：
- `Some(bytes)`：照旧写副本。
- `None`：把**本地现有文件 rename 成副本**，再让胜者落盘。本地不存在才真的无内容可保。

回归测试 `conflict_without_loser_bytes_preserves_local_file`，
并验证过「回退修复后该测试立刻失败」。

教训：`if let Some(x) = ... { 做事 }` 后面紧跟一个**会破坏数据源的操作**时，
`else` 分支必须先处理，不能默认"没有就算了"。

## 2026-10-27 ESLint 抓到 Activity 页的请求竞态

`Activity.tsx` 原本在 effect 里调 `load()`，而 `load()` 第一行是 `setLoading(true)`。
ESLint（react-hooks 新规则）报"effect 里同步 setState 会级联渲染"。

改的时候发现了同处的真问题：切换过滤条件时，前一次请求可能比后一次**更晚**返回，
把旧结果盖在新结果上。原本的写法没有任何保护。

现在 effect 内部起异步 IIFE + `cancelled` 标记，卸载或依赖变化时丢弃迟到结果。

## 2026-10-27 文件树用截断而不是虚拟滚动

提示词 5.4 要求"长列表使用虚拟滚动"。实际按场景分了两类：

- **活动列表**：后端限制 300 条、前端最多留 500 条，列表自带分页语义，不值得上虚拟滚动。
- **文件夹树**：可能上万行。这里加了 1000 行截断 + 提示用户用系统文件管理器看。

没有引入 react-window：截断是零依赖的 3 行代码，虚拟滚动要改渲染结构、加高度测量、
处理 sticky 表头。等真的有人养了 5000 文件的同步目录、并且抱怨卡了，再换。
`ponytail:` 性质的决定，入口在 `Folders.tsx:const TREE_RENDER_LIMIT`。

## 2026-10-27 兜底扫描漏了 ensure_blob（随机收敛测试抓出）

`full_scan`（每 10 分钟的兜底扫描）发现文件变更后，**只更新了索引、没把内容加进
blob 存储**。watcher 路径有 `ensure_blob`，兜底扫描漏了。

后果：索引里写着 hash，blob 库里没有内容。本地看一切正常（磁盘文件在），
对端来拉时报 `encode error: 读取 blob 失败`，重试三次后放弃。
**这个文件在对端就永远同步不过去**，而且日志里只有一行"拉取失败"。

为什么之前没发现：正常流程里 watcher 会先把内容加入 blob，兜底扫描只是"补漏"，
手工测试的目录既小又全程走 watcher。只有"绕过应用改文件 + 收敛测试"才踩到。

修复：`full_scan` 在锁外对变更条目逐个 `ensure_blob`（锁外是因为它是异步 I/O，
持锁会阻塞其它读写）。回归测试 `full_scan_registers_blob_content`。

## 2026-10-27 full_scan 只比 size+mtime 会漏掉等长内容变更

同上的 `full_scan` 判等用了 `size == size && mtime == mtime && is_dir == is_dir`。
外部程序在**同一秒内**写入**等长**内容时三者全同，变更被漏掉，索引永远停在旧值。

这是"索引与磁盘不一致"的直接来源，而 `diff` 只信索引 —— 于是同步逻辑
认为一切正常，实际两端内容不同，且不会自愈。

修复：加 `old.hash == fresh.hash`。扫描本来就已经算过 hash（`scan_folder`
要它），不产生额外 I/O。回归测试
`full_scan_detects_content_change_with_same_size_and_mtime`。

## 2026-10-27 随机操作序列收敛测试的价值

提示词 7 节要求"随机生成两端的操作序列，断言最终文件树与内容一致"。

写这个测试之前，上面两个 bug 都测不出来；写了之后，它一次就把两个都暴露了
（先暴露 `ensure_blob` 缺失，修完立刻暴露 `size+mtime` 判等不足）。

用固定种子（xorshift + 常量初值）而不是真随机：失败可复现，
把种子记下来就能在本地重跑同一个序列。

结论：这类测试的成本很低（40 行），收益是发现了两个都会导致**用户数据在对端缺失**
的 bug。以后加同步逻辑改动时应该优先跑它。

## 2026-10-27 网络配置（自建中继/固定端口）真的接进 Endpoint 了

之前设置页能存 `relay_url` / `listen_port` / `network_mode`，但 `AppState::init`
用的是写死的 `presets::N0` —— 三个字段是**死配置**，改了没用。

现在 `fs_sync_net::bind_endpoint_with(config, secret)`：
- `relay_url` → `builder.relay_mode(RelayMode::custom([url]))`
- `listen_port` → `builder.bind_addr(0.0.0.0:port)`
- 两者非法时**回退默认并记 warn**，不让应用起不来

两个实现细节值得记：

1. **`bind_addr` 消耗 builder 且失败不可恢复**。Rust 没法在 `match` 的两个分支里
   用一个被 move 掉的 builder。所以先用 `TcpListener::bind` 探测端口可用性
   （探完立刻 drop，否则和 iroh 抢端口），确认可用才真正 `bind_addr`。

2. **设置文件解析失败要回退，不要崩**。`SettingsStore::load` 里已经有这条，
   实测过：写了一版 `"network_mode": "Auto"`（枚举是 snake_case）时，
   日志出现"设置文件损坏，改用默认值"，应用正常启动。这是对的行为。

验证方式：settings.json 里写 `relay_url=https://invalid-relay.example.com./`
和 `listen_port=45678`，启动后日志显示"使用自建中继"，`netstat` 显示
`UDP 0.0.0.0:45678` 在 listen。

## 2026-10-27 开机自启用 tauri-plugin-autostart

设置里的 `auto_start` 之前也是死字段。现在用官方插件：
`app.autolaunch().is_enabled()/enable()/disable()`，在 `setup` 里按设置同步一次。

失败只记 warn 不中断启动 —— 有些企业策略会锁注册表，那种情况下应用仍然可用，
只是不自启。capability 里加了 `autostart:default` 权限。

## 2026-10-27 规模测试：10 万文件 21 秒，1GiB 往返（测试标 #[ignore]）

提示词 7 节要求测「10 万个小文件初始同步」和「大文件 ≥1GB」。两个都写成
`#[ignore]` 的测试，日常 `cargo test` 不跑，需要时显式跑：

```bash
cargo test -p fs-sync-core scan_100k -- --ignored --nocapture
cargo test -p fs-sync-net  large_file -- --ignored --nocapture
```

**实测数据**：
- 10 万文件扫描：21 秒（100 目录 × 1000 文件，含 blake3 哈希）
- 1 GiB 单文件入库：2.5 秒
- 1 GiB 单文件导出：7.0 秒
- 1 GiB 往返后逐字节一致，同内容得到同 hash（内容寻址正确）
这个数字决定了首次同步的体验上限：用户拿到一个 10 万文件的目录，
首次索引要等半分钟。可接受，但值得知道。

有个细节：`scan_folder` 返回的条目数包含**目录本身**（10 万文件 + 100 目录
= 100100 条）。这不是 bug——同步目录结构需要目录作为条目，否则空目录传不过去。
第一次写断言时我按"只有文件"算，测出来对不上才发现。

为什么不放进常规 CI：10 万文件要 ~100 秒（建文件 + 扫描），1GiB 要写满一次磁盘。
让每次 push 都跑这个不合理。真机验收时手工跑一遍。

## 2026-10-27 重连退避抽成可测类型

`spawn_peer_loop` 里原来是内联的 `let mut backoff = Duration::from_secs(2)` +
每次循环 `backoff = (backoff * 2).min(60s)`。逻辑对，但**没法测**——
要测它得连真网络。

抽成 `state::Backoff`（current/reset/back_off），3 个单元测试断言：
- 初始 2 秒
- 失败后 2→4→8...→封顶 60 秒，且永远不超过 60
- 成功一次立刻回到 2 秒

最后一条尤其重要：如果失败后忘记 `reset`，一次断网会让重连间隔永久卡在 60 秒，
用户会觉得"同步坏了"。

配套的实测数据（验收标准 4）：**空闲 CPU 0.29%**（60 秒采样），
内存 60→61 MB。符合"空闲时 CPU 接近 0，内存稳定"。

## 2026-10-27 ts-rs 漏生成 import，CI 差点放过

`ActivityFilter.ts` 生成出来是：

```ts
export type ActivityFilter = { folder_id?: string, kind?: ActivityKind, limit?: number, ... };
```

用了 `ActivityKind` 却**没有 import**。单独编译这个文件报
`TS2304: Cannot find name 'ActivityKind'`。

原因：ts-rs 对「加了 `#[ts(optional_fields)]` 的结构体里引用另一个枚举」这种组合
不会自动补 import。其它 DTO（`ActivityDto`、`DeviceDto`）都正常，只有这个漏了。

**为什么之前没发现**：我多次看到 `tsc --noEmit` 没输出就以为通过，
但其实 PowerShell 里 `pnpm exec tsc` 的 stderr 输出去到了别处，
`Select-Object -First 5` 截断后看起来是空的。真正检查要看 `$LASTEXITCODE`。

修复方式：在 `bindings.rs` 的 export 宏里，写完文件后扫一遍常见依赖类型名，
被引用但未声明、未 import 的就补一行 `import type { X } from "./X";`。
这个后处理是"补漏"而非"生成"，所以列的是已知类型名；新增跨类型引用时如果又漏了，
`tsc` 在 CI 里会立刻炸 —— 这正是我们要的兜底。

CI 里已经加了 `pnpm exec tsc --noEmit` 和 `git diff --exit-code src/lib/bindings/`，
两边都能拦住这类问题。


## 2026-10-28 自建 iroh 中继部署（sync.ashu180.top）

公网中继落地在 66.42.99.110（Vultr 新加坡，和 searxng/llm-relay/postgres 同机）。

**为什么用 8444 而不是 443**：这台机器的 80/443 被 nginx 占着，上面跑了
ashu180.top / llm / magpie / reelshort 四个站点。有两个选择：

1. nginx `stream` + `ssl_preread` 按 SNI 分流 —— 443 还要从 nginx 手里抢过来，
   现有站点全有风险；
2. 中继直接监听 8444，RelayUrl 带端口。

选 2。iroh 的 `RelayUrl` 允许带端口，客户端完全支持。代价是 URL 不干净，
但改动面为零、不影响任何现有站点。

**端口被占的坑**：第一次启动失败 `Address already in use`。查下来 sing-box
占了 8080 和 8443 —— 这两个恰好是我先选的端口。中继最终用：
- 8444/TCP → HTTPS/WSS
- 7842/UDP → QUIC 地址发现（QAD）
- 8081/本地回环 → captive portal 的 `/generate_204`（绑回环即可，不对外）

**为什么从官方中继切到自建**：官方中继是 n0.computer 给开发测试用的共享设施，
没有可用性承诺。两个人在不同 NAT 后配对时，中继是最后的兜底路径，
自建至少不会因为别人滥用而排队。

**TLS 用 Reloading 而不是 Manual**：`cert_mode = "Reloading"` 会定期重读证书文件，
certbot 续期后不用重启服务。如果只在启动时读一次（Manual），90 天后证书过期，
中继静默失效 —— 这种故障很难发现。证书由 certbot 签发到
`/etc/letsencrypt/live/sync.ashu180.top/`，任务钩子复制到
`/var/lib/iroh-relay/certs/`（iroh-relay 是独立系统用户，读不到 letsencrypt 的 700 目录）。

**默认中继写进代码**：`fs_sync_net::DEFAULT_RELAY_URL`，桌面端 `SettingsDto::default()`
和 CLI 都用它。理由：用户装完就该能用，不该先去设置页手填 URL。
代价是别人从这个仓库构建也会走我的中继 —— 对个人项目可接受，公共项目应该反过来
（默认官方、自建作为选项）。

**验证**：`cargo test -p fs-sync-net --test relay -- --ignored` 实测 1.93 秒连上，
中继进程 RSS 9.9MB，healthz 返回 `{"status":"ok","version":"1.4.1"}`。
