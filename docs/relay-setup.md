# FolderSync 中继服务器搭建指南

中继服务器用于两台电脑无法通过 Tailscale 直连时的兜底：客户端主动出站连接中继，在按房间隔离的 SFTP 空间里交换索引和文件。中继是独立二进制 `foldersync-relay`（源码在 `apps/relay`），不依赖云端服务。

> 中继是**自建可信服务器**，不是端到端加密存储。SSH/SFTP 只保护传输链路；客户端上传的文件会以普通文件形式落在中继磁盘上，服务器管理员或入侵者可以读取。请使用自己控制的服务器，并做好磁盘加密、权限与备份。

## 1. 工作原理

- 监听 **TCP 9443**（可用 `--port` 修改）。
- 每个房间是一个独立的 SFTP 沙箱：
  - SSH 用户名 = 房间 ID（64 位 hex）；
  - SSH 密码 = 房间口令（64 位 hex）；
  - 服务端校验 `blake3(口令) == 房间 ID`，不需要预先注册房间。
- 房间目录：`<data-dir>/rooms/<room-id>/`。客户端只看到自己房间的 `/`。
- 客户端只需出站访问中继端口，不需要在客户端开放任何入站端口。

## 2. 前置条件

- 一台有公网 IP、或被两端都能访问到的服务器（VPS、家里的小主机、公司内网服务器均可）。
- 放行 **TCP 9443**（或你选择的端口）：
  - 云服务器：安全组/防火墙入站规则；
  - 自建路由器后：端口转发到中继主机。
- 构建需要 Rust 工具链；也可以在与服务器同架构的机器上构建后拷贝二进制。
- 磁盘空间按「所有配对房间中实际同步的文件总量」预留，中继会保存客户端上传的文件内容。

## 3. 构建

在仓库根目录：

```bash
cargo build --release -p foldersync-relay
```

产物：

- Windows：`target/release/foldersync-relay.exe`
- Linux：`target/release/foldersync-relay`

## 4. Linux 部署

以下以 Debian/Ubuntu + systemd 为例，数据目录使用 `/var/lib/foldersync-relay`。

### 4.1 安装二进制与专用用户

```bash
sudo install -m 0755 target/release/foldersync-relay /usr/local/bin/foldersync-relay
sudo useradd --system --home /var/lib/foldersync-relay --shell /usr/sbin/nologin foldersync
sudo mkdir -p /var/lib/foldersync-relay
sudo chown -R foldersync:foldersync /var/lib/foldersync-relay
```

### 4.2 前台试运行

```bash
sudo -u foldersync /usr/local/bin/foldersync-relay \
  --port 9443 \
  --data-dir /var/lib/foldersync-relay
```

看到 `FolderSync 中继服务器已启动：0.0.0.0:9443` 即表示监听成功。`Ctrl+C` 停止。

### 4.3 systemd 服务

创建 `/etc/systemd/system/foldersync-relay.service`：

```ini
[Unit]
Description=FolderSync Relay
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=foldersync
Group=foldersync
WorkingDirectory=/var/lib/foldersync-relay
ExecStart=/usr/local/bin/foldersync-relay --port 9443 --data-dir /var/lib/foldersync-relay
Restart=on-failure
RestartSec=5
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/var/lib/foldersync-relay

[Install]
WantedBy=multi-user.target
```

启用并启动：

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now foldersync-relay
sudo systemctl status foldersync-relay
sudo journalctl -u foldersync-relay -n 50 --no-pager
```

### 4.4 放行防火墙

`ufw`：

```bash
sudo ufw allow 9443/tcp
sudo ufw status
```

`firewalld`：

```bash
sudo firewall-cmd --permanent --add-port=9443/tcp
sudo firewall-cmd --reload
```

如果是云服务器，还要在控制台的安全组里放行 TCP 9443。

## 5. Windows 部署

### 5.1 放置文件

把 `foldersync-relay.exe` 放到例如 `C:\FolderSyncRelay\`，数据目录使用 `C:\FolderSyncRelay\data`。

### 5.2 放行防火墙（管理员 PowerShell）

```powershell
New-NetFirewallRule `
  -DisplayName "FolderSync Relay" `
  -Direction Inbound `
  -Protocol TCP `
  -LocalPort 9443 `
  -Action Allow `
  -Profile Any
```

如果服务器在路由器后面，还需要在路由器上把 TCP 9443 转发到这台机器。

### 5.3 前台试运行

```powershell
cd C:\FolderSyncRelay
.\foldersync-relay.exe --port 9443 --data-dir C:\FolderSyncRelay\data
```

### 5.4 开机自启（任务计划程序）

管理员 PowerShell：

```powershell
$action = New-ScheduledTaskAction `
  -Execute "C:\FolderSyncRelay\foldersync-relay.exe" `
  -Argument "--port 9443 --data-dir C:\FolderSyncRelay\data" `
  -WorkingDirectory "C:\FolderSyncRelay"
$trigger = New-ScheduledTaskTrigger -AtStartup
Register-ScheduledTask `
  -TaskName "FolderSyncRelay" `
  -Action $action `
  -Trigger $trigger `
  -RunLevel Highest `
  -User "SYSTEM"
```

也可以用 NSSM/WinSW 把 exe 注册成 Windows 服务，参数相同。

## 6. 客户端填写

### 桌面应用

1. 打开「设置」。
2. 「中继服务器」填：`tcp://<服务器公网IP或域名>:9443`，例如 `tcp://203.0.113.10:9443`。
3. 「网络模式」选：
   - `auto`：有 Tailscale 就直连，否则走中继；
   - `relay`：强制走中继。
4. 保存后重新生成配对码。配对码会携带中继地址，对端首次配对即可使用。

两端建议填写同一个中继地址。即使只配了一端，配对码里也会带发起方的中继地址；但两端配置一致更便于排障。

### CLI

```bash
foldersync --relay tcp://203.0.113.10:9443 status
foldersync --relay tcp://203.0.113.10:9443 pair
foldersync --relay tcp://203.0.113.10:9443 serve --auto-confirm
```

或设置环境变量：

```bash
export FOLDERSYNC_RELAY=tcp://203.0.113.10:9443
```

## 7. 验证

服务端：

```bash
# Linux
ss -ltnp | grep 9443
nc -vz 127.0.0.1 9443

# Windows
Test-NetConnection -ComputerName 127.0.0.1 -Port 9443
```

客户端：

```bash
# Linux
nc -vz <中继IP> 9443

# Windows
Test-NetConnection -ComputerName <中继IP> -Port 9443
```

预期：

- 服务端日志出现 `中继服务器已启动`；
- 端口测试为 `succeeded` / `open`；
- 客户端设置页的环境状态显示中继可达；
- 配对或同步后，`<data-dir>/rooms/<room-id>/` 下出现对应房间目录。

## 8. 目录结构与备份

```text
<data-dir>/
  ssh_host_ed25519              # 中继 SSH 主机密钥（重要）
  rooms/
    <room-id>/
      devices/
        <device-id>/
          index.json            # 该设备上传的索引（原子写）
          blobs/
            <blake3>            # 文件内容，按 hash 寻址
```

- 备份整个 `<data-dir>` 即可保留房间与文件。建议用文件系统快照，或先停服务再复制。
- `ssh_host_ed25519` 建议一并备份；保持服务器主机密钥稳定，便于以后启用固定校验。
- `rooms/` 中可能有用户文件内容，按敏感数据对待。
- 恢复时保持目录路径和权限不变。

## 9. 安全说明

- **自建可信**：中继不做端到端加密存储。SSH/SFTP 加密只覆盖传输过程；服务器磁盘上的文件是明文普通文件，管理员或入侵者可以读取。不要把它交给不信任的运营方。
- **房间隔离**：每个房间是独立的 SFTP 沙箱，客户端不能通过绝对路径、`..` 或符号链接逃逸。但知道房间口令的人都能进入该房间，口令泄露等于房间失守。
- **只出站**：客户端不需要开放任何入站端口；中继只需要暴露 TCP 9443。
- **最小暴露面**：如果两端 IP 固定，可在防火墙/安全组里只允许这些 IP 访问 9443；不要把中继端口暴露给无关公网。
- **专用用户**：用独立系统用户运行，限制数据目录权限，不要用 root 直接跑。
- **磁盘保护**：对数据目录启用磁盘/卷加密，限制备份访问权限。
- **保持更新**：及时更新 `foldersync-relay` 和操作系统；SSH 服务端本身有认证尝试次数限制，但不能替代系统补丁与防火墙。
- **主机密钥校验**：当前中继客户端采用 TOFU（首次连接接受服务器出示的主机密钥，不持久化固定校验）。首次连接应在可信网络中进行；需要更强防护时再给客户端加主机密钥固定。
- **不要用公共共享主机**：共享主机上的其他租户可能读取你的文件或进程内存。

## 10. 升级

1. 构建或获取新版本二进制。
2. 停止服务。
3. 替换 `/usr/local/bin/foldersync-relay`（或 Windows 的 exe）。
4. 启动服务。`--data-dir`、`ssh_host_ed25519` 和 `rooms/` 保持不变。

```bash
foldersync-relay --version
```

## 11. 排障

| 现象 | 排查 |
|------|------|
| 客户端连不上 | 检查服务器 TCP 9443 是否监听、云安全组/系统防火墙/路由器端口转发是否放行 |
| 启动报 `Address already in use` | 端口被占用，换 `--port` 或在占用进程中释放端口 |
| 认证失败 / 房间不存在 | 配对码过期或房间信息不一致，重新生成配对码并配对 |
| 磁盘写满 | 检查 `<data-dir>/rooms` 占用，扩容或清理不再使用的房间（先确认没有设备在使用） |
| 服务启动后退不出 | 前台运行按 `Ctrl+C`；systemd 用 `systemctl stop foldersync-relay` |
| 升级后客户端连不上 | 检查新二进制版本、数据目录权限、端口和防火墙；`--data-dir` 应保持原路径 |
