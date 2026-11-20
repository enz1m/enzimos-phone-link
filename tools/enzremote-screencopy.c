/**
 * enzremote-screencopy: Low-latency wlr-screencopy frame capture.
 *
 * Persistent process that captures frames from a Wayland compositor
 * using the zwlr_screencopy_manager_v1 protocol and writes raw pixel
 * data (BGRA or RGB) to stdout or a FIFO.
 *
 * Usage:
 *   enzremote-screencopy [--fps N] [--format rgb|bgra]
 *
 * Environment:
 *   WAYLAND_DISPLAY   Target compositor (e.g., wayland-2)
 *   XDG_RUNTIME_DIR   Runtime dir for Wayland socket
 *
 * Output protocol:
 *   First line on stdout: "READY <width> <height> <stride> <format>\n"
 *   Then: raw pixel data frames, each exactly width*height*bpp bytes.
 *   Frames are written continuously at the requested FPS.
 *
 * Unlike grim (which spawns a new process per frame), this keeps a
 * persistent Wayland connection and reuses SHM buffers, eliminating
 * ~10-15ms of process-spawn and protocol-negotiation overhead per frame.
 */

#include "wlr-screencopy-unstable-v1-client-protocol.h"

#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <time.h>
#include <unistd.h>
#include <wayland-client.h>

static volatile sig_atomic_t running = 1;

static void handle_signal(int sig) {
    (void)sig;
    running = 0;
}

/* ── Globals ─────────────────────────────────────────────── */

struct capture_state {
    struct wl_display *display;
    struct wl_shm *shm;
    struct wl_output *output;
    struct zwlr_screencopy_manager_v1 *screencopy_mgr;

    /* Frame buffer */
    struct wl_shm_pool *pool;
    struct wl_buffer *buffer;
    void *shm_data;
    int shm_fd;
    size_t shm_size;

    /* Frame info (from buffer event) */
    uint32_t width;
    uint32_t height;
    uint32_t stride;
    uint32_t format; /* wl_shm format */

    /* Output FD (stdout or FIFO) */
    int out_fd;

    /* Capture state machine */
    enum { IDLE, WAITING, READY, FAILED } status;
    bool buffer_done;  /* buffer() callback received */

    /* Target FPS */
    int target_fps;

    /* Output format: 0 = native (BGRA/XRGB), 1 = convert to RGB */
    int output_rgb;

    /* Stats */
    uint64_t frames_captured;
    uint64_t frames_written;
};

/* ── SHM buffer allocation ───────────────────────────────── */

static int create_shm_file(size_t size) {
    char name[] = "/enzremote-screencopy-XXXXXX";
    int fd = shm_open(name, O_RDWR | O_CREAT | O_EXCL, 0600);
    if (fd < 0) return -1;
    shm_unlink(name);
    if (ftruncate(fd, (off_t)size) < 0) {
        close(fd);
        return -1;
    }
    return fd;
}

static bool alloc_buffer(struct capture_state *state) {
    /* Free previous */
    if (state->buffer) {
        wl_buffer_destroy(state->buffer);
        state->buffer = NULL;
    }
    if (state->pool) {
        wl_shm_pool_destroy(state->pool);
        state->pool = NULL;
    }
    if (state->shm_data) {
        munmap(state->shm_data, state->shm_size);
        state->shm_data = NULL;
    }
    if (state->shm_fd >= 0) {
        close(state->shm_fd);
        state->shm_fd = -1;
    }

    state->shm_size = (size_t)state->stride * state->height;
    state->shm_fd = create_shm_file(state->shm_size);
    if (state->shm_fd < 0) return false;

    state->shm_data = mmap(NULL, state->shm_size, PROT_READ | PROT_WRITE,
                           MAP_SHARED, state->shm_fd, 0);
    if (state->shm_data == MAP_FAILED) {
        state->shm_data = NULL;
        close(state->shm_fd);
        state->shm_fd = -1;
        return false;
    }

    state->pool = wl_shm_create_pool(state->shm, state->shm_fd,
                                     (int32_t)state->shm_size);
    state->buffer = wl_shm_pool_create_buffer(state->pool, 0,
                                               (int32_t)state->width,
                                               (int32_t)state->height,
                                               (int32_t)state->stride,
                                               state->format);
    return true;
}

/* ── wlr-screencopy callbacks ────────────────────────────── */

static void frame_buffer(void *data,
                         struct zwlr_screencopy_frame_v1 *frame,
                         uint32_t format, uint32_t width, uint32_t height,
                         uint32_t stride) {
    struct capture_state *state = data;
    (void)frame;

    bool need_realloc = (width != state->width ||
                         height != state->height ||
                         stride != state->stride ||
                         format != state->format ||
                         state->buffer == NULL);

    state->width = width;
    state->height = height;
    state->stride = stride;
    state->format = format;

    if (need_realloc) {
        if (!alloc_buffer(state)) {
            state->status = FAILED;
            return;
        }
    }

    state->buffer_done = true;
}

static void frame_flags(void *data,
                        struct zwlr_screencopy_frame_v1 *frame,
                        uint32_t flags) {
    (void)data; (void)frame; (void)flags;
}

static void frame_ready(void *data,
                        struct zwlr_screencopy_frame_v1 *frame,
                        uint32_t tv_sec_hi, uint32_t tv_sec_lo,
                        uint32_t tv_nsec) {
    struct capture_state *state = data;
    (void)frame; (void)tv_sec_hi; (void)tv_sec_lo; (void)tv_nsec;
    state->status = READY;
}

static void frame_failed(void *data,
                         struct zwlr_screencopy_frame_v1 *frame) {
    struct capture_state *state = data;
    (void)frame;
    state->status = FAILED;
}

/* damage event (protocol v3) – ignored, we capture full frames */
static void frame_damage(void *data,
                         struct zwlr_screencopy_frame_v1 *frame,
                         uint32_t x, uint32_t y, uint32_t w, uint32_t h) {
    (void)data; (void)frame; (void)x; (void)y; (void)w; (void)h;
}

/* linux_dmabuf event – we don't use DMA-BUF, ignore */
static void frame_linux_dmabuf(void *data,
                               struct zwlr_screencopy_frame_v1 *frame,
                               uint32_t fmt, uint32_t w, uint32_t h) {
    (void)data; (void)frame; (void)fmt; (void)w; (void)h;
}

/* buffer_done event (sent after all buffer/linux_dmabuf events) */
static void frame_buffer_done(void *data,
                              struct zwlr_screencopy_frame_v1 *frame) {
    (void)data; (void)frame;
    /* We already handle buffer allocation in frame_buffer */
}

static const struct zwlr_screencopy_frame_v1_listener frame_listener = {
    .buffer = frame_buffer,
    .flags = frame_flags,
    .ready = frame_ready,
    .failed = frame_failed,
    .damage = frame_damage,
    .linux_dmabuf = frame_linux_dmabuf,
    .buffer_done = frame_buffer_done,
};

/* ── Registry ────────────────────────────────────────────── */

static void registry_global(void *data, struct wl_registry *registry,
                            uint32_t name, const char *interface,
                            uint32_t version) {
    struct capture_state *state = data;

    if (strcmp(interface, wl_shm_interface.name) == 0) {
        state->shm = wl_registry_bind(registry, name,
                                       &wl_shm_interface, 1);
    } else if (strcmp(interface, wl_output_interface.name) == 0) {
        if (!state->output) {
            /* Bind the first output */
            state->output = wl_registry_bind(registry, name,
                                              &wl_output_interface, 1);
        }
    } else if (strcmp(interface,
                      zwlr_screencopy_manager_v1_interface.name) == 0) {
        uint32_t bind_ver = version < 3 ? version : 3;
        state->screencopy_mgr = wl_registry_bind(
            registry, name,
            &zwlr_screencopy_manager_v1_interface, bind_ver);
    }
}

static void registry_global_remove(void *data, struct wl_registry *registry,
                                   uint32_t name) {
    (void)data; (void)registry; (void)name;
}

static const struct wl_registry_listener registry_listener = {
    .global = registry_global,
    .global_remove = registry_global_remove,
};

/* ── Time helpers ────────────────────────────────────────── */

static uint64_t now_us(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000000ULL + (uint64_t)ts.tv_nsec / 1000ULL;
}

/* ── BGRA→RGB conversion (in-place would corrupt, use separate buf) ── */

static void bgra_to_rgb(const uint8_t *src, uint8_t *dst,
                        uint32_t width, uint32_t height, uint32_t stride) {
    for (uint32_t y = 0; y < height; y++) {
        const uint8_t *row = src + y * stride;
        uint8_t *out = dst + y * width * 3;
        for (uint32_t x = 0; x < width; x++) {
            out[x * 3 + 0] = row[x * 4 + 2]; /* R */
            out[x * 3 + 1] = row[x * 4 + 1]; /* G */
            out[x * 3 + 2] = row[x * 4 + 0]; /* B */
        }
    }
}

/* ── Write helpers ───────────────────────────────────────── */

static bool write_all(int fd, const void *buf, size_t count) {
    const uint8_t *p = buf;
    while (count > 0) {
        ssize_t n = write(fd, p, count);
        if (n < 0) {
            if (errno == EINTR) continue;
            return false;
        }
        p += n;
        count -= (size_t)n;
    }
    return true;
}

/* ── Main ────────────────────────────────────────────────── */

int main(int argc, char *argv[]) {
    if (getenv("ENZREMOTE_CAPTURE_OUTPUT")) {
        fprintf(stderr, "screencopy: named-output capture is not supported by this legacy helper\n");
        return 2;
    }
    struct capture_state state = {
        .shm_fd = -1,
        .out_fd = STDOUT_FILENO,
        .target_fps = 30,
        .output_rgb = 1, /* default: convert to RGB for rawvideoparse */
    };

    /* Parse args */
    for (int i = 1; i < argc; i++) {
        if (strcmp(argv[i], "--fps") == 0 && i + 1 < argc) {
            state.target_fps = atoi(argv[++i]);
            if (state.target_fps < 1) state.target_fps = 1;
            if (state.target_fps > 120) state.target_fps = 120;
        } else if (strcmp(argv[i], "--format") == 0 && i + 1 < argc) {
            i++;
            if (strcmp(argv[i], "bgra") == 0 || strcmp(argv[i], "native") == 0)
                state.output_rgb = 0;
            else
                state.output_rgb = 1;
        }
    }

    signal(SIGINT, handle_signal);
    signal(SIGTERM, handle_signal);
    signal(SIGPIPE, SIG_IGN); /* Don't crash on broken pipe */

    /* Connect to Wayland */
    state.display = wl_display_connect(NULL);
    if (!state.display) {
        fprintf(stderr, "screencopy: cannot connect to Wayland\n");
        return 1;
    }

    struct wl_registry *registry = wl_display_get_registry(state.display);
    wl_registry_add_listener(registry, &registry_listener, &state);
    wl_display_roundtrip(state.display);

    if (!state.shm || !state.output || !state.screencopy_mgr) {
        fprintf(stderr, "screencopy: missing required protocols "
                "(shm=%p output=%p screencopy=%p)\n",
                (void *)state.shm, (void *)state.output,
                (void *)state.screencopy_mgr);
        wl_display_disconnect(state.display);
        return 1;
    }

    /* Do one probe capture to learn dimensions */
    struct zwlr_screencopy_frame_v1 *probe_frame =
        zwlr_screencopy_manager_v1_capture_output(
            state.screencopy_mgr, 0, state.output);
    zwlr_screencopy_frame_v1_add_listener(probe_frame, &frame_listener,
                                          &state);
    state.status = WAITING;
    state.buffer_done = false;

    /* Wait for buffer() callback to learn dimensions */
    while (state.status == WAITING && running) {
        if (wl_display_dispatch(state.display) < 0) break;
        if (state.buffer_done && state.status == WAITING) {
            /* Got dimensions, now copy the frame */
            zwlr_screencopy_frame_v1_copy(probe_frame, state.buffer);
            state.buffer_done = false;
        }
    }

    if (state.status != READY || !state.width || !state.height) {
        fprintf(stderr, "screencopy: probe capture failed\n");
        wl_display_disconnect(state.display);
        return 1;
    }
    zwlr_screencopy_frame_v1_destroy(probe_frame);

    /* Allocate RGB conversion buffer if needed */
    uint8_t *rgb_buf = NULL;
    size_t rgb_size = 0;
    const char *fmt_name;
    uint32_t out_stride;

    if (state.output_rgb) {
        rgb_size = (size_t)state.width * state.height * 3;
        rgb_buf = malloc(rgb_size);
        if (!rgb_buf) {
            fprintf(stderr, "screencopy: out of memory\n");
            wl_display_disconnect(state.display);
            return 1;
        }
        fmt_name = "rgb";
        out_stride = state.width * 3;
    } else {
        fmt_name = "bgra";
        out_stride = state.stride;
    }

    /* Print READY line */
    dprintf(state.out_fd, "READY %u %u %u %s\n",
            state.width, state.height, out_stride, fmt_name);

    /* Capture loop */
    uint64_t frame_interval_us = 1000000ULL / (uint64_t)state.target_fps;
    uint64_t next_frame = now_us();

    while (running) {
        /* Throttle to target FPS */
        uint64_t now = now_us();
        if (next_frame > now) {
            usleep((useconds_t)(next_frame - now));
        }
        next_frame += frame_interval_us;
        /* Don't accumulate debt if we're behind */
        if (next_frame < now_us()) {
            next_frame = now_us() + frame_interval_us;
        }

        /* Request frame capture */
        struct zwlr_screencopy_frame_v1 *frame =
            zwlr_screencopy_manager_v1_capture_output(
                state.screencopy_mgr, 0, state.output);
        zwlr_screencopy_frame_v1_add_listener(frame, &frame_listener,
                                              &state);
        state.status = WAITING;
        state.buffer_done = false;

        /* Wait for buffer() → copy → ready */
        while (state.status == WAITING && running) {
            if (wl_display_dispatch(state.display) < 0) {
                running = 0;
                break;
            }
            if (state.buffer_done && state.status == WAITING) {
                zwlr_screencopy_frame_v1_copy(frame, state.buffer);
                state.buffer_done = false;
            }
        }

        zwlr_screencopy_frame_v1_destroy(frame);

        if (state.status != READY) {
            if (state.status == FAILED) {
                /* Transient failure, try again next tick */
                continue;
            }
            break;
        }

        state.frames_captured++;

        /* Write frame data */
        bool ok;
        if (state.output_rgb) {
            bgra_to_rgb(state.shm_data, rgb_buf,
                        state.width, state.height, state.stride);
            ok = write_all(state.out_fd, rgb_buf, rgb_size);
        } else {
            ok = write_all(state.out_fd, state.shm_data, state.shm_size);
        }

        if (!ok) {
            /* Broken pipe or write error — reader is gone */
            break;
        }
        state.frames_written++;
    }

    /* Cleanup */
    free(rgb_buf);
    if (state.buffer) wl_buffer_destroy(state.buffer);
    if (state.pool) wl_shm_pool_destroy(state.pool);
    if (state.shm_data) munmap(state.shm_data, state.shm_size);
    if (state.shm_fd >= 0) close(state.shm_fd);
    if (state.screencopy_mgr)
        zwlr_screencopy_manager_v1_destroy(state.screencopy_mgr);
    wl_display_disconnect(state.display);

    fprintf(stderr, "screencopy: %lu frames captured, %lu written\n",
            state.frames_captured, state.frames_written);
    return 0;
}
