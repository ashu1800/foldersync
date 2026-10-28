# FolderSync

文件夹实时双向 P2P 同步软件（iroh + Tauri）。两台电脑（如公司/家里）在 NAT 后也能通过公钥寻址自动打洞直连，不依赖第三方云盘。

## 技术栈与版本锁定

| 组件 | 版本 | 说明 |
|------|------|------|
| iroh | **1.4.1** | P2P QUIC 传输，公钥寻址。注意 1.x 已将 `NodeId`/`NodeAddr` 更名为 `EndpointId`/`EndpointAddr`，Endpoint 构建使用 `Endpoint::bind(iroh::endpoint::presets::N0)` |
| iroh-blobs | **0.103.1** | 内容寻址 blob 传输（0.103.0 已被 yank，勿用） |
| Tauri | 2.x | 桌面壳（tauri 2.12 验证） |
| Rust | 1.99+（workspace MSRV 1.91） | edition 2024 |
| 前端 | React 18 + TypeScript + Vite 6 | pnpm |

iroh 1.4.1 关键 API 速记（以 docs.rs 为准）：

- 绑定：`Endpoint::bind(presets::N0)`（N0 preset = 官方 DNS 寻址 + 官方中继 + ring TLS）。
- 服务：`Router::builder(endpoint).accept(b"/foldersync/1", handler).spawn()`。
- 连接：`endpoint.connect(EndpointAddr, alpn)`，`EndpointAddr::from(EndpointId)` 仅凭公钥即可（经 DNS pkarr 发现 + 中继）。
- 流：`conn.open_bi()` / `conn.accept_bi()`；`send.finish()` 后**不要**在两端都 `conn.closed().await`（会互等，一端直接 drop 即可）。

## 结构

```
crates/
  fs-sync-core/    同步引擎纯逻辑（扫描、版本向量、冲突、忽略规则）
  fs-sync-net/     iroh 传输层（协议、回显、后续 blob 传输）
  fs-sync-store/   SQLite 持久化
  fs-sync-daemon/  编排层（core+net+store 组合，供桌面与 CLI 共用）
apps/
  cli/             命令行（foldersync serve / ping）
  desktop/         Tauri 2 + React 前端
docs/              协议与设计决策
```

## 运行

```bash
# CLI：身份与配对
cargo run -p foldersync-cli -- status                      # 设备 ID、指纹、Tailscale 状态、信任列表
cargo run -p foldersync-cli -- serve --auto-confirm        # 启动节点（调试用自动确认配对）
cargo run -p foldersync-cli -- pair                        # 生成配对码（5 分钟、一次性）
cargo run -p foldersync-cli -- join <配对码>                # 用配对码完成配对

# 同步一个文件夹到对端（M3）
cargo run -p foldersync-cli -- sync <本地路径> <对端EndpointId>

# M0 连通性自检
cargo run -p foldersync-cli -- ping <ENDPOINT_ID> "hello"

# 桌面应用（M5 完成前仅有占位窗口）
cd apps/desktop && pnpm install && pnpm tauri dev
```

默认数据目录：`~/.foldersync`（可用 `--data-dir` 覆盖），内含 `secret.key`（设备身份）、`state.sqlite3`（索引/信任/活动）、`blobs`（服务端内容缓存）、`blobs-client`（同步客户端缓存）。

> 注意：`serve` 与 `sync` 是两个进程，各自打开独立的 blob 库（redb 不支持跨进程争锁）。要同时跑两者，用不同 `--data-dir` 或等 M4 的常驻单进程模式。

## 当前进度

- [x] M0 脚手架 + iroh 跨机连通验证（家 ↔ 公司，双 NAT，经中继/打洞）
- [x] M1 本地引擎（core + store，单元测试覆盖版本向量/冲突/恶意路径/忽略规则/墓碑）
- [x] M2 配对与连接（真实跨机配对：中继连接、8ms 延迟、双向信任、伪造 token 被拒）
- [x] M3 单向同步（真实跨机传输 3 个文件含嵌套目录，内容一致，失败 0）
- [x] M4 双向实时同步（实测：新增 1.0~1.1s、删除 1.0s 双向传播；冲突保留副本不丢数据且不重复）
- [x] M5 桌面应用（6 个页面、托盘、开机自启、关闭到托盘、6 个事件推送、ts-rs 类型自动生成、中英双语、ESLint+Prettier）
- [x] M6 打磨发布（带宽限速、自建中继、固定端口、日志落盘、诊断包导出、规模测试、NSIS 安装包）
  - 自建中继已实际部署：`https://sync.ashu180.top:8444`（见下节）。

## 同步实测数据（家 ↔ 公司，经 Tailscale/打洞）

| 场景 | 结果 |
|------|------|
| 新增文件（单向） | 1.0 ~ 1.1 秒 |
| 删除文件（墓碑传播） | 1.0 秒 |
| 双向同时新增 | 各自 1.0 ~ 1.1 秒，最终列表一致 |
| 同文件并发修改 | 生成 `名字.sync-conflict-时间-设备名.扩展名`，两边原文件都保留 |
| 连接类型演进 | 中继 → 经 Tailscale → 直连（iroh 自动打洞升级） |
| 10 万文件扫描 | 21 秒（100 目录 × 1000 文件，含 blake3） |
| 1 GiB 单文件往返 | 见下方"规模测试" |
| 测试总数 | 177 个（含 16 个双节点集成测试） |

## 规模测试（手工运行，日常测试不跑）

```bash
cargo test -p fs-sync-core scan_100k -- --ignored --nocapture
cargo test -p fs-sync-net  large_file -- --ignored --nocapture
```

| 场景 | 结果 |
|------|------|
| 10 万个小文件扫描 | 21 秒（含 blake3 哈希） |
| 1 GiB 单文件入库 | 2.5 秒 |
| 1 GiB 单文件导出 | 7.0 秒 |
| 1 GiB 完整性 | 逐字节一致，同内容同 hash |

这两个测试标了 `#[ignore]`：跑一次要写满磁盘、耗时数分钟，不适合放进每次 CI。

## 打包与分发

```bash
cd apps/desktop
pnpm exec tauri build          # 产出 NSIS 安装包
```

产物：

| 文件 | 大小 | 说明 |
|------|------|------|
| `target/release/bundle/nsis/FolderSync_0.1.0_x64-setup.exe` | 8.4 MB | Windows 安装包（当前用户安装，无需管理员） |
| `target/release/foldersync-desktop.exe` | 34 MB | 免安装可执行文件 |

安装包内置中文/英文安装向导。macOS/Linux 打包需在对应平台执行同样命令（Tauri bundler 已配置但本机未验证）。

## 数据目录

`~/.foldersync/`（可用 `--data-dir` 覆盖）：

| 路径 | 内容 | 备份价值 |
|------|------|----------|
| `secret.key` | 设备身份私钥 | **必须备份**，丢失等于换设备 |
| `state.sqlite3` | 索引、信任列表、活动、配对 token | 有价值 |
| `settings.json` | 界面与网络设置 | 可重建 |
| `logs/foldersync.log.*` | 按天滚动日志 | 排障用 |
| `blobs/` | 内容缓存（可重建，删了会重新拉取） | 无 |

## 验收标准实测

| 标准 | 实测结果 |
|------|----------|
| 不同 NAT 后配对后 5 秒内连接 | 8ms（中继），随后自动升级到直连 |
| A 端保存后 B 端 3 秒内看到 | 1.0 ~ 1.1 秒 |
| 冲突必产生副本，不丢数据 | `名字.sync-conflict-时间-设备名.扩展名`，两端原文件都保留 |
| 不会因同步循环无限回写 | 无变更的重复同步不递增版本（有测试锁定） |
| 空闲 CPU 接近 0 | 0.29%（60 秒采样） |
| 内存稳定 | 60 → 61 MB（60 秒） |
| 断网重连自动恢复 | 无限重连 + 指数退避 2→60 秒，成功即重置 |
| 关闭再开机自动恢复 | 文件夹配置持久化到 SQLite，启动时 `restore_folders` |

## 自动更新与版本发布

### 客户端行为

应用通过 [Tauri updater](https://v2.tauri.app/plugin/updater/) 检查 GitHub Releases：

- **检查地址**：`https://github.com/ashu1800/foldersync/releases/latest/download/latest.json`
- **触发方式**：设置页 →「检查更新」（手动，不在后台偷偷检查）
- **签名校验**：每个更新包都带 minisign 签名，公钥内置在 `tauri.conf.json`
  的 `plugins.updater.pubkey`。签名不匹配会拒绝安装，不会装到篡改的包。
- **安装后自动重启**：`relaunch()`，数据目录 `~/.foldersync/` 原样保留。

### 发版流程

```bash
# 1. 改版本号（三处一起改，脚本会校验一致性）
./scripts/check-version.sh 0.2.0

# 2. 提交
git add -A && git commit -m "release: v0.2.0"

# 3. 打 tag 并推送 —— 这一步触发 GitHub Actions 自动构建与发布
git tag v0.2.0
git push origin main --tags
```

推 tag 后 `.github/workflows/release.yml` 会：

1. 先复用 `ci.yml` 跑 fmt/clippy/test/tsc/eslint（不通过就不发版）
2. 在 Windows / macOS(arm64+x64) / Ubuntu 四平台并行构建
3. 用 `TAURI_SIGNING_PRIVATE_KEY` 签名更新包
4. 创建 GitHub Release，上传安装包 + `.sig` + `latest.json`
5. 已装旧版的用户点「检查更新」就能看到新版本

### 密钥管理

| 项 | 位置 | 说明 |
|----|------|------|
| 私钥 | `~/.tauri/foldersync.key`（本机）+ GitHub Secret `TAURI_SIGNING_PRIVATE_KEY` | **丢失后无法再给老用户发更新**，务必备份 |
| 公钥 | `tauri.conf.json` 的 `plugins.updater.pubkey` | 随代码提交，公开无害 |

私钥没有密码（`TAURI_SIGNING_PRIVATE_KEY_PASSWORD` 为空），因为 CI 里无人值守。
若日后要加密码，改 secret 并给 workflow 传该变量即可。

## 自建中继

公网中继部署在 `66.42.99.110`（Vultr 新加坡），域名为 `sync.ashu180.top`。

| 项 | 值 |
|----|-----|
| RelayUrl | `https://sync.ashu180.top:8444` |
| HTTPS / WSS | TCP 8444（**必须带端口**：443/80 被同机 nginx 占用） |
| QUIC 地址发现（QAD） | UDP 7842，用于探测公网地址、提高打洞成功率 |
| 服务 | systemd `iroh-relay.service`，二进制 `/usr/local/bin/iroh-relay` |
| 配置 | `/etc/iroh-relay/config.toml` |
| 证书 | certbot 签发到 `/etc/letsencrypt/live/sync.ashu180.top/`，复制到 `/var/lib/iroh-relay/certs/`，`cert_mode = "Reloading"` 自动热加载 |
| 版本 | 与客户端同版本 `iroh-relay 1.4.1`（中继协议必须对齐） |

应用默认就用这个中继（`SettingsDto::default()`），设置页里清空「中继服务器」即可退回 iroh 官方中继。

运维命令：

```bash
systemctl status iroh-relay
journalctl -u iroh-relay -n 50 --no-pager
curl --fail https://sync.ashu180.top:8444/healthz
```

连通性自查（需要外网）：

```bash
cargo test -p fs-sync-net --test relay -- --ignored --nocapture
```

## 已知限制

| 限制 | 影响 | 升级路径 |
|------|------|----------|
| `folder_id` 由文件夹名派生 | 两台机器上同名文件夹自动配对；不同名文件夹不会同步 | 引入邀请码 + UUID，替换 `folder_id` 的生成方式 |
| 限速是"传输后结算" | 用户感知准确，网络统计不精确 | 在 `limits.rs` 加流包装器，调用点不动 |
| 文件树超过 1000 行截断 | 大文件夹只看得到前 1000 项 | 换成 react-window 虚拟滚动 |
| 活动列表最多 500 条 | 更早的记录要从数据库查 | 加分页游标（DTO 里已有 `before_id`） |
| 不跟随符号链接 | 链接目标不会同步 | 初版有意为之，见提示词 4.8 |
| macOS/Linux 未实机验证 | 只在 Windows 上跑过 | Tauri bundler 已配好，需在对应平台构建 |

## 排障

- **界面空白**：debug 构建需要先起 `pnpm dev`（连 5173 端口）；release 构建内置前端资源。
- **配不上对**：两台机器时钟差太多会让配对码被判过期（5 分钟 TTL）。先对时。
  另一个高频原因：**两台机器的「中继服务器」设置不一致**。中继只在连在同一个中继上的节点间转发，一端用自建、一端用官方时互相不可达，表现为「连接对码失败: timed out」。两边都填同一个中继即可。
- **一直走中继**：让两端都加入同一个 Tailscale 网络，连接类型会从「中继」升级到「经 Tailscale」，再尝试打洞到「直连」。
- **导出诊断包**：设置页 →「导出诊断包」，生成的文件不含文件内容与私钥，可直接贴给开发者。
- **某个文件夹不动了**：暂停再恢复会重新扫描；仍不动就看日志里有没有 `拉取 blob 失败`。
