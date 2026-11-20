#include <libei.h>

#include <errno.h>
#include <linux/input-event-codes.h>
#include <poll.h>
#include <math.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

#define MAX_TOUCH_SLOTS 16

struct worker_state {
    struct ei *ei;
    struct ei_device *device;
    struct ei_touch *touches[MAX_TOUCH_SLOTS];
    int ready_announced;
    uint32_t sequence;
};

static int request_eis_fd_from_socket(int socket_fd) {
    struct msghdr message = {0};
    struct iovec iov;
    char payload = 'E';
    char control[CMSG_SPACE(sizeof(int))];
    struct cmsghdr *header;
    if (write(socket_fd, &payload, 1) != 1) {
        perror("could not request EIS descriptor");
        return -1;
    }

    iov.iov_base = &payload;
    iov.iov_len = 1;
    message.msg_iov = &iov;
    message.msg_iovlen = 1;
    message.msg_control = control;
    message.msg_controllen = sizeof(control);
    if (recvmsg(socket_fd, &message, MSG_CMSG_CLOEXEC) != 1 || payload != 'E') {
        fprintf(stderr, "EIS broker returned an invalid response\n");
        return -1;
    }
    header = CMSG_FIRSTHDR(&message);
    if (header == NULL || header->cmsg_level != SOL_SOCKET ||
        header->cmsg_type != SCM_RIGHTS || header->cmsg_len < CMSG_LEN(sizeof(int))) {
        fprintf(stderr, "EIS broker response did not contain a descriptor\n");
        return -1;
    }
    return *(int *)CMSG_DATA(header);
}

static int request_eis_fd(const char *path) {
    struct sockaddr_un address = {.sun_family = AF_UNIX};
    int eis_fd, socket_fd;

    if (strlen(path) >= sizeof(address.sun_path)) {
        fprintf(stderr, "broker socket path is too long\n");
        return -1;
    }
    strcpy(address.sun_path, path);
    socket_fd = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
    if (socket_fd < 0 ||
        connect(socket_fd, (struct sockaddr *)&address, sizeof(address)) < 0) {
        perror("could not connect to EIS broker");
        if (socket_fd >= 0)
            close(socket_fd);
        return -1;
    }
    eis_fd = request_eis_fd_from_socket(socket_fd);
    close(socket_fd);
    return eis_fd;
}

static int parse_coordinate(const char *value, double *result) {
    char *end = NULL;
    double parsed;

    errno = 0;
    parsed = strtod(value, &end);
    if (errno != 0 || *value == '\0' || *end != '\0' || parsed < 0.0)
        return -1;
    *result = parsed;
    return 0;
}

static int send_smoke_events(struct ei *ei, struct ei_device *device, double x,
                             double y) {
    struct ei_region *region;
    struct ei_touch *first, *second;
    double second_x, second_y;

    if (!ei_device_has_capability(device, EI_DEVICE_CAP_POINTER_ABSOLUTE) ||
        !ei_device_has_capability(device, EI_DEVICE_CAP_BUTTON) ||
        !ei_device_has_capability(device, EI_DEVICE_CAP_KEYBOARD) ||
        !ei_device_has_capability(device, EI_DEVICE_CAP_TOUCH)) {
        fprintf(stderr, "EIS device lacks required input capabilities\n");
        return -1;
    }
    region = ei_device_get_region_at(device, x, y);
    if (region == NULL) {
        fprintf(stderr, "coordinates are outside the EIS device region\n");
        return -1;
    }

    second_x = x + 16.0;
    second_y = y + 16.0;
    if (!ei_region_contains(region, second_x, second_y)) {
        second_x = x > 16.0 ? x - 16.0 : x;
        second_y = y > 16.0 ? y - 16.0 : y;
    }

    ei_device_start_emulating(device, 1);

    ei_device_pointer_motion_absolute(device, x, y);
    ei_device_frame(device, ei_now(ei));
    ei_device_button_button(device, BTN_LEFT, true);
    ei_device_frame(device, ei_now(ei));
    ei_device_button_button(device, BTN_LEFT, false);
    ei_device_frame(device, ei_now(ei));

    ei_device_keyboard_key(device, KEY_A, true);
    ei_device_frame(device, ei_now(ei));
    ei_device_keyboard_key(device, KEY_A, false);
    ei_device_frame(device, ei_now(ei));

    first = ei_device_touch_new(device);
    second = ei_device_touch_new(device);
    if (first == NULL || second == NULL) {
        fprintf(stderr, "could not create EIS touch points\n");
        first = ei_touch_unref(first);
        second = ei_touch_unref(second);
        ei_device_stop_emulating(device);
        return -1;
    }
    ei_touch_down(first, x, y);
    ei_touch_down(second, second_x, second_y);
    ei_device_frame(device, ei_now(ei));
    ei_touch_motion(first, second_x, second_y);
    ei_device_frame(device, ei_now(ei));
    ei_touch_up(first);
    ei_touch_cancel(second);
    ei_device_frame(device, ei_now(ei));
    first = ei_touch_unref(first);
    second = ei_touch_unref(second);

    ei_device_stop_emulating(device);
    printf("EIS smoke events sent: mapping=%s position=%.0f,%.0f\n",
           ei_region_get_mapping_id(region), x, y);
    return 0;
}

static int send_pointer_event(struct ei *ei, struct ei_device *device, double x,
                              double y, int click) {
    struct ei_region *region;

    if (!ei_device_has_capability(device, EI_DEVICE_CAP_POINTER_ABSOLUTE) ||
        (click && !ei_device_has_capability(device, EI_DEVICE_CAP_BUTTON))) {
        fprintf(stderr, "EIS device lacks required pointer capabilities\n");
        return -1;
    }
    region = ei_device_get_region_at(device, x, y);
    if (region == NULL) {
        fprintf(stderr, "coordinates are outside the EIS device region\n");
        return -1;
    }

    ei_device_start_emulating(device, 1);
    ei_device_pointer_motion_absolute(device, x, y);
    ei_device_frame(device, ei_now(ei));
    if (click) {
        ei_device_button_button(device, BTN_LEFT, true);
        ei_device_frame(device, ei_now(ei));
        ei_device_button_button(device, BTN_LEFT, false);
        ei_device_frame(device, ei_now(ei));
    }
    ei_device_stop_emulating(device);
    printf("EIS pointer event sent: mapping=%s position=%.0f,%.0f click=%s\n",
           ei_region_get_mapping_id(region), x, y, click ? "true" : "false");
    return 0;
}

static void worker_clear_device(struct worker_state *state) {
    for (size_t i = 0; i < MAX_TOUCH_SLOTS; ++i)
        state->touches[i] = ei_touch_unref(state->touches[i]);
    if (state->device != NULL) {
        ei_device_unref(state->device);
        state->device = NULL;
    }
}

static int worker_handle_events(struct worker_state *state) {
    struct ei_event *event;

    ei_dispatch(state->ei);
    while ((event = ei_get_event(state->ei)) != NULL) {
        struct ei_device *device = ei_event_get_device(event);
        switch (ei_event_get_type(event)) {
        case EI_EVENT_SEAT_ADDED:
            ei_seat_bind_capabilities(
                ei_event_get_seat(event), EI_DEVICE_CAP_POINTER,
                EI_DEVICE_CAP_POINTER_ABSOLUTE, EI_DEVICE_CAP_BUTTON,
                EI_DEVICE_CAP_SCROLL, EI_DEVICE_CAP_KEYBOARD,
                EI_DEVICE_CAP_TOUCH, NULL);
            break;
        case EI_EVENT_DEVICE_RESUMED:
            worker_clear_device(state);
            state->device = ei_device_ref(device);
            ei_device_start_emulating(state->device, ++state->sequence);
            if (!state->ready_announced) {
                puts("READY");
                fflush(stdout);
                state->ready_announced = 1;
            }
            break;
        case EI_EVENT_DEVICE_PAUSED:
        case EI_EVENT_DEVICE_REMOVED:
            if (device == state->device)
                worker_clear_device(state);
            break;
        case EI_EVENT_DISCONNECT:
            ei_event_unref(event);
            return -1;
        default:
            break;
        }
        ei_event_unref(event);
    }
    return 0;
}

static int worker_pointer(struct worker_state *state, double x, double y,
                          int click) {
    if (state->device == NULL ||
        ei_device_get_region_at(state->device, x, y) == NULL)
        return -1;
    ei_device_pointer_motion_absolute(state->device, x, y);
    ei_device_frame(state->device, ei_now(state->ei));
    if (click) {
        ei_device_button_button(state->device, BTN_LEFT, true);
        ei_device_frame(state->device, ei_now(state->ei));
        ei_device_button_button(state->device, BTN_LEFT, false);
        ei_device_frame(state->device, ei_now(state->ei));
    }
    return 0;
}

static int worker_key(struct worker_state *state, unsigned int code,
                      int pressed) {
    if (state->device == NULL ||
        !ei_device_has_capability(state->device, EI_DEVICE_CAP_KEYBOARD))
        return -1;
    ei_device_keyboard_key(state->device, code, pressed != 0);
    ei_device_frame(state->device, ei_now(state->ei));
    return 0;
}

static int worker_touch(struct worker_state *state, unsigned int slot,
                        const char *phase, double x, double y) {
    struct ei_touch *touch;

    if (state->device == NULL || slot >= MAX_TOUCH_SLOTS ||
        !ei_device_has_capability(state->device, EI_DEVICE_CAP_TOUCH))
        return -1;
    touch = state->touches[slot];
    if (strcmp(phase, "down") == 0) {
        if (touch != NULL || ei_device_get_region_at(state->device, x, y) == NULL)
            return -1;
        touch = ei_device_touch_new(state->device);
        if (touch == NULL)
            return -1;
        state->touches[slot] = touch;
        ei_touch_down(touch, x, y);
    } else if (strcmp(phase, "move") == 0) {
        if (touch == NULL || ei_device_get_region_at(state->device, x, y) == NULL)
            return -1;
        ei_touch_motion(touch, x, y);
    } else if (strcmp(phase, "up") == 0) {
        if (touch == NULL)
            return -1;
        ei_touch_up(touch);
        state->touches[slot] = ei_touch_unref(touch);
    } else if (strcmp(phase, "cancel") == 0) {
        if (touch == NULL)
            return -1;
        ei_touch_cancel(touch);
        state->touches[slot] = ei_touch_unref(touch);
    } else {
        return -1;
    }
    ei_device_frame(state->device, ei_now(state->ei));
    return 0;
}

static int worker_scroll(struct worker_state *state, double dx, double dy) {
    if (state->device == NULL ||
        !ei_device_has_capability(state->device, EI_DEVICE_CAP_SCROLL))
        return -1;
    ei_device_scroll_delta(state->device, dx, dy);
    ei_device_frame(state->device, ei_now(state->ei));
    return 0;
}

static int worker_pointer_relative(struct worker_state *state, double dx,
                                   double dy) {
    if (state->device == NULL ||
        !ei_device_has_capability(state->device, EI_DEVICE_CAP_POINTER))
        return -1;
    ei_device_pointer_motion(state->device, dx, dy);
    ei_device_frame(state->device, ei_now(state->ei));
    return 0;
}

static int worker_button(struct worker_state *state, unsigned int code,
                          int pressed) {
    if (state->device == NULL ||
        !ei_device_has_capability(state->device, EI_DEVICE_CAP_BUTTON))
        return -1;
    ei_device_button_button(state->device, code, pressed != 0);
    ei_device_frame(state->device, ei_now(state->ei));
    return 0;
}

/*
 * US QWERTY ASCII-to-keycode mapping for text input.
 * Returns the Linux input keycode for a printable ASCII character, or 0
 * if the character is not mapped. The shift flag is set to 1 when the
 * character requires the Shift modifier.
 */
struct ascii_key {
    unsigned int code;
    int shift;
};

static struct ascii_key ascii_to_key(char c) {
    struct ascii_key result = {0, 0};

    if (c >= 'a' && c <= 'z') {
        /* KEY_Q=16 ... KEY_P=25, KEY_A=30 ... KEY_L=38, KEY_Z=44 ... KEY_M=50 */
        static const unsigned int lower[] = {
            /* a-z */
            30, 48, 46, 32, 18, 33, 34, 35, 23, 36,
            37, 38, 50, 49, 24, 25, 16, 19, 31, 20,
            22, 47, 17, 45, 21, 44
        };
        result.code = lower[c - 'a'];
    } else if (c >= 'A' && c <= 'Z') {
        static const unsigned int lower[] = {
            30, 48, 46, 32, 18, 33, 34, 35, 23, 36,
            37, 38, 50, 49, 24, 25, 16, 19, 31, 20,
            22, 47, 17, 45, 21, 44
        };
        result.code = lower[c - 'A'];
        result.shift = 1;
    } else if (c >= '0' && c <= '9') {
        /* KEY_1=2 ... KEY_0=11 */
        result.code = (c == '0') ? 11 : (unsigned int)(c - '0') + 1;
    } else {
        switch (c) {
        case ' ':  result.code = 57; break;  /* KEY_SPACE */
        case '\n': result.code = 28; break;  /* KEY_ENTER */
        case '\t': result.code = 15; break;  /* KEY_TAB */
        case '-':  result.code = 12; break;  /* KEY_MINUS */
        case '=':  result.code = 13; break;  /* KEY_EQUAL */
        case '[':  result.code = 26; break;  /* KEY_LEFTBRACE */
        case ']':  result.code = 27; break;  /* KEY_RIGHTBRACE */
        case '\\': result.code = 43; break;  /* KEY_BACKSLASH */
        case ';':  result.code = 39; break;  /* KEY_SEMICOLON */
        case '\'': result.code = 40; break;  /* KEY_APOSTROPHE */
        case '`':  result.code = 41; break;  /* KEY_GRAVE */
        case ',':  result.code = 51; break;  /* KEY_COMMA */
        case '.':  result.code = 52; break;  /* KEY_DOT */
        case '/':  result.code = 53; break;  /* KEY_SLASH */
        /* Shifted symbols */
        case '!':  result.code = 2;  result.shift = 1; break;
        case '@':  result.code = 3;  result.shift = 1; break;
        case '#':  result.code = 4;  result.shift = 1; break;
        case '$':  result.code = 5;  result.shift = 1; break;
        case '%':  result.code = 6;  result.shift = 1; break;
        case '^':  result.code = 7;  result.shift = 1; break;
        case '&':  result.code = 8;  result.shift = 1; break;
        case '*':  result.code = 9;  result.shift = 1; break;
        case '(':  result.code = 10; result.shift = 1; break;
        case ')':  result.code = 11; result.shift = 1; break;
        case '_':  result.code = 12; result.shift = 1; break;
        case '+':  result.code = 13; result.shift = 1; break;
        case '{':  result.code = 26; result.shift = 1; break;
        case '}':  result.code = 27; result.shift = 1; break;
        case '|':  result.code = 43; result.shift = 1; break;
        case ':':  result.code = 39; result.shift = 1; break;
        case '"':  result.code = 40; result.shift = 1; break;
        case '~':  result.code = 41; result.shift = 1; break;
        case '<':  result.code = 51; result.shift = 1; break;
        case '>':  result.code = 52; result.shift = 1; break;
        case '?':  result.code = 53; result.shift = 1; break;
        default:   break; /* result.code stays 0 = unmapped */
        }
    }
    return result;
}

/*
 * Type a text string by synthesizing key press/release events for each
 * character. Only printable ASCII characters are supported; non-ASCII
 * characters are silently skipped.
 */
static int worker_text(struct worker_state *state, const char *text) {
    if (state->device == NULL ||
        !ei_device_has_capability(state->device, EI_DEVICE_CAP_KEYBOARD))
        return -1;

    for (const char *p = text; *p != '\0'; ++p) {
        struct ascii_key key = ascii_to_key(*p);
        if (key.code == 0)
            continue; /* skip unmapped characters */
        if (key.shift) {
            ei_device_keyboard_key(state->device, KEY_LEFTSHIFT, true);
            ei_device_frame(state->device, ei_now(state->ei));
        }
        ei_device_keyboard_key(state->device, key.code, true);
        ei_device_frame(state->device, ei_now(state->ei));
        ei_device_keyboard_key(state->device, key.code, false);
        ei_device_frame(state->device, ei_now(state->ei));
        if (key.shift) {
            ei_device_keyboard_key(state->device, KEY_LEFTSHIFT, false);
            ei_device_frame(state->device, ei_now(state->ei));
        }
    }
    return 0;
}

static int worker_command(struct worker_state *state, const char *line) {
    char kind[24], phase[24], extra;
    double x, y, dx, dy;
    unsigned int value;
    int flag;

    if (sscanf(line, "%23s %lf %lf %lf %lf %d %c", kind, &x, &y, &dx, &dy, &flag, &extra) == 6 &&
        strcmp(kind, "scroll_pixels") == 0) {
        if (state->device == NULL || !ei_device_has_capability(state->device, EI_DEVICE_CAP_SCROLL))
            return -1;
        if (flag) {
            ei_device_scroll_stop(state->device, true, true);
            ei_device_frame(state->device, ei_now(state->ei));
            return 0;
        }
        if (worker_pointer(state, x, y, 0) < 0) return -1;
        double scale = 1.0;
        const char *raw_scale = getenv("ENZREMOTE_INPUT_SCALE");
        if (raw_scale) {
            char *end;
            scale = strtod(raw_scale, &end);
            if (*end || !isfinite(scale) || scale < 0.25 || scale > 8.0) return -1;
        }
        const char *compat = getenv("ENZREMOTE_ICED_SCROLL_COMPAT");
        if (compat && strcmp(compat, "0") != 0 && strcmp(compat, "1") != 0) return -1;
        double divisor = scale * (compat && strcmp(compat, "1") == 0 ? scale : 1.0);
        return worker_scroll(state, dx / divisor, dy / divisor);
    }
    if (sscanf(line, "%23s %lf %lf %d %c", kind, &x, &y, &flag, &extra) == 4 &&
        strcmp(kind, "pointer") == 0)
        return worker_pointer(state, x, y, flag);
    if (sscanf(line, "%23s %u %d %c", kind, &value, &flag, &extra) == 3 &&
        strcmp(kind, "key") == 0)
        return worker_key(state, value, flag);
    if (sscanf(line, "%23s %u %23s %lf %lf %c", kind, &value, phase, &x, &y,
               &extra) == 5 &&
        strcmp(kind, "touch") == 0)
        return worker_touch(state, value, phase, x, y);
    if (sscanf(line, "%23s %u %d %c", kind, &value, &flag, &extra) == 3 &&
        strcmp(kind, "button") == 0)
        return worker_button(state, value, flag);
    if (sscanf(line, "%23s %lf %lf %c", kind, &x, &y, &extra) == 3 &&
        strcmp(kind, "scroll") == 0)
        return worker_scroll(state, x, y);
    if (sscanf(line, "%23s %lf %lf %c", kind, &x, &y, &extra) == 3 &&
        (strcmp(kind, "pointer_relative") == 0 || strcmp(kind, "pointer_rel") == 0))
        return worker_pointer_relative(state, x, y);
    /* text command: "text <string>\n" — everything after "text " is the text */
    if (strncmp(line, "text ", 5) == 0) {
        const char *text = line + 5;
        /* Strip trailing newline if present */
        size_t len = strlen(text);
        char buf[4097]; /* max text is 4096 + NUL */
        if (len == 0 || len > 4096)
            return -1;
        memcpy(buf, text, len);
        if (len > 0 && buf[len - 1] == '\n')
            buf[len - 1] = '\0';
        else
            buf[len] = '\0';
        if (buf[0] == '\0')
            return -1;
        return worker_text(state, buf);
    }
    return -1;
}

static int run_worker(const char *socket_path, int broker_fd) {
    struct worker_state state = {0};
    struct pollfd pollfds[2];
    char line[4128]; /* text commands can carry up to 4096 payload bytes */
    int eis_fd, result = 1;

    state.ei = ei_new_sender(NULL);
    if (state.ei == NULL)
        return 1;
    ei_configure_name(state.ei, "enzremote WebRTC input worker");
    eis_fd = broker_fd >= 0 ? request_eis_fd_from_socket(broker_fd)
                            : request_eis_fd(socket_path);
    if (broker_fd >= 0)
        close(broker_fd);
    if (eis_fd < 0 || ei_setup_backend_fd(state.ei, eis_fd) < 0)
        goto out;

    pollfds[0] = (struct pollfd){.fd = ei_get_fd(state.ei), .events = POLLIN};
    pollfds[1] = (struct pollfd){.fd = STDIN_FILENO, .events = POLLIN};
    for (;;) {
        if (poll(pollfds, 2, -1) < 0) {
            if (errno == EINTR)
                continue;
            break;
        }
        if ((pollfds[0].revents & (POLLIN | POLLHUP | POLLERR)) &&
            worker_handle_events(&state) < 0)
            break;
        if (pollfds[1].revents & POLLIN) {
            if (fgets(line, sizeof(line), stdin) == NULL) {
                result = 0;
                break;
            }
            if (worker_command(&state, line) == 0)
                puts("OK");
            else
                puts("ERROR");
            fflush(stdout);
            continue;
        }
        if (pollfds[1].revents & (POLLHUP | POLLERR)) {
            result = 0;
            break;
        }
    }

out:
    worker_clear_device(&state);
    if (state.ei != NULL) {
        ei_disconnect(state.ei);
        ei_unref(state.ei);
    }
    return result;
}

int main(int argc, char **argv) {
    struct ei *ei;
    struct pollfd pollfd;
    double x, y;
    int eis_fd;
    int pointer_only, click;
    int result = 1;

    if (argc == 3 && strcmp(argv[1], "--worker") == 0)
        return run_worker(argv[2], -1);
    if (argc == 3 && strcmp(argv[1], "--worker-fd") == 0) {
        char *end = NULL;
        long broker_fd = strtol(argv[2], &end, 10);
        if (end == argv[2] || *end != '\0' || broker_fd < 0 || broker_fd > INT32_MAX)
            return 1;
        return run_worker(NULL, (int)broker_fd);
    }

    pointer_only = argc >= 5 && strcmp(argv[4], "--pointer") == 0;
    click = argc == 6 && strcmp(argv[5], "--click") == 0;
    if (!((argc == 4) || (pointer_only && (argc == 5 || click))) ||
        parse_coordinate(argv[2], &x) < 0 || parse_coordinate(argv[3], &y) < 0) {
        fprintf(stderr,
                "usage: enzremote-input-eis SOCKET X Y [--pointer [--click]]\n");
        return 2;
    }

    ei = ei_new_sender(NULL);
    if (ei == NULL) {
        fprintf(stderr, "could not create libei sender\n");
        return 1;
    }
    ei_configure_name(ei, "enzremote input smoke");
    eis_fd = request_eis_fd(argv[1]);
    if (eis_fd < 0 || ei_setup_backend_fd(ei, eis_fd) < 0) {
        fprintf(stderr, "could not connect through EIS broker: %s\n", argv[1]);
        if (eis_fd >= 0)
            close(eis_fd);
        ei_unref(ei);
        return 1;
    }

    pollfd.fd = ei_get_fd(ei);
    pollfd.events = POLLIN;
    for (int attempt = 0; attempt < 20 && result != 0; ++attempt) {
        if (poll(&pollfd, 1, 250) < 0) {
            if (errno == EINTR)
                continue;
            perror("poll");
            break;
        }
        if (!(pollfd.revents & POLLIN))
            continue;

        ei_dispatch(ei);
        for (struct ei_event *event = ei_get_event(ei); event != NULL;
             event = ei_get_event(ei)) {
            switch (ei_event_get_type(event)) {
            case EI_EVENT_SEAT_ADDED:
                if (pointer_only)
                    ei_seat_bind_capabilities(
                        ei_event_get_seat(event),
                        EI_DEVICE_CAP_POINTER_ABSOLUTE, EI_DEVICE_CAP_BUTTON,
                        NULL);
                else
                    ei_seat_bind_capabilities(
                        ei_event_get_seat(event),
                        EI_DEVICE_CAP_POINTER_ABSOLUTE, EI_DEVICE_CAP_BUTTON,
                        EI_DEVICE_CAP_KEYBOARD, EI_DEVICE_CAP_TOUCH, NULL);
                break;
            case EI_EVENT_DEVICE_RESUMED:
                result = pointer_only
                             ? send_pointer_event(ei, ei_event_get_device(event),
                                                  x, y, click)
                             : send_smoke_events(ei, ei_event_get_device(event),
                                                 x, y);
                break;
            case EI_EVENT_DISCONNECT:
                fprintf(stderr, "EIS server disconnected before device resume\n");
                attempt = 20;
                break;
            default:
                break;
            }
            ei_event_unref(event);
        }
    }

    if (result != 0)
        fprintf(stderr, "timed out waiting for a resumed EIS device\n");
    ei_disconnect(ei);
    ei_unref(ei);
    return result == 0 ? 0 : 1;
}
