; FolderSync 安装钩子：放行 Windows 防火墙的入站 UDP/TCP。
;
; 为什么要这么做：Windows 防火墙默认丢弃入站包，而 FolderSync 是 P2P 应用，
; 必须能接受对端主动连进来。没有这条规则时，表现是「两端都连上了中继，
; 但直连永远超时」，而程序没签名，系统也不会弹窗提示用户放行。
;
; installMode = currentUser，安装器不一定有管理员权限；netsh 失败也不影响
; 安装流程（用户仍可手动放行），所以这里忽略返回码。

!macro customInstall
  nsExec::Exec 'netsh advfirewall firewall delete rule name="FolderSync"'
  nsExec::Exec 'netsh advfirewall firewall delete rule name="FolderSync TCP"'
  nsExec::Exec 'netsh advfirewall firewall add rule name="FolderSync" dir=in action=allow program="$INSTDIR\foldersync-desktop.exe" protocol=udp enable=yes profile=any'
  nsExec::Exec 'netsh advfirewall firewall add rule name="FolderSync TCP" dir=in action=allow program="$INSTDIR\foldersync-desktop.exe" protocol=tcp enable=yes profile=any'
!macroend

!macro customUnInstall
  nsExec::Exec 'netsh advfirewall firewall delete rule name="FolderSync"'
  nsExec::Exec 'netsh advfirewall firewall delete rule name="FolderSync TCP"'
!macroend
