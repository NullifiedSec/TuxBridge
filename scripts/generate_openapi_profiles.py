#!/usr/bin/env python3
"""Generate <=30-operation GPT Action schemas from canonical openapi.yaml.

The canonical OpenAPI document is JSON-compatible YAML. This script deliberately
uses only the Python standard library so schema generation does not add a Python
package-management dependency.
"""
from __future__ import annotations

import copy
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
CANONICAL = ROOT / "openapi.yaml"
PROFILES = ROOT / "openapi-profiles.json"
LIMIT = 30


def load_json(path: Path):
    with path.open("r", encoding="utf-8") as handle:
        return json.load(handle)


def operation_count(spec: dict) -> int:
    return sum(
        1
        for path_item in spec.get("paths", {}).values()
        for method in path_item
        if method.lower() in {"get", "put", "post", "delete", "patch", "head", "options", "trace"}
    )


def generate(profile: str, wanted_ids: list[str], canonical: dict) -> dict:
    if len(wanted_ids) > LIMIT:
        raise SystemExit(f"profile {profile!r} defines {len(wanted_ids)} operations; maximum is {LIMIT}")
    if len(set(wanted_ids)) != len(wanted_ids):
        raise SystemExit(f"profile {profile!r} contains duplicate operationId values")

    wanted = set(wanted_ids)
    found: set[str] = set()
    paths: dict = {}
    methods = {"get", "put", "post", "delete", "patch", "head", "options", "trace"}

    for path, path_item in canonical.get("paths", {}).items():
        selected = {}
        for key, value in path_item.items():
            if key.lower() not in methods:
                # Preserve path-level parameters only if an operation survives.
                continue
            operation_id = value.get("operationId")
            if operation_id in wanted:
                selected[key] = copy.deepcopy(value)
                found.add(operation_id)
        if selected:
            if "parameters" in path_item:
                selected["parameters"] = copy.deepcopy(path_item["parameters"])
            paths[path] = selected

    missing = wanted - found
    if missing:
        raise SystemExit(f"profile {profile!r} references missing operationIds: {sorted(missing)}")

    spec = {
        "openapi": canonical["openapi"],
        "info": {
            "title": f"TuxBridge {profile.title()} GPT API",
            "version": canonical.get("info", {}).get("version", "0.1.0"),
            "description": f"Role-scoped TuxBridge Action surface for the {profile} occupation.",
        },
        "servers": copy.deepcopy(canonical.get("servers", [])),
        "security": copy.deepcopy(canonical.get("security", [])),
        "paths": paths,
        # Keeping canonical components makes each generated file standalone and
        # avoids brittle reference-pruning logic. Components do not count toward
        # the GPT Actions operation limit.
        "components": copy.deepcopy(canonical.get("components", {})),
    }
    count = operation_count(spec)
    if count != len(wanted_ids):
        raise SystemExit(
            f"profile {profile!r} generated {count} operations, expected {len(wanted_ids)}"
        )
    if count > LIMIT:
        raise SystemExit(f"profile {profile!r} generated {count} operations; maximum is {LIMIT}")
    return spec


def main() -> None:
    canonical = load_json(CANONICAL)
    profiles = load_json(PROFILES)
    for profile, operation_ids in profiles.items():
        spec = generate(profile, operation_ids, canonical)
        output = ROOT / f"openapi-{profile}.yaml"
        output.write_text(json.dumps(spec, indent=2) + "\n", encoding="utf-8")
        print(f"{output.name}: {operation_count(spec)} operations")


if __name__ == "__main__":
    main()
