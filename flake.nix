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
        ];

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
          inherit viewport wpewebkit;
          default = viewport;
        };

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
          ]);

          shellHook = ''
            echo "viewport devshell"
            echo "  wlroots     : $(pkg-config --modversion wlroots-0.20 2>/dev/null || echo MISSING)"
            echo "  wpe-webkit  : $(pkg-config --modversion wpe-webkit-2.0 2>/dev/null || echo MISSING)"
            echo "  wpe-platform: $(pkg-config --modversion wpe-platform-2.0 2>/dev/null || echo MISSING)"
            echo
            echo "  meson setup build && ninja -C build"
          '';
        };
      }) // {

      # ----------------------------------------------------------------------
      # NixOS module: session entry + seatd, so viewport can be picked from a
      # display manager or launched straight from a TTY.
      # ----------------------------------------------------------------------
      # Portal wiring, on its own so it can be imported without adopting the
      # session module. Getting screen sharing working is four lines of routing
      # that nobody should have to rediscover: a browser's getDisplayMedia
      # simply rejects, and no log anywhere names a portal or a desktop.
      nixosModules.portal = { config, lib, pkgs, ... }:
        {
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

              # The ScreenCast and Screenshot implementation. Viewport speaks
              # the wlr-screencopy and ext-image-copy-capture protocols this
              # backend needs.
              wlr.enable = lib.mkDefault true;
              extraPortals = [ pkgs.xdg-desktop-portal-gtk ];

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
                "org.freedesktop.impl.portal.ScreenCast" = [ "wlr" ];
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
          configFile = pkgs.writeText "viewport-config.json" (builtins.toJSON
            ({
              inherit (cfg) url;
              timeout_ms = cfg.timeoutMs;
            }
            // lib.optionalAttrs (cfg.terminal != null) { inherit (cfg) terminal; }
            // lib.optionalAttrs (cfg.menu != null) { inherit (cfg) menu; }
            // lib.optionalAttrs (cfg.binds != { }) { inherit (cfg) binds; }
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
              type = types.str;
              default = "http://localhost:3000";
              description = "Web endpoint the shell UI is loaded from.";
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
                Keybindings, as chord to action. Actions are
                `exec COMMAND`, `close`, `exit` or `reload`.

                Setting this to a non-empty value replaces the built-in
                defaults entirely, so include an exit binding — otherwise a
                shell that fails to load leaves no way out but a TTY.
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
