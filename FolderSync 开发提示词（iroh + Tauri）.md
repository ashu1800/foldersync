# FolderSync 开发提示词（iroh + Tauri）

> 用法：把下面“提示词正文”整体复制给 AI 编程助手（Claude Code、Cursor 等）。建议按里程碑分阶段投喂，每完成一个里程碑再进入下一个。

---

## 提示词正文

### 1. 角色与目标

你是一名资深 Rust 与桌面应用工程师。请帮我从零开发一款**文件夹实时双向 P2P 同步软件**，项目代号 `FolderSync`。

使用场景：我有两台电脑（公司、家里），需要把指定文件夹（主要是代码和工作文件）在两台电脑之间保持实时一致。任意一端新增、修改、删除、重命名文件，另一端应在数秒内同步。两台电脑可能都在 NAT 后面，没有公网 IP，不依赖第三方云盘存储我的文件。

### 2. 技术栈（已确定，不要更换）

- 网络层：**iroh**（Rust，基于 QUIC，用公钥寻址，自动打洞，失败时回退中继）。文件内容传输使用 **iroh-blobs**，设备间元数据与控制消息使用自定义 ALPN 协议。
- 后端语言：Rust（stable，edition 2021 或更新）。异步运行时 tokio。
- 文件监听：`notify` + `notify-debouncer-full`。
- 忽略规则：`ignore` crate（gitignore 语法）。
- 哈希：`blake3`。
- 序列化：`serde` + `postcard`（或 `bincode`），消息采用长度前缀帧。
- 本地状态库：SQLite（`rusqlite` 或 `sqlx`），WAL 模式。
- 日志：`tracing` + `tracing-subscriber`。
- 错误处理：`thiserror`（库）+ `anyhow`（应用层）。
- 桌面壳与前端：**Tauri 2** + **React 18 + TypeScript + Vite** + **Tailwind CSS** + **shadcn/ui** + **zustand**（状态）+ **TanStack Query**（命令调用缓存，可选）+ `lucide-react` 图标 + `qrcode.react`（配对二维码）。
- 包管理：Rust 用 Cargo workspace，前端用 pnpm。

**重要约束**：iroh 的 API 在 1.0 前后有过重命名与调整（例如节点标识相关类型、Endpoint 构建方式、presets 等）。**写任何 iroh 代码前，必须先查阅当前版本的 docs.rs 文档与官方仓库 examples，以实际版本为准，不得凭记忆编造 API。** 在 `Cargo.toml` 里固定具体版本号，并在 README 记录。若 iroh-blobs 与当前 iroh 版本不兼容，请先说明并选择兼容组合。

### 3. 总体架构

```
foldersync/
├─ Cargo.toml                 # workspace
├─ crates/
│  ├─ fs-sync-core/           # 同步引擎：扫描、索引、版本向量、差异计算、冲突处理（纯逻辑，可单测，不依赖网络）
│  ├─ fs-sync-net/            # 基于 iroh 的传输层：Endpoint、配对、协议编解码、blob 传输
│  ├─ fs-sync-store/          # SQLite 持久化：设备、文件夹、文件索引、墓碑、活动日志
│  └─ fs-sync-daemon/         # 把以上组合成后台服务，暴露 trait/事件总线，供 Tauri 与 CLI 共用
├─ apps/
│  ├─ desktop/                # Tauri 2 应用
│  │  ├─ src-tauri/           # Rust 命令与事件桥接
│  │  └─ src/                 # React 前端
│  └─ cli/                    # 无界面命令行（用于服务器、调试、集成测试）
└─ docs/                      # 协议文档、设计决策
```

分层原则：`core` 不依赖 iroh 与 Tauri；`net` 不关心文件系统细节；`daemon` 负责编排；前端只通过 Tauri 命令与事件与 daemon 交互。

### 4. 核心设计

#### 4.1 设备身份与信任

- 每台设备首次启动生成一个 iroh `SecretKey`，保存到应用数据目录（文件权限 0600；条件允许时使用系统钥匙串）。其公钥即设备 ID。
- 只有“已配对”的设备才允许连接。`accept` 连接时必须校验对端公钥在信任列表内，否则立即关闭。
- 设备有可编辑的显示名称（默认取主机名）。

#### 4.2 配对流程

1. 设备 A 点击“添加设备”，生成**配对码**：包含 A 的寻址信息（公钥 + 中继 URL 等，使用 iroh 提供的 ticket 能力）+ 一次性随机 token + 过期时间（默认 5 分钟）。以文本和二维码展示。
2. 设备 B 输入或扫码配对码，连接 A，发送 token 与自己的设备名。
3. A 端弹出确认（显示 B 的设备名与公钥指纹），用户确认后双方互相写入信任列表。token 一次性，使用后立即作废。
4. 配对完成后，之后重连只靠公钥，不再需要配对码。

#### 4.3 文件夹共享模型

- 一个“同步文件夹”有全局唯一 `folder_id`（UUID）。两端各自选择本地路径，路径可以不同。
- 创建文件夹时选择与哪些已配对设备共享。对端收到邀请后，选择本地路径并接受，才开始同步。
- 每个文件夹独立：可暂停/恢复、独立的忽略规则、独立的状态。

#### 4.4 文件索引与版本

每个文件条目（FileEntry）：

- `path`（相对路径，统一为 `/` 分隔、Unicode NFC 规范化）
- `size`、`mtime`、`blake3` 内容哈希
- `is_dir`、`deleted`（墓碑）
- `version`：**版本向量**（`device_id -> counter`）
- `modified_by`：最后修改的设备

比较规则：

- 一方版本向量支配另一方 → 以较新者为准，直接同步。
- 双方并发（互不支配）且内容哈希不同 → **冲突**。
- 哈希相同但版本向量不同 → 合并版本向量，不传内容。

冲突处理：不丢任何一方数据。按 mtime 较新者为胜出版本（相同则按设备 ID 字典序），落选版本改名为 `原名.sync-conflict-YYYYMMDD-HHMMSS-设备名.扩展名` 保留在同目录，并在前端“冲突”页出现，用户可选择“保留此版本/保留另一版本/都保留”。

删除：使用墓碑，保留 N 天（默认 30，可配置）。若一端删除、另一端并发修改，则视为冲突，**修改优先于删除**（恢复文件）。

重命名：初版允许以“删除 + 新建”实现，但需通过 blake3 哈希复用内容，避免重复传输；后续里程碑可优化为真正的 rename 检测。

#### 4.5 同步协议（自定义 ALPN：`/foldersync/1`）

消息使用长度前缀帧，所有消息带协议版本。核心消息类型：

- `Hello { protocol_version, device_name, folders: Vec<FolderSummary> }`
- `Index { folder_id, entries: Vec<FileEntry> }`（连接建立后做全量索引交换，之后为增量）
- `IndexUpdate { folder_id, entries }`
- `Request { folder_id, path, hash }`（请求文件内容；实际内容通过 iroh-blobs 按 hash 拉取）
- `FolderInvite / FolderInviteResponse`
- `Ping / Pong`、`Error { code, message }`

流程：连接 → Hello → 交换索引 → 计算差异 → 对需要拉取的文件通过 iroh-blobs 下载到临时目录 → 校验 blake3 → **原子替换**到目标路径 → 更新本地索引 → 广播 IndexUpdate。

#### 4.6 实时监听与回写保护

- `notify` 监听文件夹，防抖 300\~500ms，合并同一文件的连续事件。
- 应用自身写入（同步落盘）不能再触发一次同步：维护“自写抑制表”（路径 + 预期哈希 + 短 TTL），命中则忽略事件。
- 写文件一律先写同目录下的 `.foldersync-tmp-xxxx`，校验后 `rename` 覆盖。
- 兜底：每 N 分钟（默认 10）做一次轻量全量扫描（先比较 size+mtime，变化才算哈希），防止监听丢事件。
- 启动时先做一次扫描，与数据库索引比对，生成离线期间的变更。

#### 4.7 忽略规则

- 每个文件夹根目录支持 `.syncignore`（gitignore 语法）。
- 内置默认忽略（可在界面开关）：`node_modules/`、`target/`、`dist/`、`build/`、`__pycache__/`、`.venv/`、`.idea/`、`.vscode/`（可选）、`*.swp`、`.DS_Store`、`Thumbs.db`、`.foldersync-tmp-*`。
- **`.git/` 目录默认忽略**，并在界面提示原因：两端同时写入 `.git` 易导致仓库损坏，建议用 git 自己的 push/pull 管理版本。用户可手动开启，开启时给出风险确认。

#### 4.8 安全与健壮性

- **路径安全**：对端发来的路径必须校验——拒绝绝对路径、`..`、空组件、Windows 保留名与非法字符；落盘前确认规范化后的路径仍位于文件夹根目录内。
- 符号链接：初版不跟随、不同步符号链接，记录日志并跳过。
- 大小写：Windows/macOS 默认大小写不敏感，Linux 敏感。检测到仅大小写不同的冲突路径时，按冲突处理，不得互相覆盖。
- 文件被占用/只读：失败后指数退避重试，记录到活动日志，不阻塞其他文件。
- 断线重连：指数退避（1s 起，上限 60s），重连后重新交换索引。
- 可选限速：上传/下载带宽上限。
- 所有传输由 iroh/QUIC 加密，无需额外加密层。

#### 4.9 连接可观测性

界面需要展示与每台设备的连接状态：离线 / 连接中 / 已连接（**直连** 或 **中继**），以及延迟。默认使用 iroh 官方中继与发现；设置中允许填写自建中继 URL。

#### 4.10 Tailscale 适配（可选网络路径）

目标：两台电脑都安装了 Tailscale 时，同步流量优先走 Tailscale 网络（WireGuard 加密，使用 100.64.0.0/10 的 IP 或 MagicDNS 名称），不依赖 iroh 中继；没装 Tailscale 时，行为与前文完全一致。

设计原则：

- **iroh 仍是主体，Tailscale 只是一条附加的网络路径。** 设备身份与信任仍然由 iroh 公钥认证，不能因为对方在同一个 tailnet 里就放行。
- 不引入对 Tailscale 的强依赖：不调用 Tailscale 账号接口，不要求用户登录；探测失败一律静默回退。

实现要求：

1. **探测**：枚举本机网卡，识别 Tailscale 地址（IPv4 属于 100.64.0.0/10，IPv6 属于 fd7a:115c:a1e0::/48，常见接口名 `tailscale0`、`utun*`、`Tailscale`）。可选地调用 `tailscale status --json` 获取本机 MagicDNS 名称，命令不存在或失败时忽略。
2. **地址发布**：把本机 Tailscale 地址作为额外的直连地址加入配对码，以及后续连接时使用的对端地址信息。写代码前先查 iroh 当前版本如何给 Endpoint 地址添加自定义直连地址、如何固定监听端口，并验证 iroh 默认收集本机网卡地址时是否已把 Tailscale 网卡包含在内；以文档与实测为准，不要凭记忆假设。
3. **网络模式**（设置中可选，默认“自动”）：
   - 自动：交给 iroh 选择最佳路径，Tailscale 地址作为候选之一。
   - 优先 Tailscale：先尝试 Tailscale 地址，超时（如 3 秒）后回退到 iroh 的直连/中继。
   - 仅 Tailscale：关闭 iroh 公共中继与公共发现，只允许 Tailscale 地址连接，适合对隐私要求高的场景；需先确认当前 iroh 版本支持关闭中继与发现。
4. **手动地址**：设备详情中允许为对端填写 Tailscale IP 或 MagicDNS 名称（如 `home-pc.xxx.ts.net`）加端口，用于配对码过期后的重新连接。Tailscale 相关场景下提供“固定监听 UDP 端口”选项，便于在 Tailscale ACL 与系统防火墙中放行。
5. **连接类型展示**：在“直连/中继”之外增加“经 Tailscale”，并显示延迟；无法判断 Tailscale 自身是否走了它的中继时，不要猜测，只显示“经 Tailscale”。
6. **去重**：同一对设备始终只保持一个同步连接，不能因为多条路径同时可用而重复同步或重复建连。

新增 Tauri commands：`get_network_status()`（本机 Tailscale 检测结果、当前模式、监听端口）、`set_network_mode(mode)`、`add_manual_address(device_id, addr)`；新增 event：`network://status`。

验收：

- 两台电脑都装了 Tailscale 且关闭 iroh 中继（仅 Tailscale 模式）时，仍能完成配对并双向同步。
- 在“自动”模式下，关闭其中一台的 Tailscale，连接自动回退到 iroh 直连或中继，同步不中断（允许短暂重连）。
- 只有一台安装了 Tailscale 时，不报错，行为与普通模式一致。
- 测试：对地址分类函数（是否属于 Tailscale 网段）写单元测试；集成测试里用回环地址模拟“仅指定地址”连接。

### 5. 前端需求（Tauri 2 + React + TypeScript）

#### 5.1 视觉与交互

- 风格：简洁、克制的桌面工具风，自动跟随系统亮/暗色主题，使用 shadcn/ui 组件。
- 左侧导航栏 + 右侧内容区；窗口最小尺寸 900×600。
- 系统托盘图标：显示同步状态（空闲/同步中/出错/暂停），菜单包含“打开主窗口、暂停全部/恢复、退出”。关闭窗口默认最小化到托盘。
- 支持开机自启（可在设置中开关）。
- 界面语言：中文为默认，预留 i18n（`react-i18next`），同时提供英文。

#### 5.2 页面

1. **概览（Dashboard）**
   - 顶部总状态：全部已同步 / 同步中（进度、剩余文件数）/ 有错误 / 已暂停。
   - 设备卡片：设备名、在线状态、连接方式（直连/中继/经 Tailscale）、延迟。
   - 文件夹卡片：名称、本地路径、状态、最近同步时间、待处理文件数、暂停/恢复按钮。
2. **文件夹**
   - 列表 + “添加文件夹”向导：选择本地目录（Tauri dialog）→ 选择共享设备 → 配置忽略规则 → 完成。
   - 文件夹详情：文件树只读浏览（显示每个文件的同步状态图标）、忽略规则编辑器（带默认规则开关）、高级选项（墓碑保留天数、扫描间隔）、删除该文件夹同步（不删本地文件，需二次确认）。
   - 接收到对端邀请时弹出通知卡片，选择本地保存路径后接受或拒绝。
3. **设备**
   - 已配对设备列表：重命名、查看公钥指纹、取消配对。
   - “添加设备”对话框，两个标签页：**生成配对码**（文本 + 二维码 + 倒计时 + 复制按钮）、**输入配对码**（粘贴 + 提交）。配对确认时显示对端设备名与指纹供核对。
4. **活动**
   - 实时事件流：上传/下载/删除/冲突/错误，带时间、文件路径、大小、方向、进度条。
   - 支持按文件夹、类型过滤，支持暂停滚动与清空显示（不清数据库）。
5. **冲突**
   - 列出所有 `sync-conflict` 文件，展示两个版本的大小、修改时间、来源设备；操作：打开所在目录、保留此版本、保留另一版本、保留两者。文本文件提供简易并排 diff 预览（可用 `react-diff-viewer` 或类似库，初版可后置）。
6. **设置**
   - 设备名称、开机自启、托盘行为、带宽限速、中继服务器（默认/自定义）、网络模式（自动/优先 Tailscale/仅 Tailscale）、固定监听端口、全局默认忽略规则、日志级别、打开日志目录、检查更新（占位）。

#### 5.3 前后端接口约定

Tauri **commands**（前端调用，均返回 `Result<T, ErrorDto>`）：

- `get_overview()`
- `list_devices()`、`create_pairing_code()`、`accept_pairing_code(code)`、`confirm_pairing(request_id, accept)`、`rename_device(id, name)`、`remove_device(id)`
- `list_folders()`、`add_folder(path, device_ids, ignore_config)`、`remove_folder(id)`、`pause_folder(id)`、`resume_folder(id)`、`pause_all()`、`resume_all()`
- `get_folder_tree(folder_id, subpath)`、`get_ignore_rules(folder_id)`、`set_ignore_rules(folder_id, rules)`
- `accept_invite(invite_id, local_path)`、`reject_invite(invite_id)`
- `list_conflicts(folder_id?)`、`resolve_conflict(conflict_id, resolution)`
- `list_activity(filter, limit, cursor)`
- `get_settings()`、`update_settings(patch)`

Tauri **events**（后端推送，前端监听并更新 zustand store）：

- `device://status`（连接状态变化）
- `folder://status`（文件夹状态与计数）
- `sync://progress`（单文件进度，需节流，每文件每 200ms 最多一次）
- `sync://activity`（新活动条目）
- `pairing://request`（对端请求配对，待用户确认）
- `folder://invite`（收到共享邀请）
- `conflict://new`

要求：所有 DTO 在 Rust 侧用 `serde` 定义，并使用 `specta` 或 `ts-rs` **自动生成 TypeScript 类型**，避免前后端手写类型漂移。

#### 5.4 前端工程要求

- 目录建议：`src/pages`、`src/components`、`src/stores`、`src/lib/tauri.ts`（命令与事件的类型化封装）、`src/i18n`。
- 严格 TypeScript（`strict: true`），ESLint + Prettier。
- 所有列表需有空状态、加载状态、错误状态。
- 长列表（活动、文件树）使用虚拟滚动。
- 对高频事件做节流，避免界面卡顿。

### 6. 里程碑（请严格按顺序实现，每个里程碑结束时给出：已完成内容、如何运行与验证、遗留问题）

- **M0 脚手架**：Cargo workspace、Tauri 2 + React 项目、CI（fmt/clippy/test）、README。验证 iroh 示例：两个进程通过公钥互相连接并回显一条消息（先在同机，再在两台不同网络的机器）。
- **M1 本地引擎**：`fs-sync-core` + `fs-sync-store`。扫描、哈希、SQLite 索引、版本向量、差异计算、冲突命名、忽略规则、路径校验，全部有单元测试。
- **M2 配对与连接**：设备身份、配对码流程、信任列表、`/foldersync/1` 协议的 Hello 与索引交换、断线重连、连接类型（直连/中继/经 Tailscale）上报，以及 Tailscale 地址探测（见 4.10）。CLI 可完成配对并显示状态。
- **M3 单向同步**：A→B 的文件新增/修改/删除，iroh-blobs 传输，原子落盘，校验，进度事件。
- **M4 双向实时同步**：notify 监听 + 防抖 + 自写抑制 + 增量 IndexUpdate + 冲突处理 + 墓碑 + 周期兜底扫描 + 离线期间变更合并。
- **M5 桌面应用**：完成第 5 节全部页面、托盘、自启、事件桥接、类型生成、i18n。
- **M6 打磨与发布**：限速、自建中继配置、日志与诊断包导出、Windows/macOS/Linux 打包（Tauri bundler）、自动更新（可选）、使用文档。

### 7. 测试与验收

**自动化测试**

- 单元：版本向量比较、冲突判定、路径校验（含恶意路径用例）、忽略规则、墓碑过期。
- 集成：同一进程内启动两个 daemon，各用独立临时目录和独立 iroh Endpoint，覆盖：新增/修改/删除/重命名、双端同时修改同一文件（冲突）、一端删除另一端修改、离线期间变更后重连、大文件（≥1GB）中断续传、10 万个小文件初始同步。
- 收敛性测试：随机生成两端的操作序列，断言最终两端文件树与内容一致（冲突副本也应一致）。

**验收标准**

- 两台处于不同 NAT 后的电脑，配对后在 5 秒内完成首次连接；大多数家庭/公司网络下能直连，不能直连时自动走中继且同步不中断。
- 在 A 端保存一个文件，B 端在 3 秒内（局域网或直连）看到更新。
- 任何情况下不丢失用户数据：冲突必产生副本，删除可通过墓碑逻辑恢复一致性，不会因同步循环无限回写。
- 空闲时 CPU 占用接近 0，内存稳定。
- 关闭再开机、断网再联网后，自动恢复同步，无需用户干预。

### 8. 工作方式要求

1. 开始前先输出你的**实施计划**与对本方案的**疑问或风险**（尤其是 iroh 与 iroh-blobs 的版本兼容、跨平台文件监听差异），确认后再编码。
2. 每个里程碑完成后运行 `cargo fmt`、`cargo clippy -- -D warnings`、`cargo test` 并确保通过；前端运行 `pnpm lint` 与 `pnpm build`。
3. 不要一次性生成整个项目；小步提交，每步可运行、可验证。
4. 遇到不确定的 iroh API，先查文档与示例，再写代码；不确定就明说，不要臆造。
5. 代码注释使用中文，公共 API 写 rustdoc；关键设计决策写入 `docs/decisions.md`。
6. 协议格式一旦确定，写入 `docs/protocol.md`，并在消息中携带版本号，保证以后可升级。
7. 不引入与需求无关的重量级依赖；新增依赖时说明理由。

---

## 补充建议（给你自己看的，不必发给 AI）

- **先用 CLI 跑通 M0 到 M4，再做界面。** 同步引擎的难点都在后端，界面可以最后做，不要被前端拖慢核心验证。
- **首次真实测试用小目录**，不要直接指向重要代码仓库；第一个版本务必先备份。
- **参考阅读**：iroh 官方文档与仓库 examples、iroh-blobs 文档、Syncthing 的块交换协议（BEP）文档，重点看它的索引交换与冲突策略。
- 如果你不想在 `.git` 上妥协，可以后续做“git 感知”模式：同步工作区文件，但让 `.git` 由 git 自己处理。
