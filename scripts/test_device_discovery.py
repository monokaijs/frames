"""Local preview/privacy regression; never opens a remote-control session.

Build frames and frames-core's discovery_probe example, then run this script.
The trusted-preview check briefly captures the local display; no pixels are saved.
"""
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time


def main():
    root = Path(__file__).resolve().parents[1]
    target = (root / "target").resolve()
    target.relative_to(root)
    executable = target / "debug" / "frames.exe"
    probe = target / "debug" / "examples" / "discovery_probe.exe"
    flags = getattr(subprocess, "CREATE_NO_WINDOW", 0)
    with tempfile.TemporaryDirectory(prefix="device-discovery-", dir=target) as temporary:
        work = Path(temporary).resolve()
        work.relative_to(target)
        host_dir, client_dir = work / "host", work / "client"
        host_dir.mkdir()
        client_dir.mkdir()
        host_env = dict(os.environ, FRAMES_CONFIG_DIR=str(host_dir), FRAMES_NO_DISCOVERY="1")
        client_env = dict(os.environ, FRAMES_CONFIG_DIR=str(client_dir))
        identity = subprocess.run([str(executable), "identity"], env=client_env, check=True,
                                  capture_output=True, text=True, creationflags=flags).stdout.splitlines()[0]
        (host_dir / "device-name.json").write_text(json.dumps("Discovery test host"), encoding="utf-8")
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sock:
            sock.bind(("127.0.0.1", 0))
            address = f"127.0.0.1:{sock.getsockname()[1]}"
        log_path = work / "host.log"
        with log_path.open("wb") as log:
            host = subprocess.Popen([str(executable), "host", "--listen", address, "--no-clipboard"],
                                    env=host_env, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                                    stderr=log, creationflags=flags)
            try:
                deadline = time.monotonic() + 30
                while "Frames host listening" not in log_path.read_text(errors="replace"):
                    if host.poll() is not None or time.monotonic() > deadline:
                        raise RuntimeError(log_path.read_text(errors="replace"))
                    time.sleep(0.1)

                def query():
                    result = subprocess.run([str(probe), address], env=client_env, check=True,
                                            capture_output=True, text=True, timeout=12,
                                            creationflags=flags)
                    return json.loads(result.stdout)

                first = query()
                assert first["name"] == "Discovery test host", first
                assert first["preview_bytes"] == 0, "Untrusted device received a screenshot"
                assert not (host_dir / "trusted.json").exists(), "A presence probe saved trust"
                peers = {identity: {"name": "Test client", "unattended": False, "allow_control": False}}
                (host_dir / "trusted.json").write_text(json.dumps({"peers": peers}), encoding="utf-8")
                assert query()["preview_bytes"] == 0, "A host-only pin granted screenshot access"
                peers[identity]["allow_control"] = True
                (host_dir / "trusted.json").write_text(json.dumps({"peers": peers}), encoding="utf-8")
                trusted = query()
                assert 0 < trusted["preview_bytes"] <= 512 * 1024, trusted
                (host_dir / "device-name.json").write_text(json.dumps("Renamed studio"), encoding="utf-8")
                assert query()["name"] == "Renamed studio", "Saved name did not update live"
                (host_dir / "trusted.json").write_text('{"peers":{}}', encoding="utf-8")
                assert query()["preview_bytes"] == 0, "Revoked device received a screenshot"
                print("PASS: names update live; untrusted, host-only, and revoked peers receive no screenshot; trusted peers receive a bounded preview.")
            finally:
                host.terminate()
                host.wait(timeout=10)


if __name__ == "__main__":
    main()
