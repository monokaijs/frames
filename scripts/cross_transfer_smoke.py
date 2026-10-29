"""Bidirectional LAN transfers between Windows and a signed, GUI-launched Mac app.

Uses fresh test identities, generated files, matching-code pairing, and dedicated
ports. Does not capture desktops, inject input, alter clipboard, or grant TCC.
Mac GUI launch is required for access to its login Keychain.
"""
import argparse
import base64
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import re
import shlex
import subprocess
import threading
import time


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("binary", type=Path)
    parser.add_argument("--mac-ssh", default="macmini")
    parser.add_argument("--mac-address", required=True)
    parser.add_argument("--windows-address", required=True)
    parser.add_argument("--control-probe", type=Path,
                        help="Optional production-session fixture client; requires Frames QA source on the Mac")
    parser.add_argument("--capture-only", action="store_true")
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    run_id = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    root = Path("dist/cross-platform").resolve() / run_id
    root.mkdir(parents=True)
    children = []
    logs = []
    remote_hosts = []
    results = []
    failure = None
    creationflags = getattr(subprocess, "CREATE_NO_WINDOW", 0)

    def ssh(script, timeout=20):
        encoded = base64.b64encode(script.encode()).decode()
        result = subprocess.run(["ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=8",
                                 args.mac_ssh, f"echo {encoded} | base64 -D | /bin/bash"],
                                capture_output=True, text=True, encoding="utf-8", timeout=timeout,
                                creationflags=creationflags)
        if result.returncode:
            raise RuntimeError(result.stderr + result.stdout)
        return result.stdout

    home = ssh("printf '%s' \"$HOME\"").strip()
    mac_root = home + "/Library/Caches/FramesCrossTest/" + run_id
    mac_app = home + "/Applications/Frames.app"
    q = shlex.quote
    ssh(f"mkdir -p {q(mac_root)}")

    def environment(role):
        env = os.environ.copy()
        for key in ("FRAMES_GUI_CHILD", "FRAMES_PRIVILEGED_AGENT"):
            env.pop(key, None)
        env.update(FRAMES_CONFIG_DIR=str(root / role), FRAMES_PAIRING_CONSOLE="1",
                   FRAMES_NO_DISCOVERY="1", FRAMES_DOWNLOAD_DIR=str(root / "downloaded"))
        if os.environ.get("FRAMES_CROSS_RUST_LOG"):
            env["RUST_LOG"] = os.environ["FRAMES_CROSS_RUST_LOG"]
        return env

    def windows(role, arguments, executable=None):
        child = subprocess.Popen([str(executable or binary), *arguments], env=environment(role),
                                 stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                 text=True, encoding="utf-8", errors="replace", creationflags=creationflags)
        children.append(child)
        output = {"out": "", "err": ""}
        logs.append((role, output))
        def reader(stream, key):
            for line in stream:
                output[key] += line
        for key, stream in (("out", child.stdout), ("err", child.stderr)):
            threading.Thread(target=reader, args=(stream, key), daemon=True).start()
        return child, output

    def read_remote(path):
        return ssh(f"if test -f {q(path)}; then cat {q(path)}; fi")

    def mac(role, arguments):
        folder = mac_root + "/" + role
        ssh(f"mkdir -p {q(folder)}\nmkfifo -m 600 {q(folder + '/input')}")
        writer = subprocess.Popen(["ssh", "-o", "BatchMode=yes", args.mac_ssh,
                                   "cat > " + q(folder + "/input")], stdin=subprocess.PIPE,
                                  stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                  text=True, encoding="utf-8", creationflags=creationflags)
        children.append(writer)
        command = ["open", "-n", "-a", mac_app,
                   "--env", "FRAMES_CONFIG_DIR=" + folder,
                   "--env", "FRAMES_PAIRING_CONSOLE=1", "--env", "FRAMES_NO_DISCOVERY=1",
                   "--env", "FRAMES_FFMPEG=/opt/homebrew/opt/ffmpeg-full/bin/ffmpeg",
                   "--stdin", folder + "/input", "--stdout", folder + "/out.log",
                   "--stderr", folder + "/err.log", "--args", *arguments]
        if os.environ.get("FRAMES_CROSS_RUST_LOG"):
            command[4:4] = ["--env", "RUST_LOG=" + os.environ["FRAMES_CROSS_RUST_LOG"]]
        ssh(shlex.join(command))
        return writer, folder

    def until(check, seconds=90):
        end = time.monotonic() + seconds
        while time.monotonic() < end:
            result = check()
            if result:
                return result
            time.sleep(0.25)
        raise TimeoutError("Cross-platform test timed out")

    def pair(win_process, win_log, mac_writer, mac_folder):
        def codes():
            remote = read_remote(mac_folder + "/err.log")
            if "Frames: " in remote:
                raise RuntimeError(remote)
            if win_process.poll() is not None:
                raise RuntimeError(win_log["err"])
            left = re.search(r"both devices: (\d{8})", win_log["err"])
            right = re.search(r"both devices: (\d{8})", remote)
            if left and right:
                assert left[1] == right[1], "TLS verification codes differ"
                return True
            return False
        until(codes, 45)
        for process in (win_process, mac_writer):
            process.stdin.write("yes\n")
            process.stdin.flush()

    def passed(name):
        results.append(name)
        print("PASS: " + name, flush=True)

    try:
        payload = bytes(range(256)) * 2048
        source = root / "Frames Việt Unicode.bin"
        source.write_bytes(payload)
        mac_source = mac_root + "/Frames Mac Unicode.bin"
        subprocess.run(["ssh", "-o", "BatchMode=yes", args.mac_ssh,
                        "cat > " + q(mac_source)], input=payload, check=True,
                       capture_output=True, timeout=20, creationflags=creationflags)
        host_args = ["host", "--listen", "0.0.0.0:47991", "--pair", "--unattended",
                     "--no-clipboard", "--receive-dir", mac_root + "/received-mac"]
        mac_writer, mac_folder = mac("mac-host", host_args)
        remote_hosts.append("0.0.0.0:47991")
        until(lambda: "Frames host listening" in read_remote(mac_folder + "/err.log"))
        win, log = windows("windows-client", ["send", args.mac_address + ":47991", str(source)])
        pair(win, log, mac_writer, mac_folder)
        until(lambda: win.poll() is not None, 30)
        assert win.returncode == 0, log["err"]
        actual = ssh(f"shasum -a 256 {q(mac_root + '/received-mac/' + source.name)}").split()[0]
        assert actual == hashlib.sha256(payload).hexdigest()
        passed("Windows client -> Mac host: matching-code pairing, Unicode upload, SHA-256 integrity")

        get, get_log = windows("windows-client", ["get", args.mac_address + ":47991",
                                               mac_source.removeprefix(home + "/")])
        until(lambda: get.poll() is not None, 30)
        assert get.returncode == 0, get_log["err"]
        assert (root / "downloaded" / "Frames Mac Unicode.bin").read_bytes() == payload
        passed("Mac host -> Windows client: trusted download and integrity")

        if args.control_probe:
            probe_args = [args.mac_address + ":47991", str(root / "mac-host-control.json")]
            probe_args += ["--capture-only"] if args.capture_only else ["--desktop"]
            probe, probe_log = windows("windows-client", probe_args, args.control_probe.resolve(strict=True))
            until(lambda: probe.poll() is not None, 120)
            assert probe.returncode == 0, probe_log["err"] + probe_log["out"]
            passed("Mac host live fixture capture/control probe")

        win_host, win_host_log = windows("windows-host", ["host", "--listen", "0.0.0.0:47992", "--pair",
                                                        "--unattended", "--no-clipboard", "--receive-dir",
                                                        str(root / "received-windows")])
        until(lambda: "Frames host listening" in win_host_log["err"])
        mac_sender, sender_folder = mac("mac-client", ["send", args.windows_address + ":47992", mac_source])
        pair(win_host, win_host_log, mac_sender, sender_folder)
        until(lambda: "Transferred " in read_remote(sender_folder + "/out.log"), 30)
        assert (root / "received-windows" / "Frames Mac Unicode.bin").read_bytes() == payload
        passed("Mac client -> Windows host: GUI Keychain identity, matching-code pairing, upload integrity")
    except Exception as error:
        failure = str(error)
        raise
    finally:
        for child in reversed(children):
            if child.poll() is None:
                child.kill()
            child.wait(timeout=10)
        for role, output in logs:
            for channel, contents in output.items():
                (root / (role + "." + channel + ".log")).write_text(contents, encoding="utf-8")
        for address in remote_hosts:
            # Only this invocation's app, dedicated port, and receive-folder marker.
            pattern = "^" + re.escape(mac_app + "/Contents/MacOS/frames host --listen " + address)
            ssh(f"pgrep -f {q(pattern)} | while read -r pid; do "
                f"if ps -p \"$pid\" -o command= | grep -Fq {q(mac_root)}; then kill \"$pid\"; fi; done")
        (root / "results.json").write_text(json.dumps({"passed": results, "failure": failure,
                                                       "mac_address": args.mac_address,
                                                       "windows_address": args.windows_address,
                                                       "mac_test_root": mac_root}, indent=2), encoding="utf-8")
        print("Report: " + str(root / "results.json"), flush=True)


if __name__ == "__main__":
    main()
