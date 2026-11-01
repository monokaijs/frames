"""Build the pinned, user-space shared input adapter. No SDK/network downloads."""
import argparse
import os
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parent.parent

def build(output: Path):
    output = output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    command = ["cmake", "-S", str(ROOT / "native/windows/shared-input"), "-B", str(output), "-A", "x64"]
    # CMake's VS discovery also works on CI. This machine has a registration-less BuildTools install.
    local = Path(r"C:\Program Files (x86)\Microsoft Visual Studio\2019\BuildTools")
    if local.is_dir() and not os.environ.get("CI"):
        command += ["-G", "Visual Studio 16 2019", "-DCMAKE_GENERATOR_INSTANCE=" + str(local) + ",version=16.11.34729.46"]
    subprocess.run(command, check=True)
    subprocess.run(["cmake", "--build", str(output), "--config", "Release", "--parallel"], check=True)

if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--output", type=Path, default=ROOT / "target/shared-input/native")
    build(parser.parse_args().output)
