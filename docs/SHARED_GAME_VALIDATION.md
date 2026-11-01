# Shared game implementation validation

Test environment: 2026-11-01, Windows 11 Pro build 26200, AMD Ryzen 7 7800X3D, x64 MSVC 14.29. Adapter revision `frames-protoinput-1`, generic profile revision 1. The catalog contains no game verification records.

Hosted Windows CI exercises production session policy, private input, identity, disconnection, and network leases using `scripts/test_shared_session.py --input-only`. Its result explicitly reports `capture_required: false`; it is not capture validation. The hosted runner rejects the fixture's Windows Graphics Capture item with `0x80070057` and has no playback device. Full capture remains the script's default and must pass on an interactive Windows desktop, together with `selected_preview_captures_its_owned_window` and `captures_the_window_while_another_window_covers_it`. The interactive Windows evidence below covers real capture; hosted CI does not replace that release acceptance work.

The 2026-11-01 release follow-up reran all four `window_selection::tests::` cases, the covered-window capture case, and both `test_shared_session.py --picker` and `--picker --network-loss` on the interactive Windows environment above. They passed; both production-session results reported `capture_required: true` and `capture_observed: true`. The separate `--input-only --network-loss` run passed with both capture fields false, checking the narrower hosted-CI coverage. The covered-window fixture now measures its expected dimensions in physical pixels so the assertion remains valid at 150% display scaling.

## Functional checks

- Fragmented byte-pipe records, idle timeouts, and partial-record rejection. Deliberately blocked status readers preserve the attachment and do not block lease expiry.
- Runtime attachment to generated running x64 fixtures: keyboard polling, legacy keyboard/mouse messages, individual and buffered-only Raw Input loops, repeated raw reads, device queries, and pre-existing registrations. Separate hidden/message-only Raw Input sinks are covered. Detachment while the message pump is stalled must release engine-held state and drain owned records/messages before returning to pass-through input.
- Private cursor, clipping, keyboard state, focus, and nonactivating game window operations. No global input synthesis is used by the adapter or test scripts.
- Lease expiry, suspension of all owned fixture threads beyond the lease with a queued heartbeat, bounded-queue overflow with a stalled message pump, independent legacy/Raw Input held-state neutralization, repeated release, normal detach, registration restoration, resident reattachment with another generation, broker process termination, minimization, explicit resume, and window destruction. Expiry is checked before applying a queued heartbeat after process scheduling resumes.
- Real production QUIC host/client: selected-window capture, compatibility metadata, input, UI-liveness expiry, a 900 ms bidirectional network outage, stale resume/key rejection, pause notifications, disconnect, and recovery. A separate protocol probe bypasses viewer guards and verifies host rejection of incompatible capabilities, desktop access, remote close, file download, device forwarding, ordinary input, and a stale generation.
- Direct Rust host lifecycle fixture: prepared/attached states, minimization, restore without automatic resume, stale-generation rejection, old-release isolation, reconnect, and selected-window destruction.
- Native covered-window capture fixture: the source window's client pixels were captured while a different colored window covered it; covering-window and non-client pixels were excluded.
- Windows virtual-cursor fixture: actual layered-child creation, rendering, sizing, image retention across deltas, hiding, and no activation. The same Windows capability manifest is used by Frames, native tests, and examples.
- Catalog tests: exact executable hash and adapter/profile revision matching, historical records, changed builds, incomplete verification evidence, insufficient soak duration, duplicate configurations, cursor bounds, and policy admission.
- Bundled data and generated compatibility documentation agree. Packaging regression checks pass; dependency notices are embedded with Frames.

## Audio isolation

The WASAPI test used generated 880 Hz audio in the selected process and 440 Hz in an unrelated process. It was repeated with the 880 Hz source running in a descendant of the selected process. The final runs captured 57,600 and 57,120 stereo frames respectively at 48 kHz. Selected-tone amplitude was approximately 0.01944; unrelated-tone amplitude was below 0.000011. Both direct-process and descendant inclusion passed. This establishes isolation for these generated sources on this Windows build.

## Sustained input measurements

Both final hidden-sink runs completed 1,800 seconds at 1,000 relative mouse events per second, with 1,800,000 workload events each and no event loss or fixture errors. Polling, legacy, and Raw Input held states were neutral at cleanup; registrations were restored. The harness then completed overflow, resident reattachment, and stalled-detach release checks. Exact results, artifact hashes, source hashes, workload details, and earlier inconclusive attempts are in [the JSON report](validation/shared-game-windows-2026-11-01.json).

| Measurement | Individual reads | Buffered-only reads |
| --- | ---: | ---: |
| Broker submission median / p95 / p99, microseconds | 17.4 / 24.3 / 34.8 | 17.4 / 24.6 / 35.1 |
| First raw-read delivery median / p95 / p99 upper bounds, microseconds | 127 / 255 / 511 | 127 / 255 / 1,023 |
| Maximum first raw-read delivery, microseconds | 12,506 | 23,270 |
| Target CPU, percent of one logical core | 7.01 | 12.13 |
| Target working set, bytes | 13,774,848 | 13,721,600 |
| Working-set growth during workload, bytes | 176,128 | 159,744 |
| GDI paint interval mean / maximum during workload, milliseconds | 25.031 / 55.878 | 25.039 / 61.468 |
| Maximum broker heartbeat gap, milliseconds | 112.46 | 114.41 |

The JSON separates benchmark broker submission, including Python marshalling/locking, from first raw-read delivery latency. Delivery percentiles are upper bounds of logarithmic microsecond bins. CPU includes the fixture and injected DLL. Two fixtures ran simultaneously, with developer builds, UI rendering, and regression checks at multiple points; this was not an isolated benchmark machine. GDI paint intervals are not game frame times or network input-to-photon latency.

Two earlier attempts with heartbeats sent by the motion loop paused control after several minutes (phase 2, error 0). They did not record heartbeat-gap diagnostics and are not counted as successful soaks; their cause was not established. Final measurements use an independent 100 ms native-broker heartbeat thread and record its maximum gap. Production viewer UI-liveness behavior is checked separately through QUIC. Full production-host liveness and latency under simultaneous network/media/1,000 Hz input remain part of release acceptance.

## Sharing scope and picker checks

The launcher has one sharing action with Everything as its default, plus Selected windows and One window. The picker supports bounded preview capture workers, search, single/multiple selection, cancellation, and rejection of stale preview results. Its layout was rendered and inspected at 960 by 620 and 820 by 540.

Owned-window tests verify that local handles are distinct from remote window IDs, selected identities cannot be forged or reused after closure, unselected capture/input/window commands are rejected, desktop capture is unavailable, and two unrelated selected processes cannot fall back to desktop audio. The picker-ticket path attaches the private adapter and passes both UI-stall and 900 ms network-loss QUIC checks in debug and optimized Windows builds. The full workspace suite passed 120 tests with 10 environment-dependent tests ignored; the relevant interactive selection, lifetime, and covered-capture fixtures were also run explicitly. Clippy, formatting, catalog generation, and packaging checks passed.

## Remaining release acceptance

These results establish fixture behavior, not generic game compatibility. The following remain required before declaring a game/build verified or treating the feature as generally validated:

- A real-game Windows/macOS viewer matrix, including relative capture, keyboard routing, virtual cursor placement at different scales, and reconnect behavior.
- A documented 30-minute simultaneous-use session with physical host typing/clicking and remote gameplay. Automated remote-input fixture soaks do not establish this human-use result.
- Game frame-time and network input-latency percentiles on the final platform configurations, including controlled loss/delay and normal-sharing regressions.
- macOS compilation and execution of the new viewer fixture on CI; this Windows workspace cannot run Swift/AppKit tests.

No game was added to the verified catalog by these implementation tests. Use [the report template](GAME_TEST_REPORT_TEMPLATE.md) for reviewed game evidence and [the mode documentation](SHARED_GAME.md) for reproduction commands and supported API boundaries.
