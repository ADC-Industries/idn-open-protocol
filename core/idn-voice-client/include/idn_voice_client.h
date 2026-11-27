/*
 * idn_voice_client.h: the C ABI of IDN Voice's client library (crate idn-voice-client,
 * src/ffi.rs; spec/extensions/idn-voice.md §V.13, §V.14; the protocol in
 * docs/VOICE-CLIENT.md). Written by hand to match src/ffi.rs.
 *
 * A handset app (a fleet app on Android, e.g. for the Inrico B01) is a Voice client through
 * it: the library connects (QUIC on IDN's link first, the WebSocket otherwise), keeps
 * the session across dropped links (resume), codes the microphone (Opus) and plays the
 * channels (a jitter buffer per talker, mixed).
 *
 *   - One opaque handle per client (IdnVoiceClient).
 *   - The configuration and commands as JSON (UTF-8, NUL-terminated); events to the
 *     callback as JSON, valid during the call only.
 *   - Sound: 16-bit mono samples; the speaker at 48 kHz, the microphone at 48 kHz (or
 *     8, 12, 16 or 24 kHz).
 *   - Results: IDN_VOICE_OK (0) or a negative IDN_VOICE_* code.
 *   - Threads: every function may be called from any thread, together, except
 *     idn_voice_close, which must be the handle's last call (nothing else on it under way
 *     or after). idn_voice_pull_speaker_i16 is for the audio output thread (it decodes
 *     there and never waits for the network). The callback runs on the library's own
 *     threads, one event at a time: hand the JSON to the app's thread, don't block.
 *   - No call unwinds into C (a panic is IDN_VOICE_PANIC, or a null handle).
 *
 * MIT OR Apache-2.0, (c) ADC Industries.
 */
#ifndef IDN_VOICE_CLIENT_H
#define IDN_VOICE_CLIENT_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define IDN_VOICE_OK 0
#define IDN_VOICE_BAD_ARGUMENT -1
#define IDN_VOICE_BAD_JSON -2
#define IDN_VOICE_BAD_RATE -3
#define IDN_VOICE_PANIC -4
#define IDN_VOICE_CODEC -5

typedef struct IdnVoiceClient IdnVoiceClient;

/* One event: {"event": "connected"|"reconnecting"|"hello"|"driver"|"joined"|"left"|
 * "talking"|"talk"|"message"|"sos"|"error"|"closed"|"other", ...}; valid during the
 * call only. */
typedef void (*IdnVoiceEventCallback)(void *user_data, const char *event_json);

/*
 * Starts a client; returns at once (it connects in the background: "connected" and
 * "hello" events follow, or "reconnecting" while it can't).
 * config_json: {"credential": "idnv1.<id>.<secret>",
 *               "link": ["HOST:PORT"], "link_cert_sha256": "3F:1A:...",
 *               "urls": ["https://HOST:29203/voice"], "cert_sha256": "3F:1A:...",
 *               "resume": "<token>", "have": "TALKER:SEQ,...",
 *               "app": "Fleet Driver", "version": "2.4.0",
 *               "transport": "auto"|"quic"|"websocket",
 *               "link_connect_ms": 3000, "link_retry_s": 60, "link_keepalive_ms": 5000,
 *               "ws_connect_ms": 10000, "hold_mic_s": 30, "fec_loss_pct": 10}
 * (the enrolment answer's fields keep their names: pass it with the credential).
 * callback may be NULL (no events). On a configuration that can't work: NULL, and the
 * callback is called once before this returns with
 * {"event": "error", "what": "config", "error": "..."} (with no callback the reason
 * isn't reported: check the configuration with one while developing).
 */
IdnVoiceClient *idn_voice_connect(const char *config_json, IdnVoiceEventCallback callback,
                                  void *user_data);

/* Any command (docs/VOICE-CLIENT.md §4.3), e.g. {"t": "join", "channel": 3},
 * {"t": "message", "channel": 3, "text": "..."}, {"t": "sos", "position": {"lat", "lon"}},
 * {"t": "answer", "call": 12}. A "talk" works as idn_voice_talk (the microphone follows
 * it; a talk on needs "channel", "user", "all" or "call"). Commands are kept until the
 * server confirms them and sent again after a drop (one sent in the second before a
 * silent drop may arrive twice). IDN_VOICE_BAD_JSON for one that isn't a JSON object
 * with "t", a talk on without a target, or "bye" (use idn_voice_close). A "ping" is
 * ignored (the library pings). */
int32_t idn_voice_send_json(const IdnVoiceClient *client, const char *json);

/* Push-to-talk on a channel: on non-zero starts (the microphone is sent from now on),
 * zero stops (channel then unused). The server may end a talk itself ("talk" event,
 * on false: time limit, taken, busy, ...). On "link lost" or "no sound" the library
 * starts the talk again itself (a dead zone): keep recording while the button is held. */
int32_t idn_voice_talk(const IdnVoiceClient *client, uint32_t channel, int32_t on);

/* The microphone: samples mono at rate (48000; or 8000, 12000, 16000, 24000), any count.
 * Coded and sent in 20 ms frames while talking (held while the link is down, up to
 * hold_mic_s, and sent after the resume); dropped when not talking. IDN_VOICE_BAD_RATE
 * for another rate, IDN_VOICE_CODEC if the encoder fails. From the recording thread, as
 * samples come. */
int32_t idn_voice_push_mic_i16(const IdnVoiceClient *client, const int16_t *pcm,
                               size_t samples, uint32_t rate);

/* The speaker: fills out with samples mono at 48 kHz, every talker mixed (silence when
 * nobody talks). From the audio output callback. */
int32_t idn_voice_pull_speaker_i16(const IdnVoiceClient *client, int16_t *out,
                                   size_t samples);

/* The device's network changed (ConnectivityManager.NetworkCallback): QUIC moves to a
 * fresh socket and goes on; a WebSocket on the old network is resumed on the new one. */
int32_t idn_voice_network_changed(const IdnVoiceClient *client);

/* The session to come back to after an app restart (within the site's resume_s), as
 * {"resume": "...", "have": "..."}: merge it into the next config_json. NULL while there
 * is no token (none in config_json and no hello yet). Free with idn_voice_free_string. */
char *idn_voice_resume_state(const IdnVoiceClient *client);

/* Signs off ("bye": the session ends now) and frees the handle. No callback comes after
 * it returns. May be called from the callback (it then doesn't wait for the bye). */
void idn_voice_close(IdnVoiceClient *client);

/* Frees a string the library returned. */
void idn_voice_free_string(char *s);

#ifdef __cplusplus
}
#endif

#endif /* IDN_VOICE_CLIENT_H */
