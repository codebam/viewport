// SPDX-License-Identifier: MIT
//
// Does an X11 window get the input focus?
//
// The compositor is a window manager for Xwayland's clients, and a window
// manager is the only thing that can tell the X server which window has the
// keyboard. Nothing else does it: an X client that maps a window and waits
// gets `PointerRoot` forever unless the WM calls `SetInputFocus`.
//
// `PointerRoot` is not obviously broken from the outside, which is why this
// test exists. Keystrokes still arrive — X sends them to whatever window the
// cursor is over — so typing works, clicking works, and only the things that
// ask "am I focused?" behave differently. A game is one of those: it captures
// the mouse on focus, so under `PointerRoot` it never captures, never asks for
// a pointer lock, and never looks around, while every other input works.
//
// Exit 0 if the window holds the focus, 1 if the server was left at
// PointerRoot or None, 2 if the test could not run.
#include <X11/Xlib.h>
#include <stdio.h>
#include <unistd.h>

int main(void) {
    Display *display = XOpenDisplay(NULL);
    if (!display) {
        fprintf(stderr, "no X display — is Xwayland running?\n");
        return 2;
    }

    int screen = DefaultScreen(display);
    Window window = XCreateSimpleWindow(display, RootWindow(display, screen), 0, 0, 800, 600, 0,
                                        BlackPixel(display, screen), WhitePixel(display, screen));
    XSelectInput(display, window, StructureNotifyMask | FocusChangeMask);
    XMapWindow(display, window);
    XFlush(display);

    // Mapping is asynchronous, and focus cannot be asked about before the
    // window manager has seen the window.
    for (;;) {
        XEvent event;
        XNextEvent(display, &event);
        if (event.type == MapNotify) {
            break;
        }
    }

    // Polled rather than waited on: focus is set by the compositor a moment
    // after the map, and how long that takes is not something to hard-code.
    for (int attempt = 0; attempt < 12; attempt++) {
        Window focus;
        int revert;
        XGetInputFocus(display, &focus, &revert);
        if (focus == window) {
            printf("focus: the window (0x%lx)\n", (unsigned long)window);
            return 0;
        }
        usleep(400000);
    }

    Window focus;
    int revert;
    XGetInputFocus(display, &focus, &revert);
    printf("focus: %s (0x%lx), wanted the window (0x%lx)\n",
           focus == PointerRoot ? "PointerRoot" : focus == None ? "None" : "another window",
           (unsigned long)focus, (unsigned long)window);
    return 1;
}
