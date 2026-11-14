// SPDX-License-Identifier: MIT
//
// An X11 client that retitles itself after it has been announced.
//
// What the test needs that the float client could not give it is a window
// whose name changes on cue, the way a browser's changes while a page loads:
// `WM_NAME` through XStoreName, and `_NET_WM_NAME` — the UTF-8 property a
// modern toolkit writes — together, so neither path to the same fact can be
// the one that works by luck.
//
// Prints `mapped` once the window is up (the moment after which the test can
// wait for the announcement of the first title), sleeps through the window
// the test uses to subscribe, then prints `retitled` after both writes and
// stays alive serving events until it is killed, like
// tests/xwayland-float-client.c. Exit 2 if there is no display to map into.
#include <X11/Xatom.h>
#include <X11/Xlib.h>
#include <X11/Xutil.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

int main(void) {
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

    XStoreName(d, win, "Before Retitle");
    XClassHint class_hint = {.res_name = "viewport-props-test", .res_class = "ViewportProps"};
    XSetClassHint(d, win, &class_hint);
    XSelectInput(d, win, StructureNotifyMask | ExposureMask);

    XMapWindow(d, win);
    XFlush(d);

    // Mapping is asynchronous; the announcement cannot be waited on before
    // the window is up and named.
    for (;;) {
        XEvent event;
        XNextEvent(d, &event);
        if (event.type == MapNotify) {
            break;
        }
    }
    printf("mapped\n");
    fflush(stdout);

    // The window the subscriber gets: the test asks for `view.props` while
    // the title is still the first one, so the event it later reads cannot
    // be a replay of the announcement.
    sleep(1);

    // Both spellings of the same fact. `WM_NAME` is what XStoreName writes;
    // `_NET_WM_NAME` is what a GTK or Qt client writes alongside it, and a
    // compositor that reads only one of the two passes half of this test.
    XStoreName(d, win, "After Retitle");
    Atom net_wm_name = XInternAtom(d, "_NET_WM_NAME", False);
    Atom utf8_string = XInternAtom(d, "UTF8_STRING", False);
    XChangeProperty(d, win, net_wm_name, utf8_string, 8, PropModeReplace,
                    (unsigned char *)"After Retitle", 13);
    XFlush(d);
    printf("retitled\n");
    fflush(stdout);

    // Stay alive serving events until the test kills us: a window that
    // exits takes its properties with it, and the test still has events
    // arriving to drain.
    for (;;) {
        XEvent event;
        XNextEvent(d, &event);
    }
    return 0;
}
