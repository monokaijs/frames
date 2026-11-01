"""Generate the bundled compatibility documentation from reviewed catalog evidence."""
import argparse
import json
from pathlib import Path
import sys

ROOT=Path(__file__).resolve().parent.parent
def render(catalog):
    lines=['# Shared game compatibility','',
        'Shared game mode is experimental. Any technically eligible game can be tried; this catalog is not an allowlist.',
        'Verification applies to the recorded executable build and Frames adapter/profile revision. Updates require retesting.',
        '',f'Adapter revision: `{catalog["adapter_revision"]}`. Upstream ProtoInput revision: `{catalog["upstream_revision"]}`.','',
        '| Status | Meaning |','| --- | --- |',
        '| Verified build | All acceptance checks passed for the recorded configuration. |',
        '| Partial | Specific working features and limitations were recorded. |',
        '| Known issues | Testing reproduced the documented failures. |',
        '| Untested | No applicable verification exists for this build and adapter revision. |','']
    if not catalog['records']:lines+=['No games have been verified yet. Attaching successfully does not grant verification.','']
    labels={'verified_build':'Verified build','partial':'Partial','known_issues':'Known issues'}
    for record in catalog['records']:
        lines += [f'## {record["title"]} — {record["build"]}','',
            f'Status: **{labels[record["status"]]}**. Edition/store: {record["edition"]} / {record["store"]}.',
            f'Tested {record["tested_at"]}, Frames {record["frames_version"]}, Windows {record["windows_version"]}, client {record["client_platform"]}.',
            f'Executable SHA-256: `{record["executable_sha256"]}`.',
            f'Adapter `{record["adapter_revision"]}`; profile `{record["profile"]["id"]}` revision {record["profile"]["revision"]}.',
            f'Report: {record["report"]}. Simultaneous-use test: {record["simultaneous_use_minutes"]} minutes.','',
            'Results: '+', '.join(f'{key}: {"pass" if value else "fail"}' for key,value in record['results'].items())+'.','']
        if record['settings']:lines+=['Required settings: '+ '; '.join(record['settings'])+'.','']
        if record['limitations']:lines+=['Limitations: '+ '; '.join(record['limitations'])+'.','']
    lines+=['## Maintaining records','',
        'Submit test evidence through a repository change. Maintainers review it before adding or updating a record; community reports are not automatically verified.',
        'Record the exact executable SHA-256, game edition/store/build, Frames version, adapter/profile revision, test date, Windows version, client platform, required settings, limitations, and report reference.',
        'Verification requires keyboard, relative and absolute mouse, background operation, window capture, process-only audio, disconnect recovery, and the 30-minute simultaneous-use check.',
        'Keep historical records when builds change. Only reviewed profiles matching the executable and adapter revision can be selected automatically.',
        'Use [the game test report template](GAME_TEST_REPORT_TEMPLATE.md) to record the acceptance evidence.',
        '', 'Regenerate this file with `python scripts/game_compatibility.py`; CI checks it with `--check`.','']
    return '\n'.join(lines)
def main():
    parser=argparse.ArgumentParser();parser.add_argument('--check',action='store_true');args=parser.parse_args()
    catalog=json.loads((ROOT/'assets/game-compatibility.json').read_text(encoding='utf-8'))
    text=render(catalog);destination=ROOT/'docs/GAME_COMPATIBILITY.md'
    if args.check:
        if not destination.exists() or destination.read_text(encoding='utf-8')!=text:
            sys.exit('Game compatibility documentation is stale; run python scripts/game_compatibility.py')
    else:destination.write_text(text,encoding='utf-8')
if __name__=='__main__':main()
