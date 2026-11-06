/*
 * Rig-only LD_PRELOAD shim for the K3 dot harness (tools/capture-host/
 * dot_equiv.py), loaded into BOTH gst-launch-1.0 and the capture host so the
 * two run identically instrumented. Never shipped, never on a device.
 *
 * 1. R-WP0-1: gst-launch dumps with GST_DEBUG_GRAPH_SHOW_ALL, which truncates
 *    every property value over 80 characters with "…", so a difference after
 *    the cut was invisible. OR in GST_DEBUG_GRAPH_SHOW_FULL_PARAMS (1 << 4,
 *    gstdebugutils.h @ 1.22) on every dump.
 * 2. R-WP0-2: mp4mux's faststart-file DEFAULT is "/tmp/qtmux%d" of
 *    g_random_int(), so it always differs between two runs. A deterministic
 *    g_random_int() (a counter from a fixed seed) makes the default identical
 *    in both programs, so the value is compared exactly, unmasked.
 *
 * Build (the harness does it): gcc -shared -fPIC -O2 -o xg-dotshim.so dotshim.c -ldl
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <stdint.h>

#define XG_SHOW_FULL_PARAMS (1 << 4)

typedef void (*dump_fn)(void *bin, int details, const char *file_name);

void gst_debug_bin_to_dot_file_with_ts(void *bin, int details, const char *file_name)
{
    static dump_fn real;
    if (!real)
        real = (dump_fn)dlsym(RTLD_NEXT, "gst_debug_bin_to_dot_file_with_ts");
    real(bin, details | XG_SHOW_FULL_PARAMS, file_name);
}

void gst_debug_bin_to_dot_file(void *bin, int details, const char *file_name)
{
    static dump_fn real;
    if (!real)
        real = (dump_fn)dlsym(RTLD_NEXT, "gst_debug_bin_to_dot_file");
    real(bin, details | XG_SHOW_FULL_PARAMS, file_name);
}

static uint32_t xg_seed = 424242u;

uint32_t g_random_int(void)
{
    return __atomic_fetch_add(&xg_seed, 1u, __ATOMIC_SEQ_CST);
}
