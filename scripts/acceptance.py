#!/usr/bin/env python3
"""显式运行真实 HTTP → native PostgreSQL 关键旅程；不属于自动化测试套件。"""

import argparse
import base64
import hashlib
import hmac
import json
import os
from pathlib import Path
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
from urllib.error import HTTPError, URLError
from urllib.parse import quote, urlencode
from urllib.request import Request, urlopen
import uuid


ROOT = Path(__file__).resolve().parent.parent


def jwt(secret):
    def encoded(value):
        return base64.urlsafe_b64encode(json.dumps(value).encode()).rstrip(b"=")

    now = int(time.time())
    # 特意超过一天：准入契约不限制 lifetime。
    unsigned = b".".join([
        encoded({"alg": "HS256", "typ": "JWT"}),
        encoded({"role": "authenticated", "iss": "inkcre-peer", "aud": "inkcre-api",
                 "iat": now, "exp": now + 172800}),
    ])
    signature = base64.urlsafe_b64encode(
        hmac.new(secret.encode(), unsigned, hashlib.sha256).digest()
    ).rstrip(b"=")
    return (unsigned + b"." + signature).decode()


class Journey:
    def __init__(self, args, directory):
        self.args = args
        self.directory = directory
        self.env = os.environ.copy()
        if args.config_json:
            values = json.loads(args.config_json.read_text())
            for key in ("DATABASE_URL", "JWT_SECRET"):
                self.env[key] = values[key]
        if not self.env.get("DATABASE_URL") or not self.env.get("JWT_SECRET"):
            raise RuntimeError("需要 DATABASE_URL/JWT_SECRET 或 --config-json")
        self.peer = str(uuid.uuid4())
        self.env.update({"INKCRE_PEER_ID": self.peer, "INKCRE_PEER_NAME": "Rust acceptance",
                         "INKCRE_STATE_DIR": str(directory / "state"), "RUST_LOG": "warn"})
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        self.env["INKCRE_BIND"] = f"127.0.0.1:{port}"
        self.base = f"http://127.0.0.1:{port}"
        self.token = jwt(self.env["JWT_SECRET"])
        self.process = None
        self.log = (directory / "app.log").open("ab")
        self.blocks = []
        self.relations = []
        self.blobs = []
        self.jobs = []
        self.job_type = None
        self.plugin = None
        self.plugin_block = None
        self.manifest_path = None
        self.observations = []

    def request(self, method, path, body=None, token=True, expected=200):
        headers = {}
        if token:
            headers["Authorization"] = "Bearer " + self.token
        data = None
        if isinstance(body, bytes):
            headers["Content-Type"] = "application/octet-stream"
            data = body
        elif body is not None:
            headers["Content-Type"] = "application/json"
            data = json.dumps(body).encode()
        try:
            response = urlopen(Request(self.base + path, data=data, headers=headers, method=method),
                               timeout=40)
        except HTTPError as error:
            if error.code == expected:
                return None
            # 不回显服务端响应，避免第三方错误包含业务数据或秘密。
            raise RuntimeError(f"{method} {path} 返回 HTTP {error.code}，预期 {expected}") from None
        with response:
            if response.status != expected:
                raise RuntimeError(f"{method} {path} 返回非预期状态 {response.status}")
            raw = response.read()
            if not raw:
                return None
            return json.loads(raw) if "application/json" in response.headers.get("Content-Type", "") else raw

    def start(self):
        self.process = subprocess.Popen([str(self.args.binary), "serve"], cwd=ROOT, env=self.env,
                                        stdin=subprocess.DEVNULL, stdout=self.log, stderr=self.log)
        deadline = time.monotonic() + 60
        while time.monotonic() < deadline:
            if self.process.poll() is not None:
                raise RuntimeError("app 启动失败；检查本机 .runtime/acceptance 运行日志")
            try:
                self.request("GET", "/health/ready", token=False)
                return
            except (URLError, RuntimeError, TimeoutError):
                time.sleep(0.25)
        raise RuntimeError("app 未在 60 秒内 ready")

    def stop(self):
        if self.process and self.process.poll() is None:
            self.process.send_signal(signal.SIGTERM)
            try:
                self.process.wait(timeout=15)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=5)
                raise RuntimeError("app 未优雅退出")

    def prepare_plugin(self):
        if not self.args.extension_binary:
            self.env.pop("INKCRE_EXTENSION_MANIFESTS", None)
            return
        plugin_dir = self.directory / "plugin"
        plugin_dir.mkdir()
        executable = plugin_dir / self.args.extension_binary.name
        shutil.copy2(self.args.extension_binary, executable)
        manifest = plugin_dir / "extension.json"
        self.manifest_path = manifest
        manifest.write_text(json.dumps({
            "schema_version": 1, "name": "example/notes", "version": "0.1.0",
            "distribution": "rust_stdio", "host_sdk": "core-rs", "host_sdk_version": "^0.1",
            "protocol_version": 1, "executable": executable.name,
            "sha256": hashlib.sha256(executable.read_bytes()).hexdigest(),
        }))
        self.env["INKCRE_EXTENSION_MANIFESTS"] = str(manifest)

    def run(self):
        self.prepare_plugin()
        self.start()
        self.request("GET", "/api/v1/blocks", token=False, expected=401)
        self.observations.append("native 数据库准入与 HTTP 匿名拒绝")
        marker = "impl-rs-acceptance-" + str(uuid.uuid4())
        for content in (marker, marker + " target"):
            block = self.request("POST", "/api/v1/blocks",
                                 {"resolver": "core.text.v1", "content": content}, expected=201)
            self.blocks.append(block["id"])
        relation = self.request("POST", "/api/v1/relations",
                                {"from_": self.blocks[0], "to_": self.blocks[1], "content": "acceptance,link"},
                                expected=201)
        self.relations.append(relation["id"])
        self.request("PATCH", f"/api/v1/blocks/{self.blocks[0]}", {"content": marker + " updated"})
        self.observations.append("Block / Relation 创建与更新")

        neighborhood = self.request("POST", "/api/v1/graph/neighborhood",
                                    {"block_id": self.blocks[0], "direction": "out",
                                     "contents": ["acceptance,link"], "limit": 50})
        graph = neighborhood.get("graph", neighborhood)
        if {block["id"] for block in graph["blocks"]} != set(self.blocks):
            raise RuntimeError("图邻域没有返回闭合端点")
        query = {"source": self.blocks[0], "target": self.blocks[1], "direction": "out",
                 "contents": json.dumps(["acceptance,link"])}
        path = self.request("GET", "/api/v1/graph/path?" + urlencode(query))
        if path["outcome"] != "found" or path["block_ids"] != self.blocks:
            raise RuntimeError("有向路径与持久图不一致")
        query.update({"source": self.blocks[1], "target": self.blocks[0]})
        reverse = self.request("GET", "/api/v1/graph/path?" + urlencode(query))
        if reverse["outcome"] != "not_found":
            raise RuntimeError("有向路径错误地允许反向边")
        self.observations.append("有向图邻域端点闭合与最短路径")

        content = marker.encode()
        blob = self.request("POST", "/api/v1/storage-blobs", content, expected=201)
        self.blobs.append(blob["id"])
        if self.request("GET", f"/api/v1/storage-blobs/{blob['id']}") != content:
            raise RuntimeError("存储字节往返不一致")
        self.observations.append("真实 PostgreSQL 二进制存储往返")

        self.job_type = "acceptance." + str(uuid.uuid4())
        self.request("POST", "/api/v1/job-types", {"id": self.job_type, "description": "Rust acceptance",
                     "parameters_schema": {"type": "object", "properties": {"marker": {"type": "string"}},
                                           "required": ["marker"], "additionalProperties": False},
                     "default_timeout_seconds": 120})
        job = self.request("POST", "/api/v1/jobs", {"type": self.job_type, "parameters": {"marker": marker}}, expected=201)
        self.jobs.append(job["id"])
        claimed = self.request("POST", f"/api/v1/jobs/{job['id']}/claim")
        if claimed["status"] != "running" or claimed["started_at"] is None:
            raise RuntimeError("Job 原子领取或数据库启动时间不正确")
        finished = self.request("POST", f"/api/v1/jobs/{job['id']}/finish",
                                {"status": "finished", "state": {"marker": marker}})
        if finished["status"] != "finished" or finished["closed_at"] is None:
            raise RuntimeError("Job 完成状态未持久化")
        self.observations.append("独立 Job catalog 参数准入、原子领取与完成")

        if self.args.extension_binary:
            self.plugin = quote("example/notes", safe="")
            # catalog 已在启动时固定0.1，路径文件改变不能静默选择另一坐标。
            original = self.manifest_path.read_text()
            changed = json.loads(original)
            changed["version"] = "0.2.0"
            self.manifest_path.write_text(json.dumps(changed))
            try:
                self.request("POST", "/api/v1/extensions/install",
                             {"name": "example/notes", "version": "0.1.0"}, expected=409)
            finally:
                self.manifest_path.write_text(original)
            self.request("POST", "/api/v1/extensions/install", {"name": "example/notes", "version": "0.1.0"})
            self.request("POST", f"/api/v1/extensions/{self.plugin}/enable")
            result = self.request("POST", f"/api/v1/extensions/{self.plugin}/invoke",
                                  {"method": "capture_text", "parameters": {"text": marker + " from plugin"}})
            self.plugin_block = result["id"]
            self.blocks.append(self.plugin_block)
            saved = self.request("GET", f"/api/v1/blocks/{self.plugin_block}")
            if saved["content"] != marker + " from plugin":
                raise RuntimeError("插件未通过 core SDK 保存真实 Block")
            self.observations.append("manifest 摘要准入、共享参与启用与插件直连数据库保存资料")

        self.stop()
        self.start()
        saved = self.request("GET", f"/api/v1/blocks/{self.blocks[0]}")
        if saved["content"] != marker + " updated":
            raise RuntimeError("重启后持久内容不一致")
        if self.plugin:
            result = self.request("POST", f"/api/v1/extensions/{self.plugin}/invoke",
                                  {"method": "get_block", "parameters": {"id": self.plugin_block}})
            if result.get("content") != marker + " from plugin":
                raise RuntimeError("重启未恢复精确插件选择与既有资料读取")
        self.observations.append("app 重启后数据库持久化与插件精确恢复")

    def cleanup(self):
        errors = []
        if self.process and self.process.poll() is None:
            operations = []
            if self.plugin:
                operations += [("POST", f"/api/v1/extensions/{self.plugin}/disable", 200),
                               ("DELETE", f"/api/v1/extensions/{self.plugin}/participation", 200)]
            operations += [("DELETE", f"/api/v1/jobs/{id_}", 204) for id_ in self.jobs]
            if self.job_type:
                operations += [("DELETE", f"/api/v1/job-types/{self.job_type}", 204)]
            operations += [("DELETE", f"/api/v1/relations/{id_}", 204) for id_ in self.relations]
            operations += [("DELETE", f"/api/v1/blocks/{id_}", 204) for id_ in reversed(self.blocks)]
            operations += [("DELETE", f"/api/v1/storage-blobs/{id_}", 204) for id_ in self.blobs]
            operations += [("DELETE", f"/api/v1/peers/{self.peer}", 204)]
            for method, path, expected in operations:
                try:
                    self.request(method, path, expected=expected)
                except (RuntimeError, URLError, TimeoutError):
                    errors.append(path)
        try:
            self.stop()
        except RuntimeError:
            errors.append("process shutdown")
        self.log.close()
        if errors:
            raise RuntimeError("部分验收资源未清理：" + ", ".join(errors))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config-json", type=Path, help="包含 DATABASE_URL/JWT_SECRET 的私密 JSON；不回显")
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/inkcre")
    parser.add_argument("--extension-binary", type=Path, help="构建出的 inkcre-note-extension，启用完整插件旅程")
    args = parser.parse_args()
    args.binary = args.binary.resolve(strict=True)
    if args.extension_binary:
        args.extension_binary = args.extension_binary.resolve(strict=True)
    runtime = ROOT / ".runtime/acceptance"
    runtime.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=runtime) as name:
        journey = Journey(args, Path(name))
        failure = None
        try:
            journey.run()
        except Exception as error:
            failure = error
        finally:
            try:
                journey.cleanup()
            except Exception as error:
                failure = error if failure is None else RuntimeError(f"{failure}；{error}")
        if failure:
            shutil.copy2(Path(name) / "app.log", runtime / "last-failure.log")
            print(json.dumps({"ok": False, "error": str(failure)}, ensure_ascii=False))
            return 1
        print(json.dumps({"ok": True, "observations": journey.observations,
                          "shared_extension_identity_retained": bool(journey.plugin)}, ensure_ascii=False))
    return 0


if __name__ == "__main__":
    sys.exit(main())
