"""Build the pinned, user-space shared input adapter. No SDK/network downloads."""
import argparse
import json
import os
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parent.parent

GENERATORS = {"16": "Visual Studio 16 2019", "17": "Visual Studio 17 2022"}


def generator_args():
    vswhere = Path(os.environ.get("ProgramFiles(x86)", r"C:\Program Files (x86)")) / "Microsoft Visual Studio/Installer/vswhere.exe"
    if vswhere.is_file():
        found = subprocess.run(
            [str(vswhere), "-products", "*", "-latest", "-requires",
             "Microsoft.VisualStudio.Component.VC.Tools.x86.x64", "-format", "json"],
            capture_output=True, text=True)
        instances = json.loads(found.stdout or "[]") if found.returncode == 0 else []
        for instance in instances:
            version = instance.get("installationVersion", "")
            generator = GENERATORS.get(version.split(".")[0])
            if generator:
                return ["-G", generator, f"-DCMAKE_GENERATOR_INSTANCE={instance['installationPath']},version={version}"]
    # A BuildTools copy the installer does not know about.
    local = Path(r"C:\Program Files (x86)\Microsoft Visual Studio\2019\BuildTools")
    if local.is_dir():
        return ["-G", "Visual Studio 16 2019", "-DCMAKE_GENERATOR_INSTANCE=" + str(local) + ",version=16.11.34729.46"]
    return []


def build(output: Path):
    output = output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    command = ["cmake", "-S", str(ROOT / "native/windows/shared-input"), "-B", str(output), "-A", "x64"]
    # CMake's VS discovery works on CI. Elsewhere, pick the instance that has
    # the C++ x64 tools (CMake would take the newest Visual Studio even
    # without them), or a registration-less BuildTools install.
    if not os.environ.get("CI"):
        command += generator_args()
    subprocess.run(command, check=True)
    subprocess.run(["cmake", "--build", str(output), "--config", "Release", "--parallel"], check=True)

if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--output", type=Path, default=ROOT / "target/shared-input/native")
    build(parser.parse_args().output)
