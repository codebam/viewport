// SPDX-License-Identifier: MIT
// A window shaped like FINAL FANTASY XVI: an X11 client whose WM_CLASS is the
// game's Steam app id, _NET_WM_WINDOW_TYPE_NORMAL, and a fullscreen request in
// whatever shape the caller names.
//
//   xwayland-fullscreen-client pre   _NET_WM_STATE=FULLSCREEN before the map
//   xwayland-fullscreen-client post  mapped windowed, then a ClientMessage
//   xwayland-fullscreen-client prop  mapped windowed, then the property is
//                                    written directly after three seconds —
//                                    the shape Wine uses, and the game's
//   xwayland-fullscreen-client plain no request at all (control)
//
// Prints `mapped 0xID` once up and painted, and `wrote property` when the prop
// mode's write has gone out; after that it serves events until killed.
#include <X11/Xatom.h>
#include <X11/Xlib.h>
#include <X11/Xutil.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

int main(int argc, char **argv) {
	const char *mode = argc > 1 ? argv[1] : "prop";

	Display *d = XOpenDisplay(NULL);
	if (!d) {
		fprintf(stderr, "no X display — is Xwayland running?\n");
		return 2;
	}
	int screen = DefaultScreen(d);
	Window root = RootWindow(d, screen);
	Window win = XCreateSimpleWindow(d, root, 0, 0, 512, 288, 0,
		BlackPixel(d, screen), WhitePixel(d, screen));

	XStoreName(d, win, "FINAL FANTASY XVI");
	XClassHint class_hint = { .res_name = "ffxvi.exe",
		.res_class = "steam_app_2515020" };
	XSetClassHint(d, win, &class_hint);
	XSelectInput(d, win, StructureNotifyMask | ExposureMask);

	Atom net_type = XInternAtom(d, "_NET_WM_WINDOW_TYPE", False);
	Atom net_normal = XInternAtom(d, "_NET_WM_WINDOW_TYPE_NORMAL", False);
	XChangeProperty(d, win, net_type, XA_ATOM, 32, PropModeReplace,
		(unsigned char *)&net_normal, 1);

	Atom net_state = XInternAtom(d, "_NET_WM_STATE", False);
	Atom fullscreen = XInternAtom(d, "_NET_WM_STATE_FULLSCREEN", False);

	if (strcmp(mode, "pre") == 0) {
		// The state is on the window before it is mapped, which is what
		// a manager reads at MapRequest time.
		XChangeProperty(d, win, net_state, XA_ATOM, 32, PropModeReplace,
			(unsigned char *)&fullscreen, 1);
	}

	XMapWindow(d, win);
	XFlush(d);

	int asked = 0;
	int painted = 0;
	time_t mapped_at = 0;
	for (;;) {
		XEvent event;
		while (XPending(d)) {
			XNextEvent(d, &event);
			if (event.type == MapNotify) {
				GC gc = XCreateGC(d, win, 0, NULL);
				XSetForeground(d, gc, WhitePixel(d, screen));
				XFillRectangle(d, win, gc, 0, 0, 512, 288);
				XFreeGC(d, gc);
				XFlush(d);
				printf("mapped 0x%lx\n", (unsigned long)win);
				fflush(stdout);
				painted = 1;
				mapped_at = time(NULL);

				if (strcmp(mode, "post") == 0 && !asked) {
					asked = 1;
					// A runtime request, the way a client asks
					// a manager directly.
					XEvent e;
					memset(&e, 0, sizeof(e));
					e.xclient.type = ClientMessage;
					e.xclient.window = win;
					e.xclient.message_type = net_state;
					e.xclient.format = 32;
					e.xclient.data.l[0] = 1; // _NET_WM_STATE_ADD
					e.xclient.data.l[1] = (long)fullscreen;
					e.xclient.data.l[2] = 0;
					e.xclient.data.l[3] = 1; // normal application
					e.xclient.data.l[4] = 0;
					XSendEvent(d, root, False,
						SubstructureRedirectMask
						| SubstructureNotifyMask, &e);
					XFlush(d);
				}
			}
		}
		if (strcmp(mode, "prop") == 0 && painted && !asked
			&& time(NULL) - mapped_at >= 3) {
			asked = 1;
			// The shape Wine actually uses: the new state is
			// written straight into the property, well after the
			// window mapped — no ClientMessage follows, and
			// nothing else announces the change.
			XChangeProperty(d, win, net_state, XA_ATOM, 32,
				PropModeReplace, (unsigned char *)&fullscreen, 1);
			XFlush(d);
			printf("wrote property\n");
			fflush(stdout);
		}
		usleep(50000);
	}
	return 0;
}
