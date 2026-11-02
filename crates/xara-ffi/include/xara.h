/*
 * xara C API: embed the xara VARA-compatible modem in an application that
 * owns the audio and PTT, such as an SDR.
 *
 * The application feeds received audio and takes transmit audio, both real
 * samples at 48 kHz in [-1, 1], in blocks (480 samples recommended). xara
 * counts time in samples, so call xara_process() at the audio rate with no
 * gaps. The VARA command/data (and optional KISS) TCP ports run on xara's own
 * threads, so Winlink clients connect to them directly.
 *
 * A xara_modem must only be used from one thread at a time.
 */

#ifndef XARA_H
#define XARA_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define XARA_SAMPLE_RATE 48000
#define XARA_BLOCK 480

typedef struct xara_modem xara_modem;

typedef enum {
    XARA_HF_500 = 0,
    XARA_HF_2300 = 1,
    XARA_HF_2750 = 2,
    XARA_FM_NARROW = 3,
    XARA_FM_WIDE = 4
} xara_mode;

typedef struct {
    int mode;              /* a xara_mode; other values are rejected by xara_create() */
    const char *call;      /* your call sign, required */
    const char *addr;      /* listen address for the TCP ports, NULL for 127.0.0.1 */
    uint16_t cmd_port;     /* command port, data is cmd_port + 1; 0 picks two free ports */
    uint16_t kiss_port;    /* KISS port, 0 for none */
    double drive_db;       /* TX drive, -30..0 dB */
    int64_t tx_latency;    /* samples from xara_process() output to RF, including PTT lead */
    int64_t rx_lag;        /* samples from RF to xara_process() input */
    int64_t ptt_tail;      /* samples PTT is held after the last TX sample */
    bool registered;       /* registered mode: the higher speed levels and compression, for a station with
                              a registered copy of VARA for its call; peers see the station as registered */
} xara_config;

/* Fills cfg with the defaults: HF 2300, 127.0.0.1:8300, KISS 8100, -5 dB, no latency, unregistered. */
void xara_config_default(xara_config *cfg);

/* Returns NULL on error, with a message in err (if err_len > 0). */
xara_modem *xara_create(const xara_config *cfg, char *err, size_t err_len);
void xara_destroy(xara_modem *m);

#define XARA_KEYED   (1u << 0) /* transmitting: key PTT, out holds TX audio */
#define XARA_TUNING  (1u << 1) /* tune tone is on */
#define XARA_BUSY    (1u << 2) /* channel busy detected */
#define XARA_RESTART (1u << 3) /* a host client asked for a restart */
#define XARA_ERROR   (1u << 31) /* internal error, out is silence; destroy the modem */

/* Processes n samples: reads in, writes out (silence when not keyed). */
uint32_t xara_process(xara_modem *m, const float *in, float *out, size_t n);

/* Updates the latency model, in samples (e.g. after measuring the TX path). False, and nothing changes, if a
 * value is negative or the modem has failed. */
bool xara_set_tx_latency(xara_modem *m, int64_t tx_latency, int64_t rx_lag);

/* Forces PTT off (e.g. the sequencer refused to key). */
void xara_force_ptt_off(xara_modem *m, bool off);

typedef struct {
    char state[24];        /* session state, e.g. "Listen", "Sending" */
    char peer[24];         /* remote call while linked */
    char bw[16];           /* link bandwidth, e.g. "2300" */
    uint32_t level;        /* speed level */
    uint32_t attempts;
    double passband_lo;    /* audio passband in Hz */
    double passband_hi;
    double afc_hz;         /* NaN when unknown */
    uint16_t cmd_port;     /* bound ports (useful with cmd_port 0) */
    uint16_t data_port;
    uint16_t kiss_port;
} xara_status;

/* After an internal error state is "Error" and only the ports are filled in. */
void xara_get_status(const xara_modem *m, xara_status *st);

#define XARA_LOG_INFO  0 /* what xara shows the user on its console */
#define XARA_LOG_DEBUG 1 /* trace that xara only writes to its log file */

/* Log lines go to cb (from any thread) instead of stderr. Set once, before xara_create(). */
typedef void (*xara_log_fn)(void *user, int level, const char *line);
void xara_set_log(xara_log_fn cb, void *user);

const char *xara_version(void);

#ifdef __cplusplus
}
#endif

#endif
