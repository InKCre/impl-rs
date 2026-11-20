#!/usr/bin/env python3
"""管理本 checkout 专属 PostgreSQL，并委托 impl-py 初始化共享协议。"""

import argparse
import json
import os
from pathlib import Path
import secrets
import shutil
import socket
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
STATE = ROOT / ".runtime/dev-database"


def run(argv, *, env=None, cwd=None, check=True):
    result = subprocess.run(argv, env=env, cwd=cwd, capture_output=True, timeout=300)
    if result.returncode and check:
        with (STATE / "operation.private.log").open("ab") as log:
            log.write(result.stdout + result.stderr)
        raise RuntimeError("命令失败，诊断保存在 .runtime/dev-database/operation.private.log")
    return result


def private_json(path, data):
    path.write_text(json.dumps(data, indent=2) + "\n")
    path.chmod(0o600)


def control(config, command):
    return run([str(Path(config["postgres_bin"]) / "pg_ctl"), "-D", str(STATE / "data"),
                *command], check=False)


def start(config):
    if control(config, ["status"]).returncode:
        result = control(config, ["-l", str(STATE / "postgres.private.log"), "-w", "start"])
        if result.returncode:
            raise RuntimeError("PostgreSQL 启动失败，检查本 checkout 的私密日志")


def with_peer_config(command):
    if not command:
        raise RuntimeError("exec 需要在 -- 后指定命令")
    config = json.loads((STATE / "native-peer.private.json").read_text())
    env = os.environ.copy()
    env.update({key: config[key] for key in ("DATABASE_URL", "JWT_SECRET")})
    os.execvpe(command[0], command, env)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("init", "start", "stop", "status", "destroy", "exec"))
    parser.add_argument("--impl-py", type=Path, help="拥有 migration 的 canonical impl-py checkout")
    parser.add_argument("--python", type=Path, help="已安装 impl-py 依赖的解释器，缺省使用其 pdm run")
    parser.add_argument("--postgres-bin", type=Path, help="带 pgvector 的 PostgreSQL bin 目录")
    arguments = sys.argv[1:]
    application = []
    if "--" in arguments:
        separator = arguments.index("--")
        application = arguments[separator + 1:]
        arguments = arguments[:separator]
    args = parser.parse_args(arguments)
    if application and args.command != "exec":
        parser.error("只有 exec 接受 -- 后的命令参数")
    os.umask(0o077)
    descriptor = STATE / "owner.private.json"
    if args.command == "init":
        if descriptor.exists():
            raise RuntimeError("已有专属数据库，使用 start 恢复或 destroy 后重新初始化")
        if not args.impl_py or not (args.impl_py / "scripts/database.py").is_file():
            raise RuntimeError("init 需要 --impl-py 指向 canonical Python checkout")
        if STATE.exists():
            raise RuntimeError("存在未完成初始化目录，请检查后移除该精确目录再重试")
        postgres_bin = args.postgres_bin
        if postgres_bin is None:
            pg_ctl = shutil.which("pg_ctl")
            if not pg_ctl:
                raise RuntimeError("需要 PostgreSQL 和 pgvector，或指定 --postgres-bin")
            postgres_bin = Path(pg_ctl).resolve().parent
        postgres_bin = postgres_bin.resolve()
        STATE.mkdir(parents=True, mode=0o700)
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        admin = secrets.token_urlsafe(36)
        password_file = STATE / "bootstrap.private"
        password_file.write_text(admin)
        config = {"owner": str(ROOT), "postgres_bin": str(postgres_bin), "port": port,
                  "source_revision": run(["git", "rev-parse", "HEAD"], cwd=args.impl_py).stdout.decode().strip()}
        private_json(descriptor, config)
        try:
            run([str(postgres_bin / "initdb"), "-D", str(STATE / "data"), "-U", "postgres",
                 "--auth-local=trust", "--auth-host=scram-sha-256", "--pwfile", str(password_file),
                 "--encoding=UTF8", "--no-locale"])
            with (STATE / "data/postgresql.conf").open("a") as output:
                output.write(f"\nlisten_addresses='127.0.0.1'\nport={port}\nunix_socket_directories=''\n")
            start(config)
            env = os.environ.copy()
            env.update(PGPASSWORD=admin, INKCRE_ENV_FILE="",
                       MIGRATION_DATABASE_URL=f"postgresql+psycopg://postgres:{admin}@127.0.0.1:{port}/inkcre",
                       CORE_DATABASE_PASSWORD=secrets.token_urlsafe(36),
                       POSTGREST_DATABASE_PASSWORD=secrets.token_urlsafe(36))
            run([str(postgres_bin / "createdb"), "-h", "127.0.0.1", "-p", str(port), "-U", "postgres", "inkcre"], env=env)
            command = [str(args.python.absolute())] if args.python else ["pdm", "run", "python"]
            run([*command, "scripts/database.py", "init", "--profile", "development"], env=env, cwd=args.impl_py)
            private_json(STATE / "native-peer.private.json", {
                "DATABASE_URL": f"postgresql://inkcre_core:{env['CORE_DATABASE_PASSWORD']}@127.0.0.1:{port}/inkcre?sslmode=disable",
                "JWT_SECRET": secrets.token_urlsafe(36),
            })
        except Exception:
            control(config, ["-m", "fast", "-w", "stop"])
            raise
        finally:
            password_file.unlink(missing_ok=True)
        print(json.dumps({"ready": True, "config_file": str(STATE / "native-peer.private.json"),
                          "source_revision": config["source_revision"]}))
        return
    if not descriptor.exists():
        raise RuntimeError("本 checkout 没有专属数据库")
    config = json.loads(descriptor.read_text())
    if config["owner"] != str(ROOT):
        raise RuntimeError("数据库 owner 路径不匹配")
    if args.command == "exec":
        with_peer_config(application)
    elif args.command == "start":
        if not (STATE / "native-peer.private.json").exists():
            raise RuntimeError("初始化尚未完成，先检查诊断并 destroy 后重新 init")
        start(config)
    elif args.command == "status":
        print(json.dumps({"running": control(config, ["status"]).returncode == 0,
                          "initialized": (STATE / "native-peer.private.json").exists()}))
    else:
        if control(config, ["status"]).returncode == 0:
            if control(config, ["-m", "fast", "-w", "stop"]).returncode:
                raise RuntimeError("数据库停止失败，保留目录供诊断")
        if args.command == "destroy":
            shutil.rmtree(STATE)


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        # 不展开驱动或子进程异常，避免输出连接串和口令。
        print(str(error) if isinstance(error, RuntimeError) else "开发数据库操作失败，详情保留在本机私密日志", file=sys.stderr)
        sys.exit(1)
