@echo off
cd /d E:\FolderSync\apps\desktop
set ""TAURI_SIGNING_PRIVATE_KEY=C:\Users\Ashu\.tauri\foldersync.key""
set ""TAURI_SIGNING_PRIVATE_KEY_PASSWORD=""
set ""CI=true""
pnpm exec tauri build --no-bundle > %TEMP%\nb.log 2>&1
echo EXITCODE=%ERRORLEVEL% >> %TEMP%\nb.log
