{
  description = "Viewport — a wlroots compositor whose shell is rendered by WPE WebKit";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    # For one thing only: `-Zsanitizer=address` is nightly-gated and nixpkgs
    # ships a stable rustc. Everything else builds with the pinned stable
    # toolchain; see `devShells.asan`.
    #
    # Following our nixpkgs so this adds a toolchain and not a second package
    # set — in particular it must not move the nixpkgs revision, because that
    # revision is what every prebuilt path in this flake substitutes against.
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; };

        # A second package set, for the toolchain and nothing else.
        #
        # Deliberately not an overlay on `pkgs`. That would put a different
        # rustc in front of every derivation that uses one — mesa builds Rust
        # components — which changes the WPE WebKit closure, and that closure
        # is a WebKit build. Everything except `devShells.asan` continues to
        # evaluate against the untouched `pkgs`.
        nightly =
          (import nixpkgs {
            inherit system;
            overlays = [ rust-overlay.overlays.default ];
          }).rust-bin.nightly.latest;

        # ------------------------------------------------------------------
        # WPE WebKit.
        #
        # nixpkgs has `libwpe` and `libwpe-fdo`, but those are only the backend
        # ABI and one implementation of it — neither contains a WebKit engine
        # (`nm -D libWPEBackend-fdo-1.0.so | grep -c webkit` == 0). There is no
        # `wpewebkit` attribute in nixpkgs at all.
        #
        # The `webkitgtk` tarball cannot be reused with -DPORT=WPE either: it
        # ships the GTK port only (no Source/WebKit/UIProcess/API/wpe, no
        # Source/cmake/OptionsWPE.cmake). WPE is a separate upstream release.
        #
        # So: same WebKit base version as nixpkgs' webkitgtk (2.52.5), but the
        # WPE tarball, and we inherit nixpkgs' dependency closure rather than
        # re-deriving ~40 buildInputs by hand.
        #
        # We build with ENABLE_WPE_PLATFORM=ON, which selects WPE_API_VERSION
        # 2.0 and gives us WPEDisplay/WPEView subclassing plus WPEBufferDMABuf.
        # That drops libwpe and libwpe-fdo from the graph entirely.
        # ------------------------------------------------------------------
        wpewebkit = (pkgs.webkitgtk_6_0.overrideAttrs (old: {
          # nixpkgs sets `name` directly (with an +abi= suffix), and `name`
          # wins over pname/version — so it has to be overridden explicitly or
          # the store path still claims to be webkitgtk.
          name = "wpewebkit-2.52.5";
          pname = "wpewebkit";
          version = "2.52.5";

          # nixpkgs declares a devdoc output, populated by gi-docgen under
          # ENABLE_DOCUMENTATION. We build with documentation off, so nothing
          # ever creates that path and Nix fails the derivation at the very end
          # — after a full WebKit compile. Drop the output instead.
          #
          # "debug" is deliberately absent: separateDebugInfo appends it, and
          # naming it here too is a duplicate-output eval error.
          outputs = [ "out" "dev" ];

          src = pkgs.fetchurl {
            url = "https://wpewebkit.org/releases/wpewebkit-2.52.5.tar.xz";
            hash = "sha256-vPxskdt2WdzyT2/3mtJ6werhvGHcoNv+4VRwaSZ0Czs=";
          };

          # Most of nixpkgs' webkitgtk patches target the GTK port's layout,
          # but fix-bubblewrap-paths.patch does not: it teaches WebKit's
          # sandbox launcher to bind-mount Nix store paths into the bwrap
          # namespace. Without it the web process dies at startup with
          #   bwrap: execvp .../xdg-dbus-proxy: No such file or directory
          # even though the path exists — bwrap simply cannot see it. The file
          # it patches, UIProcess/Launcher/glib/BubblewrapLauncher.cpp, is
          # shared glib code and is present in the WPE tarball too.
          patches = builtins.filter
            (p: builtins.match ".*fix-bubblewrap-paths.*" (toString p) != null)
            (old.patches or [ ]);

          # Likewise, GTK-specific postPatch fixups do not apply here.
          postPatch = ''
            patchShebangs .
          '';

          buildInputs = old.buildInputs or [ ] ++ (with pkgs; [
            libdrm
            mesa
            libglvnd
            libxkbcommon
            libinput
            udev
            wayland
            wayland-protocols
          ]);

          nativeBuildInputs = old.nativeBuildInputs or [ ] ++ (with pkgs; [
            wayland-scanner
          ]);

          cmakeFlags = [
            "-DPORT=WPE"

            # WPEPlatform: the modern backend API. Implies WPE_API_VERSION=2.0,
            # so pkg-config names are wpe-webkit-2.0 / wpe-platform-2.0 and the
            # shared object is libWPEWebKit-2.0.so.
            "-DENABLE_WPE_PLATFORM=ON"
            "-DENABLE_WPE_LEGACY_API=OFF"

            # OptionsWPE.cmake declares:
            #   WEBKIT_OPTION_CONFLICT(ENABLE_WPE_PLATFORM ENABLE_WPE_1_1_API)
            # and requires at least one of WPE_PLATFORM / WPE_LEGACY_API.
            "-DENABLE_WPE_1_1_API=OFF"

            # We supply our own WPEDisplay, so the bundled platform backends are
            # dead weight — except headless, which is handy for `--headless`
            # smoke tests without a seat.
            "-DENABLE_WPE_PLATFORM_DRM=OFF"
            "-DENABLE_WPE_PLATFORM_WAYLAND=OFF"
            "-DENABLE_WPE_PLATFORM_HEADLESS=ON"
            "-DUSE_GBM=ON"

            "-DENABLE_INTROSPECTION=OFF"
            "-DENABLE_DOCUMENTATION=OFF"
            "-DENABLE_MINIBROWSER=OFF"
            "-DENABLE_WEBDRIVER=OFF"
            "-DENABLE_JOURNALD_LOG=OFF"
            "-DCMAKE_BUILD_TYPE=Release"

            # Assigning cmakeFlags replaces nixpkgs' list wholesale, which
            # would silently drop the sandbox helper paths and leave the web
            # process unable to launch. Put them back.
            "-DBWRAP_EXECUTABLE=${pkgs.bubblewrap}/bin/bwrap"
            "-DDBUS_PROXY_EXECUTABLE=${pkgs.xdg-dbus-proxy}/bin/xdg-dbus-proxy"
          ];

          meta = old.meta or { } // {
            description = "WPE WebKit — WebKit port for embedded, WPEPlatform API";
            homepage = "https://wpewebkit.org/";
          };
        }));

        wlroots = pkgs.wlroots_0_20 or pkgs.wlroots;

        nativeDeps = with pkgs; [
          meson
          ninja
          pkg-config
          wayland-scanner
          # The Vulkan renderer's shaders are committed as SPIR-V, so this is
          # only needed to change one — but without it in the shell there is no
          # way to, and the .spv and the .frag drift apart silently.
          glslang
        ];

        # What the compositor links and dlopens.
        #
        # Four entries left with the C tree and are worth naming so they do not
        # come back: `wlroots` and `pixman` were its renderer, `libxcb-wm` was
        # there because wlroots' xwayland.h wants the xcb-ewmh headers, and
        # `json-glib` was its IPC parser — the Rust one is serde, in
        # `crates/viewport-ipc`. Nothing under `crates/` references any of them
        # outside comments about what the C build did.
        runtimeDeps = with pkgs; [
          wayland
          wayland-protocols
          libxkbcommon
          libdrm
          mesa
          libglvnd
          udev
          libinput
          seatd
          # Not linked — spawned. Smithay's X11Wm runs the real Xwayland
          # binary, so it has to be on PATH for X11 clients to connect.
          xwayland
          # glib and the engine go together: the WPE shim in
          # crates/viewport-web/shim is C against GObject.
          glib
          wpewebkit
          # The screencast portal's transport.
          pipewire
        ];

        # ------------------------------------------------------------------
        # CEF, in the layout the Rust bindings expect.
        #
        # nixpkgs ships the distribution as it comes: `Release/` holds the
        # engine, `Resources/` holds the .pak files and locales. `cef-dll-sys`
        # wants what its own downloader produces, which is those two flattened
        # into one directory beside `include/`, `libcef_dll/`, `cmake/` and
        # `CMakeLists.txt` — point it at the unflattened tree and it fails on a
        # missing `locales`.
        #
        # `archive.json` is what stops the build script downloading a CEF of
        # its own, which a sandbox forbids. Three fields, and the only one that
        # is read is `name`: the version is parsed out of it and accepted if it
        # is not *newer* than the version the crate wants, so 149.0.5 satisfies
        # a crate built against 149.0.6.
        #
        # Symlinks rather than copies: this is 1.3 GB of engine, and nothing in
        # the build writes to it.
        # ------------------------------------------------------------------
        cefDistribution = pkgs.runCommand "cef-flat-${pkgs.cef-binary.version}" { } ''
          mkdir -p $out
          for entry in ${pkgs.cef-binary}/Release/* ${pkgs.cef-binary}/Resources/*; do
            ln -s "$entry" "$out/$(basename "$entry")"
          done
          for entry in include libcef_dll cmake; do
            ln -s ${pkgs.cef-binary}/$entry $out/$entry
          done
          cp ${pkgs.cef-binary}/CMakeLists.txt ${pkgs.cef-binary}/CREDITS.html $out/
          cat > $out/archive.json <<'JSON'
          {
            "type": "minimal",
            "name": "cef_binary_${pkgs.cef-binary.version}+g0000000+chromium-149.0.0.0_linux64_minimal",
            "sha1": "0000000000000000000000000000000000000000"
          }
          JSON
        '';

        # The shell process for the cef backend.
        #
        # Its own derivation because the crate is outside the workspace — it
        # does not build without `CEF_PATH` — so it has its own lock file and
        # cannot be a `-p` of the compositor's build.
        viewport-shell-cef = pkgs.rustPlatform.buildRustPackage {
          pname = "viewport-shell-cef";
          version = "0.1.2";

          # The whole tree, built from inside the crate.
          #
          # `sourceRoot` rather than `buildAndTestSubdir`, because it is the
          # lock file at the source root that `buildRustPackage` validates
          # against the vendored dependencies — and this crate has its own,
          # being outside the workspace. The rest of the tree still has to be
          # here: `viewport-ipc` and `viewport-shell-bridge` inherit their
          # version and dependencies from the root manifest above them.
          src = self;
          sourceRoot = "source/crates/viewport-shell-cef";
          cargoLock.lockFile = ./crates/viewport-shell-cef/Cargo.lock;

          CEF_PATH = cefDistribution;

          nativeBuildInputs = with pkgs; [
            pkg-config
            makeWrapper
            # `libcef_dll_wrapper` is C++ built from the distribution's own
            # source. The engine beside it is a prebuilt blob and is not
            # compiled by anything here.
            cmake
            ninja
          ];

          # Both setup hooks take over phases they should not here: the build
          # script drives cmake and ninja itself, from inside cargo, and the
          # source root this builds from has no CMakeLists.txt or build.ninja
          # of its own. Left on, ninja's hook fails the build phase with
          # "loading 'build.ninja': No such file or directory", which names the
          # tool rather than the hook.
          dontUseCmakeConfigure = true;
          dontUseNinjaBuild = true;
          dontUseNinjaInstall = true;
          dontUseNinjaCheck = true;

          buildInputs = with pkgs; [ nss nspr at-spi2-atk cups libdrm libxkbcommon mesa ];

          doCheck = false;

          # The build script copies the engine and its resources next to the
          # binary, which for a nix build is the target directory rather than
          # anywhere that survives. They are taken from the flattened tree
          # instead, and the binary is pointed at them.
          postInstall = ''
            mkdir -p $out/lib/viewport-cef
            for entry in ${cefDistribution}/*; do
              case "$(basename "$entry")" in
                include|libcef_dll|cmake|CMakeLists.txt|CREDITS.html|archive.json) ;;
                *) ln -s "$entry" $out/lib/viewport-cef/ ;;
              esac
            done
            wrapProgram $out/bin/viewport-shell-cef \
              --prefix LD_LIBRARY_PATH : $out/lib/viewport-cef
          '';

          meta = with pkgs.lib; {
            description = "The Viewport shell, rendered by Chromium embedded through CEF";
            platforms = platforms.linux;
            mainProgram = "viewport-shell-cef";
          };
        };

        # The compositor, as a function of which engine draws its shell.
        #
        # There are three, and the difference between them is almost entirely a
        # packaging one — see `crates/viewport/src/shell_backend.rs`:
        #
        #   wpe        the engine in-process. `wpewebkit` above, which is a
        #              four-hour build no binary cache has.
        #   webkitgtk  the same WebKit as a separate process and an ordinary
        #              Wayland client, from the prebuilt nixpkgs package.
        #   chromium   Blink, in a browser this does not link at all: the shell
        #              process starts one and drives it over the DevTools
        #              protocol, so the engine is a runtime dependency rather
        #              than a build one.
        #
        # The last two add a second binary to bin/, which the compositor finds
        # beside itself and starts. All three install a binary called
        # `viewport`, so a system installs one of them.
        mkViewport = { shellBackend }:
          let
            # The crate that provides the shell process, where there is one.
            shellCrate = {
              webkitgtk = "viewport-shell-gtk";
              chromium = "viewport-shell-chromium";
            }.${shellBackend} or null;
          in
          pkgs.rustPlatform.buildRustPackage {
          # Named for the engine, like the attribute is. Both produce a binary
          # called `viewport`; the store path is the only thing that says which
          # of them a running compositor came from.
          pname = "viewport-${shellBackend}";
          version = "0.1.2";
          src = self;

          cargoLock = {
            lockFile = ./Cargo.lock;
            # A fork of smithay, for the tearing-control patch. A git
            # dependency has no crates.io hash to check against, so its
            # contents are pinned here instead.
            outputHashes = {
              "smithay-0.7.0" = "sha256-V8Ly3tQwChYJzZKEeRA//Vh7OmbzhgayJKlMQW3byt0=";
            };
          };

          # From the workspace root rather than `buildAndTestSubdir`, because
          # the out-of-process backend is two binaries out of one tree and a
          # subdirectory build can only produce one of them.
          cargoBuildFlags = [ "-p" "viewport" ]
            ++ pkgs.lib.optionals (shellCrate != null) [ "-p" shellCrate ];
          buildFeatures = pkgs.lib.optionals (shellBackend == "wpe") [ "wpe" ];

          nativeBuildInputs = with pkgs; [
            pkg-config
            rustPlatform.bindgenHook
            makeWrapper
            wayland-scanner
          ] ++ pkgs.lib.optionals (shellBackend == "webkitgtk") [
            # A GTK program needs its GSettings schemas and GIO modules named
            # in the environment or it aborts at startup rather than starting
            # without them.
            wrapGAppsHook4
          ];

          # This package wraps `viewport` itself, and two wrappers around one
          # binary is one too many. The hook's arguments are applied by hand
          # below, to the binary that actually needs them.
          dontWrapGApps = true;

          buildInputs =
            (if shellBackend == "wpe"
             then runtimeDeps
             else builtins.filter (dep: dep != wpewebkit) runtimeDeps)
            ++ (with pkgs; [
              vulkan-loader
              vulkan-headers
              libgbm
            ])
            ++ pkgs.lib.optionals (shellBackend == "webkitgtk") (with pkgs; [
              gtk4
              webkitgtk_6_0
              # WebKit refuses a `file://` page whose type it cannot work out,
              # and it works it out from the shared MIME database. Without this
              # the shell loads "successfully" and the desktop is empty.
              shared-mime-info
              gsettings-desktop-schemas
            ]);

          # The renderer, gbm and EGL are opened by name at run time rather
          # than linked, so the closure has to carry them and the loader has to
          # be told where they are. Getting this wrong produces "Failed to load
          # the Vulkan library", which says nothing about the cause.
          # The shell itself, and the page it falls back to. Without them an
          # installed compositor has nothing to load: the default URL resolves
          # beside the binary, and a session started from a login shell has no
          # source tree under it.
          postInstall = ''
            mkdir -p $out/share/viewport
            cp -r ${self}/data/shell $out/share/viewport/shell

            # How xdg-desktop-portal learns this backend exists. Without the
            # file the frontend does not know the name "viewport" refers to
            # anything, so a config naming it matches nothing and the request
            # goes to whichever backend is left.
            mkdir -p $out/share/xdg-desktop-portal/portals
            cp ${self}/data/portal-share/xdg-desktop-portal/portals/viewport.portal \
              $out/share/xdg-desktop-portal/portals/viewport.portal
            cp ${self}/data/fallback.html $out/share/viewport/fallback.html
            cp ${self}/data/config.example.json $out/share/viewport/config.example.json

            # Which engine this package was built to use.
            #
            # The binary picks a default of its own — the in-process engine
            # when it was compiled in, the out-of-process one otherwise — and
            # that default cannot know which shell program was installed beside
            # it. A `chromium` package whose compositor went looking for
            # `viewport-shell-gtk` came up windows-only with the reason in the
            # log, which is exactly the case this closes.
            #
            # `--set` and not `--set-default`, which it was until it bit.
            #
            # A running session exports this to every process it starts, so a
            # terminal inside a `cef` desktop has `VIEWPORT_SHELL_BACKEND=cef`
            # in its environment — and `--set-default` then does nothing for a
            # `nix run .#webkitgtk` typed into that terminal. The webkitgtk
            # package came up on cef, which is the one engine that cannot draw
            # a wallpaper terminal, and the refusal named an engine the user
            # had not asked for.
            #
            # A package named for an engine runs that engine. `--shell-backend`
            # still wins over both, which is where "use something else" lives.
            wrapProgram $out/bin/viewport \
              --set VIEWPORT_SHELL_BACKEND ${shellBackend} \
              --prefix LD_LIBRARY_PATH : ${pkgs.lib.makeLibraryPath (with pkgs; [
                vulkan-loader
                libgbm
                libglvnd
                libxkbcommon
                wayland
                seatd
                libinput
                udev
              ])}
          '' + pkgs.lib.optionalString (shellBackend == "webkitgtk") ''
            # The shell process, which the compositor starts by looking beside
            # itself in bin/. This is where the GTK hook's environment goes:
            # the schemas, the GIO modules and the MIME database.
            wrapProgram $out/bin/viewport-shell-gtk "''${gappsWrapperArgs[@]}"
          '' + pkgs.lib.optionalString (shellBackend == "cef") ''
            # The shell process, from its own derivation: the crate is outside
            # the workspace, so it cannot be built as a `-p` of this one.
            ln -s ${viewport-shell-cef}/bin/viewport-shell-cef $out/bin/
          '' + pkgs.lib.optionalString (shellBackend == "chromium") ''
            # The engine, named rather than looked for. `chromium` on PATH is
            # whatever the session happens to have installed, and a desktop
            # should not change engine because a user installed a browser.
            wrapProgram $out/bin/viewport-shell-chromium \
              --set-default VIEWPORT_CHROMIUM_BIN ${pkgs.chromium}/bin/chromium
          '';

          # The suite needs a GPU for the renderer tests and a socket for the
          # compositor ones; the sandbox has neither. The `rust` job in
          # .github/workflows/ci.yml runs `cargo test --workspace` outside the
          # sandbox, which is what covers this.
          doCheck = false;

          meta = with pkgs.lib; {
            description =
              "The Smithay rewrite of the Viewport compositor, shell on ${shellBackend}";
            platforms = platforms.linux;
            mainProgram = "viewport";
          };
        };

        # The engine in-process: what this project has always built.
        wpe = mkViewport { shellBackend = "wpe"; };

        # The engine out of process, from nixpkgs' prebuilt WebKitGTK. Nothing
        # in this closure is built from a WebKit tarball.
        webkitgtk = mkViewport { shellBackend = "webkitgtk"; };

        # Blink, in a browser this does not link. The heaviest closure of the
        # three and the lightest build: nothing here compiles an engine, and
        # the one it runs is nixpkgs' chromium.
        chromium = mkViewport { shellBackend = "chromium"; };

        # The same Blink, embedded rather than driven. No browser process, no
        # DevTools pipe over a socket — the protocol goes straight into the
        # library — and the engine is nixpkgs' prebuilt libcef.
        cef = mkViewport { shellBackend = "cef"; };

        # Shared by the two Rust shells below, which differ only in toolchain.
        rustDeps = with pkgs; [
            pkg-config
            # drm-sys and input-sys generate their bindings with bindgen, which
            # needs libclang and the C headers found for it.
            rustPlatform.bindgenHook

            # scripts/integration.sh compiles the Wayland clients that
            # tests/capture.test.sh and tests/lock.test.sh drive. They need a
            # C compiler and the marshalling code for the protocols they
            # speak, and nothing else — no wlroots, no WebKit, which is what
            # keeps the whole integration suite on an unassisted runner.
            wayland-scanner

            libdrm
            libgbm
            libinput
            seatd
            udev
            wayland
            wayland-protocols
            libxkbcommon
            pipewire

            # The headless backend composites captures through a surfaceless
            # EGL display, so the tests that ask for a pixel need a GL stack —
            # libglvnd for the EGL dispatch and mesa for the software driver
            # behind it. This is what a CI runner with no /dev/dri renders
            # with, and the reason that backend is GLES rather than Vulkan.
            libglvnd
            mesa

            # The CEF shell, crates/viewport-shell-cef. `cef-dll-sys` builds
            # `libcef_dll_wrapper` out of the CEF distribution's own source
            # with cmake and ninja — the engine itself is a prebuilt blob and
            # is not compiled by anything here.
            cmake
            ninja

            # The out-of-process shell, crates/viewport-shell-gtk.
            #
            # Both are prebuilt in cache.nixos.org, which is the entire point
            # of that backend: it costs this shell a download, where the WPE
            # backend costs a four-hour WebKit build no cache has. Same WebKit
            # version underneath — 2.52.5 — different port.
            gtk4
            webkitgtk_6_0
        ];

        # The environment both need, extracted for the same reason: an ASan
        # run that rendered through a different EGL driver than the ordinary
        # one would be testing a different program.
        rustEnv = {
          # As in the default shell: viewport-web links libgbm, and smithay's
          # wayland_frontend pulls in xkbcommon, both at build time.
          LIBRARY_PATH = "${pkgs.lib.makeLibraryPath [ pkgs.libgbm pkgs.libxkbcommon ]}";
          # Where libglvnd looks for an EGL driver.
          #
          # This is the whole reason the capture tests can run on a hosted
          # runner. libglvnd is a dispatch library: libEGL.so.1 provides no
          # driver of its own, it loads one named by a JSON file in
          # /usr/share/glvnd/egl_vendor.d or /run/opengl-driver/... — and a
          # GitHub runner has neither, while NixOS has the second, which is
          # why this worked on a workstation and failed in CI.
          #
          # With no vendor loaded there are no EGL client extensions at all,
          # so the failure is not "surfaceless is unsupported" but "nothing
          # supports anything":
          #
          #   Missing extensions: ["EGL_MESA_platform_surfaceless"]
          #   Unable to find suitable EGL platform
          #
          # Naming our own mesa fixes it and makes it reproducible: the shell
          # renders through the driver this flake pins rather than whatever
          # the host happens to have installed.
          __EGL_VENDOR_LIBRARY_DIRS = "${pkgs.mesa}/share/glvnd/egl_vendor.d";

          shellHook = ''
            # ash dlopens libvulkan.so.1 and winit dlopens libwayland-client;
            # the viewport-vulkan tests that ask for a device skip themselves
            # without one, but they have to get as far as the dlopen to do it.
            #
            # libglvnd is libEGL.so.1, which Smithay dlopens through a
            # LazyLock that panics rather than returning an error. The headless
            # backend catches that so a machine without it still runs
            # everything that does not want pixels — but a shell meant for
            # running the tests should have it, or the capture tests quietly
            # test nothing.
            export LD_LIBRARY_PATH="${pkgs.lib.makeLibraryPath [
              pkgs.vulkan-loader
              pkgs.wayland
              pkgs.libxkbcommon
              pkgs.libgbm
              pkgs.libglvnd
              pkgs.mesa
            ]}''${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
          '';
        };

      in
      {
        packages = {
          # Named for the engine that draws the shell, because that is the only
          # thing that differs between them: `.#wpe`, `.#webkitgtk`, and two
          # more names to come. `wpewebkit` is the engine itself rather than a
          # compositor, which is why it does not follow the pattern.
          inherit wpe webkitgtk chromium cef wpewebkit;
          inherit viewport-shell-cef;

          # The old name, from before these were named for the engine. It
          # follows the default rather than staying pinned to the backend that
          # happened to be the only one when it was the only name — a system
          # that asked for "the Viewport from this flake" should get the one
          # this flake recommends.
          #
          # Which means a pin to this name changes backend when the default
          # does. That is the intent and it is worth knowing: name `.#wpe`,
          # `.#webkitgtk`, `.#chromium` or `.#cef` to be held to one.
          viewport-smithay = cef;
          # `viewport` used to be here too, the wlroots build, and was the
          # default. Both compositors produced a binary called `viewport`, so a
          # system installed one or the other; there is only one now.
          #
          # The default is `cef`: of the three that build no engine it is the
          # cheapest per frame the shell paints — 0.250% of a core against
          # webkitgtk's 0.273 and chromium's 0.309 — at the same paint rate.
          # See docs/benchmarks.md.
          #
          # It costs about 156 MB more resident than `webkitgtk`, which is the
          # argument for that one on a machine short of memory rather than CPU.
          #
          # `wpe` is better than all three on every column and is not the
          # default because it is the one that compiles WebKit: several hours
          # before a machine that has just switched to this configuration has a
          # desktop.
          default = cef;
        } // pkgs.lib.optionalAttrs (system == "x86_64-linux") {
          # A disposable machine to try it in: `nix run .#vm` opens a QEMU
          # window with the whole desktop inside, on a virtual GPU it really
          # does take DRM master on. The alternative to this was a nested run,
          # which does not exercise the DRM path at all, or a TTY, which takes
          # the screen. See nix/vm.nix.
          vm = self.nixosConfigurations.vm.config.system.build.vm;
        };

        # --------------------------------------------------------------------
        # The Rust rewrite, and nothing else.
        #
        # `devShells.default` carries wlroots, WPE WebKit and a Servo build
        # recipe, which is right for a workstation and wrong for CI: the WebKit
        # derivation is a four-hour build that no binary cache has, and it is
        # the reason the compositor jobs in .github/workflows/ci.yml are gated
        # behind a repository variable.
        #
        # `cargo test --workspace` needs none of it. The web engine is behind
        # the `wpe` feature, off by default, and crates/viewport-web/build.rs
        # only probes pkg-config when it is on — so the default build is
        # Smithay, DRM, libinput and libseat, all of which substitute in
        # seconds. That is what makes a Rust job affordable on an unassisted
        # hosted runner, which in turn is what gets these 34k lines tested on
        # every push.
        # --------------------------------------------------------------------
        devShells.rust = pkgs.mkShell (rustEnv // {
          packages = (with pkgs; [ rustc cargo rustfmt clippy ]) ++ rustDeps;
        });

        # --------------------------------------------------------------------
        # The same suite under AddressSanitizer.
        #
        # What this is for is narrower than it looks. Rust already rules out
        # the use-after-free that ASan over the C compositor kept finding —
        # outputs and views outliving what points at them, see
        # docs/RUST-REWRITE.md. What it does not rule out is the ~187 `unsafe`
        # blocks and the FFI: WebKit, EGL, libinput, Vulkan. That boundary is
        # where the memory bugs still live, and instrumenting the Rust side
        # covers *both* halves of it, which the C job never did.
        #
        # The mistake that outlives Rust's guarantees — a stale view id, a
        # WeakOutput that stops upgrading — is a logic bug and no sanitizer
        # finds it. That is what the hotplug churn in
        # crates/viewport/tests/control_socket.rs is for. The churn is the
        # test; this is the amplifier.
        #
        # Nightly because -Zsanitizer is unstable, and only here: `nightly` is
        # never applied as an overlay, so nothing else in this flake changes
        # toolchain and the WPE WebKit path stays put.
        #
        # rust-src, because -Zbuild-std recompiles the standard library with
        # the same instrumentation. Without it std is uninstrumented and an
        # overflow inside a Vec operation reads as a clean run.
        # --------------------------------------------------------------------
        devShells.asan = pkgs.mkShell (rustEnv // {
          packages = [
            (nightly.default.override {
              extensions = [ "rust-src" ];
            })
          ] ++ rustDeps;

          # Leak checking off, as in the C sanitizer job and for the same
          # reason: the Wayland and GL libraries hold one-time allocations at
          # exit, so every run would be red for something that is not this
          # project's. Use-after-free and out-of-bounds are unaffected, and
          # they are what is being hunted.
          ASAN_OPTIONS = "detect_leaks=0:detect_odr_violation=0:abort_on_error=1";
        });

        devShells.default = pkgs.mkShell {
          packages = nativeDeps ++ runtimeDeps ++ (with pkgs; [
            gdb
            valgrind
            clang-tools
            wayland-utils
            weston # weston-terminal / weston-simple-egl as test clients
            foot
            # start.sh runs the compositor under `nix develop`, so this PATH is
            # inherited by every client it spawns, terminals included. The
            # stdenv bash is built --disable-readline --disable-progcomp, which
            # makes ~/.bashrc error out and leaks starship's PS1 escapes.
            bashInteractive

            # The Rust rewrite. nixpkgs ships a `cargo` on this system without a
            # matching `rustc`, which fails at the first build rather than at
            # setup, so both are named explicitly here.
            rustc
            cargo
            rustfmt
            clippy
            rust-analyzer

            # ------------------------------------------------------------
            # Servo.
            #
            # Taken from nixpkgs' own servo derivation
            # (pkgs/by-name/se/servo/package.nix) rather than worked out by
            # trial and error. Servo is a Cargo source dependency, not an
            # installable package: it is an rlib, and Rust has no stable ABI,
            # so it has to be compiled inside our build. The nixpkgs
            # derivation is therefore useful for its recipe and not its
            # output.
            # ------------------------------------------------------------
            cmake
            llvm
            llvmPackages.libstdcxxClang
            m4
            perl
            yasm
            python311
            rustPlatform.bindgenHook

            fontconfig
            freetype
            harfbuzz
            libunwind
            libGL
            zlib
            udev
            gst_all_1.gstreamer
            gst_all_1.gst-plugins-base
            gst_all_1.gst-plugins-good
            gst_all_1.gst-plugins-bad

            # The shell's buffer is allocated with GBM and imported through
            # EGL. nixpkgs splits libgbm out of mesa, and the EGL dispatch
            # library lives in libglvnd — the vendor driver under
            # /run/opengl-driver only provides libEGL_mesa.
            libgbm
            libglvnd
          ]);

          # viewport-web links against libgbm, and smithay's wayland_frontend
          # pulls in xkbcommon, both at build time.
          LIBRARY_PATH = "${pkgs.lib.makeLibraryPath [ pkgs.libgbm pkgs.libxkbcommon ]}";

          shellHook = ''
            # Everything the Rust build dlopens rather than links.
            #
            # winit is built with `wayland-dlopen`, so it looks for
            # libwayland-client.so.0 at runtime and reports nothing more useful
            # than "Failed to initialize an event loop" when it cannot find it.
            # khronos-egl dlopens libEGL.so.1, which lives in libglvnd —
            # /run/opengl-driver only provides the mesa vendor driver.
            #
            # Appended, not assigned: replacing this variable is what broke the
            # winit backend the first time.
            export LD_LIBRARY_PATH="${pkgs.lib.makeLibraryPath [
              pkgs.wayland
              pkgs.libxkbcommon
              pkgs.libglvnd
              pkgs.libgbm
              # ash dlopens libvulkan.so.1.
              pkgs.vulkan-loader
              # Servo dlopens these at runtime.
              pkgs.fontconfig
              pkgs.freetype
              pkgs.harfbuzz
              pkgs.libunwind
              pkgs.libGL
              pkgs.zlib
              pkgs.udev
              pkgs.xorg.libX11
              pkgs.xorg.libXcursor
              pkgs.xorg.libXi
              pkgs.xorg.libXrandr
            ]}:/run/opengl-driver/lib''${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"

            echo "viewport devshell"
            echo "  wlroots     : $(pkg-config --modversion wlroots-0.20 2>/dev/null || echo MISSING)"
            echo "  wpe-webkit  : $(pkg-config --modversion wpe-webkit-2.0 2>/dev/null || echo MISSING)"
            echo "  wpe-platform: $(pkg-config --modversion wpe-platform-2.0 2>/dev/null || echo MISSING)"
            echo "  rustc       : $(rustc --version 2>/dev/null || echo MISSING)"
            echo
            echo "  meson setup build && ninja -C build   # the C compositor"
            echo "  cargo test --workspace                # the Rust rewrite"
            echo "  VIEWPORT_REQUIRE_GPU=1 cargo test -p viewport-web   # dma-buf, for real"

            # The checks moved into .githooks when CI was disabled, and they
            # are off until git is told where the hooks live. Nothing else can
            # say so: a hook that is not installed does not run, so it cannot
            # complain about not running, and the first sign is a commit that
            # would not have passed. This shell is the one place every
            # contributor already goes through.
            if [ -d .git ] && [ "$(git config --get core.hooksPath || true)" != ".githooks" ]; then
              echo
              echo "  ! the pre-commit checks are not installed. To enable them:"
              echo "        git config core.hooksPath .githooks"
            fi
          '';
        };
      }) // {

      # ----------------------------------------------------------------------
      # The machine `nix run .#vm` boots. Declared here rather than inside
      # `eachDefaultSystem` because a NixOS configuration names its own system
      # and there is only one of these.
      nixosConfigurations.vm = nixpkgs.lib.nixosSystem {
        system = "x86_64-linux";
        modules = [
          self.nixosModules.default
          ./nix/vm.nix
        ];
      };

      # ----------------------------------------------------------------------
      # Portal wiring, on its own so it can be imported without adopting the
      # session module. Getting screen sharing working is four lines of routing
      # that nobody should have to rediscover: a browser's getDisplayMedia
      # simply rejects, and no log anywhere names a portal or a desktop.
      nixosModules.portal = { config, lib, pkgs, ... }:
        let
          cfg = config.programs.viewport;
        in
        {
          options.programs.viewport.package = lib.mkOption {
            type = lib.types.package;
            default = self.packages.${pkgs.system}.default;
            defaultText = lib.literalExpression "viewport-smithay.packages.\${system}.default";
            description = ''
              The Viewport package this system runs.

              Declared here rather than in the session module because the
              portal needs it too: the portal backend *is* the compositor's
              package, and a system that imports only this module — screen
              sharing without adopting the session — would otherwise pull in
              whichever package is the flake's default, on top of whichever one
              it actually installed. Two compositors in the closure, one of
              them a 1.3 GB Chromium, for a configuration that asked for
              neither.
            '';
          };

          options.programs.viewport.portals.enable =
            lib.mkEnableOption "xdg-desktop-portal wiring for Viewport" // {
              description = ''
                Route the portal interfaces Viewport does not implement to
                backends that do. Without this, screen sharing does not work:
                the frontend exposes no ScreenCast interface at all and
                applications report only that the request was not allowed.
              '';
            };

          config = lib.mkIf config.programs.viewport.portals.enable {
            xdg.portal = {
              enable = true;

              # Screenshot, and ScreenCast for a session running the C build.
              # Viewport speaks the wlr-screencopy and ext-image-copy-capture
              # protocols this backend needs.
              wlr.enable = lib.mkDefault true;
              # The compositor answers Settings and ScreenCast itself, and the
              # frontend only learns that from the .portal file this package
              # installs — a config naming "viewport" with no declaration
              # behind it matches nothing, and the request quietly goes
              # somewhere else.
              #
              # Whichever compositor package the session is installed with, so
              # that a system on the default backend does not acquire a WebKit
              # build by turning portals on. `or` because this module can be
              # imported without the session one, in which case there is no
              # `package` option to read — the same reason `cfg.enable or false`
              # is written that way above.
              extraPortals = [
                pkgs.xdg-desktop-portal-gtk
                (cfg.package or self.packages.${pkgs.system}.default)
              ];

              # Named for XDG_CURRENT_DESKTOP's first entry, which the
              # compositor sets to "viewport:wlroots".
              #
              # Settings has to name viewport — that interface is answered by
              # the compositor itself, and without it every application falls
              # back to a light theme. Everything else goes to GTK or to
              # whatever else is installed; naming viewport as the default
              # would make it preferred for interfaces it does not implement,
              # which is not an error anyone sees, only an interface that
              # never appears on the bus.
              config.viewport = {
                default = [ "gtk" "*" ];
                "org.freedesktop.impl.portal.Settings" = [ "viewport" "gtk" ];
                # ScreenCast is answered by the compositor itself, because
                # the wlroots portal can only offer monitors: wlr-screencopy
                # captures outputs and nothing else, so an application asking
                # to share a window is handed a whole screen. Viewport
                # composites windows already, and owning the interface is what
                # lets it offer one. wlr stays the fallback for a session
                # running the C build, which does not answer this.
                "org.freedesktop.impl.portal.ScreenCast" = [ "viewport" "wlr" ];
                "org.freedesktop.impl.portal.Screenshot" = [ "wlr" ];
              };
            };
          };
        };

      # ----------------------------------------------------------------------
      # NixOS module: session entry + seatd, so viewport can be picked from a
      # display manager or launched straight from a TTY.
      # ----------------------------------------------------------------------
      nixosModules.default = { config, lib, pkgs, ... }:
        let
          cfg = config.programs.viewport;
          inherit (lib) mkDefault mkEnableOption mkOption mkIf types literalExpression;

          # The compositor reads plain JSON, so the module's job is only to
          # render these options into a file and point --config at it.
          #
          # A key that is emitted unconditionally is a key that overrides the
          # compositor's own default, so an option whose default is worse than
          # the compositor's must be omitted rather than written. url is the
          # case that bit: writing http://localhost:3000 for a user who set
          # nothing pointed the shell at a port with nothing behind it, and the
          # session came up on fallback.html after the first-paint timeout.
          # timeout_ms is safe to write because 5000 is what src/main.c already
          # uses.
          configFile = pkgs.writeText "viewport-config.json" (builtins.toJSON
            ({
              timeout_ms = cfg.timeoutMs;
              # Written even though the binary would pick the same thing on its
              # own: the two builds differ only in what is in bin/, so a config
              # that says which engine it expects is the difference between a
              # blank desktop and a log line naming the reason for it.
              shell_backend = cfg.shellBackend;
            }
            // lib.optionalAttrs (cfg.url != null) { inherit (cfg) url; }
            // lib.optionalAttrs cfg.urlSpan { url_span = true; }
            // lib.optionalAttrs (cfg.terminal != null) { inherit (cfg) terminal; }
            // lib.optionalAttrs (cfg.menu != null) { inherit (cfg) menu; }
            // lib.optionalAttrs (cfg.binds != { }) { inherit (cfg) binds; }
            // lib.optionalAttrs (cfg.bindsOverride != { })
              { binds_override = cfg.bindsOverride; }
            // lib.optionalAttrs (cfg.startup != null) { inherit (cfg) startup; }
            // lib.optionalAttrs (!cfg.vtSwitching) { vt_switching = false; }
            // cfg.settings));
        in
        {
          imports = [ self.nixosModules.portal ];

          options.programs.viewport = {
            enable = mkEnableOption "the Viewport compositor";

            shellBackend = mkOption {
              type = types.enum [ "cef" "webkitgtk" "chromium" "wpe" ];
              # The cheapest per painted frame of the three that build no
              # engine, and the one that installs from a cache.
              #
              # `wpe` is better than all of them on CPU and on memory, and
              # cannot be installed from a cache nobody has: switching to a
              # configuration that enables Viewport meant several hours of
              # WebKit before the machine had a desktop, and a default that
              # cannot be reached on an ordinary connection is not a default.
              #
              # Of the other three, `cef` costs 0.250% of a core per frame the
              # shell paints against webkitgtk's 0.273 and chromium's 0.309,
              # at the same rate — for about 156 MB more resident. On a machine
              # short of memory rather than CPU, `webkitgtk` is the better
              # answer. See docs/benchmarks.md.
              default = "cef";
              description = ''
                Which engine draws the desktop.

                `cef` embeds Chromium through the Chromium Embedded Framework:
                the engine is a prebuilt library and nothing here compiles one.
                Cheapest per painted frame of the three that build no engine,
                and about 156 MB heavier than `webkitgtk` for it. It is the
                default.

                `webkitgtk` runs the shell page in a separate process, as an
                ordinary Wayland client, on nixpkgs' prebuilt WebKitGTK. The
                lightest of the three, and the answer on a machine short of
                memory rather than CPU. The shell can crash and be restarted
                without the session going with it in either case.

                `wpe` embeds WPE WebKit in the compositor. It is the original
                backend, and it costs a WebKit build of several hours that no
                binary cache has, because `wpewebkit` is not packaged in
                nixpkgs.

                `chromium` is Blink, in a browser started as a child process
                and driven over the DevTools protocol. It links no engine at
                all, so it is the fastest of the three to build and the only
                one whose engine can change without a recompile.

                Two further names — `servo` and `cef` — are recognised by the
                compositor and refused: neither is implemented. See
                crates/viewport/src/shell_backend.rs.
              '';
            };

            url = mkOption {
              type = types.nullOr types.str;
              default = null;
              example = "http://localhost:3000";
              description = ''
                Web endpoint the shell UI is loaded from. `null` writes no
                url key at all, which leaves the compositor on its bundled
                shell — the only endpoint guaranteed to answer on a machine
                that has just been switched to this configuration.
              '';
            };

            urlSpan = mkOption {
              type = types.bool;
              default = false;
              description = ''
                Whether `url` spans every monitor rather than taking the first
                one and leaving the rest to the bundled desktop.

                False is what naming a web page usually means: that site on the
                main screen, a working desktop on the others. True is for a
                shell under development, which is one page across the whole desk
                by definition. See docs/configuration.md.
              '';
            };

            timeoutMs = mkOption {
              type = types.int;
              default = 5000;
              description = ''
                Milliseconds to wait for the shell's first painted frame
                before falling back to the bundled offline page.
              '';
            };

            terminal = mkOption {
              type = types.nullOr types.str;
              default = null;
              example = literalExpression ''"''${pkgs.ghostty}/bin/ghostty"'';
              description = "Command bound to Mod4+Return.";
            };

            menu = mkOption {
              type = types.nullOr types.str;
              default = null;
              example = literalExpression ''"''${pkgs.wmenu}/bin/wmenu-run -i"'';
              description = "Command bound to Mod4+d.";
            };

            bindsOverride = mkOption {
              type = types.attrsOf (types.nullOr types.str);
              default = { };
              example = literalExpression ''
                {
                  "Mod4+Return" = "exec ghostty";
                  "Mod4+d" = null;
                }
              '';
              description = ''
                Keybindings that change the built-in keymap rather than
                replacing it. Every chord not named here keeps its default.

                A null unbinds a chord, letting it reach the focused
                application — which is not the same as leaving it out, since
                leaving it out is what asks for the built-in.

                This is the option you usually want. Use `binds` only to throw
                the whole keymap away and start again.
              '';
            };

            startup = mkOption {
              type = types.nullOr types.str;
              default = null;
              example = literalExpression ''"''${pkgs.firefox}/bin/firefox --kiosk https://example.com"'';
              description = ''
                Command run once, after the compositor is up. Nothing restarts
                it if it exits — point this at a supervised unit if it matters.

                This is how a kiosk names the application it exists to run; see
                examples/kiosk in the source tree.
              '';
            };

            vtSwitching = mkOption {
              type = types.bool;
              default = true;
              description = ''
                Whether Ctrl+Alt+F1..F12 may still switch virtual terminals.

                Leave this alone unless you are building a kiosk. It is checked
                before the config file, before the keymap and before the shell,
                so it is the one thing that still works when a shell never
                paints or the compositor wedges. With it off, a wedged
                compositor cannot be escaped from the keyboard at all — make
                sure you can reach the machine another way first, and disable
                the getty units on the other VTs as well, since this stops the
                compositor handing over the keys rather than removing the
                consoles they would have reached.
              '';
            };

            binds = mkOption {
              type = types.attrsOf types.str;
              default = { };
              example = literalExpression ''
                {
                  "Mod4+Return" = "exec ghostty";
                  "Mod4+Shift+q" = "close";
                  "Mod4+Shift+e" = "exit";
                }
              '';
              description = ''
                The entire keymap, as chord to action. Actions are
                `exec COMMAND`, `close`, `exit`, `reload` or `none`.

                Setting this to a non-empty value replaces the built-in
                defaults entirely, so include an exit binding — otherwise a
                shell that fails to load leaves no way out but a TTY. To change
                a few chords and keep the rest, use `bindsOverride`.
              '';
            };

            settings = mkOption {
              type = types.attrs;
              default = { };
              description = ''
                Extra keys merged into the generated config.json verbatim,
                for options this module does not model yet.
              '';
            };
          };

          # Which package the backend option asks for, unless something has
          # already said. `mkDefault` rather than a declaration: the option
          # itself lives in the portal module, so that a system taking only the
          # portal can set it — see there.
          config = mkIf cfg.enable {
            programs.viewport.package =
              mkDefault self.packages.${pkgs.system}.${cfg.shellBackend};

            environment.systemPackages = [ cfg.package ];

            services.seatd.enable = true;
            security.polkit.enable = true;
            hardware.graphics.enable = true;
            # Screen sharing needs interfaces routed to a backend, not merely
            # a portal frontend running.
            programs.viewport.portals.enable = lib.mkDefault true;

            services.displayManager.sessionPackages = [
              (pkgs.writeTextFile {
                name = "viewport-session";
                destination = "/share/wayland-sessions/viewport.desktop";
                text = ''
                  [Desktop Entry]
                  Name=Viewport
                  Comment=wlroots compositor with a WPE WebKit shell
                  Exec=${cfg.package}/bin/viewport --config ${configFile}
                  Type=Application
                '';
                passthru.providedSessions = [ "viewport" ];
              })
            ];
          };
        };
    };
}
