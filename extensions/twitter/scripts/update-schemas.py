#!/usr/bin/env python3
"""Regenerate field schemas from a supplied canonical impl-py checkout; no app imports."""

import argparse
import importlib.util
import json
from pathlib import Path
import sys

import pydantic

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument(
    "reference", type=Path, help="Canonical impl-py checkout (baseline 9f75cbb)"
)
args = parser.parse_args()
source = args.reference / "extensions/twitter"


def load(filename):
    name = "inkcre_twitter_schema_" + filename
    spec = importlib.util.spec_from_file_location(name, source / (filename + ".py"))
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


native = load("model")
legacy = load("schema")
models = {
    name: getattr(native, name).model_json_schema()
    for name in (
        "TwitterBatch",
        "TwitterIdentity",
        "TwitterPost",
        "TwitterUser",
        "TwitterMedia",
    )
}
for name in ("TwitterPost", "TwitterUser", "TwitterMedia"):
    models["Native" + name] = pydantic.TypeAdapter(
        getattr(native, name) | native.TwitterIdentity
    ).json_schema()
models["Tweet"] = legacy.Tweet.model_json_schema()
wire = load("archive_schema")
for name in (
    "ArchiveBatch",
    "ArchiveQuery",
    "ArchiveUser",
    "ArchivePost",
    "ArchiveMedia",
):
    models[name] = getattr(wire, name).model_json_schema()
definitions = {}
for name, schema in models.items():
    for definition, value in schema.pop("$defs", {}).items():
        if definition in definitions and definitions[definition] != value:
            raise ValueError("Conflicting canonical schema: " + definition)
        definitions[definition] = value
    if name in definitions and definitions[name] != schema:
        raise ValueError("Conflicting canonical schema: " + name)
    definitions[name] = schema
output = {"models": list(models), "$defs": definitions}
(Path(__file__).resolve().parents[1] / "src/schemas.json").write_text(
    json.dumps(output, indent=2, sort_keys=True) + "\n"
)
