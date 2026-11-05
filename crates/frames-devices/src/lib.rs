//! Device forwarding between a viewer and a host: gamepads read on the
//! viewer with SDL3 (in a helper process, `gamepad-worker`) become virtual
//! Xbox 360 controllers on a Windows host (ViGEmBus); USB devices go
//! between Windows computers over USB/IP (`usb`, `usbip`). [`protocol`] is
//! what the two ends exchange.

pub mod gamepad;
pub mod protocol;
pub mod usb;
pub mod usbip;
