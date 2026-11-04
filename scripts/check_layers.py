#!/usr/bin/env python3
"""Enforce the v2 crate layering (docs/V2_PLAN.md, "Target architecture").

For every v2 crate this walks the resolved dependency graph (normal and build
dependencies, transitively; dev-dependencies of the crate itself excluded) and
fails if it reaches:
- a Frames crate outside its allowed set (dependencies point down only), or
- a v1 crate (frames-core, frames-native, frames), or
- a crate that needs a C toolchain, so the engine and its netsim keep
  building on any CI runner.

Usage: python3 scripts/check_layers.py
"""

import json
import subprocess
import sys

# v2 crate -> Frames crates it may depend on (directly or transitively).
ALLOWED = {
    "frames-proto": set(),
    "frames-media": {"frames-proto"},
    "frames-testkit": {"frames-media", "frames-proto"},
    "frames-net": {"frames-media", "frames-proto"},
    "frames-input": set(),
    "frames-clipboard": set(),
    "frames-platform-macos": {"frames-media", "frames-proto", "frames-input", "frames-clipboard"},
    # The v2 application: every v2 crate. frames-core only through
    # frames-codec's FFmpeg runtime, until the v2 cutover moves it.
    "frames-next": {"frames-media", "frames-proto", "frames-net", "frames-codec", "frames-platform-windows", "frames-platform-macos", "frames-input", "frames-clipboard", "frames-core"},
    # Backends. frames-core is allowed only until the v2 cutover moves its
    # FFmpeg runtime into frames-codec.
    "frames-codec": {"frames-media", "frames-proto", "frames-core"},
    "frames-platform-windows": {"frames-media", "frames-proto", "frames-codec", "frames-input", "frames-clipboard", "frames-core"},
}
# Backend crates wrap native SDKs and may need a C toolchain; the engine
# crates above them may not.
# frames-net uses ring (QUIC crypto), which compiles C.
NATIVE_ALLOWED = {"frames-codec", "frames-platform-windows", "frames-net", "frames-next"}
V1_CRATES = {"frames-core", "frames-native", "frames"}
# Build-time C toolchain users.
NATIVE_BUILD = {"cc", "cmake", "bindgen", "pkg-config", "vcpkg"}


def main() -> int:
    metadata = json.loads(
        subprocess.run(
            ["cargo", "metadata", "--format-version", "1", "--locked"],
            check=True,
            capture_output=True,
            text=True,
        ).stdout
    )
    packages = {package["id"]: package for package in metadata["packages"]}
    nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
    by_name = {
        packages[member]["name"]: member for member in metadata["workspace_members"]
    }
    frames_crates = ALLOWED.keys() | V1_CRATES

    def edges(node_id):
        for dep in nodes[node_id]["deps"]:
            kinds = {kind["kind"] for kind in dep["dep_kinds"]}
            # Dev-dependencies only build the crate's own tests.
            if kinds == {"dev"}:
                continue
            yield dep["pkg"]

    failures = []
    for crate, allowed in sorted(ALLOWED.items()):
        root = by_name.get(crate)
        if root is None:
            failures.append(f"{crate}: not a workspace member")
            continue
        seen, stack, parents = {root}, [root], {}
        while stack:
            current = stack.pop()
            for dep in edges(current):
                if dep in seen:
                    continue
                seen.add(dep)
                parents[dep] = current
                stack.append(dep)
        for dep in seen - {root}:
            name = packages[dep]["name"]
            problem = None
            if name in frames_crates and name not in allowed:
                problem = "is outside its allowed layers"
            elif name in NATIVE_BUILD and crate not in NATIVE_ALLOWED:
                problem = "needs a C toolchain"
            if problem:
                chain, at = [name], dep
                while parents.get(at) and parents[at] != root:
                    at = parents[at]
                    chain.append(packages[at]["name"])
                path = " <- ".join(chain + [crate])
                failures.append(f"{crate}: depends on {name}, which {problem} ({path})")

    for failure in sorted(failures):
        print(f"layering: {failure}", file=sys.stderr)
    if not failures:
        print(f"layering: {', '.join(sorted(ALLOWED))} OK")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
