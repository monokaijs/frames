"""Autonomous cross-platform end-to-end matrix: Mac/Windows client x Mac/Windows host.

Runs on the Mac and drives the Windows test PC over SSH. For every pairing it
starts a generated "Frames QA source" fixture and an isolated host (fresh
identity, config, receive folder, dedicated port), then checks:

  discovery     the client's `frames discover` sees the host's device id
  pairing       matching TLS codes on both sides, approved, Unicode upload
  trusted       reconnect without approval; download with SHA-256 integrity
  probe         e2e_probe: window list, sustained video + latency, debug
                overlay, audio, live quality change, clipboard both ways
                (cross-machine only), clean disconnect
  control       control_probe: input, text, menu, resize, close on the fixture
  native        the real native client presents frames (diagnostic counters)
  shutdown      host stops, port released, no stray processes

Safety: hosts share only the QA fixture (`--exclude` every other app with a
window), the full-desktop view is never requested, input goes only to the
fixture, pixels are never stored, clipboards are restored, identities live in
throwaway config folders, and macOS Keychain test items are deleted.

Windows processes run inside the logged-on console session (one-shot
scheduled tasks via scripts/e2e/win_launch.ps1), where capture, the user's
clipboard, DPAPI and GUI windows work. Mac processes run from this session.

  python3 scripts/e2e_matrix.py --win-address 192.168.0.100 \
      [--pairings mac-win,win-mac,mac-mac,win-win] [--skip-build]
"""
import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import re
import shlex
import socket
import subprocess
import sys
import time
import uuid

ROOT = Path(__file__).resolve().parent.parent
CODE = re.compile(r"both devices: (\d{8})")
ANSI = re.compile(r"\x1b\[[0-9;]*m")


def plain(text):
    """Log text without terminal colour codes (tracing writes them to files too)."""
    return ANSI.sub("", text)
SSH_NOISE = ("** WARNING", "** This session", "** The server")


class Failure(Exception):
    pass


def until(check, seconds, what):
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        value = check()
        if value:
            return value
        time.sleep(0.25)
    raise Failure(f"timed out after {seconds} s waiting for {what}")


class Windows:
    """The Windows test PC over SSH; processes run in the console session."""

    def __init__(self, ssh, root, user, frames, examples):
        self.ssh_host, self.root, self.user = ssh, root, user
        self.frames, self.examples = frames, examples
        self.tasks = []

    def ssh(self, command, timeout=60, check=True):
        result = subprocess.run(["ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=8", self.ssh_host, command],
                                capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=timeout)
        output = result.stdout
        if check and result.returncode:
            error = "\n".join(l for l in result.stderr.splitlines() if not l.startswith(SSH_NOISE))
            raise Failure(f"ssh {command!r} failed: {error or output}")
        return output

    def put(self, local, remote):
        subprocess.run(["scp", "-q", str(local), f"{self.ssh_host}:{remote.replace(chr(92), '/')}"],
                       check=True, capture_output=True, timeout=60)

    def read(self, path):
        return self.ssh(f'if exist "{path}" type "{path}"', check=False)

    def exists(self, path):
        return "yes" in self.ssh(f'if exist "{path}" echo yes', check=False)

    def touch(self, path):
        self.ssh(f'type nul > "{path}"')

    def write(self, path, text, scratch):
        local = scratch / ("upload-" + uuid.uuid4().hex)
        local.write_text(text, encoding="utf-8")
        self.put(local, path)
        local.unlink()

    def launch(self, name, exe, args, env=None, timeout=300, scratch=None):
        spec = {"name": name, "exe": exe, "args": [str(a) for a in args], "env": env or {},
                "dir": self.root, "timeout": timeout}
        local = scratch / f"job-{name}.json"
        local.write_text(json.dumps(spec), encoding="utf-8")
        job = f"{self.root}\\{name}.job.json"
        self.put(local, job)
        task = f"FramesE2E-{self.root.rsplit(chr(92), 1)[-1]}-{name}"
        command = (f"powershell -NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden "
                   f"-File {self.root}\\win_launch.ps1 -Job {job}")
        self.ssh(f'schtasks /create /tn "{task}" /tr "{command}" /sc once /st 23:59 /it /ru {self.user} /f')
        self.ssh(f'schtasks /run /tn "{task}"')
        self.tasks.append(task)
        return WinProcess(self, name)

    def run(self, name, exe, args, env=None, timeout=60, scratch=None):
        process = self.launch(name, exe, args, env, timeout, scratch)
        process.wait(timeout + 15)
        return process

    def cleanup(self):
        for task in self.tasks:
            self.ssh(f'schtasks /delete /tn "{task}" /f', check=False)


class WinProcess:
    def __init__(self, machine, name):
        self.machine, self.name = machine, name
        self.base = f"{machine.root}\\{name}"
        self.approvals = 0

    def log(self):
        return plain(self.machine.read(self.base + ".err.log"))

    def out(self):
        return self.machine.read(self.base + ".out.log")

    def approve(self):
        self.machine.touch(self.base + ".approve")

    def stop(self):
        if not self.exited():
            self.machine.touch(self.base + ".stop")

    def exited(self):
        text = self.machine.read(self.base + ".exit").strip()
        return text or None

    def wait(self, seconds):
        text = until(self.exited, seconds, f"Windows {self.name} to exit")
        self.code = int(text.split()[0])
        return self.code


class MacProcess:
    def __init__(self, folder, name, argv, env):
        self.name = name
        self.err_path, self.out_path = folder / f"{name}.err.log", folder / f"{name}.out.log"
        self.process = subprocess.Popen(argv, env=env, stdin=subprocess.PIPE,
                                        stdout=self.out_path.open("w"), stderr=self.err_path.open("w"),
                                        text=True, start_new_session=True)

    def log(self):
        return plain(self.err_path.read_text(errors="replace")) if self.err_path.exists() else ""

    def out(self):
        return self.out_path.read_text(errors="replace") if self.out_path.exists() else ""

    def approve(self):
        self.process.stdin.write("yes\n")
        self.process.stdin.flush()

    def stop(self):
        if self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(10)
            except subprocess.TimeoutExpired:
                self.process.kill()

    def exited(self):
        return self.process.poll() is not None

    def wait(self, seconds):
        self.code = self.process.wait(seconds)
        return self.code


class MacAppProcess:
    """`frames ARGS` of a signed app bundle launched through LaunchServices, so
    it runs with that bundle's privacy grants and login Keychain access."""

    def __init__(self, app, folder, name, argv, env):
        self.name, self.app, self.marker = name, app, str(folder)
        self.err_path, self.out_path = folder / f"{name}.err.log", folder / f"{name}.out.log"
        fifo = folder / f"{name}.stdin"
        os.mkfifo(fifo, 0o600)
        # Hold both ends open so the app never sees EOF before an approval.
        self.stdin = os.open(fifo, os.O_RDWR)
        command = ["open", "-n", "-a", str(app), "--stdin", str(fifo),
                   "--stdout", str(self.out_path), "--stderr", str(self.err_path)]
        for key, value in env.items():
            command += ["--env", f"{key}={value}"]
        self.argv = argv
        subprocess.run(command + ["--args", *argv], check=True)
        try:
            until(lambda: self.pids() or self.out_path.exists() and self.err_path.exists() and self.err_path.stat().st_size,
                  15, f"{name} to start")
        except Failure:
            pass

    def pids(self):
        listing = subprocess.run(["ps", "eww", "-A", "-o", "pid=,command="], capture_output=True, text=True).stdout
        executable = str(self.app) + "/Contents/MacOS/Frames " + " ".join(self.argv[:2])
        return [line.split()[0] for line in listing.splitlines()
                if executable in line and self.marker in line]

    def log(self):
        return plain(self.err_path.read_text(errors="replace")) if self.err_path.exists() else ""

    def out(self):
        return self.out_path.read_text(errors="replace") if self.out_path.exists() else ""

    def approve(self):
        os.write(self.stdin, b"yes\n")

    def stop(self):
        for pid in self.pids():
            subprocess.run(["kill", pid])

    def exited(self):
        return not self.pids()

    def wait(self, seconds):
        until(self.exited, seconds, f"{self.name} to exit")
        self.code = 0
        return 0


class Matrix:
    def __init__(self, args):
        self.args = args
        self.run_id = time.strftime("%Y%m%dT%H%M%S")
        self.scratch = Path(args.artifacts).resolve() / self.run_id
        self.scratch.mkdir(parents=True)
        self.mac_home = Path.home()
        # Host download sources must be inside the host user's home folder.
        self.mac_files = self.mac_home / "Library/Caches/FramesE2E" / self.run_id
        self.mac_files.mkdir(parents=True)
        target = ROOT / "target/aarch64-apple-darwin/release"
        self.mac_frames = target / "frames"
        self.mac_examples = target / "examples"
        self.mac_helper = self.scratch / "FramesHelper"
        self.win_root = f"{args.win_work}\\{self.run_id}"
        win_target = f"{args.win_target}\\x86_64-pc-windows-gnu\\release"
        self.win = Windows(args.win_ssh, self.win_root, args.win_user,
                           f"{win_target}\\frames.exe", f"{win_target}\\examples")
        self.win_home_files = f"C:\\Users\\{args.win_user}\\AppData\\Local\\Temp\\FramesE2E-{self.run_id}"
        self.mac_address = args.mac_address or default_mac_address()
        self.results = {}
        self.pretrusted = False
        self.keychain_items = []
        self.app_identities = []
        self.mac_processes = []

    # ---------------------------------------------------------------- setup
    def prepare(self):
        helper = ROOT / "dist/macos/Frames.app/Contents/MacOS/FramesHelper"
        if not helper.exists():
            raise Failure("build the Mac helper first: bash native/macos/build.sh (signing may fail; the helper is used unbundled)")
        # Unbundled copy: macOS attributes capture to this session's app (TCC),
        # never to the dev.creaton.frames bundle and its grants.
        self.mac_helper.write_bytes(helper.read_bytes())
        self.mac_helper.chmod(0o755)
        for binary in (self.mac_frames, self.mac_examples / "e2e_probe", self.mac_examples / "control_probe",
                       ROOT / "dist/QAWindowFixture.app/Contents/MacOS/QAWindowFixture"):
            if not binary.exists():
                raise Failure(f"missing {binary}; see docs/VALIDATION.md (end-to-end matrix)")
        if "win" not in self.args.pairings:
            return
        self.win.ssh(f'mkdir "{self.win_root}" & mkdir "{self.win_home_files}"', check=False)
        for script in ("win_launch.ps1", "win_clip.ps1", "win_windows.ps1"):
            self.win.put(ROOT / "scripts/e2e" / script, f"{self.win_root}\\{script}")
        for exe in (self.win.frames, f"{self.win.examples}\\e2e_probe.exe",
                    f"{self.win.examples}\\control_probe.exe", f"{self.win.examples}\\window_fixture.exe"):
            if not self.win.exists(exe):
                raise Failure(f"missing {exe} on Windows; run scripts/e2e/win_build.ps1")

    def mac_env(self, config, extra=None):
        env = {k: v for k, v in os.environ.items() if not k.startswith("FRAMES_")}
        env.update(FRAMES_CONFIG_DIR=str(config), FRAMES_PAIRING_CONSOLE="1", FRAMES_MAC_HELPER=str(self.mac_helper),
                   FRAMES_TEST_IDENTITY_FILE="1",
                   FRAMES_DOWNLOAD_DIR=str(config / "downloads"), RUST_LOG="warn,frames=info,frames_native=info")
        env.update(extra or {})
        config.mkdir(parents=True, exist_ok=True)
        return env

    def win_env(self, config, extra=None):
        env = {"FRAMES_CONFIG_DIR": config, "FRAMES_PAIRING_CONSOLE": "1",
               "FRAMES_DOWNLOAD_DIR": config + "\\downloads", "RUST_LOG": "warn,frames=info,frames_native=info"}
        env.update(extra or {})
        return env

    def start(self, side, name, argv_tail, config_name, extra_env=None, exe=None, timeout=300):
        """Starts `frames ARGS` (or another binary) on `side` with its own config."""
        if side == "mac":
            config = self.scratch / config_name
            env = self.mac_env(config, extra_env)
            if self.args.mac_host_app and exe is None and config_name.startswith("host-"):
                env = {k: v for k, v in env.items() if k.startswith(("FRAMES_", "RUST_LOG"))
                       and k not in ("FRAMES_MAC_HELPER", "FRAMES_TEST_IDENTITY_FILE")}
                env["FRAMES_RUN_MARKER"] = str(self.scratch)
                process = MacAppProcess(Path(self.args.mac_host_app), self.scratch, name, argv_tail, env)
                self.mac_processes.append(process)
                self.app_identities.append(config)
                return process
            argv = [str(exe or self.mac_frames), *argv_tail]
            process = MacProcess(self.scratch, name, argv, env)
            self.mac_processes.append(process)
            return process
        config = f"{self.win_root}\\{config_name}"
        self.win.ssh(f'mkdir "{config}"', check=False)
        return self.win.launch(name, exe or self.win.frames, argv_tail, self.win_env(config, extra_env),
                               timeout, self.scratch)

    def record_identity(self, side, config_name):
        if side == "mac":
            der = self.scratch / config_name / "identity.der"
            if der.exists():
                self.keychain_items.append(hashlib.sha256(der.read_bytes()).hexdigest())

    # -------------------------------------------------------------- helpers
    def pair(self, host, client, host_mark, seconds=45):
        """Approves both sides once their TLS verification codes match."""
        def codes():
            if client.exited():
                raise Failure(f"{client.name} exited before pairing: {client.log()[-800:]}")
            ours = CODE.findall(client.log())
            theirs = CODE.findall(host.log())[host_mark:]
            return (ours[-1], theirs[-1]) if ours and theirs else None
        ours, theirs = until(codes, seconds, f"pairing codes for {client.name}")
        if ours != theirs:
            raise Failure(f"pairing codes differ: client {ours}, host {theirs}")
        host.approve()
        client.approve()
        return host_mark + 1

    def pretrust(self, host_side, host_config, host_id, client_side, client_config):
        """Trust each other's identities directly (no pairing prompt). Used for
        the remaining steps of a pairing whose interactive pairing failed."""
        identity = self.start(client_side, f"identity-{client_config}", ["identity"], client_config)
        identity.wait(60)
        client_id = (identity.out().split() or [""])[0]
        peer = {"unattended": True, "allow_control": True}
        for side, config, other, name in ((host_side, host_config, client_id, "e2e client"),
                                          (client_side, client_config, host_id, "e2e host")):
            if side == "mac":
                path = self.scratch / config / "trusted.json"
                store = json.loads(path.read_text()) if path.exists() else {"peers": {}}
                store["peers"][other] = {"name": name, **peer}
                path.write_text(json.dumps(store))
            else:
                path = f"{self.win_root}\\{config}\\trusted.json"
                text = self.win.read(path)
                store = json.loads(text) if text.strip() else {"peers": {}}
                store["peers"][other] = {"name": name, **peer}
                self.win.write(path, json.dumps(store), self.scratch)

    def connect(self, host, client, mark):
        """Pairs a fresh client unless this pairing runs pre-trusted."""
        return mark if self.pretrusted else self.pair(host, client, mark)

    def host_codes(self, host):
        return len(CODE.findall(host.log()))

    def exclusions(self, side):
        """App ids with windows on the host, except the QA fixture."""
        if side == "mac":
            # The Mac host matches an exclusion as a case-insensitive substring
            # of the bundle id or app name; never drop one that hits the fixture.
            listing = subprocess.run(["lsappinfo", "list"], capture_output=True, text=True).stdout
            fixture = ("dev.frames.qa.fixture", "frames qa fixture")
            # Bundle ids, plus app names for unbundled processes with windows.
            apps = set(re.findall(r'bundleID="([^"]+)"', listing))
            apps |= set(re.findall(r'^\s*\d+\) "([^"]+)"', listing, re.MULTILINE))
            ids = sorted(app for app in apps if len(app) > 2 and not any(app.lower() in name for name in fixture))
            # Unbundled native-client helpers (loopback runs) have no bundle id.
            ids.append("FramesHelper")
            if self.args.mac_host_app:
                # What the signed host itself would share, read with its grants.
                listing = self.scratch / f"windows-{uuid.uuid4().hex[:6]}.json"
                subprocess.run(["open", "-W", "-n", "-a", self.args.mac_host_app, "--stdout", str(listing),
                                "--env", f"FRAMES_CONFIG_DIR={self.scratch / 'window-list'}", "--args", "windows"],
                               capture_output=True, timeout=60)
                try:
                    shared = {w["app_id"] for w in json.loads(listing.read_text())}
                except (OSError, ValueError):
                    raise Failure("could not list the Mac host's windows for exclusions")
                ids += sorted(app for app in shared if app and not any(app.lower() in name for name in fixture))
        else:
            listing = self.win.run("windows-list", "powershell.exe",
                                   ["-NoProfile", "-ExecutionPolicy", "Bypass", "-File",
                                    f"{self.win_root}\\win_windows.ps1"], timeout=30, scratch=self.scratch).out()
            ids = {line.strip() for line in listing.splitlines()
                   if line.strip() and not line.strip().lower().endswith("window_fixture.exe")}
            # Every window the host itself would share (a process can own
            # several top-level windows besides its main one).
            config = f"{self.win_root}\\window-list"
            self.win.ssh(f'mkdir "{config}"', check=False)
            shared = self.win.run(f"windows-json-{uuid.uuid4().hex[:6]}", self.win.frames, ["windows"],
                                  {"FRAMES_CONFIG_DIR": config}, timeout=30, scratch=self.scratch).out()
            try:
                ids |= {w["app_id"] for w in json.loads(shared)
                        if not w["app_id"].lower().endswith("window_fixture.exe")}
            except ValueError:
                raise Failure("could not list the Windows host's windows for exclusions")
            ids = sorted(ids)
            # The Windows host matches a lowercase path substring; exclude our
            # own client binary so a loopback native client is never mirrored.
            ids.append(self.win.frames.lower())
        return [arg for app in ids for arg in ("--exclude", app)]

    def fixture(self, side, tag, motion=True):
        """The generated source window. control_probe checks pixel colours, so
        it gets a static fixture; video checks need continuous motion."""
        if side == "mac":
            app = ROOT / "dist/QAWindowFixture.app"
            subprocess.run(["open", "-n", "-a", str(app), "--env", f"FRAMES_QA_MOTION={int(motion)}",
                            "--env", "FRAMES_QA_NO_COVER=1", "--env", f"FRAMES_E2E_RUN={self.run_id}"], check=True)
            return None
        return self.win.launch(f"fixture-{tag}", f"{self.win.examples}\\window_fixture.exe",
                               ["--no-cover", *(["--motion"] if motion else [])], {"FRAMES_QA_SECONDS": "240"},
                               260, self.scratch)

    def stop_fixture(self, side, process):
        if side == "mac":
            # Only fixture instances started by this run carry its id.
            for pid in subprocess.run(["pgrep", "-f", "QAWindowFixture.app/Contents/MacOS/QAWindowFixture"],
                                      capture_output=True, text=True).stdout.split():
                env = subprocess.run(["ps", "eww", "-o", "command=", "-p", pid], capture_output=True, text=True).stdout
                if f"FRAMES_E2E_RUN={self.run_id}" in env:
                    subprocess.run(["kill", pid])
        elif process:
            process.stop()

    def clipboard_get(self, side, tag):
        if side == "mac":
            return subprocess.run(["pbpaste"], capture_output=True, text=True).stdout
        path = f"{self.win_root}\\clip-{tag}.txt"
        self.win.run(f"clip-get-{tag}", "powershell.exe", ["-NoProfile", "-ExecutionPolicy", "Bypass", "-File",
                     f"{self.win_root}\\win_clip.ps1", "-Mode", "get", "-Path", path], timeout=20, scratch=self.scratch)
        return self.win.read(path)

    def clipboard_set(self, side, text, tag):
        if side == "mac":
            subprocess.run(["pbcopy"], input=text, text=True, check=True)
            return
        path = f"{self.win_root}\\clip-{tag}.txt"
        self.win.write(path, text, self.scratch)
        self.win.run(f"clip-set-{tag}", "powershell.exe", ["-NoProfile", "-ExecutionPolicy", "Bypass", "-File",
                     f"{self.win_root}\\win_clip.ps1", "-Mode", "set", "-Path", path], timeout=20, scratch=self.scratch)

    def clipboard_wait(self, side, text, tag, seconds=10):
        if side == "mac":
            try:
                until(lambda: subprocess.run(["pbpaste"], capture_output=True, text=True).stdout == text,
                      seconds, "host clipboard")
                return True
            except Failure:
                return False
        path = f"{self.win_root}\\clip-{tag}.txt"
        self.win.write(path, text, self.scratch)
        process = self.win.run(f"clip-wait-{tag}", "powershell.exe", ["-NoProfile", "-ExecutionPolicy", "Bypass",
                               "-File", f"{self.win_root}\\win_clip.ps1", "-Mode", "wait", "-Path", path,
                               "-Seconds", str(seconds)], timeout=seconds + 10, scratch=self.scratch)
        return process.code == 0

    def marker(self, side, folder, name):
        if side == "mac":
            return (Path(folder) / name).exists()
        return self.win.exists(f"{folder}\\{name}")

    def fetch_report(self, side, path):
        text = Path(path).read_text() if side == "mac" else self.win.read(path)
        return json.loads(text) if text.strip() else None

    def udp_port_free(self, side, port):
        if side == "mac":
            probe = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
            try:
                probe.bind(("0.0.0.0", port))
                return True
            except OSError:
                return False
            finally:
                probe.close()
        listing = self.win.ssh("netstat -ano -p UDP", check=False)
        return not re.search(rf"[:.]{port}\s", listing)

    def stray(self, side):
        if side == "mac":
            # Our processes carry FRAMES_CONFIG_DIR inside this run's folder.
            listing = subprocess.run(["ps", "eww", "-A", "-o", "pid=,command="], capture_output=True, text=True).stdout
            return [line[:120] for line in listing.splitlines()
                    if str(self.scratch) in line and "ps eww" not in line]
        listing = self.win.run("stray-check", "powershell.exe", ["-NoProfile", "-ExecutionPolicy", "Bypass", "-File",
                               f"{self.win_root}\\win_windows.ps1", "-Pattern",
                               f"{self.run_id}|{self.args.win_target}"], timeout=30, scratch=self.scratch).out()
        return [line for line in listing.splitlines() if line.strip()]

    # ------------------------------------------------------------- pairing
    def pairing(self, host_side, client_side, port):
        tag = f"{client_side}-to-{host_side}"
        checks = {}
        self.results[tag] = checks

        def record(name, status, detail, **numbers):
            checks[name] = {"status": status, "detail": detail, **numbers}
            print(f"  {name:<20} {status.upper():<4} {detail}", flush=True)

        print(f"== {client_side} client -> {host_side} host (port {port})", flush=True)
        host_address = "127.0.0.1" if host_side == client_side else (
            self.mac_address if host_side == "mac" else self.args.win_address)
        target = f"{host_address}:{port}"
        fixture = None
        host = None
        try:
            fixture = self.fixture(host_side, tag)
            time.sleep(3)
            excluded = self.exclusions(host_side)
            receive = (str(self.scratch / f"received-{tag}") if host_side == "mac"
                       else f"{self.win_root}\\received-{tag}")
            host_config = f"host-{tag}"
            identity = self.start(host_side, f"identity-{tag}", ["identity"], host_config)
            identity.wait(60)
            device_id = (identity.out().split() or [""])[0]
            self.host_id = device_id
            self.pretrusted = False
            self.record_identity(host_side, host_config)
            host = self.start(host_side, f"host-{tag}", ["host", "--listen", f"0.0.0.0:{port}", "--pair",
                              "--unattended", "--receive-dir", receive, *excluded], host_config,
                              {"FRAMES_MEDIA_DIAGNOSTICS": "1"}, timeout=900)
            until(lambda: "Frames host listening" in host.log() or host.exited(), 90, "host to listen")
            if host.exited():
                raise Failure("host exited: " + host.log()[-1500:])
            mark = self.host_codes(host)

            # 1. Discovery.
            started = time.monotonic()
            discover = self.start(client_side, f"discover-{tag}", ["discover", "--seconds", "6"], f"discover-{tag}")
            discover.wait(60)
            found = device_id and device_id in discover.out()
            record("discovery", "pass" if found else "fail",
                   f"host {device_id[:12]} {'found' if found else 'not found'} via mDNS",
                   ms=int((time.monotonic() - started) * 1000))

            # 2. Pairing + Unicode upload.
            payload = os.urandom(256 * 1024)
            name = "Frames Việt e2e.bin"
            if client_side == "mac":
                source = self.scratch / name
                source.write_bytes(payload)
            else:
                source = f"{self.win_root}\\{name}"
                local = self.scratch / name
                local.write_bytes(payload)
                self.win.put(local, source)
            started = time.monotonic()
            client_config = f"client-{tag}"
            send = self.start(client_side, f"send-{tag}", ["send", target, str(source)], client_config)
            mark = self.pair(host, send, mark)
            code = send.wait(60)
            if host_side == "mac":
                received = (Path(receive) / name).read_bytes() if (Path(receive) / name).exists() else b""
            else:
                local = self.scratch / f"received-{tag}.bin"
                subprocess.run(["scp", "-q", f"{self.args.win_ssh}:{(receive + chr(92) + name).replace(chr(92), '/')}",
                                str(local)], capture_output=True)
                received = local.read_bytes() if local.exists() else b""
            ok = code == 0 and hashlib.sha256(received).digest() == hashlib.sha256(payload).digest()
            record("pairing_upload", "pass" if ok else "fail",
                   "matching codes approved; Unicode upload SHA-256 verified" if ok
                   else f"exit {code}, received {len(received)} of {len(payload)} bytes",
                   ms=int((time.monotonic() - started) * 1000))
            self.record_identity(client_side, client_config)
            self.pretrusted = not ok
            if self.pretrusted:
                print("  (pairing failed: remaining steps use pre-trusted identities)", flush=True)
                send.stop()
                self.pretrust(host_side, host_config, device_id, client_side, client_config)
                mark = self.host_codes(host)

            # 3. Trusted reconnect: download needs no approval.
            download_payload = os.urandom(128 * 1024)
            if host_side == "mac":
                host_file = self.mac_files / "download.bin"
                host_file.write_bytes(download_payload)
                relative = host_file.relative_to(self.mac_home).as_posix()
            else:
                local = self.scratch / "download.bin"
                local.write_bytes(download_payload)
                self.win.put(local, self.win_home_files + "\\download.bin")
                relative = f"AppData/Local/Temp/FramesE2E-{self.run_id}/download.bin"
            started = time.monotonic()
            get = self.start(client_side, f"get-{tag}", ["get", target, relative], client_config)
            code = get.wait(60)
            prompted = bool(CODE.search(get.log()))
            if client_side == "mac":
                got = self.scratch / client_config / "downloads/download.bin"
                downloaded = got.read_bytes() if got.exists() else b""
            else:
                local = self.scratch / f"downloaded-{tag}.bin"
                subprocess.run(["scp", "-q", f"{self.args.win_ssh}:{self.win_root.replace(chr(92), '/')}/{client_config}/downloads/download.bin",
                                str(local)], capture_output=True)
                downloaded = local.read_bytes() if local.exists() else b""
            ok = code == 0 and not prompted and downloaded == download_payload
            record("trusted_download", "pass" if ok else "fail",
                   "reconnected without approval; SHA-256 verified" if ok
                   else f"exit {code}, prompted={prompted}, {len(downloaded)} bytes",
                   ms=int((time.monotonic() - started) * 1000))

            # 4. e2e_probe.
            self.probe(host_side, client_side, tag, target, host, mark, record)
            mark = self.host_codes(host)

            # 5. control_probe on a static fixture (it closes the window at the end).
            self.stop_fixture(host_side, fixture)
            fixture = self.fixture(host_side, tag + "-static", motion=False)
            time.sleep(3)
            control_config = f"control-{tag}"
            report = (str(self.scratch / f"control-{tag}.json") if client_side == "mac"
                      else f"{self.win_root}\\control-{tag}.json")
            exe = (self.mac_examples / "control_probe") if client_side == "mac" else f"{self.win.examples}\\control_probe.exe"
            started = time.monotonic()
            if self.pretrusted:
                self.pretrust(host_side, host_config, device_id, client_side, control_config)
            control = self.start(client_side, f"control-{tag}", [target, report, "--tolerate-chrome"], control_config,
                                 exe=exe, timeout=180)
            mark = self.connect(host, control, mark)
            try:
                control.wait(170)
            except Failure:
                control.stop()
            self.record_identity(client_side, control_config)
            result = self.fetch_report(client_side, report) or {}
            passed = result.get("passed", [])
            failure = result.get("failure")
            issues = result.get("issues", [])
            record("control", "fail" if failure or issues or not passed else "pass",
                   f"{len(passed)} checkpoints" + (f"; {failure}" if failure else "") +
                   (f"; issues: {'; '.join(issues)}" if issues else "") +
                   (f"; {', '.join(passed)}" if passed else ""),
                   ms=int((time.monotonic() - started) * 1000), measurements=result.get("measurements", {}))

            # 6. Native client presents frames. It mirrors every shared window,
            # so restart the fixture and the host with exclusions computed now
            # (same identity: earlier trust stays valid).
            self.stop_fixture(host_side, fixture)
            fixture = self.fixture(host_side, tag + "-native")
            time.sleep(3)
            host.stop()
            until(host.exited, 30, "host restart")
            host = self.start(host_side, f"host2-{tag}", ["host", "--listen", f"0.0.0.0:{port}", "--pair",
                              "--unattended", "--receive-dir", receive, *self.exclusions(host_side)], host_config,
                              {"FRAMES_MEDIA_DIAGNOSTICS": "1"}, timeout=600)
            until(lambda: "Frames host listening" in host.log() or host.exited(), 90, "host to listen again")
            mark = self.host_codes(host)
            native_config = f"native-{tag}"
            if self.pretrusted:
                self.pretrust(host_side, host_config, device_id, client_side, native_config)
            started = time.monotonic()
            native = self.start(client_side, f"native-{tag}", ["connect", target, "--no-clipboard"], native_config,
                                {"FRAMES_MEDIA_DIAGNOSTICS": "1", "FRAMES_NATIVE_DIAGNOSTICS": "1"}, timeout=120)
            mark = self.connect(host, native, mark)

            stream = re.compile(r"window=WindowId \{ id: (\d+), generation: \d+ \}.*?presented=(\d+)"
                                r".*?capture_present_p50_us=(\d+) capture_present_p95_us=(\d+)")

            def streams():
                latest = {}
                for line in native.log().splitlines():
                    if match := stream.search(line):
                        latest[match[1]] = tuple(int(v) for v in match.groups()[1:])
                return latest

            def presented():
                counts = [values[0] for values in streams().values()]
                return max(counts) if counts and max(counts) > 30 else None
            try:
                first = until(presented, 45, "native client presentation")
                begin = time.monotonic()
                time.sleep(8)
                latest = streams()
                count, p50, p95 = max(latest.values())
                fps = (count - first) / (time.monotonic() - begin)
                # Exclusions must leave only the fixture: one streamed window.
                others = len(latest) - 1
                ok = fps >= 20 and others == 0
                record("native_client", "pass" if ok else "fail",
                       f"{count} frames presented, {fps:.1f} fps steady (unfocused windows stream at 24); "
                       f"capture→present p50 {p50 / 1000:.1f} ms p95 {p95 / 1000:.1f} ms"
                       + (f"; {others} non-fixture windows were streamed" if others else ""),
                       ms=int((time.monotonic() - started) * 1000))
            except Failure as error:
                record("native_client", "fail", f"{error}; log tail: {native.log()[-600:]!r}")
            native.stop()
            self.record_identity(client_side, native_config)

            # 7. Shutdown (the fixture first: it is ours too).
            self.stop_fixture(host_side, fixture)
            fixture = None
            host.stop()
            if host_side == "win":
                host.wait(30)
            time.sleep(1)
            try:
                free = until(lambda: self.udp_port_free(host_side, port), 15, "port release")
            except Failure:
                free = False
            leftovers = self.stray(host_side) + (self.stray(client_side) if client_side != host_side else [])
            record("shutdown", "pass" if free and not leftovers else "fail",
                   "host stopped, UDP port released, no stray processes" if free and not leftovers
                   else f"port released={free}, stray: {leftovers}")
        except Failure as error:
            record("pairing_aborted", "fail", str(error)[:1500])
        finally:
            if host:
                host.stop()
            if fixture is not None or host_side == "mac":
                self.stop_fixture(host_side, fixture)

    def probe(self, host_side, client_side, tag, target, host, mark, record):
        config = f"probe-{tag}"
        cross = host_side != client_side
        if client_side == "mac":
            marker = self.scratch / f"markers-{tag}"
            marker.mkdir()
            report = str(self.scratch / f"probe-{tag}.json")
            exe = self.mac_examples / "e2e_probe"
        else:
            marker = f"{self.win_root}\\markers-{tag}"
            self.win.ssh(f'mkdir "{marker}"', check=False)
            report = f"{self.win_root}\\probe-{tag}.json"
            exe = f"{self.win.examples}\\e2e_probe.exe"
        argv = [target, report, "--audio", "--video-seconds", "10"]
        expect, send = f"frames-e2e-host-{uuid.uuid4().hex[:8]} ✓", f"frames-e2e-client-{uuid.uuid4().hex[:8]} ✓"
        original = None
        if cross and not self.args.no_clipboard:
            original = self.clipboard_get(host_side, f"save-{tag}")
            argv += ["--marker-dir", str(marker), "--clipboard-expect", expect, "--clipboard-send", send]
        started = time.monotonic()
        if self.pretrusted:
            self.pretrust(host_side, f"host-{tag}", self.host_id, client_side, config)
        probe = self.start(client_side, f"probe-{tag}", argv, config, exe=exe, timeout=240)
        self.connect(host, probe, mark)
        client_to_host = None
        try:
            if original is not None:
                until(lambda: self.marker(client_side, marker, "ready") or probe.exited(), 150, "probe clipboard stage")
                if not probe.exited():
                    self.clipboard_set(host_side, expect, f"expect-{tag}")
                    until(lambda: self.marker(client_side, marker, "sent") or probe.exited(), 40, "client clipboard text")
                    client_to_host = self.clipboard_wait(host_side, send, f"wait-{tag}") if not probe.exited() else False
            probe.wait(200)
        except Failure:
            probe.stop()
        finally:
            if original is not None:
                self.clipboard_set(host_side, original, f"restore-{tag}")
        self.record_identity(client_side, config)
        result = self.fetch_report(client_side, report)
        if not result:
            record("probe", "fail", "no probe report; log tail: " + probe.log()[-800:])
            return
        measurements = result.get("measurements", {})
        for name, check in result["checks"].items():
            record(name, check["status"], check["detail"], measurements=measurements if name == "video" else None)
        if original is None:
            record("clipboard_client_to_host", "skip",
                   "same machine shares one clipboard" if not cross else "disabled (--no-clipboard)")
        else:
            record("clipboard_client_to_host", "pass" if client_to_host else "fail",
                   "client text reached the host clipboard" if client_to_host else "host clipboard never matched")
        record("desktop_view", "skip", "full-desktop view would capture the user's whole screen; not requested",
               ms=int((time.monotonic() - started) * 1000))

    # ------------------------------------------------------------- cleanup
    def cleanup(self):
        for process in self.mac_processes:
            process.stop()
        for item in sorted(set(self.keychain_items)):
            subprocess.run(["security", "delete-generic-password", "-s", "app.frames.identity", "-a", item],
                           capture_output=True)
        if "win" in self.args.pairings:
            self.win.cleanup()
            self.win.ssh(f'rmdir /s /q "{self.win_home_files}"', check=False)
        for path in sorted(self.mac_files.rglob("*"), reverse=True):
            path.unlink() if path.is_file() else path.rmdir()
        self.mac_files.rmdir()

    def report(self):
        path = self.scratch / "results.json"
        leftover = []
        for config in self.app_identities:
            der = config / "identity.der"
            if der.exists():
                leftover.append(hashlib.sha256(der.read_bytes()).hexdigest())
        if leftover:
            print("Keychain test items created by the signed Mac host (remove from a Terminal):")
            for item in sorted(set(leftover)):
                print(f"  security delete-generic-password -s app.frames.identity -a {item}")
        path.write_text(json.dumps({"run": self.run_id, "results": self.results,
                                    "mac_host_app": self.args.mac_host_app,
                                    "keychain_items_to_remove": sorted(set(leftover))},
                                   indent=2, ensure_ascii=False))
        print(f"\nReport: {path}")
        failed = sum(1 for checks in self.results.values() for check in checks.values() if check["status"] == "fail")
        return failed


def default_mac_address():
    interface = re.search(r"interface: (\S+)", subprocess.run(["route", "-n", "get", "default"],
                                                              capture_output=True, text=True).stdout)
    return subprocess.run(["ipconfig", "getifaddr", interface[1]], capture_output=True, text=True).stdout.strip()


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--win-ssh", default="windows-pc")
    parser.add_argument("--win-user", default="monok")
    parser.add_argument("--win-address", required=True)
    parser.add_argument("--mac-address", help="defaults to the default-route interface address")
    parser.add_argument("--win-work", default=r"F:\claude-work\e2e")
    parser.add_argument("--win-target", default=r"F:\frames-ab\target", help="CARGO_TARGET_DIR of the Windows build")
    parser.add_argument("--artifacts", required=True, help="local folder for logs and reports (not the repo)")
    parser.add_argument("--pairings", default="mac-win,win-mac,mac-mac,win-win",
                        help="client-host pairs: mac-win = Mac client to Windows host")
    parser.add_argument("--no-clipboard", action="store_true", help="skip the clipboard checks")
    parser.add_argument("--mac-host-app", help="run the Mac host from this signed app bundle via LaunchServices "
                        "(for capture grants and Keychain access when this session has neither)")
    parser.add_argument("--base-port", type=int, default=48011)
    args = parser.parse_args()
    matrix = Matrix(args)
    try:
        matrix.prepare()
        for index, pair in enumerate(args.pairings.split(",")):
            client_side, host_side = pair.split("-")
            matrix.pairing(host_side, client_side, args.base_port + index)
    finally:
        matrix.cleanup()
        failed = matrix.report()
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
