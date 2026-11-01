# Shared game compatibility

Shared game mode is experimental. Any technically eligible game can be tried; this catalog is not an allowlist.
Verification applies to the recorded executable build and Frames adapter/profile revision. Updates require retesting.

Adapter revision: `frames-protoinput-1`. Upstream ProtoInput revision: `e18e06bb345b86540e7464f7355c649bfdd17f38`.

| Status | Meaning |
| --- | --- |
| Verified build | All acceptance checks passed for the recorded configuration. |
| Partial | Specific working features and limitations were recorded. |
| Known issues | Testing reproduced the documented failures. |
| Untested | No applicable verification exists for this build and adapter revision. |

No games have been verified yet. Attaching successfully does not grant verification.

## Maintaining records

Submit test evidence through a repository change. Maintainers review it before adding or updating a record; community reports are not automatically verified.
Record the exact executable SHA-256, game edition/store/build, Frames version, adapter/profile revision, test date, Windows version, client platform, required settings, limitations, and report reference.
Verification requires keyboard, relative and absolute mouse, background operation, window capture, process-only audio, disconnect recovery, and the 30-minute simultaneous-use check.
Keep historical records when builds change. Only reviewed profiles matching the executable and adapter revision can be selected automatically.
Use [the game test report template](GAME_TEST_REPORT_TEMPLATE.md) to record the acceptance evidence.

Regenerate this file with `python scripts/game_compatibility.py`; CI checks it with `--check`.
