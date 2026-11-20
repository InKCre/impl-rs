#!/usr/bin/env python3
"""整理已构建的同平台 app/扩展产物，生成可移交的本机安装目录。"""

import argparse
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile


ROOT = Path(__file__).resolve().parents[1]
SUFFIX = ".exe" if os.name == "nt" else ""


def command(*argv):
    return subprocess.check_output(argv, cwd=ROOT, text=True)


def package(build_dir, output):
    workspace = json.loads(command("cargo", "metadata", "--locked", "--no-deps", "--format-version=1"))
    binaries = build_dir or Path(workspace["target_directory"]) / "release"
    app = binaries / ("inkcre" + SUFFIX)
    members = set(workspace["workspace_members"])
    packages = [package for package in workspace["packages"] if package["id"] in members]
    extensions = [
        (package["metadata"]["inkcre"]["extension"], package)
        for package in packages
        if package["metadata"]
        and package["metadata"].get("inkcre", {}).get("extension")
    ]
    output.parent.mkdir(parents=True, exist_ok=True)
    if output.exists():
        raise SystemExit("输出目录已经存在，指定新的目标路径")
    with tempfile.TemporaryDirectory(dir=output.parent) as temp:
        stage = Path(temp) / "runtime"
        stage.mkdir()
        shutil.copy2(app, stage / app.name)
        if any(package["name"] == "inkcre-cli" for package in packages):
            cli = binaries / ("inkcre-cli" + SUFFIX)
            shutil.copy2(cli, stage / cli.name)
        for name, package in sorted(extensions, key=lambda item: item[0]):
            binaries_for_package = [target["name"] for target in package["targets"] if target["kind"] == ["bin"]]
            if len(binaries_for_package) != 1:
                raise SystemExit("扩展 package 必须有一个 binary")
            if not re.fullmatch(r"[a-z0-9][a-z0-9-]*/[a-z0-9][a-z0-9-]*", name):
                raise SystemExit("扩展 metadata 缺少合法 namespace/name")
            source = binaries / (binaries_for_package[0] + SUFFIX)
            directory = stage / "extensions" / name
            directory.mkdir(parents=True)
            staged_binary = directory / source.name
            shutil.copy2(source, staged_binary)
            manifest = command(app, "extension-manifest", "--name", name,
                               "--version", package["version"], staged_binary)
            (directory / "manifest.json").write_text(manifest, encoding="utf-8")
        (stage / "source.json").write_text(json.dumps({
            "workspace_checkout": command("git", "rev-parse", "HEAD").strip(),
            "extension_sdk_checkout": command("git", "-C", "vendor/ext-reg", "rev-parse", "HEAD").strip(),
            "version": command(app, "--version").strip(),
        }, indent=2) + "\n", encoding="utf-8")
        stage.rename(output)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path)
    parser.add_argument("--build-dir", type=Path, help="已有同平台 binary 的目录，默认 Cargo target/release")
    args = parser.parse_args()
    package(args.build_dir.resolve() if args.build_dir else None, args.output.resolve())


if __name__ == "__main__":
    main()
