# Game compatibility test report

Copy this template into a report under `docs/game-tests/`. Submit the report and catalog change together for maintainer review. Do not add a Verified build record on attachment success alone.

## Exact configuration

| Field | Value |
| --- | --- |
| Game title | |
| Edition and store | |
| Game build | |
| Selected executable path and SHA-256 | |
| Frames version / source revision | |
| Adapter revision | |
| Profile ID and revision | |
| Test date, YYYY-MM-DD | |
| Windows edition and build | |
| Viewer OS and Frames version | |
| CPU, GPU, display scale, game resolution | |
| Windowed / borderless settings | |
| Network and added impairment | |
| Other required settings | |

## Acceptance results

For every check, record pass/fail, observations, and reproducible steps. Use **not tested** explicitly in the report when applicable; incomplete testing cannot grant Verified build status.

- Attach to an already running game with its existing input registrations. Reject unsupported integrity/architecture/protected-process configurations without accepting input.
- Keyboard polling, ordinary keys, modifiers, short taps, toggles, and held-key releases.
- Absolute pointer, relative aiming, every supported button, and both wheel axes. Include transitions interleaved with 1,000 Hz movement.
- Host types and clicks in a separate application while the viewer plays. Confirm no cross-input, host cursor movement, clipping, or stolen focus.
- Selected-window capture remains correct while covered. No desktop or covering-window content is included. Minimization pauses control and releases held input; restore requires explicit resume.
- Selected-process audio includes descendants and excludes an unrelated audio source. Record unavailable audio explicitly if process loopback fails.
- Focus loss and the release shortcut clear held input and relative capture. Viewer resizing stays local; viewer close disconnects without closing the game.
- Attempt desktop, clipboard, files, device forwarding, focus, resize, and close through a client that bypasses UI restrictions. Confirm the host refuses them.
- Queue overflow, delayed events, a stalled viewer UI, network loss, broker termination, normal stop, repeated attachment, stale generations, and target-window destruction. Confirm releases, registration recovery, and no automatic retargeting.
- At least 30 minutes of simultaneous physical host use and remote gameplay. Record duration, pauses, workload, failures, and memory growth.
- Measure median/p95/p99 input latency, CPU, memory, and game frame times with 1,000 Hz input. State the timing boundaries, sampling method, measurement error, and whether compilation or other work ran concurrently.
- Normal-sharing regression checks on the same Frames build.

## Outcome and catalog change

Proposed status: Verified build / Partial / Known issues.

List required settings, working features, failures, workarounds, and unsupported behavior. Link supporting logs and measurements without including private desktop captures or credentials. Include the reviewed profile change only when testing demonstrates a necessary difference; it must not enable host input takeover.

Populate every catalog field from the tested configuration. Mark all feature booleans true only when established by the report. Verified build requires every acceptance result and a simultaneous-use duration of at least 30 minutes. Preserve earlier build/revision records. Run:

```sh
python scripts/game_compatibility.py
python scripts/game_compatibility.py --check
cargo test -p frames-core shared_game
```
