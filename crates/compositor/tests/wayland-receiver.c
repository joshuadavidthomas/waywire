// Small native client for compositor integration tests. Build with the generated
// xdg-shell client header/private-code and pkg-config wayland-client.
#define _GNU_SOURCE
#include <wayland-client.h>
#include "xdg-shell-client-protocol.h"
#ifdef NATIVE_SCENE_TEST
#include "xdg-decoration-client-protocol.h"
#include "fractional-scale-client-protocol.h"
#include "text-input-client-protocol.h"
#endif
#include <fcntl.h>
#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

static struct wl_compositor *compositor;
static struct wl_shm *shm;
static struct wl_surface *surface;
static struct xdg_wm_base *shell;
static struct xdg_toplevel *toplevel;
static struct xdg_surface *toplevel_xdg;
static struct wl_surface *popup_surface;
static struct xdg_surface *popup_xdg;
static struct xdg_popup *popup;
static int popup_width, popup_height;
static struct wl_seat *input_seat;
static struct wl_data_device_manager *data_manager;
static struct wl_data_device *data_device;
static struct wl_data_offer *selection_offer;
static int clipboard_fd = -1;
static int width = 480, height = 320;
static int animation_frames, revision;
#ifdef NATIVE_SCENE_TEST
static struct zxdg_decoration_manager_v1 *decorations;
static struct wp_fractional_scale_manager_v1 *fractional;
static struct zwp_text_input_manager_v3 *text_manager;
static void text_enter(void *d, struct zwp_text_input_v3 *t, struct wl_surface *s) {
    (void)d; (void)s; zwp_text_input_v3_enable(t); zwp_text_input_v3_commit(t); puts("text-enter");
}
static void text_leave(void *d, struct zwp_text_input_v3 *t, struct wl_surface *s) { (void)d; (void)t; (void)s; puts("text-leave"); }
static void preedit(void *d, struct zwp_text_input_v3 *t, const char *text, int32_t begin, int32_t end) {
    (void)d; (void)t; printf("preedit %s %d %d\n", text ? text : "", begin, end);
}
static void text_commit(void *d, struct zwp_text_input_v3 *t, const char *text) { (void)d; (void)t; printf("commit %s\n", text ? text : ""); }
static void text_delete(void *d, struct zwp_text_input_v3 *t, uint32_t before, uint32_t after) { (void)d; (void)t; (void)before; (void)after; }
static void text_done(void *d, struct zwp_text_input_v3 *t, uint32_t serial) { (void)d; (void)t; printf("text-done %u\n", serial); }
static const struct zwp_text_input_v3_listener text_listener = {
    .enter = text_enter, .leave = text_leave, .preedit_string = preedit, .commit_string = text_commit,
    .delete_surrounding_text = text_delete, .done = text_done,
};
static void decoration_configure(void *d, struct zxdg_toplevel_decoration_v1 *o, uint32_t mode) {
    (void)d; (void)o; printf("decoration %u\n", mode);
}
static const struct zxdg_toplevel_decoration_v1_listener decoration_listener = { .configure = decoration_configure };
static void preferred_scale(void *d, struct wp_fractional_scale_v1 *o, uint32_t scale) {
    (void)d; (void)o; printf("scale %u\n", scale);
}
static const struct wp_fractional_scale_v1_listener fractional_listener = { .preferred_scale = preferred_scale };
#endif

static void source_target(void *d, struct wl_data_source *s, const char *mime) { (void)d; (void)s; (void)mime; }
static void source_send(void *d, struct wl_data_source *s, const char *mime, int fd) {
    (void)d; (void)s; (void)mime;
    const char text[] = "native clipboard asymmetric 73";
    if (write(fd, text, sizeof(text)-1) != sizeof(text)-1) abort();
    close(fd);
}
static void source_cancelled(void *d, struct wl_data_source *s) { (void)d; wl_data_source_destroy(s); }
static const struct wl_data_source_listener source_listener = {
    .target = source_target, .send = source_send, .cancelled = source_cancelled,
};
static void offer_mime(void *d, struct wl_data_offer *o, const char *mime) { (void)d; (void)o; (void)mime; }
static const struct wl_data_offer_listener offer_listener = { .offer = offer_mime };
static void data_offer(void *d, struct wl_data_device *o, struct wl_data_offer *offer) {
    (void)d; (void)o; wl_data_offer_add_listener(offer, &offer_listener, NULL);
}
static void data_selection(void *d, struct wl_data_device *o, struct wl_data_offer *offer) {
    (void)d; (void)o;
    if (selection_offer) wl_data_offer_destroy(selection_offer);
    selection_offer = offer;
    puts(offer ? "selection" : "selection-clear");
}
static const struct wl_data_device_listener data_listener = { .data_offer = data_offer, .selection = data_selection };

static void released(void *data, struct wl_buffer *buffer) {
    (void)data;
    wl_buffer_destroy(buffer);
}
static const struct wl_buffer_listener buffer_listener = { .release = released };

static void paint(void);
static void frame_done(void *data, struct wl_callback *callback, uint32_t time) {
    (void)data; (void)time;
    wl_callback_destroy(callback);
    printf("frame-done %d\n", revision);
    if (animation_frames > 0) { animation_frames--; revision++; paint(); }
}
static const struct wl_callback_listener frame_listener = { .done = frame_done };

static void paint_surface(struct wl_surface *target, int width, int height) {
    size_t size = (size_t)width * height * 4;
    int fd = memfd_create("receiver", MFD_CLOEXEC);
    if (fd < 0 || ftruncate(fd, size) < 0) abort();
    uint32_t *pixels = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (pixels == MAP_FAILED) abort();
    for (int y = 0; y < height; y++) {
        for (int x = 0; x < width; x++) {
            // Four unequal quadrants expose flips, crop mistakes and RGB swaps.
            pixels[y * width + x] = y < height / 3
                ? (x < width / 3 ? 0xffff3020 + revision : 0xff20b060)
                : (x < width / 3 ? 0xff3050e0 : 0xffe0b030);
        }
    }
    struct wl_shm_pool *pool = wl_shm_create_pool(shm, fd, size);
    struct wl_buffer *buffer = wl_shm_pool_create_buffer(pool, 0, width, height, width * 4, WL_SHM_FORMAT_XRGB8888);
    wl_buffer_add_listener(buffer, &buffer_listener, NULL);
    wl_shm_pool_destroy(pool);
    close(fd);
    munmap(pixels, size);
    wl_surface_attach(target, buffer, 0, 0);
    wl_surface_damage(target, 0, 0, width, height);
    wl_surface_commit(target);
}
static void paint(void) {
    if (getenv("WAYWIRE_TEST_GEOMETRY")) xdg_surface_set_window_geometry(toplevel_xdg, 17, 29, width - 17, height - 29);
    if (animation_frames > 0) wl_callback_add_listener(wl_surface_frame(surface), &frame_listener, NULL);
    paint_surface(surface, width, height);
    printf("paint %d %d\n", width, height);
}
static void popup_configure(void *d, struct xdg_popup *p, int32_t x, int32_t y, int32_t w, int32_t h) {
    (void)d; (void)p; popup_width = w; popup_height = h;
    printf("popup-configure %d %d %d %d\n", x, y, w, h);
}
static void popup_done(void *d, struct xdg_popup *p) {
    (void)d; xdg_popup_destroy(p); xdg_surface_destroy(popup_xdg);
    wl_surface_destroy(popup_surface); popup = NULL; puts("popup-done");
}
static const struct xdg_popup_listener popup_listener = { .configure = popup_configure, .popup_done = popup_done };
static void popup_surface_configure(void *d, struct xdg_surface *s, uint32_t serial) {
    (void)d; xdg_surface_ack_configure(s, serial);
    paint_surface(popup_surface, popup_width, popup_height);
    puts("popup-painted");
}
static const struct xdg_surface_listener popup_surface_listener = { .configure = popup_surface_configure };
static void open_popup(uint32_t serial) {
    if (popup) return;
    popup_surface = wl_compositor_create_surface(compositor);
    popup_xdg = xdg_wm_base_get_xdg_surface(shell, popup_surface);
    xdg_surface_add_listener(popup_xdg, &popup_surface_listener, NULL);
    struct xdg_positioner *positioner = xdg_wm_base_create_positioner(shell);
    xdg_positioner_set_size(positioner, 120, 80);
    xdg_positioner_set_anchor_rect(positioner, width-10, height-10, 10, 10);
    xdg_positioner_set_anchor(positioner, XDG_POSITIONER_ANCHOR_BOTTOM_RIGHT);
    xdg_positioner_set_gravity(positioner, XDG_POSITIONER_GRAVITY_BOTTOM_RIGHT);
    xdg_positioner_set_offset(positioner, 700, 500);
    xdg_positioner_set_constraint_adjustment(positioner, XDG_POSITIONER_CONSTRAINT_ADJUSTMENT_SLIDE_X | XDG_POSITIONER_CONSTRAINT_ADJUSTMENT_SLIDE_Y);
    popup = xdg_surface_get_popup(popup_xdg, toplevel_xdg, positioner);
    xdg_popup_add_listener(popup, &popup_listener, NULL);
    xdg_positioner_destroy(positioner);
    xdg_popup_grab(popup, input_seat, serial);
    wl_surface_commit(popup_surface);
}
static void configured(void *data, struct xdg_surface *xdg, uint32_t serial) {
    (void)data;
    xdg_surface_ack_configure(xdg, serial);
    paint();
}
static const struct xdg_surface_listener surface_listener = { .configure = configured };
static void top_configured(void *data, struct xdg_toplevel *top, int32_t w, int32_t h, struct wl_array *states) {
    (void)data; (void)top; (void)states;
    if (w > 0) width = w;
    if (h > 0) height = h;
    printf("configure %d %d\n", w, h);
}
static void closed(void *data, struct xdg_toplevel *top) { (void)data; (void)top; puts("close"); exit(0); }
static void bounds(void *data, struct xdg_toplevel *top, int32_t w, int32_t h) { (void)data; (void)top; printf("bounds %d %d\n", w, h); }
static void wm_capabilities(void *data, struct xdg_toplevel *top, struct wl_array *caps) {
    (void)data; (void)top; uint32_t *cap;
    printf("wm-capabilities"); wl_array_for_each(cap, caps) printf(" %u", *cap); puts("");
}
static const struct xdg_toplevel_listener top_listener = { .configure = top_configured, .close = closed, .configure_bounds = bounds, .wm_capabilities = wm_capabilities };
static void ping(void *data, struct xdg_wm_base *base, uint32_t serial) { (void)data; xdg_wm_base_pong(base, serial); }
static const struct xdg_wm_base_listener shell_listener = { .ping = ping };

static void keymap(void *data, struct wl_keyboard *kb, uint32_t format, int fd, uint32_t size) {
    (void)data; (void)kb; (void)format; (void)size; close(fd);
}
static void key_enter(void *data, struct wl_keyboard *kb, uint32_t serial, struct wl_surface *s, struct wl_array *keys) {
    (void)data; (void)kb; (void)serial; (void)s;
    printf("keyboard-enter");
    uint32_t *key;
    wl_array_for_each(key, keys) printf(" %u", *key);
    puts("");
}
static void key_leave(void *data, struct wl_keyboard *kb, uint32_t serial, struct wl_surface *s) {
    (void)data; (void)kb; (void)serial; (void)s; puts("keyboard-leave");
}
static void key(void *data, struct wl_keyboard *kb, uint32_t serial, uint32_t time, uint32_t code, uint32_t state) {
    (void)data; (void)kb; (void)serial; (void)time; printf("key %u %u\n", code, state);
    if (!state || !getenv("WAYWIRE_TEST_ACTIONS")) return;
    if (code == 34) { animation_frames = 4; paint(); }
    if (code == 38) { width += 240; height += 160; paint(); }
    if (code == 50) xdg_toplevel_set_maximized(toplevel);
    if (code == 22) xdg_toplevel_unset_maximized(toplevel);
    if (code == 33) xdg_toplevel_set_fullscreen(toplevel, NULL);
    if (code == 19) xdg_toplevel_unset_fullscreen(toplevel);
    if (code == 23) xdg_toplevel_resize(toplevel, input_seat, 0, XDG_TOPLEVEL_RESIZE_EDGE_TOP_LEFT);
    if (code == 45 && data_device) wl_data_device_set_selection(data_device, NULL, serial);
    if (code == 46 && data_device) {
        struct wl_data_source *source = wl_data_device_manager_create_data_source(data_manager);
        wl_data_source_add_listener(source, &source_listener, NULL);
        wl_data_source_offer(source, "text/plain;charset=utf-8");
        wl_data_device_set_selection(data_device, source, serial);
    }
    if (code == 47 && selection_offer) {
        int pipefd[2];
        if (pipe2(pipefd, O_CLOEXEC) < 0) abort();
        if (clipboard_fd >= 0) close(clipboard_fd);
        clipboard_fd = pipefd[0];
        wl_data_offer_receive(selection_offer, "text/plain;charset=utf-8", pipefd[1]);
        close(pipefd[1]);
    }
}
static void modifiers(void *data, struct wl_keyboard *kb, uint32_t serial, uint32_t depressed, uint32_t latched, uint32_t locked, uint32_t group) {
    (void)data; (void)kb; (void)serial; (void)latched; (void)locked; (void)group; printf("modifiers %u\n", depressed);
}
static void repeat(void *data, struct wl_keyboard *kb, int32_t rate, int32_t delay) { (void)data; (void)kb; (void)rate; (void)delay; }
static const struct wl_keyboard_listener keyboard_listener = {
    .keymap = keymap, .enter = key_enter, .leave = key_leave, .key = key, .modifiers = modifiers, .repeat_info = repeat,
};
static void pointer_enter(void *data, struct wl_pointer *p, uint32_t serial, struct wl_surface *s, wl_fixed_t x, wl_fixed_t y) {
    (void)data; (void)p; (void)serial; (void)s; printf("pointer-enter %.3f %.3f\n", wl_fixed_to_double(x), wl_fixed_to_double(y));
}
static void pointer_leave(void *data, struct wl_pointer *p, uint32_t serial, struct wl_surface *s) {
    (void)data; (void)p; (void)serial; (void)s; puts("pointer-leave");
}
static void motion(void *data, struct wl_pointer *p, uint32_t time, wl_fixed_t x, wl_fixed_t y) {
    (void)data; (void)p; (void)time; printf("motion %.3f %.3f\n", wl_fixed_to_double(x), wl_fixed_to_double(y));
}
static void button(void *data, struct wl_pointer *p, uint32_t serial, uint32_t time, uint32_t code, uint32_t state) {
    (void)data; (void)p; (void)serial; (void)time; printf("button %u %u\n", code, state);
    if (state && code == 273 && getenv("WAYWIRE_TEST_ACTIONS"))
        xdg_toplevel_resize(toplevel, input_seat, serial, XDG_TOPLEVEL_RESIZE_EDGE_TOP_LEFT);
    if (state && code == 274 && getenv("WAYWIRE_TEST_ACTIONS")) open_popup(serial);
}
static void axis(void *data, struct wl_pointer *p, uint32_t time, uint32_t axis, wl_fixed_t value) {
    (void)data; (void)p; (void)time; printf("axis %u %.3f\n", axis, wl_fixed_to_double(value));
}
static const struct wl_pointer_listener pointer_listener = {
    .enter = pointer_enter, .leave = pointer_leave, .motion = motion, .button = button, .axis = axis,
};
static void capabilities(void *data, struct wl_seat *seat, uint32_t caps) {
    (void)data;
    if (caps & WL_SEAT_CAPABILITY_KEYBOARD) wl_keyboard_add_listener(wl_seat_get_keyboard(seat), &keyboard_listener, NULL);
    if (caps & WL_SEAT_CAPABILITY_POINTER) wl_pointer_add_listener(wl_seat_get_pointer(seat), &pointer_listener, NULL);
}
static void seat_name(void *data, struct wl_seat *seat, const char *name) { (void)data; (void)seat; (void)name; }
static const struct wl_seat_listener seat_listener = { .capabilities = capabilities, .name = seat_name };
static void global(void *data, struct wl_registry *registry, uint32_t name, const char *interface, uint32_t version) {
    (void)data; (void)version;
    if (!strcmp(interface, "wl_compositor")) compositor = wl_registry_bind(registry, name, &wl_compositor_interface, 4);
    else if (!strcmp(interface, "wl_shm")) shm = wl_registry_bind(registry, name, &wl_shm_interface, 1);
    else if (!strcmp(interface, "xdg_wm_base")) {
        shell = wl_registry_bind(registry, name, &xdg_wm_base_interface, version < 5 ? version : 5);
        xdg_wm_base_add_listener(shell, &shell_listener, NULL);
    } else if (!strcmp(interface, "wl_seat")) {
        struct wl_seat *seat = wl_registry_bind(registry, name, &wl_seat_interface, 4);
        input_seat = seat;
        wl_seat_add_listener(seat, &seat_listener, NULL);
    } else if (!strcmp(interface, "wl_data_device_manager")) {
        data_manager = wl_registry_bind(registry, name, &wl_data_device_manager_interface, 1);
    }
#ifdef NATIVE_SCENE_TEST
    else if (!strcmp(interface, "zxdg_decoration_manager_v1"))
        decorations = wl_registry_bind(registry, name, &zxdg_decoration_manager_v1_interface, 1);
    else if (!strcmp(interface, "wp_fractional_scale_manager_v1"))
        fractional = wl_registry_bind(registry, name, &wp_fractional_scale_manager_v1_interface, 1);
    else if (!strcmp(interface, "zwp_text_input_manager_v3"))
        text_manager = wl_registry_bind(registry, name, &zwp_text_input_manager_v3_interface, 1);
#endif
}
static void removed(void *data, struct wl_registry *registry, uint32_t name) { (void)data; (void)registry; (void)name; }
static const struct wl_registry_listener registry_listener = { .global = global, .global_remove = removed };

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    if (argc == 3) { width = atoi(argv[1]); height = atoi(argv[2]); }
    if (width < 1 || width > 4096 || height < 1 || height > 4096) return 2;
    struct wl_display *display = wl_display_connect(NULL);
    if (!display) return 3;
    struct wl_registry *registry = wl_display_get_registry(display);
    wl_registry_add_listener(registry, &registry_listener, NULL);
    if (wl_display_roundtrip(display) < 0 || !compositor || !shm || !shell) return 4;
    surface = wl_compositor_create_surface(compositor);
    struct xdg_surface *xdg = xdg_wm_base_get_xdg_surface(shell, surface);
    toplevel_xdg = xdg;
    xdg_surface_add_listener(xdg, &surface_listener, NULL);
    struct xdg_toplevel *top = xdg_surface_get_toplevel(xdg);
    toplevel = top;
    xdg_toplevel_add_listener(top, &top_listener, NULL);
    xdg_toplevel_set_title(top, "Waywire native receiver");
    xdg_toplevel_set_app_id(top, "waywire-test-receiver");
    if (data_manager && input_seat) {
        data_device = wl_data_device_manager_get_data_device(data_manager, input_seat);
        wl_data_device_add_listener(data_device, &data_listener, NULL);
    }
#ifdef NATIVE_SCENE_TEST
    if (text_manager && input_seat) {
        struct zwp_text_input_v3 *text = zwp_text_input_manager_v3_get_text_input(text_manager, input_seat);
        zwp_text_input_v3_add_listener(text, &text_listener, NULL);
    }
    if (decorations && getenv("WAYWIRE_TEST_DECORATIONS")) {
        struct zxdg_toplevel_decoration_v1 *decoration = zxdg_decoration_manager_v1_get_toplevel_decoration(decorations, top);
        zxdg_toplevel_decoration_v1_add_listener(decoration, &decoration_listener, NULL);
        zxdg_toplevel_decoration_v1_set_mode(decoration, ZXDG_TOPLEVEL_DECORATION_V1_MODE_SERVER_SIDE);
    }
    if (fractional) {
        struct wp_fractional_scale_v1 *scale = wp_fractional_scale_manager_v1_get_fractional_scale(fractional, surface);
        wp_fractional_scale_v1_add_listener(scale, &fractional_listener, NULL);
    }
#endif
    wl_surface_commit(surface);
    while (1) {
        if (wl_display_dispatch_pending(display) < 0) break;
        wl_display_flush(display);
        struct pollfd fds[] = {{wl_display_get_fd(display), POLLIN, 0}, {clipboard_fd, POLLIN, 0}};
        if (poll(fds, 2, -1) < 0) break;
        if (fds[0].revents && wl_display_dispatch(display) < 0) break;
        if (fds[1].revents) {
            char text[4096];
            ssize_t n = read(clipboard_fd, text, sizeof(text)-1);
            if (n >= 0) { text[n] = 0; printf("clipboard %s\n", text); }
            close(clipboard_fd); clipboard_fd = -1;
        }
    }
    wl_display_disconnect(display);
    return 0;
}
