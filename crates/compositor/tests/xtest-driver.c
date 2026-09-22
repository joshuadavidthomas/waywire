// XWarpPointer (used by some xdotool motion paths) is not XTest injection.
#include <X11/Xlib.h>
#include <X11/keysym.h>
#include <X11/extensions/XTest.h>
#include <unistd.h>

int main(void) {
    Display *display = XOpenDisplay(NULL);
    if (!display) return 1;
    XTestFakeMotionEvent(display, -1, 130, 100, CurrentTime);
    XTestFakeButtonEvent(display, 1, True, CurrentTime);
    XTestFakeButtonEvent(display, 1, False, CurrentTime);
    XTestFakeKeyEvent(display, XKeysymToKeycode(display, XK_a), True, CurrentTime);
    XTestFakeKeyEvent(display, XKeysymToKeycode(display, XK_a), False, CurrentTime);
    XSync(display, False);
    usleep(300000);
    XCloseDisplay(display);
    return 0;
}
