# Arch and CachyOS packaging

Everything Viewport needs is already in the Arch repositories, including the
awkward one: `wpewebkit` there is built with the WPE platform API enabled, so
`wpe-platform-2.0.pc` exists and no WebKit has to be compiled. `wlroots0.20` is
packaged at exactly the version this is developed against. So this is a small
package rather than an afternoon of building browser engines.

CachyOS is Arch with different compiler flags and its own mirrors; the same
package works there unchanged.

## Installing a built package

```sh
sudo pacman -U viewport-0.1.0-1-x86_64.pkg.tar.zst
```

`pacman` pulls the dependencies from the ordinary repositories. Nothing else is
needed — the compositor ships its own shell and defaults to it, so `viewport`
from a TTY brings up a working desktop. A display manager will also offer it as
a session, since a `wayland-sessions` entry is installed.

Optional, and each one only matters for a specific default binding:
`xorg-xwayland` for X11 applications, `swaylock` for the idle lock,
`playerctl`, `wireplumber` and `brightnessctl` for the media keys, and `rio`
and `wmenu` for the terminal and launcher bindings.

## Trying it on real hardware without installing it

`run-in-container.sh` builds the package, installs it into a clean Arch image,
and runs it on the actual display and input devices — what someone receiving the
package gets, with nothing left over from development.

```sh
# from a text console, not from inside a desktop
./packaging/arch/run-in-container.sh
```

It refuses to start in TTY mode from inside a graphical session, since taking
DRM master from the compositor you are currently using does not end well. Pass
`--nested` to open it as a window in the session you are in instead, or
`--build-only` to build the images and run nothing.

TTY mode needs root. Taking DRM master and reading input devices is not
something a rootless container can do, and there is no logind inside the
container to ask — libseat's builtin backend opens the devices directly, which
only works as root. That is also why it is `--privileged`: this is a throwaway
container driving real hardware, and pinning down exactly which capabilities
libinput and the GPU driver need across kernel versions is a worse trade than
granting them.

`--shell` gives a root shell in the container with the same devices, for when
the interesting question is what the environment looks like rather than whether
the compositor starts.

## Building it

The `PKGBUILD` takes a source tarball rather than fetching, so it can be built
from a working tree that has not been pushed anywhere:

```sh
git archive --format=tar.gz --prefix=viewport-0.1.0/ -o viewport-0.1.0.tar.gz HEAD
cp packaging/arch/PKGBUILD .
makepkg -s
```

## Building without an Arch machine

A container works, with two wrinkles that are worth knowing because neither
error says what is actually wrong.

`pacman` 7 drops privileges to a separate user for downloads, which fails in a
rootless container with `failed to chown temporary download directory`. Pass
`--disable-sandbox`.

`makepkg` refuses to run as root, and rootless containers usually only give you
root. The way through is to install the dependencies as root while building an
image, then run `makepkg` as a mapped ordinary user:

```sh
podman build -t viewport-builder -f packaging/arch/Containerfile .

podman run --rm --userns=keep-id:uid=1000,gid=1000 -e HOME=/tmp \
  -v "$PWD:/out:z" localhost/viewport-builder bash -c '
    mkdir -p /tmp/work && cd /tmp/work
    cp /out/PKGBUILD /out/viewport-0.1.0.tar.gz .
    makepkg --noconfirm --nodeps
    cp *.pkg.tar.zst /out/
  '
```

`--nodeps` because the image already has them; `keep-id` because the bind mount
belongs to the host user and `chown` inside the namespace cannot change that.
