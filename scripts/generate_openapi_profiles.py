#!/usr/bin/env python3
"""Generate or validate <=30-operation GPT Action schemas.

Canonical route/method/operation metadata comes from openapi.yaml plus the
occupation allowlists in openapi-profiles.json. The default mode regenerates
standalone JSON-compatible YAML files. --check validates the committed profile
files without rewriting them, which keeps CI deterministic even when the
profile files contain hand-tuned descriptions or narrower component schemas.
"""
from __future__ import annotations

import argparse
import copy
import json
import re
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
CANONICAL = ROOT / "openapi.yaml"
PROFILES = ROOT / "openapi-profiles.json"
LIMIT = 30
METHODS = {"get", "put", "post", "delete", "patch", "head", "options", "trace"}


def load_json(path: Path):
    with path.open("r", encoding="utf-8") as handle:
        return json.load(handle)


def canonical_index(spec: dict) -> dict[str, tuple[str, str]]:
    index: dict[str, tuple[str, str]] = {}
    for path, path_item in spec.get("paths", {}).items():
        for method, operation in path_item.items():
            if method.lower() not in METHODS or not isinstance(operation, dict):
                continue
            operation_id = operation.get("operationId")
            if operation_id:
                if operation_id in index:
                    raise SystemExit(f"canonical schema duplicates operationId {operation_id!r}")
                index[operation_id] = (path, method.lower())
    return index


def operation_count(spec: dict) -> int:
    return sum(
        1
        for path_item in spec.get("paths", {}).values()
        for method in path_item
        if method.lower() in METHODS
    )


def validate_manifest(profile: str, wanted_ids: list[str], index: dict[str, tuple[str, str]]) -> None:
    if len(wanted_ids) > LIMIT:
        raise SystemExit(f"profile {profile!r} defines {len(wanted_ids)} operations; maximum is {LIMIT}")
    if len(set(wanted_ids)) != len(wanted_ids):
        raise SystemExit(f"profile {profile!r} contains duplicate operationId values")
    missing = set(wanted_ids) - index.keys()
    if missing:
        raise SystemExit(f"profile {profile!r} references missing operationIds: {sorted(missing)}")


def generate(profile: str, wanted_ids: list[str], canonical: dict) -> dict:
    index = canonical_index(canonical)
    validate_manifest(profile, wanted_ids, index)
    wanted = set(wanted_ids)
    paths: dict = {}
    for path, path_item in canonical.get("paths", {}).items():
        selected = {}
        for key, value in path_item.items():
            if key.lower() not in METHODS or not isinstance(value, dict):
                continue
            if value.get("operationId") in wanted:
                selected[key] = copy.deepcopy(value)
        if selected:
            if "parameters" in path_item:
                selected["parameters"] = copy.deepcopy(path_item["parameters"])
            paths[path] = selected
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
        "components": copy.deepcopy(canonical.get("components", {})),
    }
    if operation_count(spec) != len(wanted_ids):
        raise SystemExit(f"profile {profile!r} did not generate the expected operation count")
    return spec


def parse_profile_routes(path: Path) -> dict[str, tuple[str, str]]:
    """Parse only path/method/operationId indentation from our committed YAML."""
    current_path = None
    current_method = None
    found: dict[str, tuple[str, str]] = {}
    path_re = re.compile(r"^  (/[^:]+):\s*$")
    method_re = re.compile(r"^    (get|put|post|delete|patch|head|options|trace):\s*$")
    op_re = re.compile(r"^      operationId:\s*([A-Za-z0-9_.-]+)\s*$")
    for line in path.read_text(encoding="utf-8").splitlines():
        if match := path_re.match(line):
            current_path, current_method = match.group(1), None
            continue
        if match := method_re.match(line):
            current_method = match.group(1)
            continue
        if match := op_re.match(line):
            if current_path is None or current_method is None:
                raise SystemExit(f"{path.name}: operationId without path/method context")
            operation_id = match.group(1)
            if operation_id in found:
                raise SystemExit(f"{path.name}: duplicate operationId {operation_id!r}")
            found[operation_id] = (current_path, current_method)
    return found


def check_committed(profile: str, wanted_ids: list[str], index: dict[str, tuple[str, str]]) -> None:
    output = ROOT / f"openapi-{profile}.yaml"
    routes = parse_profile_routes(output)
    wanted = set(wanted_ids)
    actual = set(routes)
    if actual != wanted:
        raise SystemExit(
            f"{output.name}: operation set differs from manifest; "
            f"missing={sorted(wanted-actual)} extra={sorted(actual-wanted)}"
        )
    for operation_id in wanted_ids:
        if routes[operation_id] != index[operation_id]:
            raise SystemExit(
                f"{output.name}: {operation_id} maps to {routes[operation_id]}, "
                f"canonical maps to {index[operation_id]}"
            )
    if len(routes) > LIMIT:
        raise SystemExit(f"{output.name}: {len(routes)} operations exceeds maximum {LIMIT}")
    print(f"{output.name}: {len(routes)} operations OK")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--check", action="store_true", help="validate committed profile route mappings")
    args = parser.parse_args()
    canonical = load_json(CANONICAL)
    profiles = load_json(PROFILES)
    index = canonical_index(canonical)
    for profile, operation_ids in profiles.items():
        validate_manifest(profile, operation_ids, index)
        if args.check:
            check_committed(profile, operation_ids, index)
        else:
            spec = generate(profile, operation_ids, canonical)
            output = ROOT / f"openapi-{profile}.yaml"
            output.write_text(json.dumps(spec, indent=2) + "\n", encoding="utf-8")
            print(f"{output.name}: generated {operation_count(spec)} operations")


if __name__ == "__main__":
    main()
