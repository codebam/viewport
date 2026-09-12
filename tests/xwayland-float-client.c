// SPDX-License-Identifier: MIT
//
// Does a fixed-size X11 window float, and does a resizable one tile?
//
// The two windows here are the same shape Steam's updater is: an X11 client
// that sets `WM_CLASS`, a title and `WM_NORMAL_HINTS`, advertises
// `_NET_WM_WINDOW_TYPE_NORMAL`, and sets no `WM_TRANSIENT_FOR`. What differs
// between them is the one signal the compositor has left to go on:
//
//   xwayland-float-client fixed      min == max, so it cannot be resized
//   xwayland-float-client resizable  a minimum and no maximum
//
// The updater is built on SDL3, whose X11 backend sets exactly the same
// properties for a non-resizable window, which is why this reproduces it
// without Steam: no window type that says "dialog", no transient, and a size
// hint that says "this size and no other".
//
// Prints `mapped <id>` once the window is up and painted, so the test knows the
// announcement has had its chance, then stays alive serving events until it is
// killed. Exit 2 if there is no display to map into.
#include <X11/Xatom.h>
#include <X11/Xlib.h>
#include <X11/Xutil.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

int main(int argc, char **argv) {
    if (argc != 2 || (strcmp(argv[1], "fixed") != 0 && strcmp(argv[1], "resizable") != 0)) {
        fprintf(stderr, "usage: %s fixed|resizable\n", argv[0]);
        return 2;
    }
    int fixed = strcmp(argv[1], "fixed") == 0;
    const int w = 320, h = 140;

    Display *d = XOpenDisplay(NULL);
    if (!d) {
        fprintf(stderr, "no X display — is Xwayland running?\n");
        return 2;
    }
    int screen = DefaultScreen(d);
    Window root = RootWindow(d, screen);
    Window win = XCreateSimpleWindow(d, root, 0, 0, w, h, 0, BlackPixel(d, screen),
                                     WhitePixel(d, screen));

    XStoreName(d, win, fixed ? "Steam - Self Updater" : "Steam - Resizable");
    XClassHint class_hint = {.res_name = "steam", .res_class = "Steam"};
    XSetClassHint(d, win, &class_hint);
    XSelectInput(d, win, StructureNotifyMask | ExposureMask);

    // What SDL3 does for every window, and what Steam's updater therefore has:
    // a type of its own that says nothing about being a dialog.
    Atom net_type = XInternAtom(d, "_NET_WM_WINDOW_TYPE", False);
    Atom net_normal = XInternAtom(d, "_NET_WM_WINDOW_TYPE_NORMAL", False);
    XChangeProperty(d, win, net_type, XA_ATOM, 32, PropModeReplace,
                    (unsigned char *)&net_normal, 1);

    // The signal under test. A window that cannot be resized says so by
    // pinning its minimum and maximum together; one that can leaves the
    // maximum at zero, which is the protocol's "no opinion".
    XSizeHints hints;
    memset(&hints, 0, sizeof(hints));
    hints.flags = PMinSize | PMaxSize;
    hints.min_width = w;
    hints.min_height = h;
    hints.max_width = fixed ? w : 0;
    hints.max_height = fixed ? h : 0;
    if (!fixed) {
        hints.flags = PMinSize;
    }
    XSetWMNormalHints(d, win, &hints);

    XMapWindow(d, win);
    XFlush(d);

    for (;;) {
        XEvent event;
        XNextEvent(d, &event);
        if (event.type == MapNotify) {
            GC gc = XCreateGC(d, win, 0, NULL);
            XSetForeground(d, gc, WhitePixel(d, screen));
            XFillRectangle(d, win, gc, 0, 0, w, h);
            XFreeGC(d, gc);
            XFlush(d);
            // The compositor announces a window when it first paints, so this
            // is the moment after which `view.added` can be waited on.
            printf("mapped 0x%lx\n", (unsigned long)win);
            fflush(stdout);
        }
    }
    return 0;
}
