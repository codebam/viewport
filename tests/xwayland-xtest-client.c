/* SPDX-License-Identifier: MIT */
/*
 * An X client that emulates input the way X clients have for forty years:
 * XTestFakeRelativeMotionEvent, a pair of motions and nothing else.
 *
 * On a rootless Xwayland those requests used to be delivered to X clients and
 * to nothing else — the compositor never heard of them. Since Xwayland 23.2
 * the server can hand them to libei instead, negotiated through
 * `org.freedesktop.portal.RemoteDesktop`, and tests/xwayland-xtest-portal.test.sh
 * runs this client to prove the request arrives at this compositor's
 * implementation of it. Nothing about the client is special: that is the
 * point. Steam Input's desktop layout, `xdotool` and every other XTEST user
 * in existence send exactly this.
 *
 * Exit status: 0 when the motions were sent, 2 when this X server has no
 * XTEST extension (the test reports that as "cannot run here"), 1 on any
 * other failure.
 */
#include <X11/Xlib.h>
#include <X11/extensions/XTest.h>
#include <stdio.h>

int main(void) {
    Display *display = XOpenDisplay(NULL);
    if (display == NULL) {
        fprintf(stderr, "xtest-client: no display to open\n");
        return 1;
    }

    int event_base = 0, error_base = 0, major = 0, minor = 0;
    if (!XTestQueryExtension(display, &event_base, &error_base, &major, &minor)) {
        fprintf(stderr, "xtest-client: this server has no XTEST extension\n");
        XCloseDisplay(display);
        return 2;
    }

    /* Two motions, as a person's controller produces: the first is what makes
       Xwayland set up the connection, and the second arrives with one to go
       to. Both are queued by the server until the session is answered, which
       is the behaviour the portal handshake depends on. */
    XTestFakeRelativeMotionEvent(display, 40, 0, CurrentTime);
    XTestFakeRelativeMotionEvent(display, -40, 0, CurrentTime);
    XFlush(display);
    XCloseDisplay(display);
    return 0;
}
