; FolderSync 安装钩子：放行 Windows 防火墙的 TCP 入站。
;
; 为什么需要：Windows 防火墙默认丢弃入站包，而 FolderSync 内置 SSH 服务端
; （russh，默认 TCP 2222），对端必须能主动连进来。没有规则时表现为
; 「配对/连接一直超时」，而程序未签名，系统不会弹窗提示用户放行。
;
; 当前网络层是 SSH + SFTP，只需要 TCP；iroh 时代的 UDP 规则已删除，
; 不要再加 protocol=udp。
;
; installMode = currentUser，安装器通常没有管理员权限；netsh 失败不影响
; 安装流程（用户仍可在设置页「初始化/修复 SSH 环境」里用 UAC 再试一次），
; 所以这里忽略所有 netsh 返回码。
;
; 规则说明：
; - "FolderSync SSH"：TCP 2222（默认 SSH 端口）+ 程序路径，精确放行；
; - "FolderSync SSH (Program)"：程序级 TCP 放行，覆盖用户在设置里改过的端口；
; - 升级/重装时先删旧规则，避免残留 iroh 时代的 UDP 规则。

!macro customInstall
  ; 清理 iroh 时代与旧版本可能留下的规则
  nsExec::Exec 'netsh advfirewall firewall delete rule name="FolderSync"'
  nsExec::Exec 'netsh advfirewall firewall delete rule name="FolderSync TCP"'
  nsExec::Exec 'netsh advfirewall firewall delete rule name="FolderSync SSH"'
  nsExec::Exec 'netsh advfirewall firewall delete rule name="FolderSync SSH (Program)"'

  ; 默认 SSH 端口：TCP 2222，仅允许本程序
  nsExec::Exec 'netsh advfirewall firewall add rule name="FolderSync SSH" dir=in action=allow program="$INSTDIR\foldersync-desktop.exe" protocol=tcp localport=2222 enable=yes profile=any'

  ; 程序级 TCP 放行：用户改过 SSH 端口后仍然可用
  nsExec::Exec 'netsh advfirewall firewall add rule name="FolderSync SSH (Program)" dir=in action=allow program="$INSTDIR\foldersync-desktop.exe" protocol=tcp enable=yes profile=any'
!macroend

!macro customUnInstall
  nsExec::Exec 'netsh advfirewall firewall delete rule name="FolderSync SSH"'
  nsExec::Exec 'netsh advfirewall firewall delete rule name="FolderSync SSH (Program)"'
  ; 兼容清理旧版本可能留下的规则
  nsExec::Exec 'netsh advfirewall firewall delete rule name="FolderSync"'
  nsExec::Exec 'netsh advfirewall firewall delete rule name="FolderSync TCP"'
!macroend
