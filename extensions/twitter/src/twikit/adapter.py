"""Private JSON-line transport for the explicitly selected Twikit 2.3.3 runtime."""

from __future__ import annotations

import asyncio
import builtins
import contextlib
import importlib.metadata
import json
import os
from pathlib import Path
import shutil
import stat
import sys
import tempfile
import threading

VERSION = 1
REQUEST_LIMIT = 64 * 1024
RESPONSE_LIMIT = 16 * 1024 * 1024
COOKIE_LIMIT = 4 * 1024 * 1024


class RuntimeUnavailable(Exception):
    pass


class ChallengeRequired(Exception):
    pass


class ProtocolError(Exception):
    pass


class AuthenticationRequired(Exception):
    pass


class Discard:
    def write(self, value):
        return len(value)

    def flush(self):
        pass


def no_input(_prompt=""):
    raise ChallengeRequired


def runtime_version():
    try:
        if importlib.metadata.version("twikit") != "2.3.3":
            raise RuntimeUnavailable
    except importlib.metadata.PackageNotFoundError:
        raise RuntimeUnavailable from None
    return "2.3.3"


def make_client(config):
    try:
        runtime_version()
        from twikit import Client
    except (ImportError, importlib.metadata.PackageNotFoundError):
        raise RuntimeUnavailable from None
    # No CAPTCHA solver or unlock handler is installed.
    return Client(
        language=config["api_language"] or "en-US",
        proxy=config["proxy"],
        captcha_solver=None,
    )


def private_cookie(path):
    if path.is_symlink():
        raise RuntimeUnavailable
    if path.exists():
        info = path.stat()
        if not stat.S_ISREG(info.st_mode) or info.st_size > COOKIE_LIMIT:
            raise RuntimeUnavailable
        if os.name == "posix" and stat.S_IMODE(info.st_mode) & 0o077:
            raise RuntimeUnavailable
        try:
            with path.open("rb") as source:
                raw = source.read(COOKIE_LIMIT + 1)
            if len(raw) > COOKIE_LIMIT:
                raise RuntimeUnavailable
            cookies = json.loads(raw)
        except (ValueError, OSError):
            raise RuntimeUnavailable from None
        if not isinstance(cookies, dict) or any(
            not isinstance(key, str) or not isinstance(value, str)
            for key, value in cookies.items()
        ):
            raise RuntimeUnavailable


def snapshots(tweets):
    records = []
    seen = set()

    def collect(tweet, depth=0):
        if depth > 128 or len(seen) >= 4096:
            raise ProtocolError
        if tweet.id in seen:
            return
        seen.add(tweet.id)
        record = {
            "id": tweet.id,
            "user": {
                "id": tweet.user.id,
                "screen_name": tweet.user.screen_name,
                "name": tweet.user.name,
            },
            "created_at": tweet.created_at_datetime.isoformat(),
        }
        records.append(record)
        if tweet.retweeted_tweet is not None:
            record["retweeted_id"] = tweet.retweeted_tweet.id
            collect(tweet.retweeted_tweet, depth + 1)
            return
        record["full_text"] = tweet.full_text
        if tweet.in_reply_to:
            record["in_reply_to"] = tweet.in_reply_to
        record["legacy"] = {
            key: tweet._legacy[key]
            for key in (
                "conversation_id_str",
                "reply_count",
                "retweet_count",
                "quote_count",
                "favorite_count",
                "bookmark_count",
            )
            if tweet._legacy.get(key) is not None
        }
        if tweet.quote is not None:
            record["quote_id"] = tweet.quote.id
            collect(tweet.quote, depth + 1)
        if tweet.lang:
            record["lang"] = tweet.lang
        record["media"] = [
            {
                "id": item.id,
                "type": item.type,
                "media_url": item.media_url,
                "data": {
                    key: item._data[key]
                    for key in ("original_info", "ext_alt_text", "video_info")
                    if item._data.get(key) is not None
                },
            }
            for item in tweet.media
        ]

    for tweet in tweets:
        collect(tweet)
    return records


class Session:
    def __init__(self):
        self.client = None
        self.config = None
        self.cookies = None
        self.account = None
        self.authenticating = False

    async def initialize(self, params):
        if self.client is not None or set(params) != {"config", "cookies_file"}:
            raise ProtocolError
        self.config = params["config"]
        self.cookies = Path(params["cookies_file"])
        if not self.cookies.is_absolute():
            raise RuntimeUnavailable
        private_cookie(self.cookies)
        self.client = make_client(self.config)
        return {"ready": True, "twikit_version": "2.3.3"}

    def persist(self):
        private_cookie(self.cookies)
        self.cookies.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
        if os.name == "posix" and self.cookies.parent.stat().st_mode & 0o022:
            raise RuntimeUnavailable
        descriptor, name = tempfile.mkstemp(
            prefix=".twikit-", suffix=".json", dir=self.cookies.parent
        )
        os.close(descriptor)
        try:
            self.client.save_cookies(name)
            os.chmod(name, 0o600)
            if Path(name).stat().st_size > COOKIE_LIMIT:
                raise RuntimeUnavailable
            os.replace(name, self.cookies)
        finally:
            Path(name).unlink(missing_ok=True)

    async def login(self):
        private_cookie(self.cookies)
        self.cookies.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
        if os.name == "posix" and self.cookies.parent.stat().st_mode & 0o022:
            raise RuntimeUnavailable
        descriptor, name = tempfile.mkstemp(
            prefix=".twikit-login-", suffix=".json", dir=self.cookies.parent
        )
        os.close(descriptor)
        if self.cookies.exists():
            shutil.copyfile(self.cookies, name)
            os.chmod(name, 0o600)
        else:
            Path(name).unlink()
        self.authenticating = True
        try:
            await self.client.login(
                auth_info_1=self.config["email"],
                auth_info_2=self.config["username"],
                password=self.config["password"],
                totp_secret=self.config["totp_secret"],
                cookies_file=name,
                enable_ui_metrics=False,
            )
            account = await self.client.user()
            if (
                self.config["username"]
                and account.screen_name.casefold()
                != self.config["username"].removeprefix("@").casefold()
            ):
                raise AuthenticationRequired
            self.account = {"user_id": account.id, "user_handle": account.screen_name}
            self.persist()
        finally:
            self.authenticating = False
            Path(name).unlink(missing_ok=True)

    async def bookmarks(self, params):
        if self.client is None or set(params) != {"count", "cursor"}:
            raise ProtocolError
        if type(params["count"]) is not int or not 1 <= params["count"] <= 100:
            raise ProtocolError
        if params["cursor"] is not None and not isinstance(params["cursor"], str):
            raise ProtocolError
        if self.account is None:
            await self.login()
        result = await self.client.get_bookmarks(
            count=params["count"], cursor=params["cursor"]
        )
        primary = [tweet.id for tweet in result]
        response = {
            **self.account,
            "primary": primary,
            "next": result.next_cursor,
            "tweets": snapshots(result),
        }
        self.persist()
        return response

    async def close(self):
        client = self.client
        if client is None:
            return {"closed": True}
        try:
            if self.account is not None:
                self.persist()
        finally:
            self.client = None
            await client.http.aclose()
        return {"closed": True}


def error_code(error):
    if isinstance(error, (RuntimeUnavailable, OSError)):
        return "runtime_unavailable"
    if isinstance(error, ChallengeRequired):
        return "challenge_required"
    if isinstance(error, AuthenticationRequired):
        return "authentication_required"
    if isinstance(error, ProtocolError):
        return "protocol_error"
    try:
        from twikit.errors import (
            AccountLocked,
            AccountSuspended,
            Forbidden,
            TooManyRequests,
            Unauthorized,
        )

        if isinstance(error, (AccountLocked, AccountSuspended)):
            return "challenge_required"
        if isinstance(error, (Unauthorized, Forbidden)):
            return "authentication_required"
        if isinstance(error, TooManyRequests):
            return "rate_limited"
    except ImportError:
        pass
    return "provider_error"


async def serve():
    os.umask(0o077)
    output = sys.stdout.buffer
    session = Session()
    loop = asyncio.get_running_loop()
    frames = asyncio.Queue(maxsize=1)
    eof = asyncio.Event()

    def parent_closed():
        try:
            loop.call_soon_threadsafe(eof.set)
        except RuntimeError:
            return
        deadline = threading.Timer(2, os._exit, args=(0,))
        deadline.daemon = True
        deadline.start()

    def pump():
        pending = bytearray()
        try:
            while True:
                chunk = os.read(sys.stdin.fileno(), 4096)
                if not chunk:
                    parent_closed()
                    return
                for part in chunk.splitlines(keepends=True):
                    pending.extend(part)
                    if len(pending) > REQUEST_LIMIT:
                        parent_closed()
                        return
                    if pending.endswith(b"\n"):
                        asyncio.run_coroutine_threadsafe(
                            frames.put(bytes(pending)), loop
                        ).result(timeout=2)
                        pending.clear()
        except Exception:
            parent_closed()

    threading.Thread(target=pump, daemon=True).start()
    old_input = builtins.input
    builtins.input = no_input
    try:
        while True:
            next_frame = asyncio.create_task(frames.get())
            parent_exit = asyncio.create_task(eof.wait())
            done, _ = await asyncio.wait(
                (next_frame, parent_exit), return_when=asyncio.FIRST_COMPLETED
            )
            if parent_exit in done:
                next_frame.cancel()
                await asyncio.gather(next_frame, return_exceptions=True)
                break
            parent_exit.cancel()
            await asyncio.gather(parent_exit, return_exceptions=True)
            raw = next_frame.result()
            request = None
            try:
                if len(raw) > REQUEST_LIMIT or not raw.endswith(b"\n"):
                    raise ProtocolError
                request = json.loads(raw)
                if (
                    not isinstance(request, dict)
                    or set(request) != {"version", "id", "method", "params"}
                    or request["version"] != VERSION
                    or type(request["id"]) is not int
                ):
                    raise ProtocolError

                async def dispatch():
                    if request["method"] == "hello":
                        return {"ready": True, "twikit_version": runtime_version()}
                    if request["method"] == "initialize":
                        return await session.initialize(request["params"])
                    if request["method"] == "bookmarks":
                        return await session.bookmarks(request["params"])
                    if request["method"] == "close":
                        return await session.close()
                    raise ProtocolError

                with (
                    contextlib.redirect_stdout(Discard()),
                    contextlib.redirect_stderr(Discard()),
                ):
                    operation = asyncio.create_task(dispatch())
                    parent_exit = asyncio.create_task(eof.wait())
                    done, _ = await asyncio.wait(
                        (operation, parent_exit), return_when=asyncio.FIRST_COMPLETED
                    )
                    if parent_exit in done:
                        operation.cancel()
                        await asyncio.gather(operation, return_exceptions=True)
                        break
                    parent_exit.cancel()
                    await asyncio.gather(parent_exit, return_exceptions=True)
                    result = operation.result()
                response = {"version": VERSION, "id": request["id"], "result": result}
                encoded = json.dumps(
                    response, ensure_ascii=False, separators=(",", ":")
                ).encode()
                if len(encoded) + 1 > RESPONSE_LIMIT:
                    raise ProtocolError
            except Exception as error:
                response = {
                    "version": VERSION,
                    "id": request.get("id") if isinstance(request, dict) else None,
                    "error": {"code": error_code(error)},
                }
                encoded = json.dumps(response, separators=(",", ":")).encode()
            output.write(encoded + b"\n")
            output.flush()
            if request and request.get("method") == "close":
                break
    finally:
        builtins.input = old_input
        with (
            contextlib.redirect_stdout(Discard()),
            contextlib.redirect_stderr(Discard()),
        ):
            await session.close()


if __name__ == "__main__":
    asyncio.run(serve())
