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

#define FRAMES_MAC_ABI_VERSION 1u

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
int32_t frames_mac_presenter_present(void *presenter, void *pixel_buffer, bool vsync,
                                     uint64_t *presented_ns);
// Host time (ns) the most recently presented drawable reached the glass, or 0
// if not known yet (windowed presenters only). Reported asynchronously, so it
// usually describes the frame before the one just presented.
uint64_t frames_mac_presenter_last_on_glass_ns(void *presenter);
double frames_mac_presenter_refresh_hz(void *presenter);
void frames_mac_presenter_free(void *presenter);

// Runs NSApplication on the calling (main) thread until stopped or `seconds`
// elapse (0 = until stopped). Never call while another toolkit owns it.
void frames_mac_run_app(double seconds);
void frames_mac_stop_app(void);
