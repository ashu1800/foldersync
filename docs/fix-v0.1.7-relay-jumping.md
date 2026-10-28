# v0.1.7 修复：关闭 N0 的 DNS 地址发现

## 背景

两台 Windows 机器（家里 / 公司）始终无法配对，界面只显示「连接对码失败: timed out」。
中间排查过三轮，结论如下（前两轮的问题已分别在 v0.1.5 / v0.1.6 修掉）：

| 版本 | 修的问题 | 证据 |
|------|----------|------|
| v0.1.5 | 自建中继 + 官方中继混在 relay map 里，iroh 按延迟来回跳，两端落在不同中继上 | 日志里每分钟都在 `sync.ashu180.top` / `usw1` / `aps1` / `euc1` 之间切换 |
| v0.1.6 | 配对码只带 EndpointId，靠 DNS 发现找对端；工作机防火墙丢弃入站 UDP | 配对码加上中继 + Tailscale 直连地址；安装包放行防火墙 |
| v0.1.7 | `presets::N0` 会把 `PkarrPublisher` / `PkarrResolver` / `DnsAddressLookup` 一起挂上 | 见下 |

## v0.1.7 的问题

`Endpoint::builder(presets::N0)` 除了加密提供者和官方中继，还会挂载三个地址发现组件：

- `PkarrPublisher`（向 n0 DNS 发布本机地址）
- `PkarrResolver`
- `DnsAddressLookup`（走 `dns.iroh.link` 查对端）

这三个都要访问境外服务。国内网络下它们被墙或极慢，而 `endpoint.connect(addr)` 会把
地址发现和直连并发进行——只要发现这一路不返回，连接就可能拖到 30 秒超时才失败。

而配对码里其实已经带了完整地址（中继 URL + 本机 `100.x.x.x:port`），根本不需要发现。

## 修法

`bind_endpoint_with` 在配置了自建中继时改用 `presets::Minimal`：

```rust
Endpoint::builder(presets::Minimal)
    .secret_key(secret)
    .relay_mode(iroh::RelayMode::Custom(relay_map_with(relay)))
```

`Minimal` 只设置 rustls 加密提供者，不挂任何地址发现。没配自建中继时仍走 `presets::N0`。

## 怎么验证

```powershell
# 1. 两端都重启后，日志里应该只有这一行中继，且不再出现 usw1/aps1/euc1
#    "使用自建中继（关闭 DNS 发现，只连这一个中继）"

# 2. 配对时日志会打印实际拨号地址
#    "开始配对拨号 peer=... addrs={Relay(...), Ip(100.x.x.x:51900), ...}"
#    失败会打印 "配对连接失败: <原因>"
```

另外用 `crates/fs-sync-net/examples/relay_probe.rs` 可以直接验证两台机器之间的
Tailscale 直连是否通（不经过 GUI）：

```powershell
# 工作机
relay_probe.exe listen          # 打印 ID 和 SOCK 端口
# 本机
relay_probe.exe connect <ID> 100.102.104.88:<端口>
```
