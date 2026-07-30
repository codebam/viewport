{
  description = "Viewport — a wlroots compositor whose shell is rendered by WPE WebKit";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; };

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

        runtimeDeps = with pkgs; [
          wlroots
          wayland
          wayland-protocols
          libxkbcommon
          pixman
          libdrm
          mesa
          libglvnd
          udev
          libinput
          seatd
          # wlroots' xwayland.h needs the xcb-ewmh headers, and Xwayland
          # itself must be on PATH for X11 clients to run.
          libxcb-wm
          xwayland
          glib
          json-glib
          wpewebkit
          # The screencast portal's transport.
          pipewire
        ];

        # The Rust rewrite. Built from the same tree, beside the C compositor
        # rather than instead of it: both produce a binary called `viewport`,
        # so a system installs one or the other.
        #
        # The web engine is behind a feature flag, and this build turns it on:
        # without it there is no shell at all — grey where the wallpaper and
        # the bar should be — and nothing in the log says so.
        viewport-smithay = pkgs.rustPlatform.buildRustPackage {
          pname = "viewport-smithay";
          version = "0.1.0";
          src = self;

          cargoLock = {
            lockFile = ./Cargo.lock;
            # A fork of smithay, for the tearing-control patch. A git
            # dependency has no crates.io hash to check against, so its
            # contents are pinned here instead.
            outputHashes = {
              "smithay-0.7.0" = "sha256-xSv7kew3VjibRRbSJ5447PQYGDP9wqIJ+u3hj1dU4zQ=";
            };
          };

          buildAndTestSubdir = "crates/viewport";
          buildFeatures = [ "wpe" ];

          nativeBuildInputs = with pkgs; [
            pkg-config
            rustPlatform.bindgenHook
            makeWrapper
            wayland-scanner
          ];

          buildInputs = runtimeDeps ++ (with pkgs; [
            vulkan-loader
            vulkan-headers
            libgbm
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

            wrapProgram $out/bin/viewport \
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
          '';

          # The suite needs a GPU for the renderer tests and a socket for the
          # compositor ones; the sandbox has neither. `cargo test` on a machine
          # with both is what covers this.
          doCheck = false;

          meta = with pkgs.lib; {
            description = "The Smithay rewrite of the Viewport compositor";
            platforms = platforms.linux;
            mainProgram = "viewport";
          };
        };

        viewport = pkgs.stdenv.mkDerivation {
          pname = "viewport";
          version = "0.1.0";
          src = self;
          nativeBuildInputs = nativeDeps;
          buildInputs = runtimeDeps;
          meta = with pkgs.lib; {
            description = "wlroots compositor with a WPE WebKit shell";
            platforms = platforms.linux;
            mainProgram = "viewport";
          };
        };
      in
      {
        packages = {
          inherit viewport viewport-smithay wpewebkit;
          default = viewport;
        };

        # --------------------------------------------------------------------
        # `nix flake check` runs the part of the meson suite a sandbox can
        # actually run, which is the shell logic tests and nothing else.
        #
        # The compositor tests (session-lock-crash, output-order, capture-*)
        # each start viewport on the headless backend with WLR_RENDERER=vulkan,
        # and vulkan needs a device node that the build sandbox does not have —
        # the pixman fallback does not come up at all, so there is no renderer
        # left to try. They also want an XDG_RUNTIME_DIR to put a Wayland
        # socket in, and WebKit's web process wants user namespaces for bwrap.
        # Naming them here would only guarantee a red check.
        #
        # The shell tests need node and a file, which is why they are the ones
        # that survive. node is not a build dependency, so meson only defines
        # them when it finds one — hence adding it here rather than relying on
        # whatever happened to be in the closure.
        # --------------------------------------------------------------------
        checks.viewport = viewport.overrideAttrs (old: {
          doCheck = true;
          nativeBuildInputs = (old.nativeBuildInputs or [ ]) ++ [ pkgs.nodejs ];
          mesonCheckFlags = [
            "shell-tiling"
            "shell-scrolling"
            "shell-session-tiling"
            "shell-session-scrolling"
            # Neither of these needs a seat, a DRM device or a Wayland socket:
            # the first links json-glib alone, and the second drives the IPC
            # parser against a display with no backend attached. The compositor
            # tests are still excluded — they need all three.
            "unit"
            "ipc-replay"
            "binding"
            "kiosk"
          ];
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
          '';
        };
      }) // {

      # ----------------------------------------------------------------------
      # The binary cache, so that importing any part of this builds nothing it
      # does not have to.
      #
      # This used to live in the session module alone, and a system that took
      # only the portal module still built the compositor — the portal backend
      # *is* the compositor's package — from source, every time. Both modules
      # import this one, so the substituter arrives with whichever of them is
      # enabled.
      #
      # The key matters as much as the URL: nixConfig in this flake applies to
      # interactive evaluation and only with accept-flake-config, so a system
      # without the key here reaches the bucket and rejects every narinfo in it
      # as unsigned. That failure is silent, and looks exactly like a cache
      # that has nothing in it.
      # ----------------------------------------------------------------------
      # Kept as a no-op so an existing import does not break. The GCS bucket
      # it configured belonged to the GCP builder, which is gone: it only ever
      # held runtime closures, so a `dev` output was never in it and anything
      # compiling against WebKit rebuilt it from source anyway. Builds happen
      # on nixbuild.net now, which caches what it builds, outputs and all.
      nixosModules.cache = { ... }: { };
      nixosModules.portal = { config, lib, pkgs, ... }:
        {
          imports = [ self.nixosModules.cache ];

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
              extraPortals = [
                pkgs.xdg-desktop-portal-gtk
                self.packages.${pkgs.system}.viewport-smithay
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

      nixosModules.default = { config, lib, pkgs, ... }:
        let
          cfg = config.programs.viewport;
          inherit (lib) mkEnableOption mkOption mkIf types literalExpression;

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
            }
            // lib.optionalAttrs (cfg.url != null) { inherit (cfg) url; }
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

            package = mkOption {
              type = types.package;
              default = self.packages.${pkgs.system}.viewport;
              description = "The viewport package to use.";
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

          config = mkIf cfg.enable {
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
