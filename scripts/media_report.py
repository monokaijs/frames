"""Summarize bounded production media diagnostics without retaining pixels."""
import argparse
from datetime import datetime
import json
from pathlib import Path
import re

ANSI = re.compile(r"\x1b\[[0-9;]*m")


def summarize(paths):
    streams = {}
    for path in paths:
        for raw in path.read_text(encoding="utf-8", errors="replace").splitlines():
            line = ANSI.sub("", raw)
            if "media delivery" not in line and "video encoder timing" not in line:
                continue
            window = re.search(r"window=WindowId \{ id: (\d+), generation: (\d+) \}", line)
            fields = {key: int(value) for key, value in re.findall(r"(\w+)=(\d+)", line)}
            if not window or "generation" not in fields:
                continue
            key = ":".join((*window.groups(), str(fields["generation"])))
            timestamp = line.split()[0]
            stream = streams.setdefault(key, dict(first=timestamp, last=timestamp, samples=0))
            stream.update(fields)
            stream["last"] = timestamp
            stream["samples"] += 1
    for stream in streams.values():
        elapsed = (datetime.fromisoformat(stream["last"]) - datetime.fromisoformat(stream["first"])).total_seconds()
        stream["diagnostic_seconds"] = elapsed
        if "presented" in stream:
            stream["intervals_over_50ms_percent"] = 100 * stream.get("gaps", 0) / max(1, stream["presented"] - 1)
    return dict(scope="Production diagnostics, startup included. Capture-to-native-presentation acknowledgement is not input-to-photon. Clock uncertainty is reported separately.",
                logs=[str(path) for path in paths], streams=streams,
                acceptance_gates_established=False)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("logs", nargs="+", type=Path)
    parser.add_argument("--output", type=Path)
    options = parser.parse_args()
    output = json.dumps(summarize(options.logs), indent=2)
    if options.output:
        options.output.parent.mkdir(parents=True, exist_ok=True)
        options.output.write_text(output, encoding="utf-8")
    print(output)


if __name__ == "__main__":
    main()
