#!/usr/bin/env python3
"""Explicit HTTP/native PostgreSQL acceptance; requires a disposable runtime and built binaries."""

import argparse, json, os, pathlib, shlex, sys, tempfile, time, uuid
from urllib.request import Request, build_opener, HTTPRedirectHandler
from urllib.error import HTTPError
from http.cookies import SimpleCookie
import psycopg
from psycopg.types.json import Jsonb

root = pathlib.Path(__file__).resolve().parents[3]
sys.path.insert(0, str(root / "scripts"))
from acceptance import Journey

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument(
    "--config-json",
    type=pathlib.Path,
    required=True,
    help="Private DATABASE_URL/JWT_SECRET JSON; never printed",
)
parser.add_argument(
    "--app-binary", type=pathlib.Path, default=root / "target/debug/inkcre"
)
parser.add_argument(
    "--extension-binary",
    type=pathlib.Path,
    default=root / "target/debug/inkcre-twitter-extension",
)
options = parser.parse_args()
(root / ".runtime").mkdir(mode=0o700, exist_ok=True)
config = options.config_json.resolve(strict=True)
dsn = json.loads(config.read_text())["DATABASE_URL"]
prefix = str(uuid.uuid4().int % 10**15 + 10**15)
post = prefix + "01"
author = prefix + "90"
media = prefix + "80"
missing = prefix + "79"
marker = "twitter-archive-" + uuid.uuid4().hex
recovery = root / ".runtime/twitter-archive-restore.private.json"


class NoRedirect(HTTPRedirectHandler):
    def redirect_request(self, *args):
        return None


opener = build_opener(NoRedirect)
with tempfile.TemporaryDirectory(dir=root / ".runtime") as tmp:
    work = pathlib.Path(tmp)
    j = Journey(
        argparse.Namespace(
            config_json=config, binary=options.app_binary.resolve(strict=True)
        ),
        work,
    )
    j.env["INKCRE_PEER_LEASE_SECONDS"] = "3"
    j.env["TWITTER_X_MONKEY_JWT_LIFETIME_SECONDS"] = "8"
    db = psycopg.connect(dsn, autocommit=True)
    snapshot = db.execute(
        "SELECT config,state FROM inkcre.extensions WHERE name='inkcre/twitter'"
    ).fetchone() or ({}, {})
    fd = os.open(recovery, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, "w") as output:
        json.dump({"config": snapshot[0], "state": snapshot[1], "peer": j.peer}, output)
    binary = options.extension_binary.resolve(strict=True)
    manifest = work / "twitter.json"
    wrapper = work / "twitter-wrapper"
    wrapper.write_text("#!/bin/sh\nexec " + shlex.quote(str(binary)) + "\n")
    wrapper.chmod(0o700)
    import hashlib

    manifest.write_text(
        json.dumps(
            {
                "schema_version": 1,
                "name": "inkcre/twitter",
                "version": "0.1.0",
                "distribution": "rust_stdio",
                "host_sdk": "core-rs",
                "host_sdk_version": "^0.1",
                "protocol_version": 1,
                "executable": wrapper.name,
                "sha256": hashlib.sha256(wrapper.read_bytes()).hexdigest(),
            }
        )
    )
    j.env["INKCRE_EXTENSION_MANIFESTS"] = str(manifest)

    def request(
        method, path, body=None, headers=None, cookie=None, peer=False, expected=200
    ):
        headers = {"Origin": j.base, **(headers or {})}
        if cookie is not None:
            headers["Cookie"] = cookie
        if peer:
            headers["Authorization"] = "Bearer " + j.token
        data = None
        if body is not None:
            headers.setdefault("Content-Type", "application/json")
            data = json.dumps(body).encode()
        try:
            response = opener.open(
                Request(j.base + path, method=method, data=data, headers=headers),
                timeout=40,
            )
        except HTTPError as error:
            response = error
        assert response.status == expected, (
            "HTTP status",
            path,
            response.status,
            expected,
        )
        raw = response.read()
        value = (
            json.loads(raw)
            if "application/json" in response.headers.get("Content-Type", "") and raw
            else raw
        )
        return value, response.headers

    def cookie_from(headers):
        cookies = SimpleCookie()
        for value in headers.get_all("Set-Cookie") or []:
            cookies.load(value)
        return "inkcre_twitter_archive=" + cookies["inkcre_twitter_archive"].value

    try:
        j.start()
        db.execute(
            "UPDATE inkcre.peers SET config=%s WHERE id=%s",
            (Jsonb({"http_public_base_url": j.base + "/prefix"}), j.peer),
        )
        j.request(
            "POST",
            "/extensions/inkcre/twitter/selection",
            {"distribution": "rust_stdio", "version": "0.1.0"},
        )
        j.plugin = "inkcre%2Ftwitter"
        j.request("POST", "/extensions/inkcre/twitter/enable")
        status, _ = request(
            "GET",
            "/twitter/x-monkey/api/v1/session",
            headers={"x-inkcre-twitter-session-expiry": str(int(time.time()) + 10000)},
        )
        assert not status["authenticated"] and status["username"] is None
        _, headers = request("POST", "/twitter/x-monkey/session", {}, expected=401)
        assert not headers.get_all("Set-Cookie")
        _, headers = request(
            "POST",
            "/twitter/x-monkey/session",
            {},
            peer=True,
            headers={"Origin": "https://evil.invalid"},
            expected=403,
        )
        assert not headers.get_all("Set-Cookie")
        request(
            "POST",
            "/twitter/x-monkey/session",
            {},
            peer=True,
            headers={"Content-Type": "text/plain"},
            expected=415,
        )
        connected, headers = request("POST", "/twitter/x-monkey/session", {}, peer=True)
        cookie = cookie_from(headers)
        assert (
            connected["authenticated"]
            and connected["account_url"] == j.base + "/prefix/twitter/x-monkey/account"
        )
        setcookie = headers.get_all("Set-Cookie")[0]
        assert (
            "Secure" in setcookie
            and "HttpOnly" in setcookie
            and "SameSite=None" in setcookie
            and "Path=/prefix/twitter/x-monkey/" in setcookie
        )
        token = cookie.split("=", 1)[1]
        request(
            "GET",
            "/peers/self",
            headers={"Authorization": "Bearer " + token},
            expected=401,
        )
        request(
            "POST",
            "/twitter/x-monkey/session",
            {},
            headers={"Authorization": "Bearer " + token},
            expected=401,
        )
        account, headers = request("GET", "/twitter/x-monkey/account", cookie=cookie)
        assert (
            b"Connected" in account
            and b"crypto.subtle" in account
            and headers.get("Content-Security-Policy").startswith(
                "default-src 'none'; script-src 'nonce-"
            )
        )
        request(
            "GET",
            "/twitter/x-monkey/api/v1/session",
            cookie=cookie,
            headers={"sec-fetch-mode": "no-cors"},
            expected=403,
        )
        body = {
            "users": [{"id": author, "handle": "fixture", "name": "Fixture"}],
            "posts": [
                {
                    "id": post,
                    "authorId": author,
                    "createdAt": "2026-11-21T00:00:00Z",
                    "text": {"text": marker},
                    "mediaIds": [media],
                    "replyTo": {"postId": missing},
                    "counts": {"likes": 5},
                    "countsObservedAt": {"likes": "2026-11-21T00:00:00.111111Z"},
                }
            ],
            "media": [
                {
                    "id": media,
                    "type": "photo",
                    "url": "https://example.invalid/image",
                    "alt": "An image",
                }
            ],
        }
        path = "/twitter/x-monkey/api/v1/archive/submit"
        standard = {"x-tweet-db-client": "x-monkey"}
        request(
            "POST",
            path,
            body,
            headers={
                **standard,
                "x-inkcre-twitter-session-expiry": str(int(time.time()) + 10000),
            },
            expected=401,
        )
        request(
            "POST",
            path,
            body,
            cookie=cookie,
            headers={**standard, "Origin": "https://evil.invalid"},
            expected=403,
        )
        request("POST", path, body, cookie=cookie, expected=400)
        request(
            "POST",
            path,
            body,
            cookie=cookie,
            headers={**standard, "Content-Type": "text/plain"},
            expected=415,
        )
        accepted, _ = request(
            "POST",
            path,
            body,
            cookie=cookie,
            headers={**standard, "Origin": "https://x.com"},
        )
        assert accepted["summary"] == {
            "total": 3,
            "accepted": 3,
            "unchanged": 0,
            "partial": 0,
            "failed": 0,
        }
        unchanged, _ = request(
            "POST",
            path,
            body,
            cookie=cookie,
            headers={**standard, "Origin": "moz-extension://fixture-addon"},
        )
        assert unchanged["summary"]["unchanged"] == 3
        querypath = "/twitter/x-monkey/api/v1/archive/query"
        query = {"posts": [post, missing, post]}
        result, _ = request("POST", querypath, query, cookie=cookie, headers=standard)
        assert [row["status"] for row in result["posts"]] == [
            "found",
            "missing",
            "found",
        ] and "article" not in result["posts"][0]["data"]
        bad = json.loads(json.dumps(body))
        bad["posts"][0]["counts"]["likes"] = 1.0
        request("POST", path, bad, cookie=cookie, headers=standard, expected=422)
        bad = json.loads(json.dumps(body))
        bad["posts"][0]["text"] = None
        request("POST", path, bad, cookie=cookie, headers=standard, expected=422)
        bad = json.loads(json.dumps(body))
        bad["posts"][0]["createdAt"] = "0000-01-01T00:00:00Z"
        request("POST", path, bad, cookie=cookie, headers=standard, expected=422)
        request(
            "POST",
            querypath,
            {"posts": [post] * 1001},
            cookie=cookie,
            headers=standard,
            expected=422,
        )
        metadata = db.execute(
            "SELECT id FROM inkcre.blocks WHERE resolver='extensions.twitter.tweet.v2' AND content::jsonb->>'id'=%s",
            (post,),
        ).fetchone()[0]
        assert j.request("GET", f"/blocks/{metadata}/label") == "tweet <" + marker + ">"
        mediaid = db.execute(
            "SELECT id FROM inkcre.blocks WHERE resolver='extensions.twitter.media.v1' AND content::jsonb->>'id'=%s",
            (media,),
        ).fetchone()[0]
        db.execute(
            "UPDATE inkcre.blocks SET content=%s WHERE id=%s",
            (json.dumps({"id": media, "type": "photo"}), mediaid),
        )
        failed, _ = request(
            "POST", querypath, {"media": [media]}, cookie=cookie, headers=standard
        )
        assert failed["media"][0]["status"] == "failed"
        db.execute(
            "UPDATE inkcre.blocks SET content=%s WHERE id=%s",
            (json.dumps(body["media"][0]), mediaid),
        )
        time.sleep(4.2)
        renewed, headers = request(
            "GET", "/twitter/x-monkey/api/v1/session", cookie=cookie
        )
        assert renewed["authenticated"] and headers.get_all("Set-Cookie")
        renewedcookie = cookie_from(headers)
        assert renewedcookie != cookie
        _, headers = request(
            "GET",
            "/twitter/x-monkey/api/v1/session",
            cookie="inkcre_twitter_archive=invalid",
        )
        assert "Max-Age=0" in headers.get_all("Set-Cookie")[0]
        _, headers = request(
            "POST",
            "/twitter/x-monkey/disconnect",
            cookie=renewedcookie,
            headers={"sec-fetch-mode": "navigate", "sec-fetch-dest": "document"},
            expected=303,
        )
        assert (
            "Max-Age=0" in headers.get_all("Set-Cookie")[0]
            and headers["Location"] == j.base + "/prefix/twitter/x-monkey/account"
        )
        j.request("POST", "/extensions/inkcre/twitter/disable")
        assert j.request("GET", f"/blocks/{metadata}/label") == "tweet <" + marker + ">"
        request("GET", "/twitter/x-monkey/account", expected=404)
        print(
            "Twitter real X Monkey archive: Host-scoped cookie issuance/renewal/deletion, credential replay rejection, spoofed-header denial, Origin/client/content gates, sparse DTO submit/query order, identity placeholders, idempotency, strict counts/null/time/batch rejection, account CSP and passive reads after disable passed"
        )
    finally:
        j.stop()
        db.execute(
            "UPDATE inkcre.extensions SET config=%s,state=%s WHERE name='inkcre/twitter'",
            (Jsonb(snapshot[0]), Jsonb(snapshot[1])),
        )
        recovery.unlink()
        db.execute(
            "DELETE FROM inkcre.blocks WHERE resolver LIKE 'extensions.twitter.%%' AND CASE WHEN pg_input_is_valid(content,'jsonb') THEN content::jsonb->>'id' END=ANY(%s)",
            ([post, author, media, missing],),
        )
        db.execute(
            "DELETE FROM inkcre.extension_peer_participations WHERE peer_id=%s",
            (j.peer,),
        )
        db.execute("DELETE FROM inkcre.peers WHERE id=%s", (j.peer,))
        j.cleanup()
        db.close()
