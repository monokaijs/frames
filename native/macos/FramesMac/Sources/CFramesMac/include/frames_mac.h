// Frames v2 macOS platform ABI, implemented in Swift (native/macos/FramesMac)
// and called from Rust (crates/frames-platform-macos/src/ffi.rs, which must
// mirror this file; a Rust test checks FRAMES_MAC_ABI_VERSION at run time).
//
// Ownership: a `void *pixel_buffer` documented as +1 is a retained
// CVPixelBufferRef the receiver must CFRelease exactly once, on any thread.
// "Borrowed" pixel buffers stay owned by the caller for the call's duration.
// Times are CLOCK_UPTIME_RAW nanoseconds (mach_absolute_time scaled to ns).
#pragma once
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#define FRAMES_MAC_ABI_VERSION 7u

typedef struct FramesMacCallbacks {
    void *context;
    // Capture: a +1 BGRA IOSurface-backed CVPixelBufferRef.
    void (*on_frame)(void *context, void *pixel_buffer, uint32_t width, uint32_t height,
                     uint64_t captured_ns, uint64_t delivered_ns);
    // Encoder output: an Annex B access unit (parameter sets precede
    // keyframes). `status` != 0 means the encoder failed or dropped the
    // picture, and `data` is NULL.
    void (*on_encoded)(void *context, const uint8_t *data, size_t length, uint32_t flags,
                       int64_t ltr_token, uint64_t submitted_ns, uint64_t completed_ns,
                       int32_t status);
    // Cursor position in captured pixels, sent when it changes.
    void (*on_cursor)(void *context, double x, double y, bool visible);
    void (*on_event)(void *context, uint32_t kind, const uint8_t *data, size_t length);
} FramesMacCallbacks;

enum {
    FRAMES_MAC_EVENT_LOG = 1,             // UTF-8 text
    FRAMES_MAC_EVENT_CAPTURE_FAILED = 2,  // UTF-8 reason
    FRAMES_MAC_EVENT_CAPTURE_STARTED = 3, // UTF-8 "real WxH" or "synthetic WxH"
    FRAMES_MAC_EVENT_GEOMETRY = 4,        // window capture (ABI 5): UTF-8 "x y w h", the captured
                                          // rect in global points, top-left origin
};

enum {
    FRAMES_MAC_CODEC_H264 = 0,
    FRAMES_MAC_CODEC_HEVC = 1,
};

// Encoder creation flags.
enum {
    FRAMES_MAC_ENCODER_LTR = 1u << 0,
};

// Per-frame encode options.
enum {
    FRAMES_MAC_ENCODE_FORCE_KEYFRAME = 1u << 0,
    FRAMES_MAC_ENCODE_FORCE_LTR_REFRESH = 1u << 1,
};

// on_encoded flags.
enum {
    FRAMES_MAC_ENCODED_KEYFRAME = 1u << 0,
};

// Decoder results.
enum {
    FRAMES_MAC_DECODE_OK = 0,
    FRAMES_MAC_DECODE_NEED_KEYFRAME = 1,
    FRAMES_MAC_DECODE_TIMEOUT = -1,
    FRAMES_MAC_DECODE_INVALID = -2,
    FRAMES_MAC_DECODE_FAILED = -3,
};

uint32_t frames_mac_abi_version(void);
bool frames_mac_screen_capture_preflight(void);

// Captures the main display at `fps` as BGRA, with the cursor excluded from
// the pixels and reported through on_cursor. `synthetic` (or a missing
// Screen Recording permission) uses a generated 1920x1080 source instead.
void *frames_mac_capture_start(uint32_t fps, bool synthetic, FramesMacCallbacks callbacks);
void frames_mac_capture_stop(void *capture);
// Captures one window (ABI 4) by CGWindowID, cursor excluded; no generated
// fallback: a window that cannot be captured reports CAPTURE_FAILED.
void *frames_mac_capture_start_window(uint32_t window_id, uint32_t fps, FramesMacCallbacks callbacks);
// Other processes' on-screen application windows (ABI 4) as a UTF-8 JSON
// array of {"id": CGWindowID, "title": string, "x", "y", "width", "height"}
// (global points, top-left origin), front to back. Writes it to
// `buffer` when it fits in `capacity` bytes; returns its length either way.
size_t frames_mac_windows(uint8_t *buffer, size_t capacity);
// Window capture options (ABI 5): `show_header` keeps the window's standard
// title bar in the picture (otherwise it is cropped when Accessibility
// verifies one); `show_cursor` draws the cursor into the pixels. Display
// captures only take `show_cursor`.
void frames_mac_capture_set_options(void *capture, bool show_header, bool show_cursor);
// Resizes the captured window so its picture is `width` x `height` pixels
// (ABI 5), through Accessibility. False when it cannot be resized.
bool frames_mac_capture_resize_window(void *capture, uint32_t width, uint32_t height);

// A deterministic generated BGRA picture for `index` (+1). Two calls with the
// same index produce identical pixels.
void *frames_mac_synthetic_frame(uint32_t width, uint32_t height, uint64_t index);
// Mean absolute difference per B, G, R channel (0-255) of two same-sized
// BGRA buffers (borrowed); negative if they cannot be compared.
double frames_mac_pixel_buffer_difference(void *a, void *b);

// Hardware encoder: real time, no frame reordering, keyframes only when
// forced. Output is delivered through on_encoded.
void *frames_mac_encoder_new(uint32_t width, uint32_t height, uint32_t fps, uint32_t bitrate,
                             uint32_t codec, uint32_t flags, FramesMacCallbacks callbacks);
// Submits a borrowed pixel buffer. `acknowledged` lists LTR tokens the
// receiver holds (may be NULL when `acknowledged_count` is 0).
int32_t frames_mac_encoder_encode(void *encoder, void *pixel_buffer, uint64_t pts_ns,
                                  uint32_t options, const int64_t *acknowledged,
                                  size_t acknowledged_count);
int32_t frames_mac_encoder_set_bitrate(void *encoder, uint32_t bitrate, uint32_t fps);
void frames_mac_encoder_free(void *encoder);

// Hardware decoder. Decodes one Annex B access unit synchronously and, on
// FRAMES_MAC_DECODE_OK, stores a +1 BGRA IOSurface pixel buffer in *output.
void *frames_mac_decoder_new(uint32_t codec);
int32_t frames_mac_decoder_decode(void *decoder, const uint8_t *data, size_t length,
                                  bool keyframe, void **output);
void frames_mac_decoder_free(void *decoder);

// Metal presenter. With `window`, it creates an NSWindow on the main thread,
// which must be running NSApplication; without, it renders offscreen.
// `present` draws a borrowed BGRA pixel buffer on the calling thread; with
// `vsync` false it presents immediately (displaySyncEnabled = false).
void *frames_mac_presenter_new(uint32_t width, uint32_t height, bool window);
// A host menu, tooltip, popover or sheet (ABI 5): a borderless window that
// never takes focus, shown once attached.
void *frames_mac_presenter_new_popup(uint32_t width, uint32_t height);
enum {
    FRAMES_MAC_POPUP_MENU = 1,
    FRAMES_MAC_POPUP_TOOLTIP = 2,
    FRAMES_MAC_POPUP_POPOVER = 3,
    FRAMES_MAC_POPUP_SHEET = 4,
};
// Shows a popup presenter over the picture of the viewer window whose menu
// id is `parent_id` (ABI 5): `x`, `y`, `width`, `height` are its frame
// relative to its parent's frame, and `parent_width`, `parent_height` the
// parent's frame size, in host points. Waits a few seconds for the parent's
// window to open.
void frames_mac_presenter_attach(void *presenter, uint64_t parent_id, double x, double y, double width,
                                 double height, double parent_width, double parent_height, uint32_t role);
int32_t frames_mac_presenter_present(void *presenter, void *pixel_buffer, bool vsync,
                                     uint64_t *presented_ns);
// Host time (ns) the most recently presented drawable reached the glass, or 0
// if not known yet (windowed presenters only). Reported asynchronously, so it
// usually describes the frame before the one just presented.
uint64_t frames_mac_presenter_last_on_glass_ns(void *presenter);

// Viewer input from a windowed presenter's view (ABI 2). Positions are
// normalized to 0-65535 over the view, origin top left. `event_ns` is the
// event's own timestamp (CLOCK_UPTIME_RAW ns).
enum {
    FRAMES_MAC_INPUT_MOVE = 1,       // a = x, b = y
    FRAMES_MAC_INPUT_BUTTON = 2,     // a = button (0 left, 1 right, 2 middle, 3 back, 4 forward), b = down, c = x, d = y
    FRAMES_MAC_INPUT_WHEEL = 3,      // a = dx, b = dy, in 1/120 notches, positive = right / up
    FRAMES_MAC_INPUT_KEY = 4,        // a = macOS virtual key code, b = down
    FRAMES_MAC_INPUT_FOCUS_LOST = 5, // the window stopped being key
    FRAMES_MAC_INPUT_MOTION = 6,     // relative mouse (ABI 3): a = dx, b = dy in points, positive = right / down
    FRAMES_MAC_INPUT_MODE = 7,       // relative mouse turned on (a = 1) or off (a = 0) (ABI 3)
    FRAMES_MAC_INPUT_STATS = 8,      // Control+Option+F10: show or hide live statistics (ABI 4)
    FRAMES_MAC_INPUT_MENU = 10,      // an item of the host application's menus, a = id (ABI 4)
    FRAMES_MAC_INPUT_COMMAND = 9,    // a Frames menu command, a = 1-3 quality high/medium/low, 4 statistics, 6 disconnect (ABI 4)
    FRAMES_MAC_INPUT_SIZE = 11,      // the user resized the window (ABI 5): a = width, b = height, in pixels
    FRAMES_MAC_INPUT_FOCUS_GAINED = 12, // the window became key (ABI 5)
    FRAMES_MAC_INPUT_VISIBILITY = 13,   // the window was shown or hidden (minimized, covered) or came to the front
                                        // or left it (ABI 6): a = shown, b = focused; sent when either changes
};
typedef void (*FramesMacInputCallback)(void *context, uint32_t kind, int32_t a, int32_t b,
                                       int32_t c, int32_t d, uint64_t event_ns);
// Installs (or with NULL, removes) the input callback. Removing waits for a
// callback in progress, so the context may be freed once this returns.
void frames_mac_presenter_set_input(void *presenter, void *context, FramesMacInputCallback callback);
// Typed text (ABI 5), composed here (keyboard layout, dead keys, input
// methods) when the window is not in physical-keys mode: UTF-8, valid
// during the call. Installed and removed like the input callback.
typedef void (*FramesMacTextCallback)(void *context, const uint8_t *utf8, size_t length, uint64_t event_ns);
void frames_mac_presenter_set_text_input(void *presenter, void *context, FramesMacTextCallback callback);
// The host's cursor over the presenter's view (ABI 2): straight-alpha BGRA,
// top row first (borrowed). Without a shape, or with visible false, the
// system cursor is hidden over the view.
void frames_mac_presenter_set_cursor(void *presenter, const uint8_t *bgra, uint32_t width,
                                     uint32_t height, uint32_t hotspot_x, uint32_t hotspot_y);
void frames_mac_presenter_set_cursor_visible(void *presenter, bool visible);
// The debug overlay over the picture's top left (ABI 6): `utf8` is a
// frames-next `overlay::Overlay` as JSON (lines, frame and bitrate graphs),
// borrowed; length 0 hides it. Not shown on popups or windows smaller than
// 360x260 points.
void frames_mac_presenter_set_overlay(void *presenter, const uint8_t *utf8, size_t length);
// The presenter window's title (UTF-8, borrowed), e.g. live statistics.
void frames_mac_presenter_set_title(void *presenter, const uint8_t *utf8, size_t length);
// Feeds generated mouse, wheel and key events for `step` through the same
// conversion as real ones (unattended tests). Runs on the main thread.
void frames_mac_presenter_post_synthetic_input(void *presenter, uint64_t step);
// Turns relative mouse on or off as Control+Option+F11 does (ABI 3). With
// `lock_pointer` false the system pointer is left alone (unattended tests).
void frames_mac_presenter_set_relative(void *presenter, bool relative, bool lock_pointer);
double frames_mac_presenter_refresh_hz(void *presenter);
void frames_mac_presenter_free(void *presenter);
// Makes the process a regular app (Dock icon) with the viewer's menu bar,
// whose commands reach the front window as FRAMES_MAC_INPUT_COMMAND (ABI 4).
void frames_mac_viewer_app(void);
// The viewer's connection menus (ABI 5): `utf8` is the session's state as
// JSON (frames-next `control::Snapshot`), borrowed; the menu bar and every
// window's title-bar menu are rebuilt from it.
void frames_mac_viewer_set_state(const uint8_t *utf8, size_t length);
// Where menu choices go (ABI 5): `utf8` is a frames-next `control::Command`
// as JSON, valid during the call. Called on the main thread.
typedef void (*FramesMacCommandCallback)(void *context, const uint8_t *utf8, size_t length);
void frames_mac_viewer_set_command_handler(void *context, FramesMacCommandCallback callback);
// The window's entry in the connection menus (ABI 5): the host's window id,
// 0 for the desktop view.
void frames_mac_presenter_set_menu_id(void *presenter, uint64_t id);
// How the picture fits the window (ABI 5): `fill` stretches it over the
// window, otherwise it is letterboxed (input maps to the picture either
// way); `keep_aspect` keeps the window in the picture's shape while the user
// resizes it and resize() reshapes it ("Stretch locally"); without, the
// window keeps the size the user gives it ("Resize remote app").
// `physical_keys` sends every key as a key (terminals); otherwise printable
// keys go as composed text (FramesMacTextCallback).
void frames_mac_presenter_set_view_options(void *presenter, bool fill, bool keep_aspect, bool physical_keys);
// Whether the user closed a windowed presenter's window (ABI 4).
bool frames_mac_presenter_closed(void *presenter);
// With `on`, the window's first close is only recorded (a request the
// caller acts on) and a second one closes it (ABI 4).
void frames_mac_presenter_intercept_close(void *presenter, bool on);
// Files dropped on the window since the last call (ABI 4): a UTF-8 JSON
// array of paths, written to `buffer` when it fits in `capacity`; returns
// its length either way (files that did not fit are kept). 0: none.
size_t frames_mac_presenter_take_dropped(void *presenter, uint8_t *buffer, size_t capacity);
// The shared application's menus for this window's menu bar (ABI 4), as
// frames_mac_app_menus makes them; chosen items arrive as
// FRAMES_MAC_INPUT_MENU.
void frames_mac_presenter_set_menus(void *presenter, const uint8_t *utf8, size_t length);
// Whether a close was requested since the last call (ABI 4).
bool frames_mac_presenter_take_close_request(void *presenter);
// Asks another application's window (CGWindowID) to close as its close
// button would, through Accessibility (ABI 4). False when it cannot.
bool frames_mac_close_window(uint32_t window_id);
// The menus of the application owning `window_id` (ABI 4), through
// Accessibility: a UTF-8 JSON tree of {"id", "title", "enabled",
// "checked", "separator", "children"}, enabled leaf items numbered from 1
// in order. Written to `buffer` when it fits; returns its length either
// way (0: none).
size_t frames_mac_app_menus(uint32_t window_id, uint8_t *buffer, size_t capacity);
// Brings the application owning `window_id` to the front when it is not,
// and with `raise` the window above its others (ABI 5). False when the
// window is gone.
bool frames_mac_activate_window(uint32_t window_id, bool raise);
// Drops the file at `path` (UTF-8, borrowed) into the window `window_id`
// at (`x`, `y`) in global points, top-left origin, as dragging it there
// from Finder would (ABI 6): Accessibility moves the pointer for the drag.
// False when the application did not take it.
bool frames_mac_drop_file(uint32_t window_id, const uint8_t *path, size_t length, double x, double y);
// This Mac's user session (ABI 6): 0 active and unlocked, 1 locked, 2 not
// the console session (another user is in front), 3 no session.
int32_t frames_mac_session_state(void);
// Presses leaf item `id` of that tree (ABI 4). False when it is not there.
bool frames_mac_press_menu(uint32_t window_id, uint32_t id);
// The stream's new size (ABI 4): the drawable follows it and the window
// takes its shape at its current scale, within the screen.
void frames_mac_presenter_resize(void *presenter, uint32_t width, uint32_t height);

// The general pasteboard (ABI 2), for clipboard sharing. Reads return the
// length and a malloc'd copy in *out (free it with frames_mac_free), or -1
// when the pasteboard has no such content.
uint64_t frames_mac_clipboard_change_count(void);
int64_t frames_mac_clipboard_text(uint8_t **out);
int64_t frames_mac_clipboard_png(uint8_t **out);
int32_t frames_mac_clipboard_set_text(const uint8_t *utf8, size_t length);
int32_t frames_mac_clipboard_set_png(const uint8_t *png, size_t length);
void frames_mac_free(void *pointer);

// Host audio from ScreenCaptureKit (ABI 7): everything this Mac plays
// except this process (pid 0), or one application's (its pid). PCM arrives
// on a capture queue as interleaved f32, 48 kHz, `channels` (2) per frame;
// `samples` NULL with 0 frames means the capture ended. Needs the Screen
// Recording permission. Returns NULL and writes a UTF-8 reason into `error`
// (`error_capacity` bytes, NUL-terminated) when it cannot start.
typedef void (*FramesMacAudioCallback)(void *context, const float *samples, size_t frames,
                                       uint32_t channels);
void *frames_mac_audio_start(int32_t pid, FramesMacAudioCallback callback, void *context,
                             char *error, size_t error_capacity);
void frames_mac_audio_stop(void *handle);

// Runs NSApplication on the calling (main) thread until stopped or `seconds`
// elapse (0 = until stopped). Never call while another toolkit owns it.
void frames_mac_run_app(double seconds);
void frames_mac_stop_app(void);
