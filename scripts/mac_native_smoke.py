"""Exercise the real AppKit/Metal client with generated pixels in an Aqua session.

Run on the Mac after build.sh and `cargo build -p frames-native --example
menu_preview --target aarch64-apple-darwin`. Does not capture a screen or inject
desktop input. Logs GPU completion, not input-to-photon latency or pixel readback.
"""
import json
import os
from pathlib import Path
import plistlib
import subprocess
import sys
import tempfile
import time


def main():
    if sys.platform != "darwin":
        raise RuntimeError("Run this fixture on macOS in a logged-in Aqua session")
    repo = Path(__file__).resolve().parents[1]
    executable = repo / "target/aarch64-apple-darwin/debug/examples/menu_preview"
    helper = repo / "dist/macos/Frames.app/Contents/MacOS/FramesHelper"
    for path in (executable, helper):
        if not path.is_file():
            raise FileNotFoundError(path)
    root = Path(tempfile.mkdtemp(prefix="frames-native-smoke."))
    label = "app.frames.native-smoke." + str(os.getpid())
    domain = "gui/" + str(os.getuid())
    config = {
        "Label": label,
        "ProgramArguments": [str(executable)],
        "EnvironmentVariables": {
            "FRAMES_MAC_HELPER": str(helper),
            "FRAMES_NATIVE_DIAGNOSTICS": "1",
            "FRAMES_PREVIEW_SECONDS": "8",
        },
        "RunAtLoad": True,
        "LimitLoadToSessionType": ["Aqua"],
        "StandardOutPath": str(root / "out.log"),
        "StandardErrorPath": str(root / "err.log"),
    }
    manifest = root / "agent.plist"
    manifest.write_bytes(plistlib.dumps(config))
    try:
        subprocess.run(["launchctl", "bootstrap", domain, str(manifest)], check=True)
        end = time.monotonic() + 15
        while time.monotonic() < end:
            content = (root / "err.log").read_text() if (root / "err.log").exists() else ""
            events = [json.loads(line) for line in content.splitlines() if line.startswith("{")]
            snapshot = any(e.get("native_event") == "window_snapshot" and e.get("count") == 1 for e in events)
            gpu = any(e.get("native_event") == "metal_present_completed" and e.get("success")
                      and e.get("width") == 640 and e.get("height") == 400 for e in events)
            menu = any(e.get("native_event") == "menu_installed" and e.get("remote_top_level_items") == 1 for e in events)
            if snapshot and gpu and menu:
                print("PASS: Rust/Swift bridge, AppKit proxy, native application menu, Metal GPU completion")
                return
            time.sleep(0.25)
        raise RuntimeError("Native fixture did not reach all checkpoints:\n" + content)
    finally:
        subprocess.run(["launchctl", "bootout", domain + "/" + label],
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        print("Logs: " + str(root))


if __name__ == "__main__":
    main()
