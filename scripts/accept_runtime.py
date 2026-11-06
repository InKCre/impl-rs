#!/usr/bin/env python3
"""显式运行知识能力、真实 RSS/Atom worker 与插件取消的端到端旅程。"""

import argparse
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import threading
import time
from urllib.parse import quote, urlencode

from acceptance import Journey, ROOT


class FeedServer(ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self):
        super().__init__(("127.0.0.1", 0), FeedHandler)
        self.base = f"http://127.0.0.1:{self.server_port}"
        self.version = 1
        self.requests = []
        self.slow_entered = threading.Event()
        self.release_slow = threading.Event()
        self.attachment = b"InKCre opaque enclosure bytes\x00\x01\xff"


class FeedHandler(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_GET(self):
        server = self.server
        server.requests.append((self.path, {key.lower(): value for key, value in self.headers.items()}))
        status = 200
        mime = "application/rss+xml; charset=utf-8"
        etag = None
        if self.path == "/slow.xml":
            server.slow_entered.set()
            server.release_slow.wait(timeout=20)
            body = self.rss().encode()
        elif self.path == "/feed.xml":
            etag = f'"runtime-rss-{server.version}"'
            if self.headers.get("If-None-Match") == etag:
                status, body = 304, b""
            else:
                body = self.rss().encode()
        elif self.path == "/atom.xml":
            mime = "application/atom+xml; charset=utf-8"
            body = f'''<?xml version="1.0"?><feed xmlns="http://www.w3.org/2005/Atom">
<id>urn:inkcre:runtime:atom</id><title>Atom runtime</title><updated>2020-01-01T00:00:00Z</updated>
<link rel="self" href="{server.base}/atom.xml"/>
<entry><id>urn:inkcre:runtime:atom:item</id><title>Atom canonical item</title>
<updated>2020-01-01T00:00:00Z</updated><content type="html">&lt;p&gt;Atom authored content&lt;/p&gt;</content>
<link rel="alternate" href="{server.base}/article"/></entry></feed>'''.encode()
        elif self.path == "/attachment.bin":
            mime, body = "application/octet-stream", server.attachment
        elif self.path == "/article":
            mime = "text/html; charset=utf-8"
            paragraph = "This article explains durable information, source provenance, and exact identities. " * 12
            body = f'''<!doctype html><html><head><title>Runtime article</title></head><body>
<nav>Navigation outside the article</nav><main><article><h1>Runtime article</h1>
<p>{paragraph}</p><p>Read the <a href="{server.base}/reference">source reference</a>.</p>
<p>{paragraph}</p></article></main></body></html>'''.encode()
        else:
            status, mime, body = 404, "text/plain", b"not found"
        try:
            self.send_response(status)
            self.send_header("Content-Type", mime)
            self.send_header("Content-Length", str(len(body)))
            if etag:
                self.send_header("ETag", etag)
                self.send_header("Last-Modified", "Wed, 01 Jan 2020 00:00:00 GMT")
            self.end_headers()
            if body:
                self.wfile.write(body)
        except (BrokenPipeError, ConnectionResetError):
            pass

    def rss(self):
        base = self.server.base
        second = ""
        if self.server.version > 1:
            second = f'''<item><guid isPermaLink="false">urn:inkcre:runtime:rss:second</guid>
<title>Second canonical item</title><description>Second authored summary</description>
<link>{base}/article</link><pubDate>Wed, 01 Jan 2020 00:00:00 GMT</pubDate></item>'''
        return f'''<?xml version="1.0"?><rss version="2.0" xmlns:atom="http://www.w3.org/2005/Atom">
<channel><title>RSS runtime</title><link>{base}/</link><description>Canonical feed fixture</description>
<atom:link rel="self" href="{base}/feed.xml" type="application/rss+xml"/>
<item><guid isPermaLink="false">urn:inkcre:runtime:rss:first</guid><title>First canonical item</title>
<description><![CDATA[<p>Authored <b>summary</b></p>]]></description><link>{base}/article</link>
<pubDate>Wed, 01 Jan 2020 00:00:00 GMT</pubDate>
<enclosure url="{base}/attachment.bin" type="application/octet-stream" length="{len(self.server.attachment)}"/>
</item>{second}<item><title>Unidentified item must be discarded</title>
<description>No guid or alternate URL</description></item></channel></rss>'''


class RuntimeJourney(Journey):
    def __init__(self, args, directory, feed):
        super().__init__(args, directory)
        self.feed = feed
        self.sources = []
        self.crons = []
        self.pointers = []
        self.installed = False
        self.owns_info_base = False
        self.env["INKCRE_NO_CRON"] = "true"
        self.env["INKCRE_NO_WORKER"] = "true"

    def prepare_plugin(self):
        directory = self.directory / "plugin"
        directory.mkdir()
        executable = directory / self.args.extension_binary.name
        shutil.copy2(self.args.extension_binary, executable)
        manifest = directory / "extension.json"
        manifest.write_text(json.dumps({
            "schema_version": 1, "name": "inkcre/rss", "version": "0.1.0",
            "distribution": "rust_stdio", "host_sdk": "core-rs", "host_sdk_version": "^0.1",
            "protocol_version": 1, "executable": executable.name,
            "sha256": hashlib.sha256(executable.read_bytes()).hexdigest(),
        }))
        self.env["INKCRE_EXTENSION_MANIFESTS"] = str(manifest)
        self.plugin = quote("inkcre/rss", safe="")

    def items(self, path):
        return self.request("GET", path + "?limit=1000")["items"]

    def wait_job(self, id_, status="finished", timeout=90):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            job = self.request("GET", f"/api/v1/jobs/{id_}")
            if job["status"] not in ("pending", "running"):
                if job["status"] != status:
                    raise RuntimeError(f"Job {id_} ended as {job['status']}, expected {status}")
                return job
            time.sleep(0.15)
        raise RuntimeError(f"Job {id_} did not finish within the acceptance budget")

    def collect(self, source, config=None, timeout=None):
        job = self.request("POST", f"/api/v1/sources/{source}/collect",
                           {"config": config or {}, "timeout_seconds": timeout}, expected=202)
        self.jobs.append(job["id"])
        return job["id"]

    def source(self, type_, path, **config):
        source = self.request("POST", "/api/v1/sources", {
            "type": type_, "nickname": "Rust runtime acceptance", "storage": -4,
            "config": {"feed_url": self.feed.base + path, "unidentified_item_behavior": "discard", **config},
        }, expected=201)
        self.sources.append(source["id"])
        return source["id"]

    def plugin_ready(self):
        deadline = time.monotonic() + 20
        while time.monotonic() < deadline:
            status = self.request("GET", "/api/v1/extensions")
            if any(item["name"] == "inkcre/rss" and item["running"] for item in status):
                return
            time.sleep(0.2)
        raise RuntimeError("exact enabled RSS plugin did not recover")

    def knowledge(self):
        draft = self.request("POST", "/api/v1/graph/draft", {"resolver": "core.text.v1", "input": {"text": ""}})
        persisted = self.request("POST", "/api/v1/graph/submit", draft, expected=201)
        if self.request("GET", f"/api/v1/blocks/{persisted['root']}/text") != "":
            raise RuntimeError("authored empty text was confused with missing projection")
        pointer = self.request("POST", "/api/v1/storages/-4/objects", b"Stored Unicode text", expected=201)["pointer"]
        block = self.request("POST", "/api/v1/blocks", {
            "resolver": "core.text.v1", "storage": -4, "content": pointer,
        }, expected=201)
        if self.request("GET", f"/api/v1/blocks/{block['id']}/text") != "Stored Unicode text":
            raise RuntimeError("negative PostgreSQL Storage identity did not hydrate")
        self.pointers.append((-4, pointer))
        block = self.request("POST", "/api/v1/blocks", {
            "resolver": "core.html.v1", "storage": -1, "content": self.feed.base + "/article",
        }, expected=201)
        text = self.request("GET", f"/api/v1/blocks/{block['id']}/text")
        if "Runtime article" not in text or self.feed.base + "/reference" not in text:
            raise RuntimeError("HTTP Storage / HTML projection lost authored text or link target")
        self.request("PATCH", f"/api/v1/blocks/{block['id']}", {"content": "invalid url"}, expected=422)
        if self.request("GET", f"/api/v1/blocks/{block['id']}")["content"] != self.feed.base + "/article":
            raise RuntimeError("invalid PATCH changed persisted content")
        self.observations.append("typed draft/事务提交、空文本、-4/-1实际内容与PATCH准入")

    def run(self):
        check = subprocess.run([str(self.args.binary), "db-check"], env=self.env,
                               capture_output=True, text=True, timeout=60)
        if check.returncode or json.loads(check.stdout).get("environment") != "development":
            raise RuntimeError("runtime journey requires a disposable development database")
        self.prepare_plugin()
        self.start()
        if any(self.items(path) for path in ("/api/v1/blocks", "/api/v1/sources", "/api/v1/jobs", "/api/v1/crons")):
            raise RuntimeError("runtime journey requires an empty dedicated development info-base")
        self.owns_info_base = True
        self.stop()
        self.env.pop("INKCRE_NO_WORKER", None)
        self.start()
        self.knowledge()
        self.request("POST", "/api/v1/extensions/install", {"name": "inkcre/rss", "version": "0.1.0"})
        self.installed = True
        self.request("POST", f"/api/v1/extensions/{self.plugin}/enable")
        methods = self.request("GET", "/api/v1/resolvers/extensions.rss.feed_item.v1/methods")
        if not any(method["name"] == "get_solved_content" for method in methods):
            raise RuntimeError("plugin did not publish typed Resolver methods")
        rss = self.source("extensions.rss.rss.Source", "/feed.xml", fetch_full_text=True,
                          download_enclosures=True, target_storage_id=-1)
        self.request("POST", "/api/v1/sources", {"type": "extensions.rss.rss.Source", "config": {"feed_url": self.feed.base + "/feed.xml"}, "storage": -1}, expected=422)
        self.request("POST", f"/api/v1/sources/{rss}/backfill", {"config": {}}, expected=501)
        job = self.wait_job(self.collect(rss))
        item_ids = job["state"].get("primary_item_block_ids", [])
        if len(item_ids) != 1:
            raise RuntimeError("RSS identity/discard policy did not preserve exactly one known item")
        item = self.request("GET", f"/api/v1/blocks/{item_ids[0]}/solved?materialize_missing=false")
        if item["root"]["source_native_id"] != "urn:inkcre:runtime:rss:first":
            raise RuntimeError("RSS GUID was rewritten")
        if not item.get("full_text_block_id") or len(item.get("enclosure_block_ids", [])) != 1:
            raise RuntimeError("RSS full text/enclosure graph was not materialized")
        full_text = self.request("GET", f"/api/v1/blocks/{item_ids[0]}/text?materialize_missing=false")
        if "durable information" not in full_text:
            raise RuntimeError("RSS Resolver did not return article text")
        enclosure = self.request("GET", f"/api/v1/blocks/{item['enclosure_block_ids'][0]}/solved")
        if not enclosure.get("content_block_id"):
            raise RuntimeError("enclosure has no byte-backed semantic child")
        actual = self.request("GET", f"/api/v1/blocks/{enclosure['content_block_id']}/content")
        if actual != self.feed.attachment:
            raise RuntimeError("enclosure bytes changed")
        before = len(self.items("/api/v1/blocks"))
        self.wait_job(self.collect(rss))
        if len(self.items("/api/v1/blocks")) != before:
            raise RuntimeError("conditional RSS repeat created duplicate graph")
        conditional = [headers for path, headers in self.feed.requests if path == "/feed.xml"]
        if not any(headers.get("if-none-match") == '"runtime-rss-1"' for headers in conditional):
            raise RuntimeError("Source ETag was not reused for its configured URL")
        self.feed.version = 2
        self.wait_job(self.collect(rss))
        known = [block for block in self.items("/api/v1/blocks") if block["resolver"] == "extensions.rss.feed_item.v1"]
        if len(known) != 2:
            raise RuntimeError("new exact identity or old identity reconciliation is incorrect")
        self.observations.append("RSS Source→实际worker→canonical图、正文与附件、ETag304、重采集幂等")

        atom = self.source("extensions.rss.atom.Source", "/atom.xml", fetch_full_text=False)
        atom_job = self.wait_job(self.collect(atom))
        atom_item = self.request("GET", f"/api/v1/blocks/{atom_job['state']['primary_item_block_ids'][0]}/solved?materialize_missing=false")
        if atom_item["root"]["source_native_id_kind"] != "atom_id":
            raise RuntimeError("Atom identity contract was lost")
        self.observations.append("Atom同一Source/worker运行时与typed读取")

        slow = self.source("extensions.rss.rss.Source", "/slow.xml", fetch_full_text=False, request_timeout_seconds=30)
        slow_job = self.collect(slow)
        if not self.feed.slow_entered.wait(timeout=20):
            raise RuntimeError("RSS worker did not dispatch the actual HTTP request")
        self.request("POST", f"/api/v1/jobs/{slow_job}/abort")
        self.wait_job(slow_job, status="aborted", timeout=20)
        self.feed.release_slow.set()
        self.plugin_ready()
        self.wait_job(self.collect(rss))
        self.observations.append("长采集abort真实回收与精确插件恢复，后续新Job可执行")

        self.stop()
        self.env.pop("INKCRE_NO_CRON", None)
        self.start()
        self.plugin_ready()
        # 专用Source使Cron产生的工作与其他采集互不混淆。
        cron_source = self.source("extensions.rss.rss.Source", "/feed.xml", fetch_full_text=False)
        cron = self.request("POST", "/api/v1/crons", {"schedule": "* * * * *", "enabled": True,
                           "job_type": "core.source.collect.v1", "job_parameters": {"source": cron_source, "config": {}}}, expected=201)
        self.crons.append(cron["id"])
        deadline = time.monotonic() + 25
        while time.monotonic() < deadline:
            saved = self.request("GET", f"/api/v1/crons/{cron['id']}")
            if saved["last_job"]:
                break
            time.sleep(0.2)
        else:
            raise RuntimeError("app Cron loop did not create the due occurrence")
        self.wait_job(saved["last_job"])
        self.jobs.append(saved["last_job"])
        repeated = self.request("POST", f"/api/v1/crons/{cron['id']}/materialize")
        current = self.request("GET", f"/api/v1/crons/{cron['id']}")
        if repeated is not None and current["last_scheduled_for"] == saved["last_scheduled_for"]:
            raise RuntimeError("Cron created a second Job for the same occurrence")
        if repeated:
            self.jobs.append(repeated["id"])
            self.wait_job(repeated["id"])
        self.observations.append("app重启恢复、自动Cron物化与同occurrence去重")
        self.request("POST", f"/api/v1/extensions/{self.plugin}/disable")
        self.request("GET", "/api/v1/resolvers/extensions.rss.feed_item.v1/methods", expected=501)
        self.request("GET", f"/api/v1/blocks/{item_ids[0]}")
        self.observations.append("插件撤销typed能力但保留持久资料")

    def cleanup(self):
        self.feed.release_slow.set()
        errors = []
        try:
            if self.owns_info_base and self.process and self.process.poll() is None:
                for id_ in self.crons:
                    self.request("DELETE", f"/api/v1/crons/{id_}", expected=204)
                if self.installed:
                    self.request("POST", f"/api/v1/extensions/{self.plugin}/disable")
                for job in self.items("/api/v1/jobs"):
                    if job["status"] in ("pending", "running"):
                        self.request("POST", f"/api/v1/jobs/{job['id']}/abort")
                        if job["status"] == "running":
                            self.wait_job(job["id"], status="aborted", timeout=20)
                    if job["id"] not in self.jobs:
                        self.jobs.append(job["id"])
                for id_ in self.sources:
                    self.request("DELETE", f"/api/v1/sources/{id_}", expected=204)
                blocks = self.items("/api/v1/blocks")
                self.blocks = [block["id"] for block in blocks]
                self.pointers.extend((block["storage"], block["content"]) for block in blocks if block["storage"] == -4)
                for id_ in list(self.blocks):
                    self.request("DELETE", f"/api/v1/blocks/{id_}", expected=204)
                    self.blocks.remove(id_)
                for storage, pointer in set(self.pointers):
                    self.request("DELETE", f"/api/v1/storages/{storage}/objects?" + urlencode({"pointer": pointer}), expected=204)
        except Exception as error:
            errors.append(str(error))
        finally:
            if not self.installed:
                self.plugin = None
            try:
                super().cleanup()
            except Exception as error:
                errors.append(str(error))
        if errors:
            raise RuntimeError("runtime cleanup incomplete: " + "; ".join(errors))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config-json", type=Path, help="独占development数据库的私密native配置")
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/inkcre")
    parser.add_argument("--extension-binary", type=Path, default=ROOT / "target/debug/inkcre-rss-extension")
    args = parser.parse_args()
    args.binary = args.binary.resolve(strict=True)
    args.extension_binary = args.extension_binary.resolve(strict=True)
    runtime = ROOT / ".runtime/acceptance"
    runtime.mkdir(parents=True, exist_ok=True)
    feed = FeedServer()
    thread = threading.Thread(target=feed.serve_forever, daemon=True)
    thread.start()
    try:
        with tempfile.TemporaryDirectory(dir=runtime) as name:
            journey = RuntimeJourney(args, Path(name), feed)
            failure = None
            try:
                journey.run()
            except Exception as error:
                failure = error
            finally:
                try:
                    journey.cleanup()
                except Exception as error:
                    failure = error if failure is None else RuntimeError(f"{failure}; {error}")
            if failure:
                shutil.copy2(Path(name) / "app.log", runtime / "last-runtime-failure.log")
                print(json.dumps({"ok": False, "error": str(failure)}, ensure_ascii=False))
                return 1
            print(json.dumps({"ok": True, "observations": journey.observations}, ensure_ascii=False))
            return 0
    finally:
        feed.release_slow.set()
        feed.shutdown()
        feed.server_close()
        thread.join(timeout=3)


if __name__ == "__main__":
    sys.exit(main())
