# 发版流程

面向维护者。客户端行为见 [README](../README.md#自动更新与版本发布)。

## 一次发版的完整步骤

### 1. 确认当前状态干净

```bash
git status                    # 应该没有未提交改动
./scripts/check-version.sh    # 三处版本号一致
```

### 2. 改版本号

```bash
./scripts/check-version.sh 0.2.0
```

脚本会同步修改三处并校验：

| 文件 | 作用 |
|------|------|
| `Cargo.toml` | workspace version |
| `apps/desktop/src-tauri/tauri.conf.json` | 安装包名 + updater 比较基准 |
| `apps/desktop/package.json` | 前端展示 |

**三处必须一致**，否则会出现"界面显示 0.1.0、安装包叫 0.2.0"这种自相矛盾的局面。

### 3. 更新 CHANGELOG 并提交

发版说明会作为 GitHub Release 的描述，也是客户端「检查更新」里显示的文本。

```bash
git add -A
git commit -m "release: v0.2.0"
git push origin main
```

### 4. 等 CI 绿

```bash
gh run list --limit 3
gh run watch
```

`ci.yml` 会跑 fmt / clippy（含测试目标）/ test / tsc / eslint / prettier / 版本一致性 /
TS 绑定是否最新。**全绿才能进下一步。**

### 5. 打 tag 触发发布

```bash
git tag v0.2.0
git push origin v0.2.0
```

tag 必须匹配 `v*`，否则 workflow 不触发。

### 6. 盯发布进度

```bash
gh run list --limit 2          # 找 Release 那条
gh run watch <run-id>
```

Release 工作流：

```
check (复用 ci.yml)  ──┬─→ publish / windows-latest
                       ├─→ publish / macos-latest (aarch64)
                       ├─→ publish / macos-latest (x86_64)
                       └─→ publish / ubuntu-22.04
```

`check` 不通过时 publish 全部跳过 —— 这是故意的，坏代码不该变成用户的更新。

### 7. 验证发布结果

```bash
gh release view v0.2.0
```

必须能看到：

- `FolderSync_0.2.0_x64-setup.exe` + `.sig`
- `FolderSync_0.2.0_aarch64.dmg` / `x64.dmg`
- `foldersync_0.2.0_amd64.AppImage` / `.deb`
- **`latest.json`** ← 客户端靠它判断有没有新版本，缺了自动更新就失效

验证 `latest.json` 内容：

```bash
gh release download v0.2.0 -p latest.json -O - | head -30
```

里面应该有 `version`、各平台的 `url` 和 `signature`。

### 8. 实测更新

拿一台装了 v0.1.0 的机器：

1. 打开应用 → 设置 → 检查更新
2. 应显示"发现新版本 0.2.0"
3. 点「下载并安装」→ 自动重启 → 版本变成 0.2.0
4. **确认 `~/.foldersync/` 里的 `ssh_identity` 与 `ssh_host_ed25519` 没变**（设备身份与主机密钥不能丢）

## 出问题怎么办

| 现象 | 原因 | 处理 |
|------|------|------|
| tag 推上去没触发 | tag 不以 `v` 开头 | `git tag v0.2.0` 重推 |
| Release 里没有 `latest.json` | `createUpdaterArtifacts` 没开，或构建失败 | 查 workflow 日志 |
| 客户端提示"签名校验失败" | 私钥换了，与 `tauri.conf.json` 里的公钥不匹配 | 换回原私钥；实在找不回只能让用户重装 |
| 客户端看不到新版本 | Release 被标成 prerelease/draft | 编辑 release 取消勾选 |
| 构建卡在签名不动 | CI 环境没有 `CI=true` | 已在 workflow 里设了，别删 |

## 已经踩过的坑（别再踩）

| 坑 | 症状 | 根因 |
|----|------|------|
| release 是草稿 | 客户端检查更新永远 404 | 草稿不出现在 `releases/latest`，别传 `releaseDraft` |
| tauri-action 找不到产物 | `No artifacts were found` | 它按 `info.name` 猜文件名，Linux 上是 kebab-case（`folder-sync`），对不上 |
| 产物目录找不到 | `target/release/bundle: No such file` | 带 `--target` 构建时在 `target/<triple>/release/bundle` |
| Linux/macOS 没产物 | 只出了裸二进制 | 没传 `--bundles`，tauri 在 CI 上会跳过打包 |
| 签名卡死 | 构建停在 "expect a prompt for password" | 私钥无密码时要同时给空串**和** `CI=true`，否则 CLI 走交互式提示 |
| latest.json 的 URL 404 | 下载 `vv0.1.0` 或文件名带 `v` | tag 本身带 `v`，拼 URL/文件名前必须先去掉 |
| macOS 两个架构指向同一个包 | 下载下来是错的架构 | `.app.tar.gz` 两个架构同名，上传前要改名带架构 |
| TS 绑定检查永远失败 | diff 内容看起来一模一样 | 自己用 `TS::decl` 拼文件，丢了 ts-rs 的文件头和 rustdoc；要用官方的 `export_to_string` |
| Windows 上脚本崩 | `UnicodeEncodeError: charmap` | Windows runner 的 Python stdout 是 cp1252，脚本输出只能英文 |
| macOS 上脚本 exit 127 | `python: command not found` | macOS 只有 `python3`，Windows 只有 `python`，用 `command -v python3 \|\| command -v python` |

## 绝对不能做的事

- **不要删 `~/.tauri/foldersync.key` 或 GitHub secret。** 私钥丢失后，已安装的老版本
  会拒绝所有新更新（签名对不上），只能让用户手动重装。
- **不要在没有 CI 绿灯时打 tag。** 会发出一版坏代码，且用户已经更新后无法回退
  （updater 只往高版本走）。
- **不要在 tag 后修改 `tauri.conf.json` 的 `pubkey`。** 会让签名验证全线失败。
