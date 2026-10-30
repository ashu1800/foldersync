# FolderSync 配对问题诊断脚本
# 使用方法: .\scripts\diagnose-pairing.ps1

Write-Host "=== FolderSync 配对诊断工具 ===" -ForegroundColor Cyan
Write-Host ""

# 1. 检查中继服务器连通性
Write-Host "1. 检查中继服务器连通性..." -ForegroundColor Yellow
$relayHost = "36.137.140.126"
$relayPort = 8444

try {
    $tcpClient = New-Object System.Net.Sockets.TcpClient
    $connection = $tcpClient.BeginConnect($relayHost, $relayPort, $null, $null)
    $wait = $connection.AsyncWaitHandle.WaitOne(5000, $false)

    if ($wait -and $tcpClient.Connected) {
        Write-Host "   ✓ 中继服务器 ${relayHost}:${relayPort} 可连接" -ForegroundColor Green
        $tcpClient.Close()
    } else {
        Write-Host "   ✗ 中继服务器 ${relayHost}:${relayPort} 连接超时" -ForegroundColor Red
        Write-Host "   提示: 检查网络连接和防火墙设置" -ForegroundColor Yellow
    }
} catch {
    Write-Host "   ✗ 中继服务器连接失败: $_" -ForegroundColor Red
}
Write-Host ""

# 2. 检查 Tailscale 状态
Write-Host "2. 检查 Tailscale 状态..." -ForegroundColor Yellow
$tailscaleCmd = Get-Command tailscale -ErrorAction SilentlyContinue
if ($tailscaleCmd) {
    Write-Host "   ✓ Tailscale CLI 可用" -ForegroundColor Green

    $tsStatus = & tailscale status --json 2>$null | ConvertFrom-Json -ErrorAction SilentlyContinue
    if ($tsStatus) {
        $selfIP = $tsStatus.Self.TailscaleIPs -join ", "
        Write-Host "   本机 Tailscale IP: $selfIP" -ForegroundColor Cyan

        $peerCount = ($tsStatus.Peer | Measure-Object).Count
        Write-Host "   Tailscale 对等节点数: $peerCount" -ForegroundColor Cyan
    } else {
        Write-Host "   警告: 无法获取 Tailscale 状态" -ForegroundColor Yellow
    }
} else {
    Write-Host "   ✗ Tailscale CLI 未安装或不在 PATH 中" -ForegroundColor Red
    Write-Host "   提示: Tailscale 可选,但能提供更好的直连性能" -ForegroundColor Yellow
}
Write-Host ""

# 3. 检查防火墙规则
Write-Host "3. 检查防火墙规则..." -ForegroundColor Yellow
$appPath = "E:\FolderSync\apps\desktop\src-tauri\target\release\FolderSync.exe"
if (Test-Path $appPath) {
    $firewallRules = Get-NetFirewallApplicationFilter | Where-Object { $_.Program -like "*FolderSync.exe" }
    if ($firewallRules) {
        Write-Host "   ✓ 找到 FolderSync 防火墙规则" -ForegroundColor Green
    } else {
        Write-Host "   ✗ 未找到 FolderSync 防火墙规则" -ForegroundColor Red
        Write-Host "   提示: 可能需要手动添加防火墙例外" -ForegroundColor Yellow
    }
} else {
    Write-Host "   - 应用尚未编译,跳过防火墙检查" -ForegroundColor Gray
}
Write-Host ""

# 4. 检查数据目录和日志
Write-Host "4. 检查数据目录..." -ForegroundColor Yellow
$dataDir = "$env:USERPROFILE\.foldersync"
if (Test-Path $dataDir) {
    Write-Host "   ✓ 数据目录存在: $dataDir" -ForegroundColor Green

    $dbPath = Join-Path $dataDir "state.sqlite3"
    if (Test-Path $dbPath) {
        Write-Host "   ✓ 数据库文件存在" -ForegroundColor Green
    }

    $logDir = Join-Path $dataDir "logs"
    if (Test-Path $logDir) {
        $latestLog = Get-ChildItem $logDir -Filter "*.log*" | Sort-Object LastWriteTime -Descending | Select-Object -First 1
        if ($latestLog) {
            Write-Host "   ✓ 最新日志: $($latestLog.Name) ($(Get-Date $latestLog.LastWriteTime -Format 'yyyy-MM-dd HH:mm:ss'))" -ForegroundColor Green

            # 检查最近的错误
            $recentErrors = Get-Content $latestLog.FullName -Tail 50 | Select-String -Pattern "ERROR|error|失败|超时|timeout" -Context 0,1
            if ($recentErrors) {
                Write-Host "   最近的错误信息:" -ForegroundColor Yellow
                $recentErrors | Select-Object -First 3 | ForEach-Object {
                    Write-Host "     $_" -ForegroundColor Gray
                }
            }
        }
    }
} else {
    Write-Host "   - 数据目录不存在 (首次运行时会自动创建)" -ForegroundColor Gray
}
Write-Host ""

# 5. 网络接口检查
Write-Host "5. 检查网络接口..." -ForegroundColor Yellow
$activeInterfaces = Get-NetIPAddress -AddressFamily IPv4 | Where-Object { $_.InterfaceAlias -notlike "*Loopback*" }
foreach ($iface in $activeInterfaces) {
    $ifaceName = $iface.InterfaceAlias
    $ipAddr = $iface.IPAddress
    Write-Host "   - $ifaceName : $ipAddr" -ForegroundColor Cyan
}
Write-Host ""

# 6. 测试 HTTP 连接（中继现在是纯 HTTP 模式，见 docs/decisions.md）
Write-Host "6. 测试 HTTP 连接到中继服务器..." -ForegroundColor Yellow
try {
    $response = Invoke-WebRequest -Uri "http://${relayHost}:${relayPort}/healthz" -TimeoutSec 5 -UseBasicParsing -ErrorAction Stop
    Write-Host "   ✓ 中继健康检查通过 (状态码: $($response.StatusCode))" -ForegroundColor Green
} catch {
    $errorMsg = $_.Exception.Message
    if ($errorMsg -like "*404*" -or $errorMsg -like "*403*") {
        Write-Host "   ✓ 服务器响应 (非200但服务器在线)" -ForegroundColor Green
    } else {
        Write-Host "   ✗ 中继连接失败: $errorMsg" -ForegroundColor Red
    }
}
Write-Host ""

# 总结和建议
Write-Host "=== 诊断建议 ===" -ForegroundColor Cyan
Write-Host ""
Write-Host "配对失败常见原因:" -ForegroundColor Yellow
Write-Host "1. 中继服务器无法访问 - 检查网络和DNS解析" -ForegroundColor White
Write-Host "2. 防火墙阻止连接 - 添加 FolderSync.exe 到防火墙白名单" -ForegroundColor White
Write-Host "3. 配对码过期 - 配对码有效期5分钟,过期需重新生成" -ForegroundColor White
Write-Host "4. Token未同步 - pair和serve必须使用同一个数据目录" -ForegroundColor White
Write-Host ""
Write-Host "下一步操作建议:" -ForegroundColor Yellow
Write-Host "1. 在两台机器上都运行此脚本,对比中继服务器连通性" -ForegroundColor White
Write-Host "2. 查看日志文件 $dataDir\logs\ 获取详细错误信息" -ForegroundColor White
Write-Host "3. 确保两台机器的时间同步(配对码过期判断依赖系统时间)" -ForegroundColor White
Write-Host "4. 如果使用 Tailscale,确认两台机器都在同一个 Tailnet 中" -ForegroundColor White
Write-Host ""
