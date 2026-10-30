"""在本机打包、签名，并发布到 Basic 版本更新服务。不经过 GitHub。

用法（在仓库根目录）：
  python scripts/release_local.py 0.2.6          改版本号 → 打包 → 签名 → 上传
  python scripts/release_local.py 0.2.6 --build-only   只打包，不上传
  python scripts/release_local.py 0.2.5 --file 安装包.exe  不打包，直接上传现成的安装包

管理员邮箱和密码：优先读环境变量 BASIC_ADMIN_EMAIL / BASIC_ADMIN_PASSWORD，
没有就在命令行里提示输入（密码不回显、不落盘）。
更新签名私钥：读 ~/.foldersync-release/update_signing_key.hex。

依赖：pnpm、Rust、pip install cryptography
"""

import argparse
import getpass
import json
import os
import re
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DESKTOP = ROOT / "apps" / "desktop"
HOME = Path.home()
SIGNING_KEY = HOME / ".foldersync-release" / "update_signing_key.hex"
# 旧的 Tauri updater 私钥：tauri.conf.json 里 createUpdaterArtifacts 为 true 时，打包必须有它
TAURI_KEY = HOME / ".tauri" / "foldersync.key"


def read_versions():
    cargo = re.search(r'^version = "(.*?)"', (ROOT / "Cargo.toml").read_text("utf-8"), re.M)
    tauri = json.loads((DESKTOP / "src-tauri" / "tauri.conf.json").read_text("utf-8"))
    pkg = json.loads((DESKTOP / "package.json").read_text("utf-8"))
    return (cargo.group(1) if cargo else ""), tauri["version"], pkg["version"]


def set_version(v: str):
    """和 scripts/check-version.sh 一样，同步改三处版本号。"""
    edits = [
        (ROOT / "Cargo.toml", r'^version = ".*?"', f'version = "{v}"', re.M),
        (DESKTOP / "src-tauri" / "tauri.conf.json", r'"version": ".*?"', f'"version": "{v}"', 0),
        (DESKTOP / "package.json", r'"version": ".*?"', f'"version": "{v}"', 0),
    ]
    for path, pat, repl, flags in edits:
        text = path.read_text("utf-8")
        path.write_text(re.sub(pat, repl, text, count=1, flags=flags), "utf-8", newline="")
    # 依赖 workspace version 的 crate 跟着变，刷新锁文件
    subprocess.run(["cargo", "update", "--workspace", "--offline"], cwd=ROOT, capture_output=True)


def build(v: str) -> Path:
    """调用 tauri 打 NSIS 安装包，返回安装包路径。"""
    env = dict(os.environ)
    if TAURI_KEY.exists():
        env["TAURI_SIGNING_PRIVATE_KEY"] = TAURI_KEY.read_text("utf-8").strip()
        # 私钥没有密码：显式给空串 + CI=true，否则 tauri CLI 会卡在交互式密码提示
        env["TAURI_SIGNING_PRIVATE_KEY_PASSWORD"] = ""
        env["CI"] = "true"
    print(f"== 打包 {v}（第一次会比较慢，之后有编译缓存）", flush=True)
    started = time.time()
    pnpm = "pnpm.cmd" if os.name == "nt" else "pnpm"
    subprocess.run([pnpm, "tauri", "build", "--bundles", "nsis"], cwd=DESKTOP, env=env, check=True)
    exe = ROOT / "target" / "release" / "bundle" / "nsis" / f"FolderSync_{v}_x64-setup.exe"
    if not exe.exists():
        raise SystemExit(f"没找到安装包：{exe}")
    print(f"== 打包完成，用时 {time.time() - started:.0f} 秒：{exe}", flush=True)
    return exe


def publish(exe: Path, v: str):
    if not SIGNING_KEY.exists():
        raise SystemExit(f"没找到更新签名私钥：{SIGNING_KEY}")
    env = dict(os.environ)
    env["FOLDERSYNC_UPDATE_SIGNING_KEY"] = SIGNING_KEY.read_text("utf-8").strip()
    if not env.get("BASIC_ADMIN_EMAIL"):
        env["BASIC_ADMIN_EMAIL"] = input("管理员邮箱: ").strip()
    if not env.get("BASIC_ADMIN_PASSWORD"):
        env["BASIC_ADMIN_PASSWORD"] = getpass.getpass("管理员密码（输入时不显示）: ")
    print("== 签名并上传到 https://ashu180.cn", flush=True)
    subprocess.run(
        [
            sys.executable,
            str(ROOT / "scripts" / "publish_update.py"),
            str(exe),
            v,
            "--changelog",
            str(ROOT / "CHANGELOG.md"),
        ],
        env=env,
        check=True,
    )


def main():
    ap = argparse.ArgumentParser(description="本机打包、签名并发布到 Basic 版本更新服务")
    ap.add_argument("version", help="要发布的版本号，例如 0.2.6")
    ap.add_argument("--build-only", action="store_true", help="只打包，不上传")
    ap.add_argument("--file", help="不打包，直接上传这个现成的安装包")
    args = ap.parse_args()
    v = args.version.lstrip("v")
    if not re.fullmatch(r"\d+\.\d+\.\d+", v):
        raise SystemExit(f"版本号必须是 x.y.z：{args.version}")

    if args.file:
        exe = Path(args.file).resolve()
    else:
        if len(set(read_versions())) != 1 or read_versions()[0] != v:
            print(f"== 把版本号改成 {v}", flush=True)
            set_version(v)
        exe = build(v)
    if not args.build_only:
        publish(exe, v)


if __name__ == "__main__":
    try:
        main()
    except subprocess.CalledProcessError as e:
        raise SystemExit(f"命令失败（退出码 {e.returncode}）：{' '.join(map(str, e.cmd))}")
