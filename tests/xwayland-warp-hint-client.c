// SPDX-License-Identifier: MIT
//
// Does a capture end leave the cursor where the game last warped it?
//
// An X11 game (CS2 under Xwayland, or GLFW's warp fallback anywhere) does
// its mouse look by hiding the cursor, grabbing the pointer and warping it
// back to the window centre every frame. Xwayland emulates that with a
// pointer lock plus `set_cursor_position_hint`, and the hint re-states the
// game's warp target on every motion. The game reads its deltas as
// (where the pointer is now) - (where it last warped to), so any coordinate
// the compositor hands back that is not exactly the warp target is read by
// the game as camera motion, on top of the user's real mouse.
//
// This client plays that game: `_GTK_FRAME_EXTENTS` gives it CSD drop-shadow
// margins so its window is bigger than its contents — the shape where a
// surface origin and a window origin differ — then it captures (hidden
// cursor + XGrabPointer), warps to a known point, and releases. The cursor
// must come back at exactly the warp target.
//
// Driven on stdin, one command per line, answering `ok N` in order:
//   capture          hidden cursor + grab, Xwayland takes its pointer lock
//   warp X Y         XWarpPointer to this window-local point
//   query            print the X pointer position (root- and window-local)
//   release          ungrab and show the cursor: the capture ends
//   quit
// Events from the server print as `event: ...` lines interleaved.
//
// Exit 0 when quit was reached; 2 if the window or display could not be set
// up.
#include <X11/Xlib.h>
#include <X11/Xatom.h>
#include <X11/extensions/Xfixes.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static int x_error(Display *display, XErrorEvent *event) {
    char text[128];
    XGetErrorText(display, event->error_code, text, sizeof text);
    fprintf(stderr, "X error: %s (request %d, resource 0x%lx)\n", text, event->request_code,
            (unsigned long)event->resourceid);
    return 0;
}

int main(int argc, char **argv) {
    /* The window is the whole surface: contents are inset by the frame
     * extents (left, top, right, bottom), which is what _GTK_FRAME_EXTENTS
     * declares. Pass `noextents` for the control: a window with no margins,
     * where every kind of origin agrees. */
    int extents = 8;
    if (argc > 1 && strcmp(argv[1], "noextents") == 0)
        extents = 0;

    Display *display = XOpenDisplay(NULL);
    if (!display) {
        fprintf(stderr, "no X display — is Xwayland running?\n");
        return 2;
    }
    XSetErrorHandler(x_error);
    /* Every reply is a line the test greps for the moment it is printed;
     * a wall of libc buffering in between is a race the test loses. */
    setvbuf(stdout, NULL, _IONBF, 0);

    int screen = DefaultScreen(display);
    Window root = RootWindow(display, screen);
    Window window =
        XCreateSimpleWindow(display, root, 0, 0, 800, 600, 0, 0, WhitePixel(display, screen));
    XSelectInput(display, window, StructureNotifyMask | PointerMotionMask | ButtonPressMask |
                                      ButtonReleaseMask | LeaveWindowMask);

    if (extents > 0) {
        unsigned long values[4] = {(unsigned long)extents, (unsigned long)extents,
                                   (unsigned long)extents, (unsigned long)extents};
        XChangeProperty(display, window, XInternAtom(display, "_GTK_FRAME_EXTENTS", False),
                        XA_CARDINAL, 32, PropModeReplace, (unsigned char *)values, 4);
    }

    XMapWindow(display, window);
    XFlush(display);
    for (;;) {
        XEvent event;
        XNextEvent(display, &event);
        if (event.type == MapNotify)
            break;
    }
    printf("window id: 0x%lx\n", (unsigned long)window);

    /* Where the window actually is: the test moves the compositor's cursor
     * over it before the capture starts, because a constraint the cursor is
     * not over is not activated. */
    Window child;
    int global_x = 0, global_y = 0;
    XTranslateCoordinates(display, window, root, 0, 0, &global_x, &global_y, &child);
    printf("window: %d %d %d %d\n", global_x, global_y, 800, 600);
    fflush(stdout);

    int sequence = 0;
    char line[256];
    while (fgets(line, sizeof line, stdin)) {
        int x, y;
        if (sscanf(line, "warp %d %d", &x, &y) == 2) {
            XWarpPointer(display, None, window, 0, 0, 0, 0, x, y);
            XSync(display, False);
            printf("ok %d\n", ++sequence);
        } else if (strncmp(line, "capture", 7) == 0) {
            /* Hidden cursor plus a grab is the game's capture: Xwayland sees
             * the pairing and takes its own pointer lock behind the warps.
             * XFixesHideCursor is how a game hides it — a window cursor of
             * None means "inherit", not "hidden". Idempotent — released
             * first, because the mirror of a grab's confinement reaches the
             * compositor only when the grab is installed, and the test may
             * need to retry while Xwayland finishes associating the window
             * with its surface. */
            XUngrabPointer(display, CurrentTime);
            XFixesShowCursor(display, window);
            XSync(display, False);
            XFixesHideCursor(display, window);
            int grabbed = XGrabPointer(display, True, window,
                                       PointerMotionMask | ButtonPressMask | ButtonReleaseMask |
                                           LeaveWindowMask,
                                       GrabModeAsync, GrabModeAsync, window, None, CurrentTime);
            XSync(display, False);
            printf("ok %d grab=%d\n", ++sequence, grabbed);
        } else if (strncmp(line, "release", 7) == 0) {
            XUngrabPointer(display, CurrentTime);
            XFixesShowCursor(display, window);
            XSync(display, False);
            printf("ok %d\n", ++sequence);
        } else if (strncmp(line, "query", 5) == 0) {
            Window root_ret, child_ret;
            int root_x, root_y, win_x, win_y;
            unsigned int mask;
            XQueryPointer(display, root, &root_ret, &child_ret, &root_x, &root_y, &win_x, &win_y,
                          &mask);
            Window child2;
            int local_x = 0, local_y = 0;
            XTranslateCoordinates(display, root, window, root_x, root_y, &local_x, &local_y,
                                  &child2);
            printf("query: %d %d\n", local_x, local_y);
        } else if (strncmp(line, "quit", 4) == 0) {
            printf("ok %d\n", ++sequence);
            fflush(stdout);
            return 0;
        } else {
            fprintf(stderr, "unknown command: %s", line);
        }
        fflush(stdout);

        /* Report whatever the server sent along the way. */
        while (XPending(display)) {
            XEvent event;
            XNextEvent(display, &event);
            if (event.type == MotionNotify) {
                XMotionEvent *motion = &event.xmotion;
                Window child3;
                int lx = 0, ly = 0;
                XTranslateCoordinates(display, root, window, motion->x_root, motion->y_root, &lx,
                                      &ly, &child3);
                printf("event: motion %d %d\n", lx, ly);
            } else if (event.type == ButtonPress || event.type == ButtonRelease) {
                XButtonEvent *button = &event.xbutton;
                Window child3;
                int lx = 0, ly = 0;
                XTranslateCoordinates(display, root, window, button->x_root, button->y_root, &lx,
                                      &ly, &child3);
                printf("event: button %d %d %d\n", button->button, lx, ly);
            }
        }
        fflush(stdout);
    }
    return 0;
}