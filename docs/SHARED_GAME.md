# One-window sharing and input compatibility

Sharing uses one **Start sharing** control and a scope dropdown. **Everything** is the default and preserves normal full sharing. **Selected windows** opens a preview grid with multiple selection; **One window** opens the same grid with single selection. Search by application or title, select the previews, and press **Share**. Refresh reloads the list; Cancel starts no host. Previews stay local and are not saved.

On Windows, One window uses the experimental isolated-input adapter for an already running, eligible x64 application or game. The host keeps using other applications while the viewer controls the selected window. Windows and macOS viewers use physical keyboard positions in this mode. The viewer title shows attachment/control state and compatibility; its compatibility command shows the exact tested configuration when one exists. Selected windows retains normal remote input control, so it uses the host's mouse and keyboard. On macOS hosts, scoped capture uses normal input control; isolated input hosting remains Windows-only.

```sh
frames host --one-window --shared-window HWND --pair
frames connect HOST:47990 --audio
frames games
```

Use the launcher for the preview-grid picker. The CLI can select a known local HWND for automation; the legacy `--shared-game` flag remains an alias for `--one-window`. The host always owns selection. Picker identities are bound to the chosen window's lifetime rather than its title or a reusable remote ID. Closing or replacing it never selects another window. Start sharing again to choose a new source.

Selected windows excludes desktop capture and rejects capture/input/window operations against unselected identities. Clipboard, files, and device forwarding are disabled in scoped sharing. Audio is isolated to the selected process tree when all selected windows belong to one Windows process; other combinations report unavailable audio rather than capture the desktop mix.

Click or focus the viewer to resume control. **Ctrl+Alt+F11** toggles relative aiming on Windows; **Control+Option+F11** does so on macOS. **Ctrl+Alt+F12** / **Control+Option+F12** releases capture and held input. Losing viewer focus also releases input. Closing the viewer disconnects. Resizing the viewer scales its picture locally. Minimizing the host game pauses video, process audio, and control; restore it and explicitly resume from the viewer.

## Eligibility and limits

The host backend requires Windows x64 MSVC builds, a game running as the same user, in the same Windows session, and at the same integrity level. It uses ordinary process access and runtime DLL attachment. There is no elevation, driver, protected-process access, or startup injection. Process-only audio requires Windows build 20348 or later; failures are reported in the viewer without falling back to desktop audio.

The generic adapter covers User32 keyboard polling, keyboard/mouse window messages, and keyboard/mouse Raw Input. DirectInput device polling, XInput/controllers, custom input APIs, games that reject runtime attachment, and games requiring startup injection are outside this adapter's v1 coverage. A technically successful attach does not establish that a game's input implementation works. Use windowed or borderless mode; exclusive fullscreen, custom cursors, engine-specific focus handling, and replacement render windows need game-specific validation.

The hooks operate inside the selected game process. This is input routing, not a security sandbox for arbitrary game code or other processes the game starts. The remote peer receives only the selected capture surface and process-tree audio. The remote peer cannot request desktop capture, clipboard or file transfer, device forwarding, or remote focus/resize/close operations. The host enforces these restrictions independently of the viewer UI and across transport lanes.

## Adapter and lifecycle

The vendored source is pinned to SplitScreen-Me ProtoInput commit `e18e06bb345b86540e7464f7355c649bfdd17f38`; its source manifest is [SOURCE.json](../vendor/frames-protoinput/SOURCE.json). Frames maintains the headless adaptation in [native/windows/shared-input](../native/windows/shared-input). It builds the fork's local EasyHook core and Frames' remote-source hooks, rather than the upstream GUI, physical-device selection, locking, Explorer suspension, overlays, controllers, or kernel components.

`FramesInput.dll` contains the x64 adapter and local runtime-injection helper. MSVC builds embed that exact DLL into Frames and extract it into a hash-addressed private installation directory. The executable also embeds the dependency notices and compatibility catalog. No separate DLL download or version selection happens at runtime. Rebuild Frames after changing native sources; a standalone adapter rebuild does not replace the DLL embedded in an existing executable.

After peer authentication, the host validates the selected process and executable fingerprint, creates an owner-only local named pipe, and attaches the DLL. Random pipe names, a 256-bit nonce, local-only pipe access, and mutual process-ID checks bind it to the broker. All required hooks must install successfully before the broker reports readiness. Failed installation rolls back the hooks and accepts no remote input.

The DLL keeps private async and message-queue keyboard state, cursor/focus/capture state, raw registrations, and bounded raw/legacy event records. It unregisters the process's physical keyboard/mouse raw registrations while attached, tracks the game's intended registrations, and restores those on cleanup. Query hooks access local memory; they do not perform IPC, logging, or heap allocation. Input arrives through fixed-size IPC commands. A separate bounded status writer prevents cursor/status backpressure from blocking input or the lease watchdog. Byte-pipe transfers assemble complete records across partial reads; a partial-record timeout terminates the connection instead of reusing misaligned bytes.

Network input, resume, and heartbeat messages carry a fresh session generation. Consecutive relative movement sums only within the same target/generation; keys and buttons remain ordering barriers. Native heartbeat cadence is 100 ms and the lease is 500 ms. Viewer UI liveness is required to keep the network heartbeat flowing. Lease expiry, minimization, and overflow neutralize held input and pause control. A heartbeat alone cannot resume it. Delayed transitions expire instead of replaying after a stall; ordered resume requests expire after 300 ms, including time spent in the native input queue.

Disconnect, broker death, window destruction, and normal stop restore intended raw registrations. Queued owned releases remain readable until the game's individual or buffered input loop consumes them, including after a stalled message pump resumes. Pending owned message markers are drained without replacing restored physical input. The DLL remains resident and its hooks pass through after cleanup. Reattachment uses a new generation. Neither native delivery nor a game profile delegates to Frames' normal global input backend.

## Compatibility evidence

[assets/game-compatibility.json](../assets/game-compatibility.json) is the source of truth; [GAME_COMPATIBILITY.md](GAME_COMPATIBILITY.md) is generated from it. The initial catalog has no game records. Unlisted games remain available as **Untested**. **Known issues** records describe failures and do not block an attempt.

Matching requires an executable SHA-256, the current adapter revision, and the reviewed profile revision. Filenames never grant verification. Changed builds or adapter/profile revisions retain historical evidence but show Untested until revalidated. Each report records game edition/store/build, Frames version, Windows version, viewer platform, date, settings, limitations, all acceptance results, and simultaneous-use duration. A Verified build record requires all feature results and at least 30 minutes of simultaneous use. Maintainers review repository changes; submitting a community report cannot automatically verify a build.

Only `generic` revision 1 is enabled initially. Future declarative profiles require a reviewed implementation, explicit revision, exact executable matching, and tests showing why they differ. Profiles cannot relax host restrictions or enable global input takeover. Increment `ADAPTER_REVISION` and the catalog together when changing adapter behavior that invalidates verification.

## Validation and release gate

Prepare the media SDK before Cargo commands. The native fixtures create their own windows/processes and never inject global host input:

```sh
python scripts/build_shared_input.py
python scripts/test_shared_input.py
python scripts/test_shared_input.py --buffered
python scripts/test_shared_input.py --hidden-raw
python scripts/test_shared_input.py --buffered --hidden-raw
python scripts/test_shared_lifecycle.py
target/shared-input/native/Release/shared_ipc_fixture.exe
python scripts/test_process_audio.py
cargo build -p frames --bin frames --example shared_probe
python scripts/test_shared_session.py
python scripts/test_shared_session.py --network-loss
python scripts/test_shared_session.py --picker
# Interactive Windows fixture for local selection identity and capture guards:
cargo test -p frames-native window_selection::tests:: -- --include-ignored --nocapture
python scripts/game_compatibility.py --check
cargo test --workspace --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
```

For a 30-minute 1,000 Hz run, add `--seconds 1800` to each input-fixture mode. Save the JSON output together with the adapter DLL hash, Windows build, Frames/profile revision, and workload description. The output separates broker-submit percentiles from first raw-read delivery latency, target CPU/memory, and fixture paint intervals. Paint intervals are a generated GDI fixture measurement, not a game's frame rate or network input-to-photon latency.

The QUIC fixture uses isolated temporary identities and the real production session path. It checks single-window policy/capture, private input, viewer UI deadman behavior, disconnection, and registration recovery. `--network-loss` drops traffic in both directions for 900 ms while the viewer remains responsive. The input fixture checks polling state and independently tracks the keys/buttons received through legacy and Raw Input APIs, including repeated release during overflow. The lifecycle fixture kills only its own broker and exercises resident reattachment, minimization, explicit resume, and window destruction. The audio fixture plays quiet generated tones in a selected process, its descendant, and an unrelated process to verify inclusion and exclusion. Native/Mac viewer fixtures cover policy and cursor behavior; macOS execution requires the Mac CI runner.

These automated checks do not replace the release acceptance matrix: simultaneous physical host typing/clicking with remote gameplay, covered-window capture/audio isolation, network loss and delayed events, 30-minute simultaneous use, normal-sharing regressions, and Windows/macOS viewer checks on the final build. No real game should be called verified until its complete report passes review. Current measured results and remaining gates belong in [SHARED_GAME_VALIDATION.md](SHARED_GAME_VALIDATION.md).
