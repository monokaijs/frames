"""Exercise the real CLI/TLS/pairing/bulk-transfer path with isolated identities.

Does not capture windows, send input, alter the clipboard, or install a service.
Uses loopback only. Both pairing codes are compared before either is approved.
"""
import argparse
import hashlib
import os
from pathlib import Path
import queue
import re
import shutil
import subprocess
import tempfile
import threading
import time


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("binary", type=Path)
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    # Generated download fixtures must be inside the host's allowed home root.
    temporary_parent = Path.home().resolve()
    root = Path(tempfile.mkdtemp(prefix="FramesSmoke-", dir=temporary_parent)).resolve()
    assert root.parent == temporary_parent and root.name.startswith("FramesSmoke-")
    processes = []
    logs = queue.Queue()

    def environment(role):
        env = os.environ.copy()
        env.pop("FRAMES_GUI_CHILD", None)
        env.update(FRAMES_CONFIG_DIR=str(root / role), FRAMES_PAIRING_CONSOLE="1",
                   FRAMES_NO_DISCOVERY="1", FRAMES_DOWNLOAD_DIR=str(root / "downloaded"))
        return env

    def start(role, arguments):
        child = subprocess.Popen([str(binary), *arguments], env=environment(role),
                                 stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                 text=True, encoding="utf-8", errors="replace",
                                 creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0))
        processes.append(child)
        return child

    def read_errors(child, role):
        for line in child.stderr:
            logs.put((role, line.rstrip()))

    try:
        for role in ("host", "client"):
            attempts = [start(role, ["identity"]) for _ in range(4)]
            fingerprints = set()
            for attempt in attempts:
                output, error = attempt.communicate(timeout=20)
                assert attempt.returncode == 0, error
                fingerprint = output.splitlines()[0]
                assert re.fullmatch(r"[0-9a-f]{64}", fingerprint), output
                fingerprints.add(fingerprint)
            assert len(fingerprints) == 1, "concurrent startup replaced a device identity"
        print("PASS: concurrent first-run processes retain one protected identity", flush=True)

        incoming = root / "received"
        host = start("host", ["host", "--listen", "127.0.0.1:0", "--pair", "--unattended",
                              "--no-clipboard", "--receive-dir", str(incoming)])
        threading.Thread(target=read_errors, args=(host, "host"), daemon=True).start()
        address = None
        deadline = time.monotonic() + 90
        history = []
        while time.monotonic() < deadline:
            try:
                role, line = logs.get(timeout=0.2)
                history.append(line)
                match = re.search(r"Frames host listening on (127\.0\.0\.1:\d+)", line)
                if match:
                    address = match.group(1)
                    break
            except queue.Empty:
                if host.poll() is not None:
                    raise RuntimeError("host failed: " + "\n".join(history))
        assert address, "host did not start"

        source = root / "báo cáo.txt"
        payload = bytes(range(256)) * 8192
        source.write_bytes(payload)
        client = start("client", ["send", address, str(source)])
        threading.Thread(target=read_errors, args=(client, "client"), daemon=True).start()
        codes = {}
        deadline = time.monotonic() + 30
        while len(codes) < 2 and time.monotonic() < deadline:
            try:
                role, line = logs.get(timeout=0.2)
                match = re.search(r"both devices: (\d{8})", line)
                if match:
                    codes[role] = match.group(1)
            except queue.Empty:
                pass
        assert len(codes) == 2 and codes["host"] == codes["client"], "pairing codes did not match"
        for child in (host, client):
            child.stdin.write("yes\n")
            child.stdin.flush()
        client.wait(timeout=30)
        assert client.returncode == 0, "initial transfer failed"
        assert (incoming / source.name).read_bytes() == payload, "received file differs"
        print("PASS: explicit pairing with matching TLS code; verified Unicode-name upload", flush=True)

        time.sleep(0.25)
        collision = subprocess.run([str(binary), "send", address, str(source)], env=environment("client"),
                                   input="", capture_output=True, text=True, encoding="utf-8", timeout=30,
                                   creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0))
        assert collision.returncode == 0, collision.stderr
        assert (incoming / (source.name + " (1)")).read_bytes() == payload
        assert (incoming / source.name).read_bytes() == payload
        print("PASS: trusted reconnect and collision-safe publication", flush=True)

        time.sleep(0.25)
        relative = source.relative_to(Path.home()).as_posix()
        download = subprocess.run([str(binary), "get", address, relative], env=environment("client"),
                                  input="", capture_output=True, text=True, encoding="utf-8", timeout=30,
                                  creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0))
        assert download.returncode == 0, download.stderr
        assert (root / "downloaded" / source.name).read_bytes() == payload
        print("PASS: host-to-client download over separate bulk stream", flush=True)

        time.sleep(0.25)
        large = root / "resume.bin"
        large.write_bytes(bytes(range(256)) * 65536)
        interrupted = start("client", ["send", address, str(large)])
        deadline = time.monotonic() + 15
        partial = None
        while time.monotonic() < deadline:
            if interrupted.poll() is not None:
                raise RuntimeError("interrupted-transfer client exited early: " + interrupted.stderr.read())
            candidates = list(incoming.glob(".*.partial"))
            if candidates and candidates[0].stat().st_size >= 512 * 1024:
                partial = candidates[0]
                break
            time.sleep(0.02)
        assert partial, "upload did not produce a resumable partial file"
        interrupted.kill(); interrupted.wait(timeout=5)
        prefix_size = partial.stat().st_size
        time.sleep(0.5)
        resumed = subprocess.run([str(binary), "send", address, str(large)], env=environment("client"),
                                 input="", capture_output=True, text=True, encoding="utf-8", timeout=30,
                                 creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0))
        assert resumed.returncode == 0, resumed.stderr
        assert hashlib.sha256((incoming / large.name).read_bytes()).digest() == hashlib.sha256(large.read_bytes()).digest()
        assert not partial.exists()
        print(f"PASS: interrupted upload resumed from a {prefix_size}-byte prefix", flush=True)
    finally:
        for child in reversed(processes):
            if child.poll() is None:
                child.kill()
            child.wait(timeout=10)
        checked = root.resolve()
        assert checked.parent == temporary_parent and checked.name.startswith("FramesSmoke-") and not root.is_symlink()
        shutil.rmtree(checked)


if __name__ == "__main__":
    main()
