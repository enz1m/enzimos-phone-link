#include "wlr-virtual-pointer-unstable-v1-client-protocol.h"

#include <errno.h>
#include <linux/input-event-codes.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <wayland-client.h>

struct globals {
    struct wl_seat *seat;
    struct zwlr_virtual_pointer_manager_v1 *pointer_manager;
};

static void registry_global(void *data, struct wl_registry *registry,
                            uint32_t name, const char *interface,
                            uint32_t version) {
    struct globals *globals = data;

    if (strcmp(interface, wl_seat_interface.name) == 0) {
        uint32_t bind_version = version < 7 ? version : 7;
        globals->seat = wl_registry_bind(registry, name, &wl_seat_interface,
                                         bind_version);
    } else if (strcmp(interface,
                      zwlr_virtual_pointer_manager_v1_interface.name) == 0) {
        uint32_t bind_version = version < 2 ? version : 2;
        globals->pointer_manager = wl_registry_bind(
            registry, name, &zwlr_virtual_pointer_manager_v1_interface,
            bind_version);
    }
}

static void registry_global_remove(void *data, struct wl_registry *registry,
                                   uint32_t name) {
    (void)data;
    (void)registry;
    (void)name;
}

static const struct wl_registry_listener registry_listener = {
    .global = registry_global,
    .global_remove = registry_global_remove,
};

static int parse_coordinate(const char *value, uint32_t *result) {
    char *end = NULL;
    unsigned long parsed;

    errno = 0;
    parsed = strtoul(value, &end, 10);
    if (errno != 0 || *value == '\0' || *end != '\0' ||
        parsed > UINT32_MAX)
        return -1;
    *result = (uint32_t)parsed;
    return 0;
}

static uint32_t monotonic_milliseconds(void) {
    struct timespec now;

    if (clock_gettime(CLOCK_MONOTONIC, &now) != 0)
        return 0;
    return (uint32_t)((uint64_t)now.tv_sec * 1000 + now.tv_nsec / 1000000);
}

int main(int argc, char **argv) {
    struct wl_display *display;
    struct wl_registry *registry;
    struct zwlr_virtual_pointer_v1 *pointer;
    struct globals globals = {0};
    uint32_t width, height, x, y, time;
    int click;

    if ((argc != 5 && argc != 6) ||
        (argc == 6 && strcmp(argv[5], "--click") != 0)) {
        fprintf(stderr,
                "usage: enzremote-input-pointer WIDTH HEIGHT X Y [--click]\n");
        return 2;
    }
    if (parse_coordinate(argv[1], &width) < 0 ||
        parse_coordinate(argv[2], &height) < 0 ||
        parse_coordinate(argv[3], &x) < 0 ||
        parse_coordinate(argv[4], &y) < 0 || width == 0 || height == 0 ||
        x >= width || y >= height) {
        fprintf(stderr, "coordinates must be integers inside a non-empty viewport\n");
        return 2;
    }
    click = argc == 6;

    display = wl_display_connect(NULL);
    if (display == NULL) {
        fprintf(stderr, "could not connect to the Wayland display\n");
        return 1;
    }
    registry = wl_display_get_registry(display);
    wl_registry_add_listener(registry, &registry_listener, &globals);
    if (wl_display_roundtrip(display) < 0 || globals.seat == NULL ||
        globals.pointer_manager == NULL) {
        fprintf(stderr,
                "Wayland display lacks a seat or virtual pointer manager\n");
        wl_registry_destroy(registry);
        wl_display_disconnect(display);
        return 1;
    }

    pointer = zwlr_virtual_pointer_manager_v1_create_virtual_pointer(
        globals.pointer_manager, globals.seat);
    time = monotonic_milliseconds();
    zwlr_virtual_pointer_v1_motion_absolute(pointer, time, x, y, width, height);
    zwlr_virtual_pointer_v1_frame(pointer);
    if (wl_display_roundtrip(display) < 0)
        return 1;

    if (click) {
        zwlr_virtual_pointer_v1_button(pointer, monotonic_milliseconds(),
                                       BTN_LEFT,
                                       WL_POINTER_BUTTON_STATE_PRESSED);
        zwlr_virtual_pointer_v1_frame(pointer);
        if (wl_display_roundtrip(display) < 0)
            return 1;
        zwlr_virtual_pointer_v1_button(pointer, monotonic_milliseconds(),
                                       BTN_LEFT,
                                       WL_POINTER_BUTTON_STATE_RELEASED);
        zwlr_virtual_pointer_v1_frame(pointer);
        if (wl_display_roundtrip(display) < 0)
            return 1;
    }

    printf("pointer event sent: viewport=%ux%u position=%u,%u click=%s\n",
           width, height, x, y, click ? "true" : "false");
    zwlr_virtual_pointer_v1_destroy(pointer);
    zwlr_virtual_pointer_manager_v1_destroy(globals.pointer_manager);
    wl_seat_destroy(globals.seat);
    wl_registry_destroy(registry);
    wl_display_disconnect(display);
    return 0;
}
