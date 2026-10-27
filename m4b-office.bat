@echo off
cd /d C:\Users\12801\foldersync-test
foldersync.exe serve --auto-confirm --data-dir C:\Users\12801\foldersync-test\.m4 --folder C:\Users\12801\foldersync-test\.m4\shared > m4b.log 2>&1

