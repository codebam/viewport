// SPDX-License-Identifier: MIT
//
// An X11 client that asks the X server for every key, in two shapes.
//
// One binary, two modes, because the regression under test has two halves
// that need opposite windows:
//
//   xwayland-grab-client target      a window the test focuses, which then
//                                    grabs: a game's shape, and the one the
//                                    grant is supposed to answer
//   xwayland-grab-client background  a window nothing ever focuses, which
//                                    then grabs: the shape the grant is
//                                    supposed to refuse
//
// The compositor hands Smithay's grab a focus target (or not). Returning the
// wrong shape there used to hand a background client every keystroke, and —
// for the focused one — replaced a correct focus with a wrong one, so the
// first key fired `leave` on the window that had just grabbed. The client
// prints both halves as lines the test can read: `grab=<int>` (0 is
// GrabSuccess), then `focusout` and `keypress` lines while it serves events.
//
// Prints `mapped <win>` once the window is up, then stays for the window the
// test needs. Exit 2 if there is no display to map into.
#include <X11/Xlib.h>
#include <X11/Xutil.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

static void die_no_display(void) {
    fprintf(stderr, "no X display — is Xwayland running?\n");
    exit(2);
}

static Window make_window(Display *d, const char *title) {
    int screen = DefaultScreen(d);
    Window win = XCreateSimpleWindow(d, RootWindow(d, screen), 0, 0, 320, 140, 0,
                                     BlackPixel(d, screen), WhitePixel(d, screen));
    XStoreName(d, win, title);
    XClassHint class_hint = {.res_name = "viewport-grab-test", .res_class = "ViewportGrab"};
    XSetClassHint(d, win, &class_hint);
    return win;
}

static void wait_map(Display *d, Window win) {
    XSelectInput(d, win, StructureNotifyMask);
    XMapWindow(d, win);
    XFlush(d);
    for (;;) {
        XEvent event;
        XNextEvent(d, &event);
        if (event.type == MapNotify) {
            break;
        }
    }
    printf("mapped 0x%lx\n", (unsigned long)win);
    fflush(stdout);
}

// Waits until the X server names this window as the input-focus holder, the
// observable tests/xwayland-focus-client.c uses. `view.focus` is accepted
// before the compositor has called SetInputFocus — the two sides of the seat
// are not ordered against each other — and a grab that asks in between is
// refused as a grab from a window that does not hold the keyboard, which is
// the right answer to the wrong moment.
static void wait_focus(Display *d, Window win) {
    for (int attempt = 0; attempt < 50; attempt++) {
        Window focus;
        int revert;
        XGetInputFocus(d, &focus, &revert);
        if (focus == win) {
            return;
        }
        usleep(100000);
    }
    fprintf(stderr, "the window never got the X server's input focus\n");
}

int main(int argc, char **argv) {
    if (argc != 2 || (strcmp(argv[1], "target") != 0 && strcmp(argv[1], "background") != 0)) {
        fprintf(stderr, "usage: %s target|background\n", argv[0]);
        return 2;
    }
    int target = strcmp(argv[1], "target") == 0;

    Display *d = XOpenDisplay(NULL);
    if (!d) {
        die_no_display();
    }

    Window win = make_window(d, target ? "Grab Target" : "Background Grabber");
    wait_map(d, win);

    if (target) {
        // The test focuses this window before asking for the grab; the
        // server's word on whether that landed is the one the grant reads.
        wait_focus(d, win);
    }
    int grabbed = XGrabKeyboard(d, win, True, GrabModeAsync, GrabModeAsync, CurrentTime);
    printf("grab=%d\n", grabbed);
    fflush(stdout);

    if (!target) {
        // An owner of keys that never does anything with them, long enough
        // for the test to notice nobody handed them over.
        struct timespec ts = {1, 0};
        nanosleep(&ts, NULL);
        XCloseDisplay(d);
        return 0;
    }

    // Now the interesting half: does the grab cost the window its focus? A
    // grant that hands back a Wayland-shaped focus makes the first key fire
    // a FocusOut on the window that just asked to hold the keyboard.
    XSelectInput(d, win, FocusChangeMask | KeyPressMask);
    XFlush(d);

    time_t start = time(NULL);
    int keypresses = 0;
    for (;;) {
        // Both ends of the wait: an event, or five seconds gone by.
        while (XPending(d)) {
            XEvent event;
            XNextEvent(d, &event);
            if (event.type == FocusOut) {
                printf("focusout\n");
                fflush(stdout);
            } else if (event.type == KeyPress) {
                printf("keypress\n");
                fflush(stdout);
                keypresses++;
                if (keypresses >= 2) {
                    XCloseDisplay(d);
                    return 0;
                }
            }
        }
        if (time(NULL) - start >= 5) {
            XCloseDisplay(d);
            return 0;
        }
        struct timespec ts = {0, 50 * 1000 * 1000};
        nanosleep(&ts, NULL);
    }
}
