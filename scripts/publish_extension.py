#!/usr/bin/env python3
"""将同一 checkout 的已打包 Rust 扩展精确分发交给指定 Registry。"""

import argparse
from contextlib import closing
from dataclasses import dataclass
import hashlib
from http.client import HTTPException
import json
import os
from pathlib import Path
import re
import tomllib
from urllib.error import HTTPError, URLError
from urllib.parse import quote, urlsplit
from urllib.request import HTTPRedirectHandler, Request, build_opener


ROOT = Path(__file__).resolve().parents[1]
MAX_BYTES = 128 * 1024 * 1024
COMPONENT = re.compile(r"[a-z0-9][a-z0-9-]*")
TARGET = re.compile(r"[a-z0-9_]+(?:-[a-z0-9_]+){2,3}")


class NoRedirect(HTTPRedirectHandler):
    def redirect_request(self, *_args, **_kwargs):
        return None


@dataclass(frozen=True)
class Artifact:
    target: str
    executable: Path
    size: int
    sha256: str


def load_packages(name, packages, *, for_publication):
    selected = {}
    release = None
    for argument in packages:
        target, _, directory = argument.partition("=")
        if not TARGET.fullmatch(target) or not directory or target in selected:
            raise ValueError("每个 --package 需要不同的 TARGET=DIR")
        root = Path(directory).resolve(strict=True)
        extension = root / "extensions" / name
        manifest = json.loads((extension / "manifest.json").read_text(encoding="utf-8"))
        provenance = json.loads((root / "source.json").read_text(encoding="utf-8"))
        if manifest["name"] != name or manifest["distribution"] != "rust_stdio":
            raise ValueError("manifest 与发布坐标不一致")
        if for_publication and provenance.get("workspace_dirty") is not False:
            raise ValueError("发布需要由干净 checkout 生成的产物与 source.json")
        revision = provenance["workspace_checkout"]
        if not re.fullmatch(r"[0-9a-f]{40}", revision):
            raise ValueError("source.json 需要完整源码提交 ID")
        identity = (
            manifest["version"], manifest["host_sdk"], manifest["host_sdk_version"],
            manifest["protocol_version"], revision, provenance["extension_sdk_checkout"],
        )
        if release is not None and release != identity:
            raise ValueError("多平台产物必须使用相同 Release、协议和源码与 SDK 提交")
        release = identity
        executable = (extension / manifest["executable"]).resolve(strict=True)
        if executable.parent != extension.resolve() or not executable.is_file():
            raise ValueError("manifest 必须引用安装目录内的普通文件")
        size = executable.stat().st_size
        if not 1 <= size <= MAX_BYTES:
            raise ValueError("Rust 产物大小必须介于 1 byte 和 128 MiB")
        with executable.open("rb") as stream:
            digest = hashlib.file_digest(stream, "sha256").hexdigest()
        if digest != manifest["sha256"]:
            raise ValueError("本机产物与 manifest 摘要不一致")
        selected[target] = Artifact(target, executable, size, digest)
    return release, selected


class Registry:
    def __init__(self, origin, token):
        url = urlsplit(origin)
        if (not url.hostname or url.username or url.password or url.query or url.fragment
                or (url.scheme != "https" and not (url.scheme == "http" and url.hostname in {"127.0.0.1", "::1", "localhost"}))):
            raise ValueError("Registry 使用不带凭据的 HTTPS URL，本机验收可使用 loopback HTTP")
        self.origin = origin.rstrip("/")
        self.token = token
        self.client = build_opener(NoRedirect())

    def post(self, path, *, body=None, length=0, media_type="application/json"):
        request = Request(self.origin + path, method="POST", data=body, headers={
            "Authorization": "Bearer " + self.token,
            "Content-Type": media_type,
            "Content-Length": str(length),
            "Accept": "application/json",
        })
        try:
            with self.client.open(request, timeout=120) as response:
                payload = response.read(256 * 1024 + 1)
                if len(payload) > 256 * 1024:
                    raise ValueError("Registry 发布响应超过大小限制")
                return json.loads(payload)
        except HTTPError as error:
            with closing(error):
                raise ValueError(f"Registry 拒绝发布操作，HTTP {error.code}") from None
        except (URLError, HTTPException, ValueError):
            raise ValueError("Registry 请求未完成") from None


def publish(registry, base, body, artifacts):
    encoded = json.dumps(body).encode()
    registry.post(base, body=encoded, length=len(encoded))
    exact = base + "/" + quote(body["version"], safe="")
    for artifact in artifacts.values():
        with artifact.executable.open("rb") as stream:
            uploaded = registry.post(
                exact + "/artifacts/" + artifact.target, body=stream, length=artifact.size,
                media_type="application/octet-stream",
            )
        record = next((item for item in uploaded["rust_stdio"]["artifacts"]
                       if item["target"] == artifact.target), None)
        if record is None or (record["sha256"], record["size"]) != (artifact.sha256, artifact.size):
            raise ValueError(f"Registry 返回的 {artifact.target} 产物摘要或大小不符，停止发布")
    release = registry.post(exact + "/publish")
    if release["state"] != "published" or release["version"] != body["version"]:
        raise ValueError("Registry 未确认精确版本已发布")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("extension", help="namespace/name")
    parser.add_argument("--nickname", required=True)
    parser.add_argument("--registry", required=True)
    parser.add_argument("--package", action="append", required=True, metavar="TARGET=DIR")
    parser.add_argument("--build-id")
    parser.add_argument("--execute", action="store_true", help="执行上传和发布，默认只检查本机产物")
    args = parser.parse_args()
    try:
        coordinates = args.extension.split("/")
        if len(coordinates) != 2 or not all(map(COMPONENT.fullmatch, coordinates)):
            raise ValueError("扩展需要合法的 namespace/name")
        (version, sdk, sdk_version, protocol, revision, _), artifacts = load_packages(
            args.extension, args.package, for_publication=args.execute,
        )
        if (sdk, protocol) != ("core-rs", 1):
            raise ValueError("Registry 当前支持 core-rs stdio 协议 1")
        repository = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))["workspace"]["package"]["repository"]
        body = {"nickname": args.nickname, "version": version, "rust_stdio": {
            "host_sdk": sdk, "host_sdk_version": sdk_version, "source_repository": repository,
            "source_revision": revision, "targets": sorted(artifacts),
        }}
        if args.build_id:
            body["rust_stdio"]["build_id"] = args.build_id
        if args.execute:
            token = os.environ.get("INKCRE_REGISTRY_TOKEN")
            if not token or not token.isascii() or any(character.isspace() for character in token):
                raise ValueError("需要 INKCRE_REGISTRY_TOKEN 发布凭据")
            registry = Registry(args.registry, token)
            publish(registry, f"/v2/extensions/{args.extension}/distributions/rust_stdio/releases", body, artifacts)
        print(json.dumps({"extension": args.extension, "executed": args.execute, "release": body,
                          "artifacts": {target: item.sha256 for target, item in artifacts.items()}}, ensure_ascii=False))
    except (KeyError, TypeError, OSError, ValueError) as error:
        # 文件内容、上游响应及凭据无需出现在发布诊断中。
        message = str(error) if type(error) is ValueError else "无法验证本机打包产物或 Registry 发布响应"
        parser.exit(1, message + "\n")


if __name__ == "__main__":
    main()
