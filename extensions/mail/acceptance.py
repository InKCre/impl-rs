#!/usr/bin/env python3
"""显式执行 Mail loopback IMAP/TLS/STARTTLS → native PostgreSQL Worker 验收。"""

import argparse
import base64
import datetime
import ssl
import shlex
import hashlib
import json
import re
import shutil
import socketserver
import subprocess
import sys
import tempfile
import threading
import time
import uuid
from pathlib import Path
from email.parser import BytesParser
from email.policy import default as MIME_POLICY
from urllib.request import Request, urlopen

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))
from acceptance import Journey

marker = uuid.uuid4().hex
state = {
  "phase": 0,
  "epoch": 123,
  "seen": set(),
  "fetches": [],
  "commands": [],
  "mailbox": "INBOX",
}
plain = b"Newsletter plain original"
html = (
  b'<html><body><p style="display:none">secret preview</p><h1>Mail article</h1>'
  b'<a href="https://example.org/a?utm_source=mail&amp;keep=yes">Article</a>'
  b'<p>Unsubscribe</p><img src="https://tracker.invalid/p" width="1">'
  b'<img src="cid:logo"></body></html>'
)
parts = {
  "1.1": plain,
  "1.2": html,
  "2": base64.b64encode(b"\x00mail-attachment\xff"),
  "3": base64.b64encode(b"\x89PNG\r\n\x1a\ninline"),
}
structure = (
  '((("TEXT" "PLAIN" ("CHARSET" "UTF-8") NIL NIL "7BIT" '
  f"{len(plain)} 1 NIL NIL NIL NIL)"
  '("TEXT" "HTML" ("CHARSET" "UTF-8") NIL NIL "7BIT" '
  f'{len(html)} 1 NIL NIL NIL NIL) "ALTERNATIVE" NIL NIL NIL NIL)'
  '("APPLICATION" "OCTET-STREAM" ("NAME" "demo.bin") NIL NIL "BASE64" '
  f'{len(parts["2"])} NIL ("ATTACHMENT" ("FILENAME" "demo.bin")) NIL NIL)'
  '("IMAGE" "PNG" NIL "<logo>" NIL "BASE64" '
  f'{len(parts["3"])} NIL ("INLINE" NIL) NIL NIL) "MIXED" NIL NIL NIL NIL)'
)


class Handler(socketserver.StreamRequestHandler):
  def line(self, value):
    self.wfile.write(value.encode() + b"\r\n")
    self.wfile.flush()

  def literal(self, prefix, data):
    self.wfile.write(
      prefix.encode() + b" {" + str(len(data)).encode() + b"}\r\n" + data + b")\r\n"
    )
    self.wfile.flush()

  def handle(self):
    self.line("* OK loopback IMAP")
    while raw := self.rfile.readline():
      line = raw.decode().strip()
      tag, command = line.split(" ", 1)
      upper = command.upper()
      state["commands"].append(upper.split(" ")[0])
      if upper.startswith("LOGIN "):
        self.line(tag + " OK logged in")
      elif upper == "STARTTLS":
        self.line(tag + " OK upgrade")
        self.connection = self.server.context.wrap_socket(self.connection, server_side=True)
        self.rfile = self.connection.makefile("rb")
        self.wfile = self.connection.makefile("wb")
      elif upper == "CAPABILITY":
        self.line("* CAPABILITY IMAP4rev1 ENABLE QRESYNC CONDSTORE OBJECTID")
        self.line(tag + " OK capabilities")
      elif upper == "ENABLE QRESYNC":
        self.line("* ENABLED QRESYNC")
        self.line(tag + " OK enabled")
      elif upper.startswith("LIST "):
        self.line(f'* LIST (\\HasNoChildren) "/" "{state["mailbox"]}"')
        self.line('* LIST (\\Junk) "/" "Junk"')
        self.line('* LIST (\\Noselect) "/" "Unselectable"')
        self.line(tag + " OK listed")
      elif upper.startswith(("SELECT ", "EXAMINE ")):
        if shlex.split(command)[1] != state["mailbox"]:
          self.line(tag + " NO mailbox no longer exists")
          continue
        self.line("* FLAGS (\\Seen \\Flagged)")
        self.line("* 2 EXISTS")
        self.line(f"* OK [UIDVALIDITY {state['epoch']}] stable")
        next_uid = 9 if state["phase"] == 1 else 8 if state["phase"] == 0 else 2
        self.line(f"* OK [UIDNEXT {next_uid}] next")
        self.line(f"* OK [HIGHESTMODSEQ {10 + state['phase']}] changed")
        if "QRESYNC" in upper and state["phase"] == 2:
          self.line("* VANISHED (EARLIER) 2")
        self.line(tag + " OK [READ-WRITE] selected")
      elif upper.startswith("STATUS "):
        if "MAILBOXID" in upper:
          self.line('* STATUS "INBOX" (MAILBOXID (mailbox-stable))')
        else:
          self.line(f'* STATUS "INBOX" (HIGHESTMODSEQ {10 + state["phase"]})')
        self.line(tag + " OK status")
      elif upper.startswith("UID SEARCH "):
        if "BEFORE" in upper:
          uids = [1]
        elif upper.startswith("UID SEARCH UID "):
          uids = [8] if state["phase"] == 1 else []
        else:
          uids = (
            [1, 2] if state["phase"] == 0 else [1] if state["phase"] >= 2 else [1, 2, 8]
          )
        self.line("* SEARCH " + " ".join(map(str, uids)))
        self.line(tag + " OK searched")
      elif upper.startswith("UID FETCH "):
        uid = int(command.split()[2].split(":")[0])
        query = command.split(" ", 3)[3]
        if "CHANGEDSINCE" in upper:
          if state["phase"] == 1:
            self.line("* 1 FETCH (UID 2 FLAGS (\\Flagged) MODSEQ (11))")
            self.line("* 2 FETCH (UID 8 FLAGS () MODSEQ (11))")
        elif "EMAILID" in upper:
          self.line(
            f"* 1 FETCH (UID {uid} EMAILID (email-{marker}-{state['epoch']}-{uid}))"
          )
        elif "BODYSTRUCTURE" in upper:
          headers = (
            f"Message-ID: <{marker}-{state['epoch']}-{uid}@example.org>\r\n"
            "Subject: =?utf-8?b?TWFpbCBzdWJqZWN0?=\r\n"
            f"From: Reader <author-{marker}@EXAMPLE.org>\r\n"
            f"To: Subscriber <reader-{marker}@example.org>\r\n"
            f"List-ID: Weekly <{marker}.news.example.org>\r\n"
            "Date: Tue, 02 Jun 2026 10:00:00 +0000\r\n"
            f"References: <parent-{marker}@example.org>\r\n\r\n"
          ).encode()
          old = uid == 1 and state["phase"] < 2
          date = (
            "01-Jan-2020 00:00:00 +0000"
            if old
            else (
              datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(minutes=5)
            ).strftime("%d-%b-%Y %H:%M:%S +0000")
          )
          flags = "\\Seen" if (state["epoch"], uid) in state["seen"] else ""
          self.literal(
            f'* 1 FETCH (UID {uid} FLAGS ({flags}) INTERNALDATE "{date}" '
            f"BODYSTRUCTURE {structure} BODY[HEADER]",
            headers,
          )
        else:
          part = re.search(r"BODY.PEEK\[([0-9.]+)\]", query, re.I).group(1)
          state["fetches"].append((state["epoch"], uid, part))
          self.literal(f"* 1 FETCH (UID {uid} BODY[{part}]", parts[part])
        self.line(tag + " OK fetched")
      elif upper.startswith("UID STORE "):
        uid = int(command.split()[2])
        state["seen"].add((state["epoch"], uid))
        self.line(tag + " OK stored")
      elif upper == "LOGOUT":
        self.line("* BYE bye")
        self.line(tag + " OK logout")
        break
      else:
        self.line(tag + " BAD unsupported")


class Server(socketserver.ThreadingTCPServer):
  allow_reuse_address = True
  daemon_threads = True

  def handle_error(self, request, client_address):
    pass


class TlsServer(Server):
  def get_request(self):
    socket, address = super().get_request()
    return self.context.wrap_socket(socket, server_side=True), address


def main():
  parser = argparse.ArgumentParser(description=__doc__)
  parser.add_argument(
    "--config-json", type=Path, help="DATABASE_URL/JWT_SECRET 私密JSON，或从进程环境读取"
  )
  parser.add_argument("--app", type=Path, default=ROOT / "target/debug/inkcre")
  parser.add_argument(
    "--extension", type=Path, default=ROOT / "target/debug/inkcre-mail-extension"
  )
  args = parser.parse_args()
  config = args.config_json
  app = args.app.resolve(strict=True)
  extension = args.extension.resolve(strict=True)
  (ROOT / ".runtime").mkdir(exist_ok=True)
  server = Server(("127.0.0.1", 0), Handler)
  threading.Thread(target=server.serve_forever, daemon=True).start()
  try:
    with tempfile.TemporaryDirectory(dir=ROOT / ".runtime") as name:
      directory = Path(name)
      journey = Journey(argparse.Namespace(config_json=config, binary=app), directory)
      source = None
      sources = []
      existing = set()
      extra_servers = []
      try:
        for command in [
          [
            "openssl",
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-keyout",
            str(directory / "ca.key"),
            "-out",
            str(directory / "ca.pem"),
            "-days",
            "1",
            "-subj",
            "/CN=Mail acceptance CA",
          ],
          [
            "openssl",
            "req",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-keyout",
            str(directory / "server.key"),
            "-out",
            str(directory / "server.csr"),
            "-subj",
            "/CN=localhost",
          ],
        ]:
          subprocess.run(command, check=True, capture_output=True)  # noqa: S603 — fixed openssl argv
        (directory / "server.ext").write_text(
          "basicConstraints=CA:FALSE\nsubjectAltName=DNS:localhost\nextendedKeyUsage=serverAuth\nkeyUsage=digitalSignature,keyEncipherment\n"
        )
        subprocess.run(  # noqa: S603 — fixed openssl argv
          [
            "openssl",
            "x509",
            "-req",
            "-in",
            str(directory / "server.csr"),
            "-CA",
            str(directory / "ca.pem"),
            "-CAkey",
            str(directory / "ca.key"),
            "-CAcreateserial",
            "-out",
            str(directory / "server.pem"),
            "-days",
            "1",
            "-extfile",
            str(directory / "server.ext"),
          ],
          check=True,
          capture_output=True,
        )
        context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        context.load_cert_chain(directory / "server.pem", directory / "server.key")
        for cls in [TlsServer, Server]:
          service = cls(("127.0.0.1", 0), Handler)
          service.context = context
          threading.Thread(target=service.serve_forever, daemon=True).start()
          extra_servers.append(service)
        artifact = directory / "inkcre-mail-extension"
        shutil.copy2(extension, artifact)
        wrapper = directory / "run-mail"
        wrapper.write_text(
          "#!/bin/sh\nexport SSL_CERT_FILE="
          + shlex.quote(str(directory / "ca.pem"))
          + "\nexec "
          + shlex.quote(str(artifact))
          + ' "$@"\n'
        )
        wrapper.chmod(0o700)
        manifest = directory / "extension.json"
        manifest.write_text(
          json.dumps(
            {
              "schema_version": 1,
              "name": "inkcre/mail",
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
        journey.env["INKCRE_EXTENSION_MANIFESTS"] = str(manifest)
        journey.env["INKCRE_NO_WORKER"] = "false"
        journey.start()
        cursor = None
        while True:
          route = "/api/v1/blocks?limit=1000" + (f"&cursor={cursor}" if cursor else "")
          page = journey.request("GET", route)
          existing.update(row["id"] for row in page["items"])
          cursor = page["next_cursor"]
          if cursor is None:
            break
        journey.request(
          "POST",
          "/extensions/inkcre/mail/selection",
          {"distribution": "rust_stdio", "version": "0.1.0"},
        )
        journey.request("POST", "/extensions/inkcre/mail/enable")
        source = journey.request(
          "POST",
          "/sources",
          {
            "type": "extensions.mail.source.Source",
            "config": {
              "parameters": {
                "host": "127.0.0.1",
                "port": server.server_address[1],
                "security": "plain",
                "username": "fixture",
                "password": "fixture-only",
              },
              "synchronize_deletions": True,
            },
          },
          expected=201,
        )
        sources.append(source["id"])

        def block(identity):
          return journey.request("GET", f"/blocks/{identity}")

        def edges(identity):
          return journey.request("GET", f"/relations/by_block/{identity}")

        def outgoing(identity):
          return [e for e in edges(identity) if e["from_block_id"] == identity]

        def job(command="collect", body=None):
          result = journey.request(
            "POST",
            f"/sources/{source['id']}/{command}",
            {} if body is None else body,
            expected=201,
          )
          journey.jobs.append(result["id"])
          deadline = time.monotonic() + 40
          while time.monotonic() < deadline:
            result = journey.request("GET", f"/jobs/{result['id']}")
            if result["status"] in ["finished", "failed", "aborted"]:
              assert result["status"] == "finished", ("job", result["status"])
              assert not result["state"]["diagnostics"], (
                "diagnostics",
                result["state"]["diagnostics"],
              )
              return result
            time.sleep(0.1)
          raise RuntimeError("Mail worker timeout")

        first = job()
        assert first["state"]["counts"]["messages"] == 1
        source = journey.request("GET", f"/sources/{source['id']}")
        anchor = source["block"]
        mailbox = next(
          e["to_block_id"] for e in outgoing(anchor) if e["content"] == "manages"
        )
        assert len(source["state"]["checkpoints"]) == 1
        assert source["state"]["checkpoints"][str(mailbox)]["last_uid"] == 7
        email = next(
          e["to_block_id"]
          for e in outgoing(mailbox)
          if e["content"].startswith("{")
          if json.loads(e["content"]).get("type") == "contains"
        )
        email_edges = outgoing(email)
        mime = next(
          e["to_block_id"]
          for e in email_edges
          if e["content"].startswith("{")
          if json.loads(e["content"]).get("part_id") == "2"
        )
        assert not any(part in ["2", "3"] for _, _, part in state["fetches"])
        assert (123, 2) in state["seen"]
        assert (123, 1) not in state["seen"]

        multipart_paths = []

        def invoke(identity, method, body):
          path = f"/blocks/{identity}/resolver/methods/{method}"
          request = Request(
            journey.base + path,
            data=json.dumps(body).encode(),
            method="POST",
            headers={
              "Authorization": "Bearer " + journey.token,
              "Content-Type": "application/json",
            },
          )
          with urlopen(request, timeout=30) as response:
            content = response.read()
            mime = response.headers.get("Content-Type", "")
          if mime.startswith("application/octet-stream"):
            return content
          if not mime.startswith("multipart/related"):
            return json.loads(content)
          multipart_paths.append(path)
          message = BytesParser(policy=MIME_POLICY).parsebytes(
            f"Content-Type: {mime}\r\nMIME-Version: 1.0\r\n\r\n".encode() + content
          )
          parts = list(message.iter_parts())
          root = json.loads(parts[0].get_payload(decode=True))
          blobs = {
            str(part["Content-ID"]).strip("<>"): part.get_payload(decode=True)
            for part in parts[1:]
          }
          value = root["value"]
          for pointer, reference in root["parts"].items():
            binary = blobs[reference.removeprefix("cid:")]
            if not pointer:
              value = binary
              continue
            keys = [
              key.replace("~1", "/").replace("~0", "~") for key in pointer[1:].split("/")
            ]
            parent = value
            for key in keys[:-1]:
              parent = parent[int(key)] if isinstance(parent, list) else parent[key]
            parent[int(keys[-1]) if isinstance(parent, list) else keys[-1]] = binary
          return value

        solved = invoke(email, "get_solved_content", {"materialize_missing": False})
        assert solved["root"]["subject"] == "Mail subject"
        flag = solved["flags"][0]["flag"]["block"]["id"]
        assert invoke(flag, "get_text", {}) == "\\Seen: The message has been read."
        html_body = next(
          body
          for body in solved["bodies"]
          if body["block"]["resolver"] == "extensions.mail.newsletter_html.v1"
        )
        assert invoke(html_body["block"]["id"], "get_label", {}) == "html <Mail article>"
        assert (
          len(solved["bodies"]) == 2
          and len(solved["mime_parts"]) == 2
          and solved["mailing_lists"]
        )
        assert (
          solved["participants"][0]["address"]["solved_content"]["address"]
          == f"author-{marker}@example.org"
        )
        text = invoke(email, "get_text", {"materialize_missing": False})
        assert (
          "Mail article" in text
          and "secret preview" not in text
          and "Unsubscribe" not in text
          and "utm_source" not in text
          and "Newsletter plain original" not in text
        )
        lexical = invoke(
          email, "get_text", {"context": "lexical", "materialize_missing": False}
        )
        assert lexical == "Mail subject"
        assert not any(part in ["2", "3"] for _, _, part in state["fetches"])
        child = journey.request("POST", "/mail/mime-parts/materialize", {"block": mime})
        assert (
          journey.request("POST", "/mail/mime-parts/materialize", {"block": mime})["id"]
          == child["id"]
        )
        assert sum(part == "2" for _, _, part in state["fetches"]) == 1
        assert invoke(child["id"], "get_raw_content", {}) == b"\x00mail-attachment\xff"
        part_solved = invoke(mime, "get_solved_content", {"materialize_missing": False})
        assert (
          part_solved["content"]["solved_content"]["content"] == b"\x00mail-attachment\xff"
        )
        email_solved = invoke(email, "get_solved_content", {"materialize_missing": False})
        nested = next(
          part for part in email_solved["mime_parts"] if part["block"]["id"] == mime
        )
        assert (
          nested["solved_content"]["content"]["solved_content"]["content"]
          == b"\x00mail-attachment\xff"
        )
        assert len(multipart_paths) >= 2
        # Canonical reply/reference projections recurse; a graph cycle stays finite.
        parent_id = solved["references"][0]["block"]["id"]
        parent_root = solved["references"][0]["solved_content"]["root"]
        parent_root["subject"] = "Thread parent"
        journey.request(
          "PATCH", f"/blocks/{parent_id}", {"content": json.dumps(parent_root)}
        )
        cycle = journey.request(
          "POST",
          "/relations",
          {"from_block_id": parent_id, "to_block_id": email, "content": "parent:0"},
          expected=200,
        )
        journey.relations.append(cycle["id"])
        projection = invoke(email, "get_solved_content", {"materialize_missing": False})
        parent = projection["references"][0]["solved_content"]
        assert parent["root"]["subject"] == "Thread parent"
        assert (
          parent["parents"][0]["solved_content"]["message_id"]
          == solved["root"]["message_id"]
        )
        before = source["state"]["checkpoints"]
        job("backfill", {"since": "2019-01-01", "before": "2021-01-01"})
        source = journey.request("GET", f"/sources/{source['id']}")
        assert source["state"]["checkpoints"] == before and (123, 1) not in state["seen"]
        state["phase"] = 1
        state["mailbox"] = "Renamed Inbox"
        second = job()
        assert json.loads(block(mailbox)["content"])["name"] == "Renamed Inbox"
        assert (
          second["state"]["counts"]["messages"] == 1
          and second["state"]["counts"]["flag_changes"] == 1
        )
        source = journey.request("GET", f"/sources/{source['id']}")
        assert source["state"]["checkpoints"][str(mailbox)]["last_uid"] == 8
        state["phase"] = 2
        third = job()
        assert third["state"]["counts"]["removals"] == 1
        # Reset epoch invalidates locators and preserves canonical emails.
        latest_email = next(
          e["to_block_id"]
          for e in outgoing(mailbox)
          if e["content"].startswith("{") and json.loads(e["content"]).get("uid") == 8
        )
        stale_part = next(
          e["to_block_id"]
          for e in outgoing(latest_email)
          if e["content"].startswith("{") and json.loads(e["content"]).get("part_id") == "3"
        )
        state["phase"] = 3
        state["epoch"] = 456
        journey.request(
          "POST", "/mail/mime-parts/materialize", {"block": stale_part}, expected=409
        )
        assert not any(
          epoch == 456 and uid == 8 and part == "3" for epoch, uid, part in state["fetches"]
        )
        job()
        source = journey.request("GET", f"/sources/{source['id']}")
        assert source["state"]["checkpoints"][str(mailbox)]["uid_validity"] == 456
        contains = [
          json.loads(e["content"])
          for e in outgoing(mailbox)
          if e["content"].startswith("{")
          and json.loads(e["content"]).get("type") == "contains"
        ]
        assert contains and all(e["uid_validity"] == 456 for e in contains)
        assert block(email)["id"] == email
        # Rotate the secret without changing the accepted access binding.
        setup = source["config"]
        setup["parameters"]["password"] = "fixture-rotated"  # noqa: S105 — loopback fixture
        journey.request("PATCH", f"/sources/{source['id']}", {"config": setup})
        job()
        # Access-context changes require another Source.
        setup = source["config"]
        setup["parameters"]["username"] = "other-context"
        journey.request("PATCH", f"/sources/{source['id']}", {"config": setup})
        bad = journey.request("POST", f"/sources/{source['id']}/collect", {}, expected=201)
        journey.jobs.append(bad["id"])
        time.sleep(0.3)
        for _ in range(100):
          result = journey.request("GET", f"/jobs/{bad['id']}")
          if result["status"] in ["failed", "finished"]:
            break
          time.sleep(0.1)
        assert result["status"] == "failed"
        for security, service in zip(["tls", "starttls"], extra_servers):
          source = journey.request(
            "POST",
            "/sources",
            {
              "type": "extensions.mail.source.Source",
              "config": {
                "parameters": {
                  "host": "localhost",
                  "port": service.server_address[1],
                  "security": security,
                  "username": "fixture",
                  "password": "fixture-only",
                },
                "ordinary_mark_as_seen": False,
              },
            },
            expected=201,
          )
          sources.append(source["id"])
          result = job()
          assert result["state"]["counts"]["messages"] == 1
        print(
          "Mail loopback IMAP → native PG Worker passed: schemas, source horizon, "
          "UID frontier, backfill, flags/QRESYNC deletion, epoch reset, graph identities, "
          "newsletter, lazy MIME bytes/multipart, cyclic reply projections, "
          "TLS/STARTTLS with verified fixture CA "
          "and binding change rejection"
        )
      finally:
        if sys.exc_info()[0]:
          journey.log.flush()
          private_log = ROOT / ".runtime/mail-last-failure.private.log"
          shutil.copy2(directory / "app.log", private_log)
          private_log.chmod(0o600)
        if source and journey.process and journey.process.poll() is None:
          source = journey.request("GET", f"/sources/{source['id']}")
          # Only traverse newly created nodes connected to this fixture's Sources.
          pending = []
          for identity in sources:
            live = journey.request("GET", f"/sources/{identity}")
            pending.extend([live["block"]] if live.get("block") else [])
          visited = set()
          while pending:
            identity = pending.pop()
            if identity in visited:
              continue
            visited.add(identity)
            for edge in journey.request("GET", f"/relations/by_block/{identity}"):
              target = (
                edge["to_block_id"]
                if edge["from_block_id"] == identity
                else edge["from_block_id"]
              )
              if target is not None and target not in visited and target not in existing:
                pending.append(target)
          for identity in visited:
            record = journey.request("GET", f"/blocks/{identity}")
            if record["storage"] == -4:
              journey.blobs.append(json.loads(record["content"])["blob_id"])
          for identity in sources:
            journey.request("DELETE", f"/sources/{identity}", expected=204)
          journey.blocks.extend(visited)
          journey.request("POST", "/extensions/inkcre/mail/disable")
          journey.request("DELETE", "/extensions/inkcre/mail/selection", expected=200)
        journey.cleanup()
        for service in extra_servers:
          service.shutdown()
          service.server_close()
  except Exception as error:
    import traceback

    print(
      type(error).__name__,
      str(error)[:250],
      [
        (Path(f.filename).name, f.lineno) for f in traceback.extract_tb(error.__traceback__)
      ],
      file=sys.stderr,
    )
    sys.exit("Mail acceptance failed; private diagnostics suppressed")
  finally:
    server.shutdown()
    server.server_close()


if __name__ == "__main__":
  main()
