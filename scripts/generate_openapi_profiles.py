#!/usr/bin/env python3
"""Generate or validate <=30-operation GPT Action schemas.

Canonical route/method/operation metadata comes from openapi.yaml plus the
occupation policy in openapi-profiles.json. Only `operations` count toward the
GPT Actions limit; `support_routes` are server-side role permissions used by
Mission Control or other non-Action clients.

The GPT Actions importer is intentionally treated as a stricter OpenAPI
consumer than a general OpenAPI 3.1 validator. In particular, request bodies
that resolve to an `allOf` composition may be rejected as "not an object
schema" even when every composed member is an object. Generated GPT-facing
schemas therefore flatten local object compositions into explicit object
schemas while canonical openapi.yaml remains the complete source document.
"""
from __future__ import annotations

import argparse
import copy
import json
import re
from pathlib import Path
from urllib.parse import urlsplit

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


def validate_manifest(profile: str, spec: dict, index: dict[str, tuple[str, str]]) -> list[str]:
    if set(spec) - {"operations", "support_routes"}:
        raise SystemExit(f"profile {profile!r} contains unknown manifest keys")
    wanted_ids = spec.get("operations", [])
    support_routes = spec.get("support_routes", [])
    if not isinstance(wanted_ids, list):
        raise SystemExit(f"profile {profile!r} operations must be a list")
    if not 1 <= len(wanted_ids) <= LIMIT:
        raise SystemExit(f"profile {profile!r} defines {len(wanted_ids)} operations; maximum is {LIMIT}")
    if len(set(wanted_ids)) != len(wanted_ids):
        raise SystemExit(f"profile {profile!r} contains duplicate operationId values")
    missing = set(wanted_ids) - index.keys()
    if missing:
        raise SystemExit(f"profile {profile!r} references missing operationIds: {sorted(missing)}")
    if not isinstance(support_routes, list):
        raise SystemExit(f"profile {profile!r} support_routes must be a list")
    for route in support_routes:
        if not isinstance(route, dict) or set(route) != {"method", "path"}:
            raise SystemExit(f"profile {profile!r} has malformed support route {route!r}")
        method = str(route["method"]).lower()
        path = str(route["path"])
        if method not in METHODS or not path.startswith("/v1/") or any(ch.isspace() for ch in path):
            raise SystemExit(f"profile {profile!r} has invalid support route {route!r}")
    return wanted_ids


def validate_server_url(value: str) -> str:
    """Accept only a clean HTTPS origin suitable for GPT Actions."""
    parsed = urlsplit(value)
    if (
        parsed.scheme != "https"
        or not parsed.hostname
        or parsed.username is not None
        or parsed.password is not None
        or parsed.query
        or parsed.fragment
        or parsed.path not in ("", "/")
    ):
        raise SystemExit(
            "--server-url must be a plain HTTPS origin such as https://tuxbridge.example.com"
        )
    try:
        port = parsed.port
    except ValueError as error:
        raise SystemExit(f"invalid --server-url port: {error}") from error
    host = parsed.hostname.lower()
    if parsed.hostname != parsed.netloc.split(":")[0].strip("[]") and ":" not in host:
        # urlsplit already validates the structural pieces; this merely keeps the
        # normalized output from preserving surprising host casing.
        pass
    if ":" in host:
        rendered_host = f"[{host}]"
    else:
        rendered_host = host
    return f"https://{rendered_host}{f':{port}' if port is not None else ''}"


def _resolve_local_schema_ref(ref: str, schemas: dict) -> dict:
    prefix = "#/components/schemas/"
    if not ref.startswith(prefix):
        raise SystemExit(f"cannot flatten non-local schema reference {ref!r}")
    name = ref[len(prefix):]
    target = schemas.get(name)
    if not isinstance(target, dict):
        raise SystemExit(f"cannot flatten missing schema reference {ref!r}")
    return target


def _flatten_object_schema(schema: dict, schemas: dict, stack: tuple[str, ...] = ()) -> dict:
    """Flatten local allOf object compositions for GPT Actions compatibility."""
    if "allOf" not in schema:
        return copy.deepcopy(schema)

    merged: dict = {"type": "object"}
    required: list[str] = []
    properties: dict = {}
    additional_properties = None

    for member in schema.get("allOf", []):
        if not isinstance(member, dict):
            raise SystemExit("allOf members must be object schemas")
        if "$ref" in member:
            ref = member["$ref"]
            if ref in stack:
                raise SystemExit(f"recursive schema composition while flattening {ref!r}")
            member = _flatten_object_schema(
                _resolve_local_schema_ref(ref, schemas), schemas, stack + (ref,)
            )
        else:
            member = _flatten_object_schema(member, schemas, stack)

        member_type = member.get("type")
        if member_type not in (None, "object"):
            raise SystemExit(f"cannot flatten non-object allOf member of type {member_type!r}")
        for name in member.get("required", []):
            if name not in required:
                required.append(name)
        member_properties = member.get("properties", {})
        if not isinstance(member_properties, dict):
            raise SystemExit("object schema properties must be an object")
        overlap = set(properties) & set(member_properties)
        if overlap:
            raise SystemExit(f"cannot flatten conflicting schema properties: {sorted(overlap)}")
        properties.update(copy.deepcopy(member_properties))
        if "additionalProperties" in member:
            value = member["additionalProperties"]
            if additional_properties is not None and additional_properties != value:
                raise SystemExit("cannot flatten conflicting additionalProperties constraints")
            additional_properties = copy.deepcopy(value)

    for key, value in schema.items():
        if key == "allOf":
            continue
        if key in {"type", "required", "properties", "additionalProperties"}:
            continue
        merged[key] = copy.deepcopy(value)

    if required:
        merged["required"] = required
    if properties:
        merged["properties"] = properties
    if additional_properties is not None:
        merged["additionalProperties"] = additional_properties
    return merged


def make_gpt_actions_compatible(spec: dict) -> None:
    """Normalize request-body schemas around known GPT Actions importer limits."""
    schemas = spec.get("components", {}).get("schemas", {})
    if not isinstance(schemas, dict):
        return
    for name, schema in list(schemas.items()):
        if isinstance(schema, dict) and "allOf" in schema:
            schemas[name] = _flatten_object_schema(schema, schemas)


def generate(profile: str, wanted_ids: list[str], canonical: dict, server_url: str | None) -> dict:
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
    result = {
        "openapi": canonical["openapi"],
        "info": {
            "title": f"TuxBridge {profile.title()} GPT API",
            "version": canonical.get("info", {}).get("version", "0.1.0"),
            "description": f"Role-scoped TuxBridge Action surface for the {profile} occupation.",
        },
        "servers": [{"url": server_url}] if server_url else copy.deepcopy(canonical.get("servers", [])),
        "security": copy.deepcopy(canonical.get("security", [])),
        "paths": paths,
        "components": copy.deepcopy(canonical.get("components", {})),
    }
    make_gpt_actions_compatible(result)
    if operation_count(result) != len(wanted_ids):
        raise SystemExit(f"profile {profile!r} did not generate the expected operation count")
    return result


def routes_from_json(spec: dict, source: Path) -> dict[str, tuple[str, str]]:
    found: dict[str, tuple[str, str]] = {}
    for path, path_item in spec.get("paths", {}).items():
        if not isinstance(path_item, dict):
            continue
        for method, operation in path_item.items():
            if method.lower() not in METHODS or not isinstance(operation, dict):
                continue
            operation_id = operation.get("operationId")
            if not operation_id:
                continue
            if operation_id in found:
                raise SystemExit(f"{source.name}: duplicate operationId {operation_id!r}")
            found[operation_id] = (path, method.lower())
    return found


def parse_profile_routes(path: Path) -> dict[str, tuple[str, str]]:
    """Parse route metadata from either generated JSON or committed YAML."""
    text = path.read_text(encoding="utf-8")
    if text.lstrip().startswith("{"):
        try:
            return routes_from_json(json.loads(text), path)
        except json.JSONDecodeError as error:
            raise SystemExit(f"{path.name}: invalid JSON-compatible schema: {error}") from error

    current_path = None
    current_method = None
    found: dict[str, tuple[str, str]] = {}
    path_re = re.compile(r"^  (/[^:]+):\s*$")
    method_re = re.compile(r"^    (get|put|post|delete|patch|head|options|trace):\s*$")
    op_re = re.compile(r"^      operationId:\s*([A-Za-z0-9_.-]+)\s*$")
    for line in text.splitlines():
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
    parser.add_argument(
        "--server-url",
        help="override generated servers[0].url with a plain HTTPS origin",
    )
    args = parser.parse_args()
    if args.check and args.server_url:
        parser.error("--server-url cannot be combined with --check")
    server_url = validate_server_url(args.server_url) if args.server_url else None
    canonical = load_json(CANONICAL)
    profiles = load_json(PROFILES)
    index = canonical_index(canonical)
    for profile, profile_spec in profiles.items():
        operation_ids = validate_manifest(profile, profile_spec, index)
        if args.check:
            check_committed(profile, operation_ids, index)
        else:
            spec = generate(profile, operation_ids, canonical, server_url)
            output = ROOT / f"openapi-{profile}.yaml"
            output.write_text(json.dumps(spec, indent=2) + "\n", encoding="utf-8")
            suffix = f" using {server_url}" if server_url else ""
            print(f"{output.name}: generated {operation_count(spec)} operations{suffix}")


if __name__ == "__main__":
    main()
