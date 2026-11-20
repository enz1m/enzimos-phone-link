#define main input_worker_main
#include "enzremote-input-wl.c"
#undef main

#include <assert.h>
#include <stdarg.h>

struct request {
    uint32_t opcode, axis, source, key, pressed, depressed, latched, locked, group;
    double delta;
    int keyboard;
};
static struct request recorded[512];
static unsigned recorded_count;

uint32_t wl_proxy_get_version(struct wl_proxy *proxy) { (void)proxy; return 2; }
int wl_display_flush(struct wl_display *display) { (void)display; return 0; }

struct wl_proxy *wl_proxy_marshal_flags(struct wl_proxy *proxy, uint32_t opcode,
        const struct wl_interface *interface, uint32_t version, uint32_t flags, ...) {
    (void)interface; (void)version; (void)flags;
    assert(recorded_count < 512);
    struct request *r = &recorded[recorded_count++];
    *r = (struct request){.opcode = opcode, .keyboard = proxy == (struct wl_proxy *)2};
    va_list args; va_start(args, flags);
    if (r->keyboard && opcode == ZWP_VIRTUAL_KEYBOARD_V1_KEY) {
        (void)va_arg(args, uint32_t);
        r->key = va_arg(args, uint32_t);
        r->pressed = va_arg(args, uint32_t);
    } else if (r->keyboard && opcode == ZWP_VIRTUAL_KEYBOARD_V1_MODIFIERS) {
        r->depressed = va_arg(args, uint32_t);
        r->latched = va_arg(args, uint32_t);
        r->locked = va_arg(args, uint32_t);
        r->group = va_arg(args, uint32_t);
    } else if (opcode == ZWLR_VIRTUAL_POINTER_V1_AXIS) {
        (void)va_arg(args, uint32_t);
        r->axis = va_arg(args, uint32_t);
        r->delta = wl_fixed_to_double(va_arg(args, wl_fixed_t));
    } else if (opcode == ZWLR_VIRTUAL_POINTER_V1_AXIS_SOURCE) {
        r->source = va_arg(args, uint32_t);
    } else if (opcode == ZWLR_VIRTUAL_POINTER_V1_AXIS_STOP) {
        (void)va_arg(args, uint32_t);
        r->axis = va_arg(args, uint32_t);
    }
    va_end(args);
    return NULL;
}

static void assert_typed(struct globals *g, const char *expected) {
    struct xkb_state *receiver = xkb_state_new(g->keymap);
    assert(receiver);
    char actual[256] = {0};
    size_t length = 0;
    unsigned presses = 0, releases = 0;
    for (unsigned i = 0; i < recorded_count; i++) {
        struct request *r = &recorded[i];
        assert(r->keyboard);
        if (r->opcode == ZWP_VIRTUAL_KEYBOARD_V1_MODIFIERS) {
            xkb_state_update_mask(receiver, r->depressed, r->latched, r->locked, 0, 0, r->group);
        } else if (r->opcode == ZWP_VIRTUAL_KEYBOARD_V1_KEY) {
            if (r->pressed) {
                presses++;
                int count = xkb_state_key_get_utf8(receiver, r->key + 8, actual + length, sizeof(actual) - length);
                assert(count >= 0 && (size_t)count < sizeof(actual) - length);
                length += (size_t)count;
            } else releases++;
        }
    }
    assert(strcmp(actual, expected) == 0);
    assert(presses == releases);
    xkb_state_unref(receiver);
}

static void test_keyboard(void) {
    struct globals g = {.keyboard = (struct zwp_virtual_keyboard_v1 *)2};
    assert(create_key_state(&g) == 0);
    recorded_count = 0;
    char printable[96];
    for (int i = 0; i < 95; i++) printable[i] = (char)(i + 32);
    printable[95] = 0;
    assert(handle_text(&g, printable) == 0);
    assert_typed(&g, printable);
    assert(recorded[recorded_count - 1].depressed == 0);
    assert(recorded[recorded_count - 1].locked == 0);

    recorded_count = 0;
    assert(handle_key(&g, "29 1") == 0);
    assert(xkb_state_mod_name_is_active(g.key_state, XKB_MOD_NAME_CTRL, XKB_STATE_MODS_DEPRESSED));
    unsigned before = recorded_count;
    assert(handle_key(&g, "29 1") == 0);
    assert(recorded_count == before);
    assert(handle_key(&g, "29 0") == 0);
    assert(!xkb_state_mod_name_is_active(g.key_state, XKB_MOD_NAME_CTRL, XKB_STATE_MODS_DEPRESSED));
    assert(recorded[recorded_count - 1].depressed == 0);

    recorded_count = 0;
    assert(handle_key(&g, "42 1") == 0);
    assert(handle_key(&g, "54 1") == 0);
    assert(handle_key(&g, "42 0") == 0);
    assert(xkb_state_mod_name_is_active(g.key_state, XKB_MOD_NAME_SHIFT, XKB_STATE_MODS_DEPRESSED));
    assert(handle_key(&g, "54 0") == 0);
    assert(!xkb_state_mod_name_is_active(g.key_state, XKB_MOD_NAME_SHIFT, XKB_STATE_MODS_DEPRESSED));

    recorded_count = 0;
    assert(handle_key(&g, "58 1") == 0);
    assert(handle_key(&g, "58 0") == 0);
    assert(xkb_state_mod_name_is_active(g.key_state, XKB_MOD_NAME_CAPS, XKB_STATE_MODS_LOCKED));
    assert(handle_key(&g, "29 1") == 0);
    recorded_count = 0;
    assert(handle_text(&g, "Aa!a") == 0);
    assert_typed(&g, "Aa!a");
    struct request *last = &recorded[recorded_count - 1];
    assert(last->depressed != 0 && last->locked != 0);
    assert(xkb_state_mod_name_is_active(g.key_state, XKB_MOD_NAME_CTRL, XKB_STATE_MODS_DEPRESSED));
    assert(handle_key(&g, "29 0") == 0);

    recorded_count = 0;
    assert(handle_key(&g, "30 1") == 0);
    before = recorded_count;
    assert(handle_text(&g, "ab") == -1);
    assert(recorded_count == before);
    assert(handle_key(&g, "30 0") == 0);
    recorded_count = 0;
    assert(handle_key(&g, "4294967295 1") == -1);
    assert(handle_key(&g, "42 2") == -1);
    assert(handle_key(&g, "42") == -1);
    assert(recorded_count == 0);
    destroy_key_state(&g);
    puts("input-wl: literal ASCII, modifiers, locks, repeat and held-key safety passed");
}

int main(void) {
    struct globals g = {.pointer = (struct zwlr_virtual_pointer_v1 *)1};
    input_scale = 2.0;
    assert(handle_scroll_pixels(&g, "100 200 0 0.25 0") == 0);
    assert(recorded_count == 4);
    assert(recorded[0].opcode == ZWLR_VIRTUAL_POINTER_V1_MOTION_ABSOLUTE);
    assert(recorded[1].opcode == ZWLR_VIRTUAL_POINTER_V1_AXIS);
    assert(recorded[1].axis == WL_POINTER_AXIS_VERTICAL_SCROLL);
    assert(recorded[1].delta == 0.125);
    assert(recorded[2].opcode == ZWLR_VIRTUAL_POINTER_V1_AXIS_SOURCE);
    assert(recorded[2].source == WL_POINTER_AXIS_SOURCE_FINGER);
    assert(recorded[3].opcode == ZWLR_VIRTUAL_POINTER_V1_FRAME);

    recorded_count = 0;
    assert(handle_scroll_pixels(&g, "100 200 0 0 1") == 0);
    assert(recorded_count == 3);
    assert(recorded[0].opcode == ZWLR_VIRTUAL_POINTER_V1_AXIS_STOP);
    assert(recorded[0].axis == WL_POINTER_AXIS_VERTICAL_SCROLL);
    assert(recorded[1].opcode == ZWLR_VIRTUAL_POINTER_V1_AXIS_SOURCE);
    assert(finger_scroll_axes == 0);

    recorded_count = 0;
    assert(handle_scroll_pixels(&g, "100 200 -2 4 0") == 0);
    assert(recorded_count == 6);
    assert(recorded[1].delta == -1 && recorded[1].axis == WL_POINTER_AXIS_HORIZONTAL_SCROLL);
    assert(recorded[2].opcode == ZWLR_VIRTUAL_POINTER_V1_AXIS_SOURCE);
    assert(recorded[3].delta == 2 && recorded[3].axis == WL_POINTER_AXIS_VERTICAL_SCROLL);
    assert(recorded[4].opcode == ZWLR_VIRTUAL_POINTER_V1_AXIS_SOURCE);

    recorded_count = 0;
    iced_scroll_compat = 1;
    assert(handle_scroll_pixels(&g, "100 200 0 0.25 0") == 0);
    assert(recorded[1].delta == 0.0625);
    iced_scroll_compat = 0;

    recorded_count = 0;
    assert(handle_scroll(&g, "1 1") == 0);
    assert(recorded_count == 5);
    assert(recorded[0].opcode == ZWLR_VIRTUAL_POINTER_V1_AXIS_DISCRETE);
    assert(recorded[1].opcode == ZWLR_VIRTUAL_POINTER_V1_AXIS_SOURCE && recorded[1].source == WL_POINTER_AXIS_SOURCE_WHEEL);
    assert(recorded[2].opcode == ZWLR_VIRTUAL_POINTER_V1_AXIS_DISCRETE);
    assert(recorded[3].opcode == ZWLR_VIRTUAL_POINTER_V1_AXIS_SOURCE && recorded[3].source == WL_POINTER_AXIS_SOURCE_WHEEL);

    recorded_count = 0;
    assert(handle_scroll_pixels(&g, "0 0 nan 0 0") == -1);
    assert(handle_scroll_pixels(&g, "0 0 1 0 2") == -1);
    assert(recorded_count == 0);
    puts("input-wl: pixel units, source ordering, axis stops and validation passed");
    test_keyboard();
    return 0;
}
