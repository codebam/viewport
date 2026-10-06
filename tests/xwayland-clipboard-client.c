// SPDX-License-Identifier: MIT
//
// Paste the clipboard into an X11 window.
//
// This is what an X11 application does when somebody presses Ctrl+V: ask the
// X server for CLIPBOARD in the type a toolkit pastes text as, and read what
// comes back. Under Xwayland the answer is not the server's to give — the
// compositor has to have told Xwayland that it holds the selection and what
// types it offers — so this is the client half of a question a Wayland client
// cannot ask.
//
// The window is mapped and waited on rather than kept minimal because the
// compositor only lets an X client read the selection while an X window holds
// the keyboard: the request is refused otherwise, and a client that never got
// the focus would be measuring that rule instead of the one this is here for.
//
// The text arrives on stdout. Exit 0 when it did, 1 when the request was
// refused or came back empty, 2 when the test could not run at all.

#include <X11/Xlib.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

// How long to wait for the events this needs. Everything here is answered by
// the compositor and Xwayland, which are on the same machine; a deadline is
// for the failure case, not for a slow one.
#define MAP_TIMEOUT 10
#define FOCUS_TIMEOUT 10
#define SELECTION_TIMEOUT 5

static int wait_for(Display *display, int type, XEvent *event, int seconds) {
	time_t deadline = time(NULL) + seconds;
	while (time(NULL) < deadline) {
		while (XPending(display)) {
			XNextEvent(display, event);
			if (event->type == type) {
				return 1;
			}
		}
		usleep(20000);
	}
	return 0;
}

int main(void) {
	Display *display = XOpenDisplay(NULL);
	if (!display) {
		fprintf(stderr, "no display — is Xwayland running?\n");
		return 2;
	}

	int screen = DefaultScreen(display);
	Window window = XCreateSimpleWindow(display, RootWindow(display, screen), 0, 0, 400, 300, 0,
	                                    BlackPixel(display, screen), WhitePixel(display, screen));
	XStoreName(display, window, "viewport-clipboard-test");
	XSelectInput(display, window, StructureNotifyMask | FocusChangeMask);
	XMapWindow(display, window);
	XFlush(display);

	XEvent event;
	if (!wait_for(display, MapNotify, &event, MAP_TIMEOUT)) {
		fprintf(stderr, "the window was never mapped\n");
		return 2;
	}

	// Focus is set by the compositor a moment after the map, and how long that
	// takes is not something to hard-code.
	Window focused = None;
	int revert = 0;
	int focused_self = 0;
	time_t deadline = time(NULL) + FOCUS_TIMEOUT;
	while (time(NULL) < deadline) {
		XGetInputFocus(display, &focused, &revert);
		if (focused == window) {
			focused_self = 1;
			break;
		}
		usleep(100000);
	}
	if (!focused_self) {
		fprintf(stderr, "the window never held the input focus\n");
		return 2;
	}

	Atom clipboard = XInternAtom(display, "CLIPBOARD", False);
	Atom utf8 = XInternAtom(display, "UTF8_STRING", False);
	Atom property = XInternAtom(display, "VIEWPORT_CLIPBOARD_TEST", False);
	XDeleteProperty(display, window, property);
	XConvertSelection(display, clipboard, utf8, property, window, CurrentTime);
	XFlush(display);

	if (!wait_for(display, SelectionNotify, &event, SELECTION_TIMEOUT)) {
		fprintf(stderr, "the selection request was never answered\n");
		return 1;
	}
	if (event.xselection.property == None) {
		fprintf(stderr, "the selection request was refused\n");
		return 1;
	}

	Atom type;
	int format;
	unsigned long items, remaining;
	unsigned char *data = NULL;
	if (XGetWindowProperty(display, window, property, 0, 65536, False, AnyPropertyType, &type,
	                       &format, &items, &remaining, &data) != Success ||
	    data == NULL) {
		fprintf(stderr, "the selection property was empty\n");
		return 1;
	}

	fwrite(data, 1, items, stdout);
	XFree(data);
	XCloseDisplay(display);
	return 0;
}
