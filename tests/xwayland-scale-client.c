// SPDX-License-Identifier: MIT
//
// Is the X screen scaled, and were the toolkits told about it?
//
// `xwayland.scale` is two halves that only work together. The compositor
// gives the Xwayland client a scale, which makes the X screen that many times
// larger in X pixels and divides everything coming back by the same number;
// and it publishes XSETTINGS, which is the only thing that tells an X11
// toolkit to spend those pixels on detail rather than on being enormous.
// Either half alone is a bug — half one without half two is every X11 window
// at a quarter of its size, half two without half one is every X11 window at
// four times — so this checks both, from a client, over the wire.
//
// Argument: the scale expected. Exit 0 if the screen and the settings agree
// with it, 1 if either does not, 2 if the test could not run.
//
//   xwayland-scale-client 2 1920 1080
//
// The width and height are the compositor's output in logical pixels; the X
// screen has to be that multiplied by the scale.
#include <X11/Xatom.h>
#include <X11/Xlib.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

// The XSETTINGS property is a byte stream with its own byte order marker and
// four-byte alignment, described by the XSETTINGS specification. Parsed here
// rather than linked against a library because the only dependency this
// suite's clients are allowed is libX11, and reading one integer out of it is
// forty lines.
struct reader {
    const unsigned char *data;
    unsigned long len;
    unsigned long at;
    int swap;
};

static int take(struct reader *r, void *out, unsigned long n) {
    if (r->at + n > r->len) {
        return 0;
    }
    memcpy(out, r->data + r->at, n);
    r->at += n;
    return 1;
}

static void align4(struct reader *r) { r->at = (r->at + 3) & ~3UL; }

static int take_u16(struct reader *r, uint16_t *out) {
    if (!take(r, out, 2)) {
        return 0;
    }
    if (r->swap) {
        *out = (uint16_t)((*out >> 8) | (*out << 8));
    }
    return 1;
}

static int take_u32(struct reader *r, uint32_t *out) {
    if (!take(r, out, 4)) {
        return 0;
    }
    if (r->swap) {
        *out = ((*out >> 24) & 0xff) | ((*out >> 8) & 0xff00) | ((*out << 8) & 0xff0000) |
               ((*out << 24) & 0xff000000);
    }
    return 1;
}

// Reads one named integer setting. Returns 1 and writes `*value` when the
// name is present and is an integer; 0 otherwise.
static int xsettings_integer(const unsigned char *data, unsigned long len, const char *wanted,
                             int32_t *value) {
    struct reader r = {.data = data, .len = len, .at = 0, .swap = 0};
    unsigned char order;
    if (!take(&r, &order, 1)) {
        return 0;
    }
    // 0 is LSB first. The union trick asks this machine which it is rather
    // than assuming the little-endian one everything happens to be.
    const uint16_t probe = 1;
    const int host_lsb = *(const char *)&probe == 1;
    r.swap = (order == 0) != (host_lsb != 0);
    r.at = 4; // the byte order byte and three of padding
    uint32_t serial, count;
    if (!take_u32(&r, &serial) || !take_u32(&r, &count)) {
        return 0;
    }

    for (uint32_t i = 0; i < count; i++) {
        unsigned char type, pad;
        uint16_t name_len;
        if (!take(&r, &type, 1) || !take(&r, &pad, 1) || !take_u16(&r, &name_len)) {
            return 0;
        }
        char name[256];
        if (name_len >= sizeof(name) || !take(&r, name, name_len)) {
            return 0;
        }
        name[name_len] = '\0';
        align4(&r);
        uint32_t last_change;
        if (!take_u32(&r, &last_change)) {
            return 0;
        }
        switch (type) {
        case 0: { // integer
            uint32_t raw;
            if (!take_u32(&r, &raw)) {
                return 0;
            }
            if (strcmp(name, wanted) == 0) {
                *value = (int32_t)raw;
                return 1;
            }
            break;
        }
        case 1: { // string
            uint32_t str_len;
            if (!take_u32(&r, &str_len)) {
                return 0;
            }
            r.at += str_len;
            align4(&r);
            break;
        }
        case 2: // colour: four 16-bit components
            r.at += 8;
            break;
        default:
            return 0;
        }
    }
    return 0;
}

int main(int argc, char **argv) {
    if (argc != 4) {
        fprintf(stderr, "usage: %s SCALE LOGICAL_WIDTH LOGICAL_HEIGHT\n", argv[0]);
        return 2;
    }
    const int scale = atoi(argv[1]);
    const int logical_width = atoi(argv[2]);
    const int logical_height = atoi(argv[3]);
    if (scale < 1 || logical_width < 1 || logical_height < 1) {
        fprintf(stderr, "the scale and the output size have to be positive\n");
        return 2;
    }

    Display *display = XOpenDisplay(NULL);
    if (!display) {
        fprintf(stderr, "no X display — is Xwayland running?\n");
        return 2;
    }
    const int screen = DefaultScreen(display);

    int failed = 0;

    const int width = DisplayWidth(display, screen);
    const int height = DisplayHeight(display, screen);
    printf("screen: %dx%d, wanted %dx%d\n", width, height, logical_width * scale,
           logical_height * scale);
    if (width != logical_width * scale || height != logical_height * scale) {
        failed = 1;
    }

    // The settings live on whichever window owns the _XSETTINGS_S<screen>
    // selection, which is the manager the compositor started. No owner at all
    // means the compositor published nothing.
    char selection_name[32];
    snprintf(selection_name, sizeof(selection_name), "_XSETTINGS_S%d", screen);
    const Atom selection = XInternAtom(display, selection_name, False);
    const Window owner = XGetSelectionOwner(display, selection);
    if (owner == None) {
        // At 1x this is the expected answer, and the interesting half of the
        // test: a compositor that publishes a scaling factor when nobody
        // asked for one has changed what every existing X11 desktop looks
        // like. At any other scale it is the whole feature missing.
        printf("xsettings: nobody owns %s\n", selection_name);
        XCloseDisplay(display);
        return scale == 1 ? failed : 1;
    }

    const Atom property = XInternAtom(display, "_XSETTINGS_SETTINGS", False);
    Atom type;
    int format;
    unsigned long count, after;
    unsigned char *data = NULL;
    if (XGetWindowProperty(display, owner, property, 0, 8192, False, AnyPropertyType, &type,
                           &format, &count, &after, &data) != Success ||
        data == NULL) {
        printf("xsettings: the manager window carries no settings\n");
        XCloseDisplay(display);
        return scale == 1 ? failed : 1;
    }

    int32_t gdk_scale = 0;
    int32_t xft_dpi = 0;
    const int have_gdk =
        xsettings_integer(data, count * (unsigned long)(format / 8), "Gdk/WindowScalingFactor",
                          &gdk_scale);
    const int have_dpi =
        xsettings_integer(data, count * (unsigned long)(format / 8), "Xft/DPI", &xft_dpi);
    XFree(data);
    XCloseDisplay(display);

    printf("xsettings: Gdk/WindowScalingFactor=%d (present %d), Xft/DPI=%d (present %d)\n",
           gdk_scale, have_gdk, xft_dpi, have_dpi);
    if (scale == 1) {
        // Something else may own the selection on a desk that runs its own
        // settings daemon; what must not be there is a scaling factor this
        // compositor put there without being asked.
        if (have_gdk && gdk_scale != 1) {
            failed = 1;
        }
        return failed;
    }
    if (!have_gdk || gdk_scale != scale) {
        failed = 1;
    }
    // 1024ths of a point, and 96 is the density X11 assumes when nothing says
    // otherwise — so a scale of 2 is 192dpi and nothing else.
    if (!have_dpi || xft_dpi != 96 * 1024 * scale) {
        failed = 1;
    }

    return failed;
}
