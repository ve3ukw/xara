/* Checks that xara.h matches the library, from C.
 *
 *   cargo build --release -p xara-ffi
 *   cc -std=c11 -Wall -Wextra -Icrates/xara-ffi/include crates/xara-ffi/tests/smoke.c \
 *      target/release/libxara_ffi.a -lpthread -ldl -lm -o target/xara-smoke && target/xara-smoke
 */

#include <stdio.h>
#include <string.h>
#include "xara.h"

static int lines;

static void on_log(void *user, int level, const char *line)
{
    (void) user;
    (void) level;
    (void) line;
    lines++;
}

int main(void)
{
    xara_set_log(on_log, NULL);

    xara_config cfg;
    xara_config_default(&cfg);
    cfg.call = "N0CALL";
    cfg.cmd_port = 0;
    cfg.kiss_port = 0;

    char err[128];
    xara_modem *m = xara_create(&cfg, err, sizeof err);
    if (!m) {
        fprintf(stderr, "xara_create: %s\n", err);
        return 1;
    }

    float in[XARA_BLOCK] = {0}, out[XARA_BLOCK];
    for (int i = 0; i < 2 * XARA_SAMPLE_RATE / XARA_BLOCK; i++) {
        uint32_t f = xara_process(m, in, out, XARA_BLOCK);
        if (f & (XARA_ERROR | XARA_KEYED)) {
            fprintf(stderr, "unexpected flags %#x\n", f);
            return 1;
        }
    }

    xara_status st;
    xara_get_status(m, &st);
    printf("xara %s: state %s, cmd %u data %u, passband %.0f-%.0f Hz\n",
           xara_version(), st.state, st.cmd_port, st.data_port, st.passband_lo, st.passband_hi);
    int ok = strcmp(st.state, "Listen") == 0 && st.cmd_port != 0 && st.data_port != 0;

    cfg.call = "BAD CALL";
    ok = ok && !xara_create(&cfg, err, sizeof err) && strstr(err, "invalid call");
    cfg.call = "N0CALL";
    cfg.mode = 9;
    ok = ok && !xara_create(&cfg, err, sizeof err) && strstr(err, "invalid mode");
    ok = ok && !xara_set_tx_latency(m, -1, 0) && xara_set_tx_latency(m, 4800, 960);

    xara_destroy(m);
    puts(ok ? "ok" : "FAILED");
    return ok ? 0 : 1;
}
