# FolderSync 优化追踪

记录各轮发现、修复与验证结果。日期均为本地时间。

## 功能轮

| 轮次 | 发现 | 处理 | 验证 |
|---|---|---|---|
| 功能初查 | 自动邀请未处理、共享列表合并、邀请路径冲突、删除/启动竞态、忽略规则墓碑误删、每 peer outbox 串扰、CLI sync_once 无 SSH 服务 | 逐一修复；引入 `removing_folders`、`invite_lock`、`push_runtime_if_valid`、`FolderRuntime` 生命周期回滚、per-peer 游标 | `cargo test --workspace` 全绿；新增邀请/忽略/outbox 回归用例 |
| 功能复查 | 邀请失败对邀请方不可见；暂停/移除后仍被自动邀请重建；中继模式不自动创建共享；relay `stats.failed` 静默；relay `clear_all` 清其他 peer；SFTP 撤销后句柄仍可读写；Settings 落盘竞态；`start_folder`/`relocate` 暂停与路径竞态；大索引帧超限 | 增加 `FolderInviteResponse.reason` 处理与 error 活动；`declined_folder_invites` 表（`add_folder_from_invite`/`discard_folder_record`）；中继索引携带 `folder_name` 并自动创建空目录；`clear_for_peer`；句柄 `check_access`；Settings 持锁落盘 + 唯一临时文件 + fsync；最终 push 复核 paused/path/paused_all；`IndexChunk` 4000 条分页 | 207 passed / 2 ignored；`cargo clippy -D warnings` 通过 |

## UI/UX 轮

| 轮次 | 维度 | 处理 |
|---|---|---|
| UI 初查 | 图标/主题/交互/排版/布局 | SVG 图标集、设计 token、深色对比、Modal focus trap、ConfirmDialog、状态徽标、响应式布局、动画与 reduced-motion |
| UI 复查 | 状态展示/错误/字体/移动端 | 分片 loading/error、短指纹与复制、24h 时间、表格固定布局、文件详情脏数据保护、更新进度条、活动筛选标签、环境提示 |
| 用户视角 | 配对/文件夹/缺失路径/邀请 | 配对弹窗收到请求自动关闭、配对后指纹核对、共享“全部设备”哨兵语义、缺失路径“重新选择路径”、邀请接受/拒绝活动记录 |
| P2 收口 | 活动暂停/冲突对比/托盘状态/目录加载/中继清理 | 活动暂停时缓存并计数新事件、恢复后合并；冲突页并排文本预览支持 256 KiB 分段加载；托盘菜单暂停/恢复勾选与 tooltip 同步；文件夹树显示更多；中继空闲房间保留期清理；SSH 端口变更提示立即重启 |
| P2 补齐 | 旧中继索引、CLI 忽略、首扫、协议兼容 | 旧索引无 `folder_name` 自动生成回退名；CLI `sync` 应用 `default_ignore`；首次扫描并行哈希；v3 对 v2 对端自动降级 |

## 验证基线

- `cargo fmt --all -- --check`：通过
- `cargo clippy --workspace --all-targets -- -D warnings`：通过
- `cargo test --workspace`：212 passed，2 ignored，0 failed
- `pnpm exec prettier --check src`、`tsc --noEmit`、`eslint src`、`pnpm build`：通过
- 桌面端 916×639 截图检查：Dashboard / Devices / Folders / FolderDetail / Settings / Activity / Conflicts 均无溢出或遮挡

## 遗留事项

- 平台范围：桌面客户端仅支持 Windows 10/11 x64；macOS/Linux 客户端不在支持范围，不提供安装包或兼容性保证
- 长期设计限制：`folder_id` 由文件夹名派生、限速传输后结算、活动列表最多 500 条、不跟随符号链接、中继落盘未做应用层加密
