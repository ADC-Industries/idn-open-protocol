/*
 * idn_iem.h: the C ABI of IDN's phone mixing library (crate idn-iem, src/ffi.rs;
 * spec/extensions/idn-iem.md §I.8). Written by hand to match src/ffi.rs.
 *
 * A musician's phone takes a desk's IEM flow, mixes it by the musician's mix (followed
 * live from the desk) and plays it; push-to-talk goes back to the desk.
 *
 *   - One opaque handle per desk connection (IdnIemReceiver).
 *   - Structured data as JSON (UTF-8, NUL-terminated); strings the library returns are
 *     freed with idn_iem_string_free.
 *   - Results: IDN_IEM_OK (0) or a negative IDN_IEM_* code.
 *   - Every function may be called from any thread. idn_iem_render is for the audio
 *     thread: it decodes there and never waits for the network.
 *   - No call unwinds into C (a panic is IDN_IEM_PANIC).
 *
 * MIT OR Apache-2.0, (c) ADC Industries.
 */
#ifndef IDN_IEM_H
#define IDN_IEM_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define IDN_IEM_OK 0
#define IDN_IEM_BAD_ARGUMENT -1
#define IDN_IEM_IO -2
#define IDN_IEM_TLS -3
#define IDN_IEM_HTTP -4
#define IDN_IEM_TOKEN_NEEDED -5
#define IDN_IEM_TOKEN_REFUSED -6
#define IDN_IEM_FULL -7
#define IDN_IEM_BUSY -8
#define IDN_IEM_NOT_ALLOWED -9
#define IDN_IEM_BAD -10
#define IDN_IEM_NO_FLOW -11
#define IDN_IEM_NO_MULTICAST -12
#define IDN_IEM_NO_MIX -13
#define IDN_IEM_UNKNOWN_CHANNEL -14
#define IDN_IEM_TIMEOUT -15
#define IDN_IEM_DESK -16
#define IDN_IEM_CONFIG -17
#define IDN_IEM_CLOSED -18
#define IDN_IEM_PANIC -19
#define IDN_IEM_UNSUPPORTED -20

typedef struct IdnIemReceiver IdnIemReceiver;

/* Events: one JSON object a call, valid during the call only. */
typedef void (*IdnIemEventCallback)(void *user, const char *json);

/*
 * Connects (blocks up to connect_timeout_ms, 5 s by default).
 * config_json: {"desk": "HOST[:PORT]", "token": "idn1...", "mix": "drums",
 *               "transport": "auto"|"unicast"|"multicast", "talk": true,
 *               "buffer_ms": 15, "min_buffer_ms": 10, "max_buffer_ms": 100,
 *               "multicast_wait_ms": 1500, "multicast_interface": "192.168.1.30",
 *               "udp_port": 29224, "connect_timeout_ms": 5000,
 *               "ca_sha256": "AB:CD:..."}
 * ca_der/ca_len: a certificate to pin over HTTPS (the desk's own or the site CA's, DER);
 * NULL with no "ca_sha256": plain HTTP (the desk's own machine only).
 * On failure: NULL; *err_code and err (when not NULL) say why.
 */
IdnIemReceiver *idn_iem_connect(const char *config_json, const uint8_t *ca_der,
                                size_t ca_len, int32_t *err_code, char *err,
                                size_t err_len);

/* Ends the session (BYE) and frees the handle. */
void idn_iem_free(IdnIemReceiver *h);

/* The mix: `frames` frames of stereo interleaved float, 48 kHz, into out[2 * frames]. */
int32_t idn_iem_render(const IdnIemReceiver *h, float *out, size_t frames);

/* The musician's controls (ch: the desk channel's number; a pair by either side). */
int32_t idn_iem_set_level(const IdnIemReceiver *h, uint16_t ch, float db);   /* -144..+10 */
int32_t idn_iem_set_pan(const IdnIemReceiver *h, uint16_t ch, float pan);    /* -1..+1 */
int32_t idn_iem_set_mute(const IdnIemReceiver *h, uint16_t ch, int32_t on);
int32_t idn_iem_set_solo(const IdnIemReceiver *h, uint16_t ch, int32_t on);
int32_t idn_iem_set_in_mix(const IdnIemReceiver *h, uint16_t ch, int32_t on);
int32_t idn_iem_set_master(const IdnIemReceiver *h, float db);

/* JSON (free with idn_iem_string_free; NULL on a bad handle). */
char *idn_iem_mix_json(const IdnIemReceiver *h);      /* {"id","name","master_db","channels":{...}} or null */
char *idn_iem_channels_json(const IdnIemReceiver *h); /* [{"id","name","color","pair","side"}] */
char *idn_iem_status_json(const IdnIemReceiver *h);   /* {"connected","transport","buffer_ms","loss_pct","latency_ms",...} */

/* The next event, waiting up to timeout_ms; NULL for none. */
char *idn_iem_next_event(const IdnIemReceiver *h, uint32_t timeout_ms);
/* Events to cb instead (on the library's threads); NULL back to the queue. */
int32_t idn_iem_set_event_callback(const IdnIemReceiver *h, IdnIemEventCallback cb,
                                   void *user);

/* Push-to-talk: start, the microphone (any rate, interleaved channels), stop. */
int32_t idn_iem_talk_start(const IdnIemReceiver *h);
int32_t idn_iem_talk_feed(const IdnIemReceiver *h, const float *pcm, size_t frames,
                          uint32_t rate, uint16_t channels);
int32_t idn_iem_talk_stop(const IdnIemReceiver *h);

/* The app's sound output latency (ms), counted in the status's latency_ms. */
int32_t idn_iem_set_output_latency_ms(const IdnIemReceiver *h, float ms);
/* A new token (before the old one expires). */
int32_t idn_iem_set_token(const IdnIemReceiver *h, const char *token);
/* 0 auto, 1 unicast, 2 multicast. */
int32_t idn_iem_set_transport(const IdnIemReceiver *h, int32_t transport);

/* The library's own sound output (built with feature "play"; else IDN_IEM_UNSUPPORTED). */
int32_t idn_iem_play_start(const IdnIemReceiver *h, const char *device);
int32_t idn_iem_play_stop(const IdnIemReceiver *h);

void idn_iem_string_free(char *s);
/* Static; not freed. */
const char *idn_iem_version(void);

#ifdef __cplusplus
}
#endif

#endif /* IDN_IEM_H */
