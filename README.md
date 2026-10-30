# FolderSync

文件夹实时双向同步软件。两台电脑（如公司/家里）在 NAT 后也能同步，不依赖第三方云盘。网络层为 **SSH + SFTP + Tailscale（直连）** 或 **自建中继服务器** 两种方案。

## 技术栈与版本

| 组件 | 版本 | 说明 |
|------|------|------|
| russh | **0.65.0** | 嵌入应用的 SSH 服务端/客户端（公钥认证、子系统） |
| russh-sftp | **3.0.1** | SFTP 文件传输与沙箱文件系统 |
| Tauri | 2.x | 桌面壳 |
| Rust | 1.91+（workspace MSRV） | edition 2024 |
| 前端 | React 18 + TypeScript + Vite 6 | pnpm |
| rusqlite | 0.37（bundled） | SQLite：索引、信任列表、活动 |
| Tailscale | 可选 | 两台电脑登录同一 tailnet 后走 SFTP 直连 |

## 架构

- 每台设备内置 SSH 服务端（russh），默认监听 **TCP 2222**；对端通过 SSH 公钥认证后访问。
- 首次启动自动生成：
  - `~/.foldersync/ssh_identity`：本机 SSH 身份私钥（设备 ID = 公钥 SHA256 指纹）；
  - `~/.foldersync/ssh_host_ed25519`：本机 SSH 服务端主机密钥（配对时由对端固定校验）。
- SFTP 根目录为 `~/.foldersync/share`；文件内容按 blake3 寻址存放在 `share/blobs/<blake3>`，下载后先校验再原子落盘。
- 配对码交换双方 SSH 公钥 + 主机公钥，并携带 Tailscale 地址与中继地址；配对完成后只用公钥认证。
- 控制消息走 SSH 子系统 `foldersync`，文件走 SFTP，配对走 `foldersync-pair`。协议细节见 [docs/protocol.md](docs/protocol.md)。
- 网络模式：
  - `auto`（默认）：Tailscale 可用则 SFTP 直连，否则走中继；
  - `tailscale`：只走 Tailscale 直连；
  - `relay`：只走中继。

## 结构

```
crates/
  fs-sync-core/    同步引擎纯逻辑（扫描、版本向量、冲突、忽略规则）
  fs-sync-net/     SSH/SFTP 传输层（身份、协议、配对、中继客户端、沙箱文件系统）
  fs-sync-store/   SQLite 持久化
  fs-sync-daemon/  编排层（core+net+store 组合，供桌面与 CLI 共用）
apps/
  cli/             命令行（serve / pair / join / status / sync）
  relay/           中继服务器二进制 foldersync-relay
  desktop/         Tauri 2 + React 前端
docs/              协议与设计决策
```

## 运行

```bash
# CLI：身份、配对与同步
cargo run -p foldersync-cli -- status                      # 设备 ID、指纹、Tailscale 状态、中继、信任列表
cargo run -p foldersync-cli -- serve --auto-confirm        # 启动 SSH 服务端与同步（调试用自动确认配对）
cargo run -p foldersync-cli -- pair                        # 生成配对码（5 分钟、一次性）
cargo run -p foldersync-cli -- join <配对码>                # 用配对码完成配对
cargo run -p foldersync-cli -- sync <本地路径> <对端设备ID>  # 单向推送一次（调试）

# 中继服务器（独立 binary）
cargo run -p foldersync-relay -- --port 9443 --data-dir ./foldersync-relay-data

# 桌面应用
cd apps/desktop && pnpm install && pnpm tauri dev
```

默认数据目录：`~/.foldersync`（可用 `--data-dir` 覆盖）。中继地址可在 CLI 用 `--relay tcp://IP:9443` 或环境变量 `FOLDERSYNC_RELAY` 指定；桌面端在设置页填写。

## 数据目录

`~/.foldersync/`（可用 `--data-dir` 覆盖）：

| 路径 | 内容 | 备份价值 |
|------|------|----------|
| `ssh_identity` | SSH 身份私钥（设备 ID 来源） | **必须备份**，丢失等于换设备 |
| `ssh_host_ed25519` | SSH 服务端主机密钥 | **必须备份**，丢失后需重新配对 |
| `state.sqlite3` | 索引、信任列表、活动、配对 token | 有价值 |
| `settings.json` | 界面与网络设置 | 可重建 |
| `logs/foldersync.log.*` | 按天滚动日志 | 排障用 |
| `share/` | SFTP 沙箱根目录（对端可读写） | — |
| `share/blobs/` | 按 blake3 寻址的内容缓存（可重建） | 无 |

旧版 iroh 留下的 `secret.key` 已不再使用，可在确认不再回退旧版本后删除。

## 网络方案

### Tailscale 直连（推荐）

1. 两台电脑都安装 Tailscale，并登录**同一个 tailnet**。
2. 确认 `tailscale status` 显示已连接，能看到对方设备。
3. FolderSync 只监听本机 SSH 端口（默认 TCP 2222），需要防火墙放行；安装器会自动添加，失败时可在设置页点「初始化/修复 SSH 环境」重试。

配对码会带 Tailscale 地址，`auto` 模式优先走这条路径。

### 中继服务器

没有 Tailscale、或两端网络不允许直连时使用。中继是独立二进制 `foldersync-relay`，服务端按房间隔离 SFTP 空间；客户端只主动出站连接，不需要在客户端开放入站端口。官方实例：`tcp://36.137.140.126:8444`。

```bash
foldersync-relay --port 9443 --data-dir ./foldersync-relay-data
```

部署方需要放行 **TCP 9443**。完整搭建步骤（Windows/Linux、防火墙、systemd/服务、安全说明）见 [docs/relay-setup.md](docs/relay-setup.md)。

## 实现状态

- [x] SSH 服务端 + 公钥认证 + `foldersync` / `foldersync-pair` 子系统
- [x] SFTP 沙箱（`share/`，路径穿越与符号链接防护）与 blake3 内容寻址
- [x] 配对：SSH 直连配对 + 中继文件交换配对；配对码 5 分钟有效、一次性 token
- [x] 双端索引交换、增量同步、冲突副本、断线重连
- [x] 中继服务器二进制 `foldersync-relay`（按房间隔离）
- [x] 桌面应用：配对/设备/文件夹/活动/设置/诊断页面
- [ ] macOS/Linux 实机验证（当前主要在 Windows 上验证）

## 打包与分发

```bash
cd apps/desktop
pnpm exec tauri build          # 产出 NSIS 安装包
```

产物（版本号以 `tauri.conf.json` 的 `version` 为准）：

| 文件 | 说明 |
|------|------|
| `target/release/bundle/nsis/FolderSync_<version>_x64-setup.exe` | Windows 安装包（currentUser 安装，无需管理员） |
| `target/release/foldersync-desktop.exe` | 免安装可执行文件 |

安装包内置中文/英文安装向导。安装时 `installer.nsh` 会尝试添加防火墙规则 `FolderSync SSH`（TCP 2222 + 程序路径）和程序级 TCP 放行；`currentUser` 模式没有管理员权限时 netsh 可能失败，应用内设置页的「初始化/修复 SSH 环境」会再次尝试（会弹 UAC）。macOS/Linux 打包需在对应平台执行同样命令。

## 自动更新与版本发布

### 客户端行为

从 0.2.5 起，应用对接自建的 Basic 版本更新服务（实现见 `apps/desktop/src-tauri/src/updater.rs`）：

- **检测地址**：`GET https://ashu180.cn/api/version/info?uuid=af61462d-d513-4ef5-9064-f7619550f615`
- **版本号**：服务端用单调递增的整数，由 semver 换算：`major*10000 + minor*100 + patch`（0.2.5 → 205）
- **触发方式**：启动 15 秒后检测一次，之后每 6 小时一次；设置页也能手动「检查更新」。检测失败只记日志，不影响同步
- **强制更新**（`update=true`）：弹窗不能关闭，只能升级；普通更新可以「稍后」
- **禁用版本**（`status=1`）：不提供下载
- **校验**：下载支持断点续传、最多重试 3 次；装之前必须同时通过 **MD5** 与 **Ed25519 签名**校验。
  没有签名或验签失败一律拒绝安装
- **安装**：以被动模式运行 NSIS 安装器（`/P /R /UPDATE`），装完自动重启，`~/.foldersync/` 原样保留

0.2.4 及更早的版本仍用 Tauri updater 检查 GitHub Releases 的 `latest.json`。
所以发布流程同时保留 GitHub Release，旧版本靠它升到 0.2.5，之后改用新服务。

### 发版流程

```bash
# 1. 改版本号（三处一起改，脚本会校验一致性）
./scripts/check-version.sh 0.2.5

# 2. 在 CHANGELOG.md 写好 `## 0.2.5` 段落（会作为更新说明展示给用户），提交
git add -A && git commit -m "release: v0.2.5"

# 3. 打 tag 并推送 —— 这一步触发 GitHub Actions 自动构建与发布
git tag v0.2.5
git push origin main v0.2.5
```

推 tag 后 `.github/workflows/release.yml` 会：

1. 先复用 `ci.yml` 跑 fmt/clippy/test/tsc/eslint（不通过就不发版）
2. 构建 Windows 安装包，创建 GitHub Release 并上传安装包、`.sig`、`latest.json`（给旧版本用）
3. `publish-basic`：用 `scripts/publish_update.py` 计算 MD5、Ed25519 签名，
   登录 → presign → PUT → confirm 发到更新服务，再写入更新说明并回读校验

需要强制更新时，在 Basic 管理后台把该版本的「强制更新」打开即可，不用重新发版。
手动发布（例如 CI 不可用）：

```bash
export BASIC_ADMIN_EMAIL=... BASIC_ADMIN_PASSWORD=...
export FOLDERSYNC_UPDATE_SIGNING_KEY="$(cat ~/.foldersync-release/update_signing_key.hex)"
python scripts/publish_update.py FolderSync_0.2.5_x64-setup.exe 0.2.5 --changelog CHANGELOG.md
```

### 密钥与凭据

| 项 | 位置 | 说明 |
|----|------|------|
| 更新包签名私钥（Ed25519） | `~/.foldersync-release/update_signing_key.hex`（本机）+ GitHub Secret `FOLDERSYNC_UPDATE_SIGNING_KEY` | **丢失后客户端无法再验过任何更新**，务必另外备份 |
| 更新包签名公钥 | `updater.rs` 的 `SIGNING_PUBKEY` | 随代码提交，公开无害 |
| 更新服务管理员账号 | GitHub Secrets `BASIC_ADMIN_EMAIL` / `BASIC_ADMIN_PASSWORD` | 只用于 CI 上传 |
| Tauri updater 私钥（旧） | `~/.tauri/foldersync.key` + GitHub Secret `TAURI_SIGNING_PRIVATE_KEY` | 只给 0.2.4 及更早的客户端签 `latest.json`；旧客户端都升级后可删 |

## 自建中继

默认预填官方自建中继 `tcp://36.137.140.126:8444`（2026-10-30 重新部署）；不需要时可在设置页清空，或改成自己的服务器。

```bash
cargo build --release -p foldersync-relay
./target/release/foldersync-relay --port 9443 --data-dir ./foldersync-relay-data
```

客户端在设置页填写 `tcp://<服务器IP>:9443`，网络模式选 `auto` 或 `relay`；两端建议填写同一个中继地址。配对码也会携带中继地址，便于对端首次配对。

安全边界：中继是**自建可信服务器**，SSH/SFTP 只保护传输链路；客户端上传的文件会以普通文件形式落在中继服务器的磁盘上，应用层没有做端到端加密存储。服务器管理员/入侵者能读取这些文件，请用磁盘加密、文件权限、备份与防火墙保护它。详见 [docs/relay-setup.md](docs/relay-setup.md)。

## 已知限制

| 限制 | 影响 | 升级路径 |
|------|------|----------|
| `folder_id` 由文件夹名派生 | 两台机器上同名文件夹自动配对；不同名文件夹不会同步 | 引入邀请码 + UUID，替换 `folder_id` 的生成方式 |
| 限速是“传输后结算” | 用户感知准确，网络统计不精确 | 在 `limits.rs` 加流包装器，调用点不动 |
| 文件树超过 1000 行截断 | 大文件夹只看得到前 1000 项 | 换成 react-window 虚拟滚动 |
| 活动列表最多 500 条 | 更早的记录要从数据库查 | 加分页游标（DTO 里已有 `before_id`） |
| 不跟随符号链接 | 链接目标不会同步 | 初版有意为之，见设计决策 |
| 中继落盘文件未做应用层加密 | 中继管理员/入侵者可读取文件内容 | 自建可信服务器 + 磁盘加密；需要更强隔离时做客户端加密 |
| macOS/Linux 未实机验证 | 主要在 Windows 上验证 | Tauri bundler 已配好，需在对应平台构建 |
| 旧 iroh 身份/信任不兼容 | 升级后旧设备记录无法继续认证 | 双方升级后重新配对 |

## 排障

- **界面空白**：debug 构建需要先起 `pnpm dev`（连 5173 端口）；release 构建内置前端资源。
- **配不上对**：先确认两台机器时钟差不超过配对码有效期（5 分钟）；再看网络路径：
  - Tailscale：两端登录同一 tailnet，`tailscale status` 能看到对方；
  - 中继：中继地址可连通、两端使用同一个中继；
  - 防火墙：对端 SSH 端口（默认 TCP 2222）是否放行，中继服务器 TCP 9443 是否放行。
- **一直走中继**：检查设置里的网络模式是否为 `auto`/`tailscale`，以及 Tailscale 是否已登录并运行；`auto` 在 Tailscale 未运行时按设计走中继。
- **防火墙规则没加上**：设置页 →「初始化/修复 SSH 环境」，会以管理员权限重试添加 `FolderSync SSH` 规则。
- **中继连不上**：在客户端执行 `Test-NetConnection <中继IP> -Port 9443`（Linux 用 `nc -vz <中继IP> 9443`），并检查服务器防火墙/云安全组是否放行 TCP 9443。
- **导出诊断包**：设置页 →「导出诊断包」，生成的文件不含文件内容与私钥，可直接贴给开发者。
- **某个文件夹不动了**：暂停再恢复会重新扫描；仍不动就看日志里有没有 `拉取 blob 失败` / `从中继拉取 blob 失败`。
