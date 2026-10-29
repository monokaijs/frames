#ifndef FRAMES_H
#define FRAMES_H
#include <stdint.h>
#ifdef __cplusplus
extern "C" {
#endif
typedef struct FramesSession FramesSession;
uint32_t frames_abi_version(void);
FramesSession *frames_session_new(void);
/* Returns an owned UTF-8 JSON result. Calls on a session must be serialized. */
char *frames_session_apply(FramesSession *session, const char *json);
void frames_session_free(FramesSession *session);
void frames_string_free(char *string);
#ifdef __cplusplus
}
#endif
#endif
