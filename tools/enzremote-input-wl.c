/**
 * enzremote-input-wl: Wayland-native input worker.
 *
 * Persistent process that reads input commands from stdin and injects
 * them into a Wayland compositor using zwlr_virtual_pointer_v1 and
 * zwp_virtual_keyboard_v1 protocols.
 *
 * It affects the explicitly selected WAYLAND_DISPLAY. ENZREMOTE_INPUT_OUTPUT
 * binds absolute pointer mapping to one advertised output and fails if absent.
 *
 * Protocol: line-delimited commands on stdin, "OK\n" response on stdout.
 * Same format as enzremote-input-eis for compatibility with InputBridge.
 *
 * Commands:
 *   pointer <x> <y> <click>     - absolute pointer move (+ optional click)
 *   touch <slot> <phase> <x> <y> - legacy touch-to-pointer (down/move/up/cancel)
 *   button <code> <pressed>      - mouse button press/release
 *   key <code> <pressed>         - keyboard key press/release
 *   scroll <dx> <dy>             - scroll wheel
 *   scroll_pixels <x> <y> <dx> <dy> <stop> - anchored continuous pixel scroll
 *   pointer_relative <dx> <dy>   - relative pointer motion
 *   text <text>                  - type literal ASCII text
 */

#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif

#include "wlr-virtual-pointer-unstable-v1-client-protocol.h"
#include "virtual-keyboard-unstable-v1-client-protocol.h"

#include <errno.h>
#include <fcntl.h>
#include <linux/input-event-codes.h>
#include <math.h>
#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <time.h>
#include <unistd.h>
#include <wayland-client.h>
#include <xkbcommon/xkbcommon.h>

/* Maximum simultaneous touch slots. */
#define MAX_TOUCH_SLOTS 16

/* Maximum line length from stdin. */
#define MAX_LINE 4096

/* Viewport dimensions (set from command-line args). */
static uint32_t viewport_width = 390;
static uint32_t viewport_height = 844;
static double input_scale = 1.0;
static int iced_scroll_compat = 0;

struct output_record {
    struct wl_output *output;
    uint32_t registry_name;
    char name[128];
};

struct globals {
    struct wl_display *display;
    struct wl_seat *seat;
    struct zwlr_virtual_pointer_manager_v1 *pointer_manager;
    struct zwlr_virtual_pointer_v1 *pointer;
    struct zwp_virtual_keyboard_manager_v1 *keyboard_manager;
    struct zwp_virtual_keyboard_v1 *keyboard;
    struct xkb_context *xkb_context;
    struct xkb_keymap *keymap;
    struct xkb_state *key_state;
    unsigned char key_down[KEY_MAX + 1];
    const char *requested_output;
    struct output_record outputs[32];
    struct wl_output *selected_output;
    int output_lost;
};

/* Touch slot tracking: -1 means slot is free. */
static int touch_active[MAX_TOUCH_SLOTS];

static void output_geometry(void *data, struct wl_output *output, int32_t x, int32_t y,
    int32_t width, int32_t height, int32_t subpixel, const char *make, const char *model, int32_t transform) {
    (void)data; (void)output; (void)x; (void)y; (void)width; (void)height; (void)subpixel; (void)make; (void)model; (void)transform;
}
static void output_mode(void *data, struct wl_output *output, uint32_t flags, int32_t width, int32_t height, int32_t refresh) {
    (void)data; (void)output; (void)flags; (void)width; (void)height; (void)refresh;
}
static void output_done(void *data, struct wl_output *output) { (void)data; (void)output; }
static void output_scale(void *data, struct wl_output *output, int32_t scale) { (void)data; (void)output; (void)scale; }
static void output_name(void *data, struct wl_output *output, const char *name) {
    (void)output;
    struct output_record *record=data;
    if (strlen(name)<sizeof(record->name)) strcpy(record->name,name);
}
static void output_description(void *data, struct wl_output *output, const char *description) { (void)data; (void)output; (void)description; }
static const struct wl_output_listener output_listener = {
    .geometry=output_geometry,.mode=output_mode,.done=output_done,.scale=output_scale,.name=output_name,.description=output_description,
};

static void registry_global(void *data, struct wl_registry *registry,
                            uint32_t name, const char *interface,
                            uint32_t version) {
    struct globals *g = data;
    if (strcmp(interface, wl_seat_interface.name) == 0 && !g->seat) {
        uint32_t v = version < 7 ? version : 7;
        g->seat = wl_registry_bind(registry, name, &wl_seat_interface, v);
    } else if (strcmp(interface,
                      zwlr_virtual_pointer_manager_v1_interface.name) == 0) {
        uint32_t v = version < 2 ? version : 2;
        g->pointer_manager = wl_registry_bind(
            registry, name, &zwlr_virtual_pointer_manager_v1_interface, v);
    } else if (strcmp(interface,
                      zwp_virtual_keyboard_manager_v1_interface.name) == 0) {
        g->keyboard_manager = wl_registry_bind(
            registry, name, &zwp_virtual_keyboard_manager_v1_interface, 1);
    } else if (g->requested_output && strcmp(interface,wl_output_interface.name)==0 && version>=4) {
        for (unsigned i=0;i<32;++i) if (!g->outputs[i].output) {
            struct output_record *record=&g->outputs[i];
            record->registry_name=name; record->name[0]='\0';
            record->output=wl_registry_bind(registry,name,&wl_output_interface,4);
            if (record->output) wl_output_add_listener(record->output,&output_listener,record);
            break;
        }
    }
}

static void registry_global_remove(void *data, struct wl_registry *registry,
                                   uint32_t name) {
    (void)registry;
    struct globals *g=data;
    for (unsigned i=0;i<32;++i) if (g->outputs[i].output && g->outputs[i].registry_name==name) {
        if (g->outputs[i].output==g->selected_output) { g->output_lost=1; g->selected_output=NULL; }
        wl_output_release(g->outputs[i].output); g->outputs[i].output=NULL;
    }
}

static const struct wl_registry_listener registry_listener = {
    .global = registry_global,
    .global_remove = registry_global_remove,
};

static uint32_t now_ms(void) {
    struct timespec ts;
    if (clock_gettime(CLOCK_MONOTONIC, &ts) != 0)
        return 0;
    return (uint32_t)((uint64_t)ts.tv_sec * 1000 + ts.tv_nsec / 1000000);
}

static int flush_display(struct globals *g) {
    if (wl_display_flush(g->display) < 0) {
        fprintf(stderr, "input-wl: display flush failed\n");
        return -1;
    }
    return 0;
}

static int handle_pointer(struct globals *g, const char *args) {
    double x, y;
    int click = 0;
    if (sscanf(args, "%lf %lf %d", &x, &y, &click) < 2)
        return -1;

    /* Clamp to viewport. */
    if (x < 0) x = 0;
    if (y < 0) y = 0;
    if (x >= viewport_width) x = viewport_width - 1;
    if (y >= viewport_height) y = viewport_height - 1;

    uint32_t t = now_ms();
    zwlr_virtual_pointer_v1_motion_absolute(g->pointer, t,
        (uint32_t)x, (uint32_t)y, viewport_width, viewport_height);
    zwlr_virtual_pointer_v1_frame(g->pointer);

    if (click) {
        zwlr_virtual_pointer_v1_button(g->pointer, now_ms(),
            BTN_LEFT, WL_POINTER_BUTTON_STATE_PRESSED);
        zwlr_virtual_pointer_v1_frame(g->pointer);
        /* Flush press to compositor and wait ~20ms so GTK/libadwaita
         * widgets register the press before seeing the release. */
        flush_display(g);
        usleep(20000);
        zwlr_virtual_pointer_v1_button(g->pointer, now_ms(),
            BTN_LEFT, WL_POINTER_BUTTON_STATE_RELEASED);
        zwlr_virtual_pointer_v1_frame(g->pointer);
    }

    return flush_display(g);
}

static int handle_touch(struct globals *g, const char *args) {
    uint32_t slot;
    char phase[16];
    double x, y;

    if (sscanf(args, "%u %15s %lf %lf", &slot, phase, &x, &y) < 4)
        return -1;
    if (slot >= MAX_TOUCH_SLOTS)
        return -1;

    /* Clamp to viewport. */
    if (x < 0) x = 0;
    if (y < 0) y = 0;
    if (x >= viewport_width) x = viewport_width - 1;
    if (y >= viewport_height) y = viewport_height - 1;

    uint32_t t = now_ms();

    if (strcmp(phase, "down") == 0) {
        /* Touch down: move pointer and press button. */
        touch_active[slot] = 1;
        zwlr_virtual_pointer_v1_motion_absolute(g->pointer, t,
            (uint32_t)x, (uint32_t)y, viewport_width, viewport_height);
        zwlr_virtual_pointer_v1_frame(g->pointer);
        zwlr_virtual_pointer_v1_button(g->pointer, now_ms(),
            BTN_LEFT, WL_POINTER_BUTTON_STATE_PRESSED);
        zwlr_virtual_pointer_v1_frame(g->pointer);
    } else if (strcmp(phase, "move") == 0) {
        /* Touch move: move pointer (drag). */
        zwlr_virtual_pointer_v1_motion_absolute(g->pointer, t,
            (uint32_t)x, (uint32_t)y, viewport_width, viewport_height);
        zwlr_virtual_pointer_v1_frame(g->pointer);
    } else if (strcmp(phase, "up") == 0) {
        /* Touch up: release button. */
        touch_active[slot] = 0;
        zwlr_virtual_pointer_v1_motion_absolute(g->pointer, t,
            (uint32_t)x, (uint32_t)y, viewport_width, viewport_height);
        zwlr_virtual_pointer_v1_frame(g->pointer);
        zwlr_virtual_pointer_v1_button(g->pointer, now_ms(),
            BTN_LEFT, WL_POINTER_BUTTON_STATE_RELEASED);
        zwlr_virtual_pointer_v1_frame(g->pointer);
    } else if (strcmp(phase, "cancel") == 0) {
        /* Cancel: release if active. */
        if (touch_active[slot]) {
            touch_active[slot] = 0;
            zwlr_virtual_pointer_v1_button(g->pointer, now_ms(),
                BTN_LEFT, WL_POINTER_BUTTON_STATE_RELEASED);
            zwlr_virtual_pointer_v1_frame(g->pointer);
        }
    } else {
        return -1;
    }

    return flush_display(g);
}

static int handle_button(struct globals *g, const char *args) {
    uint32_t code;
    int pressed;
    if (sscanf(args, "%u %d", &code, &pressed) < 2)
        return -1;

    zwlr_virtual_pointer_v1_button(g->pointer, now_ms(), code,
        pressed ? WL_POINTER_BUTTON_STATE_PRESSED
                : WL_POINTER_BUTTON_STATE_RELEASED);
    zwlr_virtual_pointer_v1_frame(g->pointer);
    return flush_display(g);
}

static int create_key_state(struct globals *g) {
    g->xkb_context = xkb_context_new(XKB_CONTEXT_NO_FLAGS);
    if (!g->xkb_context) return -1;
    const struct xkb_rule_names names = {
        .rules = "evdev", .model = "pc105", .layout = "us", .variant = "", .options = ""
    };
    g->keymap = xkb_keymap_new_from_names(g->xkb_context, &names, XKB_KEYMAP_COMPILE_NO_FLAGS);
    if (!g->keymap) return -1;
    g->key_state = xkb_state_new(g->keymap);
    return g->key_state ? 0 : -1;
}

static void destroy_key_state(struct globals *g) {
    xkb_state_unref(g->key_state);
    xkb_keymap_unref(g->keymap);
    xkb_context_unref(g->xkb_context);
    g->key_state = NULL;
    g->keymap = NULL;
    g->xkb_context = NULL;
}

static void send_modifiers(struct globals *g) {
    /* Virtual-keyboard key requests do not update modifier state at the server.
     * Serialize it from exactly the keymap advertised for this keyboard. */
    zwp_virtual_keyboard_v1_modifiers(g->keyboard,
        xkb_state_serialize_mods(g->key_state, XKB_STATE_MODS_DEPRESSED),
        xkb_state_serialize_mods(g->key_state, XKB_STATE_MODS_LATCHED),
        xkb_state_serialize_mods(g->key_state, XKB_STATE_MODS_LOCKED),
        xkb_state_serialize_layout(g->key_state, XKB_STATE_LAYOUT_EFFECTIVE));
}

static int handle_key(struct globals *g, const char *args) {
    uint32_t code;
    int pressed;
    if (!g->keyboard || !g->key_state || sscanf(args, "%u %d", &code, &pressed) != 2 ||
        code > KEY_MAX || (pressed != 0 && pressed != 1)) return -1;
    /* Repeated down packets must not add XKB references; the receiver owns repeat. */
    if (g->key_down[code] == pressed) return 0;
    g->key_down[code] = (unsigned char)pressed;
    zwp_virtual_keyboard_v1_key(g->keyboard, now_ms(), code,
        pressed ? WL_KEYBOARD_KEY_STATE_PRESSED : WL_KEYBOARD_KEY_STATE_RELEASED);
    if (xkb_state_update_key(g->key_state, code + 8, pressed ? XKB_KEY_DOWN : XKB_KEY_UP))
        send_modifiers(g);
    return flush_display(g);
}

/* Map an ASCII character to its evdev keycode on US QWERTY layout.
 * Returns the keycode, and sets *shift=1 if Shift is needed. */
static uint32_t ascii_to_keycode(char c, int *shift) {
    *shift = 0;
    if (c >= 'a' && c <= 'z') {
        static const uint8_t map[] = {
            30,48,46,32,18,33,34,35,23,36,37,38,50,49,24,25,16,19,31,20,
            22,47,17,45,21,44
        };
        return map[c - 'a'];
    }
    if (c >= 'A' && c <= 'Z') {
        *shift = 1;
        static const uint8_t map[] = {
            30,48,46,32,18,33,34,35,23,36,37,38,50,49,24,25,16,19,31,20,
            22,47,17,45,21,44
        };
        return map[c - 'A'];
    }
    switch (c) {
        case '0': return 11;
        case '1': return 2;  case '2': return 3;  case '3': return 4;
        case '4': return 5;  case '5': return 6;  case '6': return 7;
        case '7': return 8;  case '8': return 9;  case '9': return 10;
        case ' ': return 57;
        case '\n': return 28;
        case '\t': return 15;
        case '-': return 12; case '_': *shift=1; return 12;
        case '=': return 13; case '+': *shift=1; return 13;
        case '[': return 26; case '{': *shift=1; return 26;
        case ']': return 27; case '}': *shift=1; return 27;
        case '\\': return 43; case '|': *shift=1; return 43;
        case ';': return 39; case ':': *shift=1; return 39;
        case '\'': return 40; case '"': *shift=1; return 40;
        case '`': return 41; case '~': *shift=1; return 41;
        case ',': return 51; case '<': *shift=1; return 51;
        case '.': return 52; case '>': *shift=1; return 52;
        case '/': return 53; case '?': *shift=1; return 53;
        case '!': *shift=1; return 2;
        case '@': *shift=1; return 3;
        case '#': *shift=1; return 4;
        case '$': *shift=1; return 5;
        case '%': *shift=1; return 6;
        case '^': *shift=1; return 7;
        case '&': *shift=1; return 8;
        case '*': *shift=1; return 9;
        case '(': *shift=1; return 10;
        case ')': *shift=1; return 11;
        default: return 0; /* unknown */
    }
}

static int handle_text(struct globals *g, const char *args) {
    if (!args || !*args) return 0;
    if (!g->keyboard || !g->key_state) return -1;
    xkb_mod_index_t shift_index = xkb_keymap_mod_get_index(g->keymap, XKB_MOD_NAME_SHIFT);
    if (shift_index == XKB_MOD_INVALID || shift_index >= 32) return -1;
    /* Do not release a physical key as a side effect of a synthetic text tap. */
    for (const char *p = args; *p; p++) {
        int shift;
        uint32_t kc = ascii_to_keycode(*p, &shift);
        if (kc && g->key_down[kc]) return -1;
    }
    int rc = 0;
    for (const char *p = args; *p; p++) {
        int shift = 0;
        uint32_t kc = ascii_to_keycode(*p, &shift);
        if (!kc) continue;
        /* Committed text is literal, even while Ctrl, Shift or Caps Lock is held.
         * Override wire modifiers temporarily; preserve the physical XKB state. */
        zwp_virtual_keyboard_v1_modifiers(g->keyboard, shift ? (1u << shift_index) : 0, 0, 0, 0);
        uint32_t t = now_ms();
        zwp_virtual_keyboard_v1_key(g->keyboard, t, kc, WL_KEYBOARD_KEY_STATE_PRESSED);
        zwp_virtual_keyboard_v1_key(g->keyboard, t, kc, WL_KEYBOARD_KEY_STATE_RELEASED);
        if (flush_display(g) < 0) { rc = -1; break; }
    }
    send_modifiers(g);
    if (flush_display(g) < 0) rc = -1;
    return rc;
}

static unsigned finger_scroll_axes;

static int handle_scroll_pixels(struct globals *g, const char *args) {
    double x, y, dx, dy;
    int stop;
    if (sscanf(args, "%lf %lf %lf %lf %d", &x, &y, &dx, &dy, &stop) != 5 ||
        !isfinite(x) || !isfinite(y) || !isfinite(dx) || !isfinite(dy) ||
        fabs(dx) > 1000000.0 || fabs(dy) > 1000000.0 || (stop != 0 && stop != 1))
        return -1;
    /* Wayland axes are logical units. ICED compatibility additionally compensates
     * for physical pixel deltas being interpreted as logical widget offsets. */
    double divisor = input_scale * (iced_scroll_compat ? input_scale : 1.0);
    dx /= divisor;
    dy /= divisor;
    uint32_t t = now_ms();
    if (stop) {
        if (finger_scroll_axes & 1) {
            zwlr_virtual_pointer_v1_axis_stop(g->pointer, t, WL_POINTER_AXIS_HORIZONTAL_SCROLL);
            zwlr_virtual_pointer_v1_axis_source(g->pointer, WL_POINTER_AXIS_SOURCE_FINGER);
        }
        if (finger_scroll_axes & 2) {
            zwlr_virtual_pointer_v1_axis_stop(g->pointer, t, WL_POINTER_AXIS_VERTICAL_SCROLL);
            zwlr_virtual_pointer_v1_axis_source(g->pointer, WL_POINTER_AXIS_SOURCE_FINGER);
        }
        finger_scroll_axes = 0;
    } else {
        if (x < 0) x = 0;
        if (y < 0) y = 0;
        if (x >= viewport_width) x = viewport_width - 1;
        if (y >= viewport_height) y = viewport_height - 1;
        zwlr_virtual_pointer_v1_motion_absolute(g->pointer, t, (uint32_t)x, (uint32_t)y,
                                               viewport_width, viewport_height);
        if (dx != 0) {
            finger_scroll_axes |= 1;
            zwlr_virtual_pointer_v1_axis(g->pointer, t, WL_POINTER_AXIS_HORIZONTAL_SCROLL, wl_fixed_from_double(dx));
            /* Hyprland's axis() replaces pending state; source must follow each axis. */
            zwlr_virtual_pointer_v1_axis_source(g->pointer, WL_POINTER_AXIS_SOURCE_FINGER);
        }
        if (dy != 0) {
            finger_scroll_axes |= 2;
            zwlr_virtual_pointer_v1_axis(g->pointer, t, WL_POINTER_AXIS_VERTICAL_SCROLL, wl_fixed_from_double(dy));
            zwlr_virtual_pointer_v1_axis_source(g->pointer, WL_POINTER_AXIS_SOURCE_FINGER);
        }
    }
    zwlr_virtual_pointer_v1_frame(g->pointer);
    return flush_display(g);
}

static int handle_scroll(struct globals *g, const char *args) {
    double dx, dy;
    if (sscanf(args, "%lf %lf", &dx, &dy) < 2)
        return -1;

    uint32_t t = now_ms();

    /* Send both discrete and continuous (pixel) axis events.
     * axis_discrete rounds to the nearest integer step for apps that
     * only handle discrete scroll (e.g. GTK list views, Firefox). */
    if (dy != 0.0) {
        int32_t discrete_dy = (int32_t)(dy >= 0.0 ? dy + 0.5 : dy - 0.5);
        if (discrete_dy == 0) discrete_dy = dy > 0 ? 1 : -1;
        zwlr_virtual_pointer_v1_axis_discrete(g->pointer, t,
            WL_POINTER_AXIS_VERTICAL_SCROLL,
            wl_fixed_from_double(dy * 15.0), discrete_dy);
        zwlr_virtual_pointer_v1_axis_source(g->pointer, WL_POINTER_AXIS_SOURCE_WHEEL);
    }
    if (dx != 0.0) {
        int32_t discrete_dx = (int32_t)(dx >= 0.0 ? dx + 0.5 : dx - 0.5);
        if (discrete_dx == 0) discrete_dx = dx > 0 ? 1 : -1;
        zwlr_virtual_pointer_v1_axis_discrete(g->pointer, t,
            WL_POINTER_AXIS_HORIZONTAL_SCROLL,
            wl_fixed_from_double(dx * 15.0), discrete_dx);
        zwlr_virtual_pointer_v1_axis_source(g->pointer, WL_POINTER_AXIS_SOURCE_WHEEL);
    }
    zwlr_virtual_pointer_v1_frame(g->pointer);
    return flush_display(g);
}

static int handle_pointer_relative(struct globals *g, const char *args) {
    double dx, dy;
    if (sscanf(args, "%lf %lf", &dx, &dy) < 2)
        return -1;

    zwlr_virtual_pointer_v1_motion(g->pointer, now_ms(),
        wl_fixed_from_double(dx), wl_fixed_from_double(dy));
    zwlr_virtual_pointer_v1_frame(g->pointer);
    return flush_display(g);
}

static int process_command(struct globals *g, char *line) {
    /* Strip trailing newline. */
    size_t len = strlen(line);
    if (len > 0 && line[len - 1] == '\n') line[--len] = '\0';
    if (len > 0 && line[len - 1] == '\r') line[--len] = '\0';
    if (len == 0) return 0;

    /* Parse command word. */
    char *space = strchr(line, ' ');
    const char *cmd = line;
    const char *args = "";
    if (space) {
        *space = '\0';
        args = space + 1;
    }

    int rc;
    if (strcmp(cmd, "pointer") == 0)
        rc = handle_pointer(g, args);
    else if (strcmp(cmd, "touch") == 0)
        rc = handle_touch(g, args);
    else if (strcmp(cmd, "button") == 0)
        rc = handle_button(g, args);
    else if (strcmp(cmd, "key") == 0)
        rc = handle_key(g, args);
    else if (strcmp(cmd, "scroll") == 0)
        rc = handle_scroll(g, args);
    else if (strcmp(cmd, "scroll_pixels") == 0)
        rc = handle_scroll_pixels(g, args);
    else if (strcmp(cmd, "pointer_relative") == 0)
        rc = handle_pointer_relative(g, args);
    else if (strcmp(cmd, "text") == 0)
        rc = handle_text(g, args);
    else if (strcmp(cmd, "cursor_visibility") == 0)
        rc = 0; /* No-op for now. */
    else {
        fprintf(stderr, "input-wl: unknown command: %s\n", cmd);
        rc = -1;
    }

    return rc;
}

int main(int argc, char **argv) {
    struct globals g = {0};
    struct wl_registry *registry;
    char line[MAX_LINE];

    /* Parse optional viewport dimensions. */
    if (argc >= 3) {
        viewport_width = (uint32_t)atoi(argv[1]);
        viewport_height = (uint32_t)atoi(argv[2]);
    }
    if (viewport_width == 0 || viewport_height == 0) {
        fprintf(stderr, "usage: enzremote-input-wl [WIDTH HEIGHT]\n");
        return 2;
    }

    const char *scale = getenv("ENZREMOTE_INPUT_SCALE");
    if (scale) {
        char *end;
        input_scale = strtod(scale, &end);
        if (*end || !isfinite(input_scale) || input_scale < 0.25 || input_scale > 8.0) {
            fprintf(stderr, "input-wl: invalid output scale\n");
            return 2;
        }
    }
    const char *compat = getenv("ENZREMOTE_ICED_SCROLL_COMPAT");
    if (compat) {
        if (strcmp(compat, "0") != 0 && strcmp(compat, "1") != 0) return 2;
        iced_scroll_compat = strcmp(compat, "1") == 0;
    }
    memset(touch_active, 0, sizeof(touch_active));
    g.requested_output=getenv("ENZREMOTE_INPUT_OUTPUT");
    if (g.requested_output && (!*g.requested_output || strlen(g.requested_output)>80)) return 2;

    g.display = wl_display_connect(NULL);
    if (!g.display) {
        fprintf(stderr, "input-wl: cannot connect to Wayland display\n");
        return 1;
    }

    registry = wl_display_get_registry(g.display);
    wl_registry_add_listener(registry, &registry_listener, &g);
    if (wl_display_roundtrip(g.display) < 0 || !g.seat || !g.pointer_manager) {
        fprintf(stderr, "input-wl: compositor lacks seat or "
                "zwlr_virtual_pointer_manager_v1\n");
        if (registry) wl_registry_destroy(registry);
        wl_display_disconnect(g.display);
        return 1;
    }

    if (g.requested_output) {
        if (wl_display_roundtrip(g.display)<0) return 1;
        for (unsigned i=0;i<32;++i) if (g.outputs[i].output && strcmp(g.outputs[i].name,g.requested_output)==0) g.selected_output=g.outputs[i].output;
        if (!g.selected_output || wl_proxy_get_version((struct wl_proxy *)g.pointer_manager)<2) {
            fprintf(stderr,"input-wl: requested output is unavailable\n");
            for (unsigned i=0;i<32;++i) if (g.outputs[i].output) wl_output_release(g.outputs[i].output);
            if (g.keyboard_manager) zwp_virtual_keyboard_manager_v1_destroy(g.keyboard_manager);
            zwlr_virtual_pointer_manager_v1_destroy(g.pointer_manager);
            wl_seat_destroy(g.seat); wl_registry_destroy(registry);
            wl_display_disconnect(g.display); return 1;
        }
        g.pointer=zwlr_virtual_pointer_manager_v1_create_virtual_pointer_with_output(g.pointer_manager,g.seat,g.selected_output);
    } else {
        g.pointer = zwlr_virtual_pointer_manager_v1_create_virtual_pointer(g.pointer_manager,g.seat);
    }
    if (!g.pointer) {
        fprintf(stderr, "input-wl: failed to create virtual pointer\n");
        wl_registry_destroy(registry);
        wl_display_disconnect(g.display);
        return 1;
    }

    /* Create a virtual keyboard if the protocol is available. */
    if (g.keyboard_manager && g.seat) {
        g.keyboard = zwp_virtual_keyboard_manager_v1_create_virtual_keyboard(
            g.keyboard_manager, g.seat);
        if (g.keyboard) {
            int ready = 0;
            char *keymap_str = NULL;
            if (create_key_state(&g) == 0)
                keymap_str = xkb_keymap_get_as_string(g.keymap, XKB_KEYMAP_FORMAT_TEXT_V1);
            if (keymap_str) {
                size_t keymap_len = strlen(keymap_str) + 1;
                int fd = memfd_create("enzremote-keymap", MFD_CLOEXEC);
                if (fd >= 0) {
                    if (ftruncate(fd, (off_t)keymap_len) == 0) {
                        void *map = mmap(NULL, keymap_len, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
                        if (map != MAP_FAILED) {
                            memcpy(map, keymap_str, keymap_len);
                            munmap(map, keymap_len);
                            zwp_virtual_keyboard_v1_keymap(g.keyboard, WL_KEYBOARD_KEYMAP_FORMAT_XKB_V1,
                                fd, (uint32_t)keymap_len);
                            ready = wl_display_roundtrip(g.display) >= 0;
                        }
                    }
                    close(fd);
                }
                free(keymap_str);
            }
            if (ready) {
                fprintf(stderr, "input-wl: virtual keyboard ready\n");
            } else {
                fprintf(stderr, "input-wl: virtual keyboard keymap setup failed\n");
                zwp_virtual_keyboard_v1_destroy(g.keyboard);
                g.keyboard = NULL;
                destroy_key_state(&g);
            }
        } else {
            fprintf(stderr, "input-wl: failed to create virtual keyboard\n");
        }
    } else {
        fprintf(stderr, "input-wl: no virtual keyboard manager; key requests unavailable\n");
    }

    /* Signal readiness to parent process. */
    printf("READY %ux%u\n", viewport_width, viewport_height);
    fflush(stdout);

    size_t used = 0;
    int invalid_line = 0;
    for (;;) {
        if (g.requested_output) {
            if (wl_display_dispatch_pending(g.display)<0 || g.output_lost) break;
            struct pollfd events[2] = {
                {.fd=STDIN_FILENO,.events=POLLIN},
                {.fd=wl_display_get_fd(g.display),.events=POLLIN}
            };
            int ready=poll(events,2,-1);
            if (ready<0) { if (errno==EINTR) continue; break; }
            if (events[1].revents&(POLLERR|POLLHUP|POLLNVAL)) break;
            if ((events[1].revents&POLLIN) && wl_display_dispatch(g.display)<0) break;
            if (g.output_lost || (events[0].revents&(POLLERR|POLLNVAL))) break;
            if (!(events[0].revents&(POLLIN|POLLHUP))) continue;
        }
        char chunk[MAX_LINE];
        ssize_t count=read(STDIN_FILENO,chunk,sizeof(chunk));
        if (count<0) { if (errno==EINTR) continue; break; }
        if (count==0) break;
        for (ssize_t i=0;i<count;++i) {
            if (chunk[i]=='\n') {
                line[used]='\0';
                int rc=invalid_line?-1:process_command(&g,line);
                printf(rc==0?"OK\n":"ERR\n");
                fflush(stdout);
                used=0;invalid_line=0;
            } else if (chunk[i]=='\0') invalid_line=1;
            else if (used<sizeof(line)-1) line[used++]=chunk[i];
            else invalid_line=1;
        }
    }

    /* Cleanup. */
    if (g.keyboard) zwp_virtual_keyboard_v1_destroy(g.keyboard);
    if (g.keyboard_manager) zwp_virtual_keyboard_manager_v1_destroy(g.keyboard_manager);
    destroy_key_state(&g);
    zwlr_virtual_pointer_v1_destroy(g.pointer);
    zwlr_virtual_pointer_manager_v1_destroy(g.pointer_manager);
    wl_seat_destroy(g.seat);
    for (unsigned i=0;i<32;++i) if (g.outputs[i].output) wl_output_release(g.outputs[i].output);
    wl_registry_destroy(registry);
    wl_display_disconnect(g.display);

    return 0;
}
