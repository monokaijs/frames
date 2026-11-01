# Device forwarding

This release forwards standard gamepads from Windows or macOS clients to a
Windows host, and generic USB devices between Windows computers. Devices use the
existing paired, encrypted Frames connection. The audio/video processing path
is unchanged by device forwarding.

## Gamepads

Connect a controller to the client using USB or Bluetooth. Under **Devices →
Gamepads**, select it to forward or disconnect it. Up to four controllers are
represented as Xbox-compatible gamepads on the host. Buttons, sticks, triggers
and ordinary rumble are supported through SDL3 and ViGEmBus. Game-specific button
labels may therefore differ from the physical controller.

If needed, use **Set up gamepad driver on host**. Setup downloads the pinned
upstream installer, verifies its SHA256 and Windows signature, and requests
administrator authorization on the host. Existing working drivers are preserved.
ViGEmBus upstream is retired; it is an optional, documented compatibility dependency.

Controllers release when Frames loses focus or Ctrl+Alt+F12 releases input.
Disconnects remove virtual controllers; missing reports neutralize held controls
within 100 ms. Rumble effects expire locally if feedback stops. Hotplug does not
automatically reattach a controller.

## Wired USB (Windows to Windows)

1. On the client, use **Devices → USB devices → Set up local USB forwarding**.
2. Set up the USB receiving driver on the Windows host from the same menu.
3. Refresh devices, then select a device to forward it. Administrator authorization
   on the client claims that individual device for the active session.
4. Select it again to safely eject and disconnect it.

The client uses usbipd-win 5.3.0; the host uses usbip-win2 0.9.8.1. Installers are
pinned in `assets/device-packages.json`. No custom USB kernel driver is installed.
Drivers remain optional; Frames itself does not need to run as administrator.

Local exporter setup adds a program-specific Windows Firewall rule restricting
USB/IP access to loopback. Windows Firewall must remain enabled. Active external
USB/IP clients and devices already bound outside Frames are reported as conflicts.
The setup action changes access to the local USB/IP service; other firewall rules
are preserved. Windows may restart USB hubs while installing the receiving driver;
perform driver setup before starting device work.

Forwarded devices have exclusive remote ownership. Storage volumes are locked and
unmounted before capture; busy volumes are refused. Normal detach asks Windows for
safe eject and refuses a veto. Losing the connection is a surprise removal: pending
writes cannot be guaranteed, transactions are not replayed, and reattachment is
explicit. A physical device cannot use gamepad forwarding and raw USB at the same time.

Configurations containing isochronous endpoints (including USB audio, microphones
and webcams) are rejected before remote attachment. Use standard gamepad mode for
controllers with composite audio interfaces. Other device-specific driver and
firmware restrictions can still prevent forwarding.

The existing Android ADB bridge remains independently available on Windows and Mac.

## Validation and compatibility

Automated fixtures cover controller queue ordering/coalescing, short button taps,
expiry, bounded protocol messages, QUIC isolation from blocked control traffic,
USB import selection and USB configuration probing. They do not establish physical
device compatibility or input-to-photon performance.

Physical release checks remain required for DualSense and Xbox pads over USB and
Bluetooth, four simultaneous pads and rumble, plus USB storage, serial adapters,
phones and dongles. Validate installation/removal under Secure Boot and Memory
Integrity, storage integrity, focus loss, network outages and concurrent media.
No physical-device combinations are certified by the generated fixtures.

Mac receiving, Mac USB, advanced DualSense effects, arbitrary Bluetooth forwarding,
automatic reattachment and the 1440p/120 fps performance project are deferred.

CLI equivalents:

```powershell
Frames devices setup gamepad
Frames devices setup usb-export
Frames devices setup usb-host
```
