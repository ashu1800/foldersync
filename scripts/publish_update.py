"""把安装包发布到 Basic 版本更新服务（https://ashu180.cn）。

流程：登录 → presign 取直传链接 → PUT 上传 → confirm（带 MD5 与 Ed25519 签名）
→ update 写更新说明 → 回读 info 校验。四步缺一不可，任何一步失败都以非 0 退出。

用法：
  python scripts/publish_update.py <安装包路径> <semver> [--notes-file F] [--force]

环境变量：
  BASIC_ADMIN_EMAIL / BASIC_ADMIN_PASSWORD  管理员账号
  FOLDERSYNC_UPDATE_SIGNING_KEY             Ed25519 私钥种子（64 位 hex）
  BASIC_HOST / BASIC_UUID                   可选，覆盖默认服务地址与项目 KEY

依赖：pip install cryptography
"""

import argparse
import hashlib
import json
import os
import sys
import urllib.error
import urllib.request

HOST = os.environ.get("BASIC_HOST", "https://ashu180.cn")
UUID = os.environ.get("BASIC_UUID", "af61462d-d513-4ef5-9064-f7619550f615")


def version_code(semver: str) -> int:
    """semver → 服务端要求的单调递增整数：major*10000 + minor*100 + patch。

    必须与客户端 updater.rs 里的 version_code 保持一致。
    """
    parts = semver.lstrip("v").split("-")[0].split(".")
    if len(parts) != 3 or not all(p.isdigit() for p in parts):
        raise SystemExit(f"版本号必须是 x.y.z：{semver}")
    major, minor, patch = (int(p) for p in parts)
    if minor > 99 or patch > 99:
        raise SystemExit(f"minor/patch 不能超过 99（编码会冲突）：{semver}")
    return major * 10000 + minor * 100 + patch


def call(method, path, body=None, token=None):
    """调用管理/公开接口；成功判定 = HTTP 200 且 code == 200，否则原样报出 message。"""
    req = urllib.request.Request(HOST + path, method=method)
    data = None
    if body is not None:
        data = json.dumps(body).encode("utf-8")
        req.add_header("Content-Type", "application/json")
    if token:
        req.add_header("Authorization", f"Bearer {token}")
    try:
        with urllib.request.urlopen(req, data=data, timeout=60) as r:
            status, raw = r.status, r.read()
    except urllib.error.HTTPError as e:
        status, raw = e.code, e.read()
    try:
        doc = json.loads(raw)
    except ValueError:
        raise SystemExit(f"{method} {path} 返回非 JSON（HTTP {status}）: {raw[:200]!r}")
    if status != 200 or doc.get("code") != 200:
        raise SystemExit(f"{method} {path} 失败（HTTP {status}）: {doc.get('message')}")
    return doc.get("data")


def sign(payload: bytes) -> str:
    from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey

    seed = os.environ.get("FOLDERSYNC_UPDATE_SIGNING_KEY", "").strip()
    if len(seed) != 64:
        raise SystemExit("FOLDERSYNC_UPDATE_SIGNING_KEY 未设置或不是 64 位 hex")
    key = Ed25519PrivateKey.from_private_bytes(bytes.fromhex(seed))
    return key.sign(payload).hex()


def put_to_cos(url: str, payload: bytes, attempts: int = 3):
    """PUT 文件字节到 COS 预签名地址（地址自带签名，不要再带 Token）。

    失败时把 COS 返回的 XML 错误原文打出来（之前只有 "HTTP 400"，看不出原因），
    并重试几次：跨境上传慢，偶尔会被 COS 以 RequestTimeout（400）断开。
    """
    for i in range(1, attempts + 1):
        put = urllib.request.Request(url, data=payload, method="PUT")
        put.add_header("Content-Type", "application/octet-stream")
        try:
            with urllib.request.urlopen(put, timeout=600) as r:
                if r.status in (200, 204):
                    return
                detail = f"HTTP {r.status}"
        except urllib.error.HTTPError as e:
            detail = f"HTTP {e.code}: {e.read()[:500].decode('utf-8', 'replace')}"
        except (urllib.error.URLError, TimeoutError) as e:
            detail = f"网络错误: {e}"
        print(f"PUT 上传失败（第 {i}/{attempts} 次）: {detail}", flush=True)
    raise SystemExit("PUT 上传到 COS 失败，已放弃")


def changelog_section(path: str, semver: str) -> str:
    """取 CHANGELOG.md 里 `## x.y.z` 到下一个 `## ` 之间的内容；找不到时报错，避免发出空说明。"""
    out, inside = [], False
    for line in open(path, encoding="utf-8"):
        if line.startswith("## "):
            if inside:
                break
            inside = line[3:].split()[0] == semver.lstrip("v")
            continue
        if inside:
            out.append(line.rstrip())
    text = "\n".join(out).strip()
    if not text:
        raise SystemExit(f"CHANGELOG 里没有 {semver} 的段落")
    return text


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("file")
    ap.add_argument("version")
    ap.add_argument("--notes-file")
    ap.add_argument("--changelog", help="从 CHANGELOG.md 取该版本的段落作为更新说明")
    ap.add_argument("--force", action="store_true", help="标记为强制更新")
    args = ap.parse_args()

    email = os.environ.get("BASIC_ADMIN_EMAIL")
    password = os.environ.get("BASIC_ADMIN_PASSWORD")
    if not email or not password:
        raise SystemExit("缺少 BASIC_ADMIN_EMAIL / BASIC_ADMIN_PASSWORD")

    payload = open(args.file, "rb").read()
    # 文件名带版本号：COS 对象键 = versions/{uuid}/{file_name}，
    # 同名覆盖时正在下载旧包的客户端会拿到新内容、MD5 对不上
    file_name = os.path.basename(args.file)
    code = version_code(args.version)
    md5 = hashlib.md5(payload).hexdigest()
    signature = sign(payload)
    if args.notes_file:
        notes = open(args.notes_file, encoding="utf-8").read().strip()
    elif args.changelog:
        notes = changelog_section(args.changelog, args.version)
    else:
        notes = ""
    print(
        f"发布 {file_name}  version={args.version}（{code}） size={len(payload)} md5={md5}",
        flush=True,
    )

    token = call("POST", "/api/user/login", {"email": email, "password": password})["access_token"]

    pre = call(
        "POST",
        "/api/version/upload/presign",
        {"uuid": UUID, "version": code, "file_name": file_name},
        token,
    )
    put_to_cos(pre["upload_url"], payload)
    print("已上传到 COS:", pre.get("cos_key"), flush=True)

    call(
        "POST",
        "/api/version/upload/confirm",
        {
            "uuid": UUID,
            "version": code,
            "file_name": file_name,
            "file_size": len(payload),
            "md5": md5,
            "signature": signature,
        },
        token,
    )
    call(
        "POST",
        "/api/version/update",
        {"uuid": UUID, "status": 0, "update": args.force, "notice": notes},
        token,
    )

    info = call("GET", f"/api/version/info?uuid={UUID}")
    if info["version"] != code or info["md5"] != md5 or info.get("signature") != signature:
        raise SystemExit(f"回读校验不一致: {json.dumps(info, ensure_ascii=False)[:300]}")
    print(f"发布完成: version={info['version']} file={info['file_name']} force={info['update']}")


if __name__ == "__main__":
    sys.exit(main())
