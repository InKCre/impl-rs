#!/usr/bin/env python3
"""显式验收 Memos HTTP、共享图和真实32 MiB附件。不接入自动化测试套件。"""

from __future__ import annotations

import argparse
import base64
import hashlib
import importlib.util
import json
from pathlib import Path
import shutil
import subprocess
import tempfile
import time
from urllib.error import HTTPError
from urllib.parse import urlencode
from urllib.request import Request, urlopen

import psycopg
from psycopg.types.json import Jsonb

ROOT = Path(__file__).resolve().parents[3]
spec = importlib.util.spec_from_file_location(
    "acceptance", ROOT / "scripts/acceptance.py"
)
acceptance = importlib.util.module_from_spec(spec)
spec.loader.exec_module(acceptance)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config-json", required=True, type=Path)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--extension-binary", required=True, type=Path)
    args = parser.parse_args()
    args.binary = args.binary.resolve(strict=True)
    args.extension_binary = args.extension_binary.resolve(strict=True)
    runtime = ROOT / ".runtime/memos-acceptance"
    runtime.mkdir(parents=True, exist_ok=True)
    settings = json.loads(args.config_json.read_text())
    blocks, blobs = set(), set()
    with psycopg.connect(settings["DATABASE_URL"], autocommit=True) as database:
        original = database.execute(
            "SELECT nickname,config,state FROM inkcre.extensions WHERE name='inkcre/memos'"
        ).fetchone()
        with tempfile.TemporaryDirectory(dir=runtime) as temp:
            directory = Path(temp)
            journey = acceptance.Journey(args, directory)
            plugin = directory / "plugin"
            plugin.mkdir()
            executable = plugin / "memos"
            shutil.copy2(args.extension_binary, executable)
            manifest = plugin / "extension.json"
            manifest.write_bytes(
                subprocess.check_output(
                    [str(args.binary), "extension-manifest", "--name", "inkcre/memos",
                     "--version", "0.1.0", str(executable)],
                    timeout=30,
                )
            )
            journey.env["INKCRE_EXTENSION_MANIFESTS"] = str(manifest)
            pat = "memos_pat_" + "a" * 32
            admitted = False

            def http(method, path, body=None, token=pat, expected=200):
                data = None if body is None else json.dumps(body).encode()
                headers = {} if token is None else {"Authorization": "Bearer " + token}
                if data is not None:
                    headers["Content-Type"] = "application/json"
                try:
                    result = urlopen(
                        Request(
                            journey.base + path,
                            data=data,
                            headers=headers,
                            method=method,
                        ),
                        timeout=60,
                    )
                except HTTPError as error:
                    if error.code == expected:
                        return None
                    raise RuntimeError(
                        f"Memos {method} returned HTTP {error.code}, expected {expected}"
                    ) from None
                with result:
                    if result.status != expected:
                        raise RuntimeError("Memos returned unexpected HTTP status")
                    raw = result.read()
                    return (
                        json.loads(raw)
                        if "application/json" in result.headers.get("Content-Type", "")
                        else raw
                    )

            def memo(body, parent=None):
                path = "/memos/api/v1/memos" + (f"/{parent}/comments" if parent else "")
                result = http("POST", path, body)
                blocks.add(int(result["name"].split("/")[1]))
                return result

            def attachment(content, owner=None):
                body = {
                    "filename": "fixture.bin",
                    "type": "application/octet-stream",
                    "content": base64.b64encode(content).decode(),
                }
                if owner:
                    body["memo"] = owner
                result = http("POST", "/memos/api/v1/attachments", body)
                id_ = int(result["name"].split("/")[1])
                blocks.add(id_)
                child = database.execute(
                    "SELECT to_ FROM inkcre.relations WHERE from_=%s AND content='content'",
                    (id_,),
                ).fetchone()[0]
                blocks.add(child)
                pointer = database.execute(
                    "SELECT content FROM inkcre.blocks WHERE id=%s", (child,)
                ).fetchone()[0]
                blobs.add(json.loads(pointer)["blob_id"])
                return result

            try:
                check = subprocess.run(
                    [str(args.binary), "db-check"],
                    env=journey.env,
                    capture_output=True,
                    timeout=30,
                )
                if (
                    check.returncode
                    or json.loads(check.stdout).get("environment") != "development"
                ):
                    raise RuntimeError(
                        "Memos acceptance requires a dedicated development database"
                    )
                admitted = True
                journey.start()
                journey.request(
                    "POST",
                    "/extensions/inkcre/memos/selection",
                    {"distribution": "rust_stdio", "version": "0.1.0"},
                )
                journey.request("POST", "/extensions/inkcre/memos/enable")
                deadline = time.monotonic() + 20
                while time.monotonic() < deadline:
                    status = journey.request("GET", "/api/v1/extensions")
                    if any(
                        item["name"] == "inkcre/memos" and item["running"]
                        for item in status
                    ):
                        break
                    time.sleep(0.2)
                else:
                    raise RuntimeError(
                        "Memos did not start with its exact local selection"
                    )
                journey.request(
                    "PUT",
                    "/extensions/inkcre/memos/config",
                    {"personal_access_token": pat},
                )
                assert http("GET", "/memos/api/v1/instance/profile", token=None) == {
                    "version": "0.29.1"
                }
                http("GET", "/memos/api/v1/auth/me", token=None, expected=401)
                http("GET", "/memos/connection", token=pat, expected=401)
                journey.request("GET", "/memos/connection", expected=409)
                database.execute(
                    "UPDATE inkcre.peers SET config=%s WHERE id=%s",
                    (Jsonb({"http_public_base_url": journey.base}), journey.peer),
                )
                assert (
                    journey.request("GET", "/memos/connection")["server_url"]
                    == journey.base + "/memos"
                )
                assert (
                    http("GET", "/memos/api/v1/auth/me")["user"]["name"]
                    == "users/inkcre"
                )
                root = memo(
                    {
                        "content": "Memos durable body",
                        "visibility": "PUBLIC",
                        "createTime": "2020-01-01T08:00:00+08:00",
                    }
                )
                root_id = int(root["name"].split("/")[1])
                saved = database.execute(
                    "SELECT resolver,content FROM inkcre.blocks WHERE id=%s", (root_id,)
                ).fetchone()
                assert saved[0] == "extensions.memos.memo.v1"
                assert json.loads(saved[1])["visibility"] == "public"
                assert json.loads(saved[1])["created_at"].startswith(
                    "2020-01-01T00:00:00"
                )
                assert (
                    journey.request("GET", f"/api/v1/blocks/{root_id}/text")
                    == "Memos durable body"
                )
                child = memo({"content": "comment", "visibility": "PRIVATE"}, root_id)
                assert (
                    child["visibility"] == "PUBLIC" and child["parent"] == root["name"]
                )
                assert (
                    http("GET", f"/memos/api/v1/memos/{root_id}/comments")["totalSize"]
                    == 1
                )
                http("GET", "/memos/api/v1/memos", expected=400)
                query = urlencode(
                    {"filter": 'creator == "users/inkcre"', "pageSize": 1}
                )
                page = http("GET", "/memos/api/v1/memos?" + query)
                assert root["name"] in {row["name"] for row in page["memos"]}
                patched = http(
                    "PATCH",
                    f"/memos/api/v1/memos/{root_id}?updateMask=content,pinned",
                    {"content": "edited", "pinned": True},
                )
                assert patched["content"] == "edited" and patched["pinned"]
                http(
                    "PATCH",
                    f"/memos/api/v1/memos/{root_id}",
                    {"content": None},
                    expected=400,
                )
                payload = bytes(range(256)) * (32 * 1024 * 1024 // 256)
                attached = attachment(payload, root["name"])
                id_ = int(attached["name"].split("/")[1])
                downloaded = http("GET", f"/memos/file/attachments/{id_}/fixture.bin")
                assert (
                    len(downloaded) == len(payload)
                    and hashlib.sha256(downloaded).digest()
                    == hashlib.sha256(payload).digest()
                )
                other = memo({"content": "other root"})
                other_id = int(other["name"].split("/")[1])
                page = http("GET", "/memos/api/v1/memos?" + query)
                assert page["memos"][0]["name"] == other["name"]
                assert page["nextPageToken"]
                next_query = urlencode(
                    {
                        "filter": 'creator == "users/inkcre"',
                        "pageSize": 1,
                        "pageToken": page["nextPageToken"],
                    }
                )
                assert (
                    http("GET", "/memos/api/v1/memos?" + next_query)["memos"][0]["name"]
                    == root["name"]
                )
                wrong_query = urlencode(
                    {
                        "filter": 'creator == "users/inkcre"',
                        "state": "ARCHIVED",
                        "pageToken": page["nextPageToken"],
                    }
                )
                http("GET", "/memos/api/v1/memos?" + wrong_query, expected=400)
                memo({"content": "second comment"}, root_id)
                comment_page = http(
                    "GET", f"/memos/api/v1/memos/{root_id}/comments?pageSize=1"
                )
                assert comment_page["totalSize"] == 2 and comment_page["nextPageToken"]
                comment_token = urlencode({"pageToken": comment_page["nextPageToken"]})
                http(
                    "GET",
                    f"/memos/api/v1/memos/{other_id}/comments?" + comment_token,
                    expected=400,
                )
                http(
                    "PATCH",
                    f"/memos/api/v1/memos/{other_id}",
                    {"attachments": [{"name": attached["name"]}]},
                    expected=400,
                )
                reset = http(
                    "PATCH",
                    f"/memos/api/v1/memos/{other_id}?updateMask=content,pinned",
                    {},
                )
                assert reset["content"] == "" and reset["pinned"] is False
                detached = attachment(b"detached bytes", other["name"])
                detached_id = int(detached["name"].split("/")[1])
                http("PATCH", f"/memos/api/v1/memos/{other_id}", {"attachments": []})
                assert (
                    database.execute(
                        "SELECT count(*) FROM inkcre.blocks WHERE id=%s", (detached_id,)
                    ).fetchone()[0]
                    == 0
                )
                database.execute(
                    "INSERT INTO inkcre.relations(from_,to_,content) VALUES(%s,%s,'reference')",
                    (root_id, other_id),
                )
                config = {"personal_access_token": "memos_pat_" + "b" * 32}
                database.execute(
                    "UPDATE inkcre.extensions SET config=%s WHERE name='inkcre/memos'",
                    (Jsonb(config),),
                )
                http("GET", "/memos/api/v1/auth/me", expected=401)
                assert (
                    http(
                        "GET",
                        "/memos/api/v1/auth/me",
                        token=config["personal_access_token"],
                    )["user"]["name"]
                    == "users/inkcre"
                )
                database.execute(
                    "UPDATE inkcre.extensions SET config=%s WHERE name='inkcre/memos'",
                    (Jsonb({"personal_access_token": pat}),),
                )
                http("DELETE", f"/memos/api/v1/memos/{root_id}")
                assert (
                    database.execute(
                        "SELECT count(*) FROM inkcre.blocks WHERE id=ANY(%s)",
                        ([root_id, int(child["name"].split("/")[1]), id_],),
                    ).fetchone()[0]
                    == 0
                )
                assert (
                    database.execute(
                        "SELECT count(*) FROM inkcre.blocks WHERE id=%s", (other_id,)
                    ).fetchone()[0]
                    == 1
                )
                assert (
                    database.execute(
                        "SELECT count(*) FROM inkcre.storage_blobs WHERE id=ANY(%s::uuid[])",
                        (list(blobs),),
                    ).fetchone()[0]
                    == 0
                )
                shared = attachment(b"shared content bytes")
                shared_id = int(shared["name"].split("/")[1])
                shared_child = database.execute(
                    "SELECT to_ FROM inkcre.relations WHERE from_=%s AND content='content'",
                    (shared_id,),
                ).fetchone()[0]
                database.execute(
                    "INSERT INTO inkcre.relations(from_,to_,content) VALUES(%s,%s,'content')",
                    (other_id, shared_child),
                )
                http("DELETE", f"/memos/api/v1/attachments/{shared_id}")
                assert (
                    database.execute(
                        "SELECT count(*) FROM inkcre.blocks WHERE id=%s",
                        (shared_child,),
                    ).fetchone()[0]
                    == 1
                )
                assert (
                    database.execute(
                        "SELECT count(*) FROM inkcre.storage_blobs WHERE id=ANY(%s::uuid[])",
                        (list(blobs),),
                    ).fetchone()[0]
                    == 1
                )
                journey.request("POST", "/extensions/inkcre/memos/disable")
                http("GET", "/memos/api/v1/instance/profile", token=None, expected=404)
                assert journey.request("GET", f"/api/v1/blocks/{other_id}/text") == ""
                print(
                    "PASS: Memos PAT/JWT, connection, canonical graph CRUD/comments/keyset/filter/mask, ownership, 32 MiB real bytes, live PAT revocation, owned cleanup, shared content preservation, route removal"
                )
            except Exception:
                shutil.copy2(directory / "app.log", runtime / "last-failure.log")
                raise
            finally:
                try:
                    if journey.process and journey.process.poll() is None:
                        journey.request("POST", "/extensions/inkcre/memos/disable")
                        journey.request("DELETE", "/extensions/inkcre/memos/selection")
                finally:
                    journey.stop()
                    journey.log.close()
                    if admitted:
                        database.execute(
                            "DELETE FROM inkcre.blocks WHERE id=ANY(%s)",
                            (list(blocks),),
                        )
                        database.execute(
                            "DELETE FROM inkcre.storage_blobs WHERE id=ANY(%s::uuid[])",
                            (list(blobs),),
                        )
                        database.execute(
                            "DELETE FROM inkcre.peers WHERE id=%s", (journey.peer,)
                        )
                        if original:
                            database.execute(
                                "UPDATE inkcre.extensions SET nickname=%s,config=%s,state=%s WHERE name='inkcre/memos'",
                                (original[0], Jsonb(original[1]), Jsonb(original[2])),
                            )
                        else:
                            database.execute(
                                "DELETE FROM inkcre.extensions e WHERE name='inkcre/memos' AND NOT EXISTS(SELECT 1 FROM inkcre.extension_peer_participations p WHERE p.name=e.name)"
                            )


if __name__ == "__main__":
    main()
