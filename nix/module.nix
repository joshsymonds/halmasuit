# NixOS module for halmasuit — Linux system compositor.
#
# Phase A shape: one systemd unit running the halmasuit binary that
# hosts the greetd listener (greetd.sock) and the Wayland listener
# (wayland-0) under /run/halmasuit/. Greeters authorize via SO_PEERCRED:
# only the configured `greeterUid` may speak the greetd protocol.
#
# The unit starts as root (User= unset on purpose). halmasuit binds
# its sockets while still privileged, then in-process `setresuid`s to
# the configured `compositorUid`. From that point on every halmasuit
# code path runs unprivileged. The compositor holds NO PAM handle and
# execs NO setuid helper: it relays the greetd auth conversation over
# a SOCK_SEQPACKET channel to the privileged, host-ns,
# socket-activated `halmasuit-session` broker (Epic #1 R2/R3), which
# owns the one pam_handle_t and forks-then-drops the session leader in
# a non-setuid child. There is no setuid binary on the closure.

{ config, lib, pkgs, ... }:

let
  cfg = config.services.halmasuit;

  # The uid the broker's SO_PEERCRED gate authorizes as its trusted
  # relay peer (Epic R5/R8 — authenticates the peer; identity is still
  # independently PAM-derived). In the live topology the compositor is
  # that peer: greeter →[compositor's greetd greeter-gate]→ compositor
  # →[broker's relay-peer gate]→ broker. When the broker is deployed
  # standalone (no compositor), whatever drives it directly (the
  # greeter uid, as the direct-broker VM gates do) is the peer.
  brokerPeerUid =
    if cfg.enable || cfg.fromInitrd.enable then cfg.compositorUid else cfg.greeterUid;

  # Wallpaper config JSON — the file halmasuit's wallpaper engine
  # reads via HALMASUIT_WALLPAPER_CONFIG. Computed once here so both
  # the env attrs AND the initramfs storePaths can reference the
  # same store path. `null` when `cfg.wallpaper == null`.
  wallpaperConfigFile =
    if cfg.wallpaper == null then null else
    let
      wp = cfg.wallpaper;
      jsonContent =
        if wp.type == "image" then {
          type   = "image";
          source = "${wp.source}";
        } else if wp.type == "shader" then {
          type     = "shader";
          source   = "${wp.source}";
          uniforms = wp.uniforms;
        } else {
          type   = "video";
          source = "${wp.source}";
          "loop" = wp.loop;
        } // lib.optionalAttrs (wp.fallback != null) {
          fallback = "${wp.fallback}";
        };
    in pkgs.writeText "halmasuit-wallpaper.json" (builtins.toJSON jsonContent);

  # Wallpaper env attrs — consumed by BOTH the rootfs `enable`
  # halmasuit unit AND the `fromInitrd.enable` initramfs unit so the
  # wallpaper plane composites from frame 0 in both deployments
  # (G1/R3 — no pre-client solid phase). Returns `{}` when
  # `cfg.wallpaper == null`; otherwise the JSON config file path +
  # the path fallback + the decoder path for video wallpapers.
  wallpaperEnv =
    if cfg.wallpaper == null then {} else {
      HALMASUIT_WALLPAPER_CONFIG = "${wallpaperConfigFile}";
      HALMASUIT_WALLPAPER_PATH   = "${cfg.wallpaper.source}";
    } // lib.optionalAttrs (cfg.wallpaper.type == "video") {
      HALMASUIT_DECODER_PATH = lib.getExe cfg.decoder.package;
    };
in
{
  imports = [
    # Hard-cut, no alias: the prior `witnessImage` option is removed
    # by the wallpaper-engine epic. Configs setting it now fail with
    # this message rather than silently mapping to the new option
    # shape (user preference; see CLAUDE.md "delete replaced code
    # completely; no backwards-compatibility shims").
    (lib.mkRemovedOptionModule [ "services" "halmasuit" "witnessImage" ] ''
      This option was renamed to `services.halmasuit.wallpaper` and
      reshaped into a discriminated union. Replace the old
      single-path form with:

        services.halmasuit.wallpaper = {
          type   = "image";
          source = ./branding/wallpaper.png;
        };

      `type = "shader"` and `type = "video"` are Phase-A typed
      scaffolding; the wallpaper-engine epic's follow-up tasks wire
      the backends. See ARCHITECTURE.md for the design.
    '')
  ];

  options.services.halmasuit = {
    enable = lib.mkEnableOption "halmasuit — Linux system compositor";

    fromInitrd.enable = lib.mkEnableOption ''
      halmasuit started from initramfs (Phase B).

      Registers halmasuit as a `boot.initrd.systemd.services.halmasuit`
      unit with `SurviveFinalKillSignal=yes`, so the same process
      spans kernel handoff → initramfs → switch_root → rootfs without
      restarting. halmasuit holds DRM master directly (no libseat /
      seatd in this deployment) and emits a single NDJSON event
      stream across the pivot, observable in rootfs journald.

      Mutually exclusive with `services.halmasuit.enable`: each is a
      different deployment shape (rootfs-only vs boot-from-initrd) and
      they cannot coexist on the same system. drm-master-probe Phases
      1+2 validated the survival mechanics this option deploys
    '';

    package = lib.mkOption {
      type        = lib.types.package;
      default     = pkgs.halmasuit;
      defaultText = lib.literalExpression "pkgs.halmasuit";
      description = ''
        halmasuit package to use. Override with a flake-built derivation
        when iterating on the compositor without rebuilding nixpkgs.
      '';
    };

    greeterUid = lib.mkOption {
      # `ints.unsigned` (≥ 0) matches the consumer's u32 type in
      # `crates/halmasuit/src/main.rs` and POSIX uid_t. A plain `int`
      # would accept negative values that then fail silently at
      # runtime when the env var doesn't parse as u32.
      type        = lib.types.ints.unsigned;
      default     = 999;
      description = ''
        UID of the greeter system user. halmasuit's greetd listener
        rejects connections whose SO_PEERCRED uid does not match —
        this is the load-bearing authorization on the auth socket.

        Defaults to 999 with a matching `halmasuit-greeter` system
        user created automatically by this module. Override to point
        at an existing system user with a different uid; if you do,
        also override `greeterUser` so the module names match.
      '';
    };

    greeterUser = lib.mkOption {
      type        = lib.types.str;
      default     = "halmasuit-greeter";
      description = ''
        Name of the greeter system user. Created automatically by this
        module with uid = `greeterUid` and primary group =
        `greeterGroup`. Override if you have an existing system user
        you want halmasuit to authenticate via SO_PEERCRED; the module
        won't create a second user with the same name when overridden
        to point at an existing one — define your own
        `users.users.<name> = { uid = …; group = …; };` in that case.
      '';
    };

    greeterGroup = lib.mkOption {
      type        = lib.types.str;
      default     = "halmasuit-greeter";
      description = ''
        Primary group for the greeter + compositor system users, and
        the `Group=` on halmasuit's systemd unit. Files bound by
        halmasuit (greetd.sock, wayland-0) inherit this group, so a
        greeter whose primary or supplementary group matches can
        connect through the 0660 socket mode without further wiring.

        Defaults to `halmasuit-greeter`, created automatically with
        gid = `greeterUid`.
      '';
    };

    compositorUid = lib.mkOption {
      type        = lib.types.ints.unsigned;
      default     = 998;
      description = ''
        UID halmasuit `setresuid`s to in-process after binding its
        sockets. Defaults to 998 with a matching `halmasuit-compositor`
        system user created automatically by this module.

        UID 0 is rejected by the assertion below: the whole point of
        the privilege drop is to NOT run as root. Override to point at
        an existing system user with a different uid; also override
        `compositorUser` so names match.
      '';
    };

    compositorUser = lib.mkOption {
      type        = lib.types.str;
      default     = "halmasuit-compositor";
      description = ''
        Name of the compositor system user. Created automatically by
        this module with uid = `compositorUid` and primary group =
        `greeterGroup`. Override if you have an existing system user
        with the post-drop identity halmasuit should adopt.
      '';
    };

    greeterCommand = lib.mkOption {
      type        = lib.types.nullOr lib.types.str;
      default     = null;
      example     = lib.literalExpression ''"''${pkgs.dankgreeter}/bin/dankgreeter"'';
      description = ''
        Absolute path to the greeter binary halmasuit fork+execs as a
        child of itself at startup. The child runs as the greeter
        system user (uid = `greeterUid`) and inherits a minimal env
        including `XDG_RUNTIME_DIR=/run/halmasuit`,
        `WAYLAND_DISPLAY=wayland-0`, and
        `GREETD_SOCK=/run/halmasuit/greetd.sock` so the greeter can
        connect to halmasuit's Wayland and greetd sockets as a normal
        client.

        Set to `null` to run halmasuit without spawning a greeter —
        useful for dev / VM tests; production deployments always set
        this to the greeter binary's path.

        No support for argv: the value is a single path. If you need
        arguments, wrap in a `pkgs.writeShellScript`.
      '';
    };

    wallpaper = lib.mkOption {
      type = lib.types.nullOr (lib.types.submodule {
        options = {
          type = lib.mkOption {
            type        = lib.types.enum [ "image" "shader" "video" ];
            default     = "image";
            description = ''
              Which wallpaper backend to use. Phase-A wires only
              `image' (PNG/JPG/WebP); `shader' (GLSL fragment) and
              `video' (h264/AV1) are typed config entries the
              wallpaper-engine epic's follow-up tasks fill in. Picking
              an unwired backend fails the compositor closed at
              startup with a clear error.
            '';
          };
          source = lib.mkOption {
            type        = lib.types.path;
            description = ''
              Absolute path to the wallpaper file (image / shader
              source / video). String-interpolated into the unit
              environment so a path literal is realized into the
              store; an absolute runtime path interpolates to itself.
            '';
          };
          uniforms = lib.mkOption {
            type        = lib.types.attrsOf lib.types.anything;
            default     = {};
            description = ''
              Named GLSL uniforms for `type = "shader"'. Phase-A: not
              yet wired (the shader-uniforms task lands the parser).
              The schema admits four uniform kinds — auto-*
              (engine-driven time/resolution/frame/delta/mouse),
              static-typed (float/vec2/vec3/vec4/int/bool),
              event-time and event-value (bus-driven, Phase-B).
            '';
          };
          loop = lib.mkOption {
            type        = lib.types.bool;
            default     = true;
            description = ''
              Whether `type = "video"' wallpapers loop. Defaults to
              true for wallpaper use.
            '';
          };
          raiseSocketBuffers = lib.mkOption {
            type        = lib.types.bool;
            default     = true;
            description = ''
              When `type = "video"`, raise `net.core.wmem_max` /
              `net.core.rmem_max` to 16 MiB so a single RGBA frame
              (up to 8.3 MiB at 1080p) fits in one SOCK_SEQPACKET
              datagram. This is a SYSTEM-WIDE sysctl; on hosts with
              hostile local users it widens unprivileged kernel-
              memory pinning surface. Set false to keep the
              kernel defaults; the decoder will then EMSGSIZE on its
              first frame send and the wallpaper will degrade to the
              placeholder.
            '';
          };
          fallback = lib.mkOption {
            type        = lib.types.nullOr lib.types.path;
            default     = null;
            description = ''
              Absolute path to a static image (PNG/JPEG/WebP) the
              wallpaper engine swaps in when the video decoder's
              restart budget (3 crashes / 10s) exhausts. Only
              meaningful for `type = "video"'; ignored otherwise.

              Without a fallback the engine keeps rendering the
              last good frame (or the 1×1 black placeholder if no
              frame ever arrived). With one, an `ImageBackend` is
              constructed against this path the first time the
              decoder relay reports dead.
            '';
          };
        };
      });
      default     = null;
      example     = lib.literalExpression ''{ type = "image"; source = ./branding/wallpaper.png; }'';
      description = ''
        Wallpaper config. The wallpaper engine composites this as the
        bottom-most plane of every frame from frame 0 (epic G1/R3/R6).
        When set, the source path is exported to halmasuit's unit
        environment as `HALMASUIT_WALLPAPER_PATH' and decoded by
        halmasuit itself at startup — there is no separate client.
        `null' runs halmasuit with no wallpaper (legacy clear-only —
        non-visual tests).
      '';
    };

    cursor = {
      theme = lib.mkOption {
        type        = lib.types.str;
        default     = "default";
        example     = "Adwaita";
        description = ''
          Xcursor theme halmasuit loads at startup for the
          visible-cursor render path (R8b-render). Exported as
          `XCURSOR_THEME` in halmasuit.service's environment AND
          passed through the broker's session-leader env allowlist
          so the child compositor (niri / cosmic-comp / etc.)
          renders the same theme inside its own surface tree.
          Falls back to a procedural arrow if the theme cannot be
          loaded.
        '';
      };
      size = lib.mkOption {
        type        = lib.types.int;
        default     = 24;
        example     = 32;
        description = ''
          Xcursor size in logical pixels. Exported as
          `XCURSOR_SIZE` alongside the theme; same propagation
          path through the broker session-leader allowlist.
        '';
      };
    };

    pamService = lib.mkOption {
      type        = lib.types.str;
      default     = "halmasuit";
      description = ''
        Name of the PAM service halmasuit uses for authentication. Maps
        to `/etc/pam.d/''${pamService}` — declare its contents via
        `security.pam.services.''${pamService}` (or leave
        `installPamConfig = true` to let this module install a default).
      '';
    };

    homeVt = lib.mkOption {
      type        = lib.types.nullOr (lib.types.ints.between 1 63);
      default     = null;
      example     = 8;
      description = ''
        The virtual terminal halmasuit owns as its "home" VT. When set,
        halmasuit opens `/dev/tty''${homeVt}` in its root startup window,
        becomes its `VT_PROCESS` controller, and brings it to the
        foreground; cooperative VT switching (Ctrl+Alt+F<n> to a text
        console and back) is then enabled via the kernel's relsig/acqsig
        handshake. When null, VT switching is disabled.

        The operator MUST ensure no getty runs on the home VT — it is
        halmasuit's, the way greetd owns its VT. The simplest way is to
        pick a VT outside logind's autovt range (`NAutoVTs`, default 6),
        e.g. tty8, which never gets a getty.
      '';
    };

    watchdogSec = lib.mkOption {
      type        = lib.types.str;
      default     = "30s";
      example     = "15s";
      description = ''
        `WatchdogSec` for halmasuit's systemd unit (Epic #71 R-honest.8).
        halmasuit pings `WATCHDOG=1` from its calloop loop at half this
        interval; if the event loop hangs, systemd SIGKILLs it, the
        kernel reverts its home VT to VT_AUTO (VT switching recovers),
        and `Restart=on-failure` brings it back. Must exceed halmasuit's
        cold-start DRM/EGL bring-up (>10s in a VM) so a healthy startup
        is never killed. Set `"0"`/`"infinity"` to disable.
      '';
    };

    installPamConfig = lib.mkOption {
      type        = lib.types.bool;
      default     = true;
      description = ''
        When true, this module declares
        `security.pam.services.''${pamService} = {}`, giving you NixOS's
        default unixAuth-backed PAM stack (pam_unix for password auth,
        pam_env, pam_limits, pam_motd, etc.).

        Set to false when you want full control over the PAM stack —
        e.g. adding pam_u2f or pam_fprintd modules.
      '';
    };

    logLevel = lib.mkOption {
      type        = lib.types.enum [ "error" "warn" "info" "debug" ];
      default     = "info";
      description = ''
        Value passed as `RUST_LOG`. halmasuit consumes this via
        `tracing-subscriber::EnvFilter`; the default keeps event-stream
        output at INFO, which is what the introspection sink emits at.

        `trace` is deliberately not exposed: halmasuit-introspect's redaction
        contract is enforced at construction time, not at filter time, and a
        `trace` filter could surface unredacted PAM challenge text from the
        future `halmasuit-greetd` path before that contract is in place.
      '';
    };

    session = {
      enable = lib.mkEnableOption ''
        the socket-activated privileged `halmasuit-session` PAM-lifecycle
        broker (Epic #1 R6 / Amendment A2) explicitly, on its own (for
        deploying / VM-gating the broker without the compositor).

        You normally do NOT set this: enabling
        `services.halmasuit.enable` implies it, because the compositor's
        only auth path is relaying the greetd conversation to this
        broker (Epic #1 R3 / Amendment A4 — there is no in-compositor
        PAM and no setuid spawn helper). The broker is a self-contained
        host-ns root unit that PID 1 activates on the first greeter
        connection and which idle-exits when no auth/session is in
        flight, so there is no standing root process at the login
        screen
      '';

      package = lib.mkOption {
        type        = lib.types.package;
        default     = pkgs.halmasuit-session;
        defaultText = lib.literalExpression "pkgs.halmasuit-session";
        description = ''
          The `halmasuit-session` broker package. Override with a
          flake-built derivation when iterating without rebuilding
          nixpkgs. Requires the halmasuit overlay (or a manual
          `pkgs.halmasuit-session`) for the default to resolve.
        '';
      };
    };

    decoder = {
      package = lib.mkOption {
        type        = lib.types.package;
        default     = pkgs.halmasuit-decoder;
        defaultText = lib.literalExpression "pkgs.halmasuit-decoder";
        description = ''
          The `halmasuit-decoder` sandboxed video-decoder subprocess
          package (Epic #12). Forked at runtime by halmasuit's
          DecoderRelay when `services.halmasuit.wallpaper.type = "video"`.
          Override with a flake-built derivation for iteration.
          Unused when the wallpaper type is `image` or `shader`.
        '';
      };
    };

    luks = {
      package = lib.mkOption {
        type        = lib.types.package;
        default     = pkgs.halmasuit-luks;
        defaultText = lib.literalExpression "pkgs.halmasuit-luks";
        description = ''
          The `halmasuit-luks` systemd password-agent Wayland client
          (Phase B). Registered as a `boot.initrd.systemd.services`
          unit alongside halmasuit when
          `services.halmasuit.fromInitrd.enable = true`; watches
          `/run/systemd/ask-password/` for LUKS unlock requests and
          renders a passphrase prompt over halmasuit's Wayland socket.
          Replaceable by any other implementation of the systemd
          password-agent protocol — substitute the package and the
          new binary will be wired into the same unit slot.
          Ignored when `fromInitrd.enable` is false (rootfs-only
          deployments use rootfs systemd's password-agent stack).
        '';
      };

      passphraseFile = lib.mkOption {
        type        = lib.types.nullOr lib.types.path;
        default     = null;
        description = ''
          Optional unattended-unlock passphrase source. When non-null,
          halmasuit-luks runs in non-interactive mode — no Wayland UI,
          no keyboard input — and responds to every ask-password
          request in initramfs with the contents of this file. The
          file is baked into the initramfs closure via
          `boot.initrd.systemd.contents` and read once at startup
          into a zeroizing buffer.

          ⚠️ SECURITY: `lib.types.path` imports the file into
          `/nix/store/<hash>-<name>` at evaluation time. The Nix
          store is world-readable on the running system, so passing
          a literal Nix path here publishes the LUKS passphrase to
          every local user. The materialised initramfs copy at
          `/etc/halmasuit-luks-passphrase` inherits the source
          file's mode and is similarly readable.

          Safe usage patterns:

          * The LUKS VM gate (`tests/luks-unlock.nix`) — the
            passphrase isn't a secret because the volume is created
            at test time.
          * Out-of-band passphrase materialised by an earlier
            initramfs step (e.g. TPM-derived, USB-key-loaded). In
            that flow pass a runtime path string (`lib.types.str`,
            not via this option), or write the file from a
            preceding `boot.initrd.systemd.services` unit with
            `chmod 0400`.

          For interactive workstation use, leave this `null` and
          let the Wayland UI prompt.
        '';
      };
    };
  };

  config = lib.mkMerge [

   (lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = cfg.compositorUid != 0;
        message   = ''
          services.halmasuit.compositorUid must not be 0 (root). The
          privilege drop is load-bearing per the ARCHITECTURE.md threat
          model; setting it to 0 would defeat the split.
        '';
      }
    ];

    # halmasuit's renderer is GLES + DrmCompositor. That requires Mesa
    # + libglvnd available at runtime: libEGL.so.1 is `dlopen`ed by
    # smithay via libloading, and Mesa's DRI driver (loaded as
    # `dri_gbm.so` from `/run/opengl-driver/lib/gbm/`) is the actual
    # software-rendering backend when LIBGL_ALWAYS_SOFTWARE=1 or the
    # only backend when virtio-gpu-pci is the VM substrate.
    # `hardware.graphics.enable` is NixOS's canonical setup for both.
    hardware.graphics.enable = true;

    # D-Bus system bus + policy for the test-only
    # `org.halmasuit.Debug.Introspect` interface (the `Snapshot()`
    # method, present only in the `frame_audit`/`halmasuit-debug`
    # build). halmasuit's D-Bus server thread connects to the system
    # bus BEFORE the in-process privilege drop, so it authenticates as
    # root and requests the `org.halmasuit` name as root — hence the
    # `user="root"` own-grant. The policy is completely inert for the
    # production `halmasuit` package, which never links zbus and never
    # requests the name; shipping it unconditionally keeps the module
    # single-codepath. `services.dbus.enable` is required because the
    # minimal VM-test images don't bring the system bus up otherwise.
    # Epic #47 R2.3: seatd is NOT enabled. halmasuit is a system
    # compositor that owns DRM master + input device fds for its
    # entire process lifetime; it opens /dev/dri/card0 and
    # /dev/input/event* directly via setup_drm_direct +
    # setup_libinput_direct while still root, then privilege-drops.
    # No libseat / no seatd anywhere in the runtime closure —
    # collapsing the standing-root-daemon survival surface that
    # would otherwise have to be carried across the rootfs→
    # shutdownRamfs pivot.

    services.dbus.enable = true;
    services.dbus.packages = [
      (pkgs.writeTextDir "share/dbus-1/system.d/org.halmasuit.conf" ''
        <!DOCTYPE busconfig PUBLIC
          "-//freedesktop//DTD D-BUS Bus Configuration 1.0//EN"
          "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
        <busconfig>
          <policy user="root">
            <allow own="org.halmasuit"/>
            <!-- Epic #71 R3.3: production observability surface. -->
            <allow own="org.halmasuit.Compositor1"/>
          </policy>
          <policy context="default">
            <allow send_destination="org.halmasuit"/>
            <allow receive_sender="org.halmasuit"/>
            <!-- Compositor1 read methods are unauthenticated per
                 Epic #71 (no Set*/Force*/Inject*/Override*; the
                 read/write split is enforced in code). -->
            <allow send_destination="org.halmasuit.Compositor1"/>
            <allow receive_sender="org.halmasuit.Compositor1"/>
          </policy>
        </busconfig>
      '')
    ];

    # No setuid wrapper: the compositor execs no privilege-drop helper.
    # Session launch is the privileged halmasuit-session broker
    # forking-then-dropping a non-setuid child (Epic #1 R7/R15) — see
    # the broker unit below. There is no setuid inode in the closure.

    systemd.services.halmasuit = {
      description = "halmasuit — Linux system compositor";
      wantedBy    = [ "multi-user.target" ];
      # `halmasuit-session.socket` ordered before us so the broker's
      # SOCK_SEQPACKET listening socket is bound (PID 1 owns it) by the
      # time the compositor relays its first greeter auth to it (Epic
      # #1 R3). NOT `requires`: the broker is socket-activated and
      # idle-exits — only the socket need exist, not a running service.
      #
      # `DefaultDependencies = false` (in unitConfig below) suppresses
      # the implicit `Conflicts=shutdown.target` + `Before=shutdown.target`
      # + `After=sysinit.target` + `After=basic.target` injection — we
      # WANT halmasuit to survive the shutdown sequence, but we still
      # need the sysinit / basic ordering, so they're re-added here
      # explicitly. `Before=shutdown.target` ordering is also explicit
      # so that systemd-shutdown runs AFTER halmasuit has been started.
      after       = [
        "sysinit.target"
        "basic.target"
        "local-fs.target"
        "halmasuit-session.socket"
      ];
      # `Wants` rather than `Requires`: required at boot for halmasuit
      # to function (some sysinit paths are mandatory), but `Requires`
      # causes systemd to cascade-stop halmasuit when sysinit.target
      # stops during shutdown, defeating the survive-the-pivot
      # architecture. Boot ordering is enforced by `After=` (above);
      # if sysinit fails, halmasuit's own initialization fails for
      # cause, not via a propagation cascade. `Before=shutdown.target`
      # is explicit so the start ordering is preserved (we still want
      # halmasuit started before shutdown.target is considered
      # reachable), but with `DefaultDependencies=false` there is no
      # implicit `Conflicts=shutdown.target` so reaching shutdown.target
      # doesn't trigger halmasuit's stop.
      wants       = [ "sysinit.target" ];
      before      = [ "shutdown.target" ];

      unitConfig = {
        # Epic #47 R2.2: halmasuit MUST survive systemd-shutdown's
        # final kill spree so it can keep painting the wallpaper
        # plane through the rootfs→shutdownRamfs pivot until the
        # kernel halts.
        #
        # `DefaultDependencies=false` suppresses the implicit
        # `Conflicts=shutdown.target` + `Before=shutdown.target`
        # pair systemd would otherwise inject; without it systemd
        # stops halmasuit during the normal shutdown unit-stop
        # sequence, the unit enters 'failed' state, and when
        # systemd-shutdown's broad kill spree fires the
        # `SurviveFinalKillSignal=yes` exemption no longer applies
        # to the unit's PID (it's no longer "active"). With
        # DefaultDependencies=false halmasuit stays active through
        # the entire shutdown sequence; the only kill attempt is
        # systemd-shutdown's final SIGTERM/SIGKILL, which
        # SurviveFinalKillSignal=yes blocks. Same pattern the
        # halmasuit-shutdown-probe-phase{0,1,2} units use; same
        # pattern is load-bearing for the production binary.
        DefaultDependencies   = false;
        SurviveFinalKillSignal = "yes";
      };

      serviceConfig = {
        Type           = "simple";
        ExecStart      = lib.getExe cfg.package;
        # `on-failure` rather than `always`: a clean Shutdown event
        # followed by exit 0 is a deliberate poweroff path, not a crash
        # to recover from.
        Restart        = "on-failure";
        RestartSec     = "1s";
        # Epic #71 R-honest.8: systemd watchdog. halmasuit pings
        # WATCHDOG=1 from its calloop loop at WatchdogSec/2; if the
        # event loop hangs, the pings stop, systemd SIGKILLs halmasuit,
        # the kernel's reset_vc reverts its home VT to VT_AUTO (VT
        # switching recovers), and Restart=on-failure brings it back.
        # This is the recovery complement that makes compositor-owned
        # VT_PROCESS safe (a DEAD controller is already safe via
        # reset_vc; only a HUNG-but-alive one needs this).
        #
        # The default (`watchdogSec` = 30s) is generous: halmasuit's
        # DRM/EGL/GBM bring-up before the event loop's first ping can
        # take >10s in a cold VM. It pings once very early in main() too,
        # so the clock resets near exec.
        #
        # NotifyAccess=main guarantees $NOTIFY_SOCKET is provided to the
        # main process under Type=simple (no Type=notify readiness
        # gating, which would change boot ordering). The ping is
        # loop-driven and continues through the shutdown wallpaper-paint
        # loop, so SurviveFinalKillSignal's survival window is intact:
        # halmasuit keeps pinging while it keeps painting.
        WatchdogSec    = cfg.watchdogSec;
        NotifyAccess   = "main";
        # Epic #47 R2.2: `KillMode=process` confines `systemctl stop
        # halmasuit.service` (dev workflow) to signaling halmasuit's
        # main PID only, leaving any child trees alone. Paired with
        # `DefaultDependencies=false` in unitConfig (which removes the
        # implicit shutdown.target conflict), this unit no longer
        # participates in systemd's unit-stop phase during system
        # shutdown — the only kill attempt halmasuit sees during
        # shutdown is systemd-shutdown's broad SIGTERM/SIGKILL kill
        # spree, which `SurviveFinalKillSignal=yes` blocks. The
        # SIGTERM IS forwarded to halmasuit at that point (the kill
        # spree sends SIGTERM first, then SIGKILL; SurviveFinalKillSignal
        # only suppresses the SIGKILL), triggering
        # `graceful_shutdown` and the wallpaper-only post-shutdown
        # paint loop right before the rootfs→shutdownRamfs pivot.
        KillMode       = "process";
        # halmasuit emits its NDJSON event stream on stderr via
        # tracing-subscriber; stdout is reserved for the R2.2 shutdown-
        # liveness writes (one line per HALMASUIT_LIVENESS_INTERVAL_MS
        # while the always-on liveness timer is running). Routing
        # stdout to `file:/dev/kmsg` is what makes those lines survive
        # the entire shutdown sequence end-to-end: systemd opens fd 1
        # against /dev/kmsg directly (NOT through the journal socket,
        # which `StandardOutput=kmsg` does), so the fd remains valid
        # after systemd-journald is killed by the shutdown kill spree
        # and across the rootfs→shutdownRamfs pivot. The compositor
        # has `ProtectKernelLogs=true` and can't open /dev/kmsg
        # itself, but the pre-opened fd inherited from systemd works
        # regardless. The /dev/kmsg character device is kernel-owned
        # and survives every userspace teardown, so writes land in
        # the kernel ring buffer (visible via dmesg and on the serial
        # console) all the way until the kernel halts.
        StandardOutput = "file:/dev/kmsg";
        StandardError  = "journal";
        # RuntimeDirectory creates /run/halmasuit/ with the unit's UID.
        # Unit starts as root, so /run/halmasuit is owned root:<Group=>;
        # halmasuit binds its sockets here BEFORE the in-process
        # `setresuid`, so after the drop the compositor user still has
        # accept() on the sockets even though it can't create new
        # files under this dir.
        RuntimeDirectory     = "halmasuit";
        RuntimeDirectoryMode = "0755";
        # No `SupplementaryGroups`: the compositor runs NO PAM and has
        # no business reading /etc/shadow. PAM (and its `shadow`-group
        # getspnam fast-path) lives in the privileged halmasuit-session
        # broker unit only (Epic #1 R2/R14; least authority).
        #
        # Hardening posture. The primary defense is the privilege
        # split: the compositor deprivileges to compositorUid, holds no
        # PAM handle, and execs no setuid helper (all PAM/privileged
        # work is the separate host-ns broker unit). The directives
        # below are defense-in-depth. NNP-implying directives
        # (MemoryDenyWriteExecute, RestrictNamespaces, SystemCallFilter,
        # LockPersonality, RestrictSUIDSGID, …) are deliberately left
        # OFF here: with the setuid helper gone NNP is no longer
        # forbidden for correctness, but the compositor's DRM / libseat
        # / dlopen'd Mesa surface under seccomp+NNP is unaudited —
        # enabling them is a dedicated hardening pass, not part of the
        # privilege-separation epic. Tracked as a follow-up.
        ProtectSystem          = "strict";
        ProtectHome            = true;
        PrivateTmp             = true;
        ProtectControlGroups   = true;
        # ProtectKernelLogs does NOT imply NNP when the unit starts
        # privileged (it's CAP_SYS_ADMIN-gated in
        # systemd.exec(5)::context_has_no_new_privileges) — safe to
        # apply unconditionally. Blocks dmesg access from the
        # compositor process.
        ProtectKernelLogs      = true;
        # Process egid → inherited by Unix sockets bound under
        # RuntimeDirectory. Members of this group can connect through
        # halmasuit's 0660 sockets without further wiring. The default
        # `halmasuit-greeter` group is auto-created by this module
        # (see the shared `cfg.enable || cfg.fromInitrd.enable` block
        # below); overriding `greeterGroup` is honored here too.
        Group = cfg.greeterGroup;
      };

      environment = {
        RUST_LOG = cfg.logLevel;
        # Point smithay's ListeningSocketSource at the unit's
        # RuntimeDirectory. /run/halmasuit/wayland-0 is the production
        # socket path documented in ARCHITECTURE.md.
        XDG_RUNTIME_DIR = "/run/halmasuit";
        # Greetd-listener authorization: only this uid passes SO_PEERCRED.
        HALMASUIT_GREETER_UID = toString cfg.greeterUid;
        # PAM service file lookup key — must match
        # /etc/pam.d/<HALMASUIT_PAM_SERVICE>.
        HALMASUIT_PAM_SERVICE = cfg.pamService;
        # Privilege-drop target. halmasuit reads this after binding
        # sockets and `setresuid`s to it in-process.
        HALMASUIT_COMPOSITOR_UID = toString cfg.compositorUid;
        # No HALMASUIT_SPAWN_BIN: the compositor execs no helper. Its
        # broker socket defaults to /run/halmasuit-session.sock — the
        # ListenSequentialPacket the broker unit binds below — so no
        # HALMASUIT_BROKER_SOCKET override is needed here.
        # Force Mesa to use llvmpipe (software rasterizer) until the
        # epic's real-hardware shakedown subtask validates virgl /
        # native GPU paths on gnomon. Deterministic, doesn't need
        # virtio-gpu-gl or host EGL backend, and produces stable
        # goldens.
        LIBGL_ALWAYS_SOFTWARE = "1";
        # NixOS routes runtime OpenGL through /run/opengl-driver/lib.
        # halmasuit's binary has libglvnd's lib dir in RPATH (via the
        # halmasuit derivation's postFixup) but Mesa's DRI driver
        # (dri_gbm.so) still loads from the dlopen search path. The
        # libglvnd dispatch also looks here for vendor JSON.
        LD_LIBRARY_PATH = "/run/opengl-driver/lib";
        # R8b-render — xcursor theme + size for halmasuit's visible
        # cursor render path. Propagated through the broker
        # session-leader env allowlist so the child compositor
        # renders the same theme. See `services.halmasuit.cursor`.
        XCURSOR_THEME = cfg.cursor.theme;
        XCURSOR_SIZE  = toString cfg.cursor.size;
      } // lib.optionalAttrs (cfg.greeterCommand != null) {
        # Greeter binary halmasuit fork+execs at startup as the
        # greeter user. See `services.halmasuit.greeterCommand`.
        HALMASUIT_GREETER_COMMAND = cfg.greeterCommand;
      } // lib.optionalAttrs (cfg.homeVt != null) {
        # Epic #71 R-honest.7: the VT halmasuit owns as VT_PROCESS
        # controller for cooperative switching. See `homeVt`.
        HALMASUIT_HOME_VT = toString cfg.homeVt;
      } // wallpaperEnv;
    };
   })

   # Epic #12: kernel socket-buffer ceiling. The decoder→compositor
   # IPC sends one RGBA frame per SOCK_SEQPACKET datagram (up to
   # `MAX_FRAME_BYTES` = 16 MiB; 1080p RGBA is 8.3 MiB). Linux's
   # default `net.core.wmem_max` / `rmem_max` is ~208 KiB; setsockopt
   # SO_SNDBUF/SO_RCVBUF silently caps at those, so without raising
   # the sysctls the relay's setsockopt has no effect and the
   # decoder's first send fails with EMSGSIZE.
   #
   # Security trade-off (Epic #12 review finding): these sysctls are
   # SYSTEM-WIDE. Raising wmem_max/rmem_max from 208 KiB to 16 MiB
   # (~80×) lets any local process on the host `setsockopt SO_SNDBUF`
   # up to 16 MiB; a malicious user holding N sockets can pin
   # N × 16 MiB of unswappable kernel memory. Phase B replaces the
   # single-datagram model with a shm-pool, eliminating the sysctl
   # raise entirely; until then the trade-off is "video wallpapers
   # work" vs. "tighter per-user kernel-memory ceiling". We default
   # to raising the ceiling because video wallpaper is opt-in (only
   # raised when `wallpaper.type = "video"`); operators with hostile
   # local users on the same machine can opt out:
   #
   #   services.halmasuit.wallpaper.raiseSocketBuffers = false;
   #
   # but the decoder will then EMSGSIZE on first send and the
   # wallpaper will fall back to the placeholder.
   (lib.mkIf ((cfg.enable || cfg.fromInitrd.enable)
              && cfg.wallpaper != null
              && cfg.wallpaper.type == "video"
              && cfg.wallpaper.raiseSocketBuffers) {
     boot.kernel.sysctl."net.core.wmem_max" = lib.mkDefault 16777216;
     boot.kernel.sysctl."net.core.rmem_max" = lib.mkDefault 16777216;
   })

   # Epic #1 R6 / Amendment A2: the socket-activated privileged broker.
   # A SEPARATE unit from the compositor (above) — it is the host-ns
   # root PAM-lifecycle owner. PID 1 owns the listening socket and
   # activates the service on the first greeter connection; the broker
   # is a single calloop event loop that evicts an in-flight worker on
   # a reconnect (R5) and `exit(0)`s when no auth/session has been in
   # flight for its idle window, so the unit deactivates and there is
   # NO standing root process at the idle login screen (R6). PID 1
   # keeps the socket and re-activates losslessly on the next
   # connection. There is no setuid helper anywhere in this path: the
   # broker is already root and forks-then-drops the session leader in
   # a non-setuid child (Epic R7/R15).
   #
   # System users + group, created automatically for any halmasuit
   # deployment (`enable` OR `fromInitrd.enable`). Defaults keep the
   # operator off the "uid trap" path — `services.halmasuit.enable =
   # true` is sufficient out-of-the-box; identities can be overridden
   # via `compositorUid`/`compositorUser`/`greeterUid`/`greeterUser`
   # for sites that already have system accounts to reuse.
   #
   # Each attr wraps in `lib.mkDefault` so a test or operator can
   # redeclare any individual field (description, shell, home, …)
   # without colliding with the module's defaults at the same priority.
   #
   # The compositor user gets the greeter group as its primary group
   # so the post-drop process retains the gid the wayland-0 and
   # greetd.sock files are bound with — without it, halmasuit can't
   # accept() on the sockets it bound while still root.
   (lib.mkIf (cfg.enable || cfg.fromInitrd.enable) {
     users.users.${cfg.compositorUser} = {
       isSystemUser = lib.mkDefault true;
       uid          = lib.mkDefault cfg.compositorUid;
       group        = lib.mkDefault cfg.greeterGroup;
       description  = lib.mkDefault "halmasuit compositor process identity";
     };
     users.users.${cfg.greeterUser} = {
       isSystemUser = lib.mkDefault true;
       uid          = lib.mkDefault cfg.greeterUid;
       group        = lib.mkDefault cfg.greeterGroup;
       description  = lib.mkDefault "halmasuit greeter peer (SO_PEERCRED-authorized greetd client)";
     };
     users.groups.${cfg.greeterGroup}.gid = lib.mkDefault cfg.greeterUid;
   })

   # Epic #1 R6 / Amendment A2: the socket-activated privileged broker.
   # See block-level comment below for full context.
   #
   # Provisioned whenever the compositor is enabled (`cfg.enable`) OR
   # the broker is requested on its own (`cfg.session.enable`): the
   # compositor's only auth path is relaying to this broker (Epic #1
   # R3/A4), so it is mandatory infrastructure, not an opt-in add-on.
   (lib.mkIf (cfg.enable || cfg.fromInitrd.enable || cfg.session.enable) {
     # Default PAM service file — unixAuth-backed pam_unix + pam_env +
     # pam_limits, the stack the privileged halmasuit-session broker
     # authenticates against. Provisioned wherever the broker is — the
     # rootfs `enable` deployment, the boot-from-initrd deployment, and
     # the standalone `session.enable` shape all reach this PAM service
     # through `cfg.pamService`. Operators wanting custom modules
     # disable `installPamConfig` and declare
     # `security.pam.services.<name>` themselves.
     security.pam.services = lib.mkIf cfg.installPamConfig {
       ${cfg.pamService} = {};
     };

     # Epic #47 R2.2: ship halmasuit (+ its transitive closure: Mesa,
     # libgbm, libglvnd, libdrm, glibc, ld-linux, …) into the shutdown
     # initramfs. systemd-shutdown pivots into /run/initramfs at the
     # tail of the shutdown sequence; processes that survive via
     # `SurviveFinalKillSignal=yes` continue running with the same
     # PID + fds, but their mmap'd executable + libraries must be
     # backed by the shutdownRamfs tmpfs — otherwise the rootfs
     # unmount that follows pulls them out from under the running
     # process. halmasuit-shutdown-probe-phase{1,2} validated this
     # is sufficient (with `SurviveFinalKillSignal=yes`) for the
     # process + its DRM master to survive the pivot. nix-store
     # closure resolution via storePaths picks up the transitive
     # deps automatically.
     systemd.shutdownRamfs.storePaths = [ "${cfg.package}/bin/halmasuit" ];

     systemd.sockets."halmasuit-session" = {
       description = "halmasuit-session privileged PAM-lifecycle broker socket";
       wantedBy    = [ "sockets.target" ];
       socketConfig = {
         # SOCK_SEQPACKET: the broker's wire codec is one logical
         # message per datagram (matches halmasuit-greetd's framing).
         #
         # Path-vs-abstract: the rootfs `enable` deployment uses a
         # filesystem path under /run; the fromInitrd deployment uses
         # an abstract Linux socket name (kernel net-ns-scoped, no
         # filesystem inode) so halmasuit — stuck in initramfs's
         # mount namespace post-pivot — can still reach the broker.
         # The `HALMASUIT_BROKER_SOCKET` env on halmasuit's unit
         # selects the matching connect side; both sides agree on
         # the path via that env.
         ListenSequentialPacket =
           if cfg.fromInitrd.enable
           then "@halmasuit-session"
           else "/run/halmasuit-session.sock";
         # The binary owns the accept loop and the global single slot
         # (Epic R5 / Amendment A2.1) — NOT one instance per
         # connection.
         Accept = false;
         # SO_PEERCRED in the broker is the load-bearing authorization
         # (only the HALMASUIT_BROKER_PEER_UID relay peer may drive
         # auth — AuthSlot::create gate, plus the `RequestRootFd`
         # gate in serve_root_fd_request). The socket mode is
         # defence-in-depth ONLY; the compositor will broker greeter
         # connections through a tighter SocketUser/SocketGroup once
         # the G-layer lands. Until then a permissive mode lets the
         # gated VM client connect; the SO_PEERCRED check still
         # refuses any non-greeter peer.
         #
         # For the abstract-socket fromInitrd shape the socket-mode
         # gate doesn't apply at all (abstract sockets have no
         # filesystem inode to chmod), making SO_PEERCRED the SOLE
         # authorization. Network-namespace isolation
         # (`PrivateNetwork=true` on this unit) would hide the
         # abstract name from the host net-ns, but it would also
         # hide it from halmasuit, which lives in initramfs PID1's
         # (host) net-ns — cross-systemd JoinsNamespaceOf isn't
         # practical between rootfs systemd's broker and the
         # initramfs-systemd-spawned halmasuit. Further isolation
         # is a deployment-shape change (shared net-ns via
         # boot.specialFileSystems or a privileged broker spawned
         # by initramfs systemd), tracked as a follow-up. For now
         # the SO_PEERCRED gate is sufficient against
         # non-root-non-relay-peer attackers; a root attacker in
         # the broker's net-ns already holds equivalent authority
         # by other paths.
         SocketMode = "0666";
       };
     };

     systemd.services."halmasuit-session" = {
       description = "halmasuit-session privileged PAM-lifecycle broker";
       # Socket-activated: deliberately NO wantedBy / install target.
       # No standing root process when idle (Epic R6).
       requires = [ "halmasuit-session.socket" ];
       after    = [ "halmasuit-session.socket" ];

       serviceConfig = {
         Type      = "simple";
         ExecStart = lib.getExe cfg.session.package;
         # The broker idle-exits cleanly (exit 0) when no auth/session
         # is in flight; that is the normal R6 deactivation path, not
         # a failure. Socket activation restarts it on demand.
         RemainAfterExit = false;
         Restart         = "no";
         StandardOutput  = "null";
         StandardError   = "journal";
         # Runs as root in the HOST mount namespace (User= unset on
         # purpose): pam_systemd/logind + pam_mount need host-ns root.
         # `shadow` group lets pam_unix's getspnam fast-path read
         # /etc/shadow in-process rather than forking the setuid
         # unix_chkpwd helper — the fork path is fragile under any
         # sandboxed parent (memory project-pam-unix-shadow-group).
         #
         # Capabilities the broker uses (all implicit via root —
         # there is NO `CapabilityBoundingSet=` / `AmbientCapabilities=`
         # restriction, this is documentation of the surface):
         #   CAP_SYS_ADMIN     pam_namespace, pam_loginuid, fork+exec
         #   CAP_DAC_READ_SEARCH  /etc/shadow read via shadow group
         #   CAP_SYS_PTRACE    pam_keyinit edge cases
         #   CAP_SYS_TTY_CONFIG  Epic #71 R1 VT_ACTIVATE ioctl
         #                       (compositor never holds this; broker
         #                       fires the ioctl on its behalf)
         SupplementaryGroups = [ "shadow" ];
         # Generous backstop ONLY (a wedged module is bounded by the
         # broker's per-worker RLIMIT_CPU + SIGKILL-anytime + idle
         # exit; this just caps a pathologically stuck instance).
         RuntimeMaxSec = "12h";
         # DELIBERATELY NO NoNewPrivileges-implying directives
         # (MemoryDenyWriteExecute, RestrictNamespaces, RestrictRealtime,
         # SystemCallFilter, LockPersonality, ProtectKernelTunables,
         # ProtectKernelModules, RestrictSUIDSGID, ProtectClock,
         # ProtectHostname, ProtectKernelLogs when unprivileged — see
         # systemd.exec(5) context_has_no_new_privileges). The broker
         # forks-then-drops the session leader in a NON-setuid child
         # whose own setres*/getres*-verify privilege drop and the
         # pam_unix unix_chkpwd fallback both break under NNP=yes
         # (memory project-nnp-implying-directives). The privilege
         # split itself (host-ns root broker, fuzzed in-child drop,
         # UID floor, no setuid inode) is the defense — NOT seccomp on
         # this unit.
       };

       environment = {
         RUST_LOG = cfg.logLevel;
         # SO_PEERCRED authorization: only the trusted relay peer may
         # drive auth (Epic R5/R8). In the live topology that is the
         # unprivileged compositor (it owns its own greeter gate on the
         # greetd socket); standalone it is whatever drives the broker
         # directly. Identity is still independently PAM-derived (R8).
         HALMASUIT_BROKER_PEER_UID = toString brokerPeerUid;
         # PAM service file lookup key — /etc/pam.d/<value>.
         HALMASUIT_PAM_SERVICE = cfg.pamService;
         # Epic #47 R1: broker is the policy authority for greeter
         # spawn. The compositor is unprivileged + can't setuid
         # itself; it sends `SpawnGreeter` and the broker reads
         # these env vars to fork-then-drop the greeter child. Same
         # values the compositor unit's env has (so the in-compositor
         # and broker-side resolution match exactly — drift here would
         # mean the greeter runs as a different uid depending on which
         # path spawned it, which is unsafe).
         HALMASUIT_GREETER_UID  = toString cfg.greeterUid;
         HALMASUIT_GREETER_GID  = toString config.users.groups.${cfg.greeterGroup}.gid;
         HALMASUIT_GREETER_NAME = cfg.greeterUser;
         HALMASUIT_GREETER_HOME = "/var/empty";
         HALMASUIT_GREETD_SOCKET = "/run/halmasuit/greetd.sock";
       } // lib.optionalAttrs (cfg.greeterCommand != null) {
         HALMASUIT_GREETER_COMMAND = cfg.greeterCommand;
       };
     };

     # No assertions: `greeterUid` and `compositorUid` are
     # always non-null now (typed `ints.unsigned`, with defaults
     # 999/998 created by the shared user-creation block above).
   })

   # Phase B: boot-from-initrd deployment. halmasuit registered as an
   # initramfs systemd unit; SurviveFinalKillSignal=yes keeps the
   # process alive across switch_root (RESEARCH.md Phase 2 / Plymouth's
   # mechanism). NOT registered in rootfs systemd — rootfs systemd
   # observes the surviving PID via /proc but doesn't manage it, same
   # shape as `tests/drm-master-probe-phase2.nix`.
   #
   # The deployment is mutually exclusive with `services.halmasuit.enable`:
   # rootfs-only and boot-from-initrd are two different topologies, not
   # composable. The assertion below enforces this.
   (lib.mkIf cfg.fromInitrd.enable {
     assertions = [
       {
         assertion = !cfg.enable;
         message   = ''
           services.halmasuit.fromInitrd.enable and
           services.halmasuit.enable cannot both be true. They are
           mutually exclusive deployment shapes:
             - enable = true       → rootfs-only, direct DRM
             - fromInitrd.enable   → boot-from-initrd, direct DRM
           (R2.3: both shapes are direct-DRM / direct-input — no
            libseat / no seatd anywhere in the runtime closure.)
         '';
       }
       {
         assertion = cfg.compositorUid != 0;
         message   = ''
           services.halmasuit.compositorUid must not be 0 (root). The
           post-pivot privilege drop is load-bearing per the
           ARCHITECTURE.md threat model; setting it to 0 would defeat
           the split.
         '';
       }
     ];

     # Same Mesa runtime story as the rootfs deployment: halmasuit's
     # renderer dlopens libEGL/libGL via libglvnd, then Mesa's DRI
     # driver. `hardware.graphics.enable = true` provisions the
     # `/run/opengl-driver/lib` farm in rootfs; that path also exists
     # post-pivot here since rootfs systemd brings it up after the
     # pivot, but halmasuit needs Mesa reachable BEFORE the pivot.
     # The initramfs storePaths list below ships Mesa + libglvnd into
     # the initramfs closure; LD_LIBRARY_PATH points the binary at
     # those store paths directly (no `/run/opengl-driver/lib`
     # indirection in initramfs).
     hardware.graphics.enable = true;

     # Epic #47 R2.3: seatd is NOT enabled — halmasuit opens DRM +
     # input devices directly via setup_drm_direct +
     # setup_libinput_direct, no libseat brokerage anywhere in the
     # runtime closure.

     # System bus + halmasuit ownership policy for the post-pivot
     # rootfs dbus-broker. halmasuit-debug's `Snapshot()` D-Bus thread
     # connects to the rootfs system bus in `run_post_pivot_setup`
     # (the initramfs system bus denied the name; this is the retry
     # site that succeeds because the policy below grants it). The
     # production `halmasuit` package never requests the name; this
     # policy is inert there. Shipped from the fromInitrd block so the
     # cfg.enable block above doesn't need to mkForce.
     services.dbus.enable = true;
     services.dbus.packages = [
       (pkgs.writeTextDir "share/dbus-1/system.d/org.halmasuit.conf" ''
         <!DOCTYPE busconfig PUBLIC
           "-//freedesktop//DTD D-BUS Bus Configuration 1.0//EN"
           "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
         <busconfig>
           <policy user="root">
             <allow own="org.halmasuit"/>
           </policy>
           <policy context="default">
             <allow send_destination="org.halmasuit"/>
             <allow receive_sender="org.halmasuit"/>
           </policy>
         </busconfig>
       '')
     ];

     # `boot.initrd.systemd.enable = true` is required for
     # `boot.initrd.systemd.services.*` to take effect. NixOS's older
     # initramfs (without systemd) can't host a long-running unit.
     boot.initrd.systemd.enable = true;
     # virtio_gpu for the VM test; real-hardware deployments add
     # nvidia-drm / amdgpu / i915 themselves outside this option.
     boot.initrd.availableKernelModules = [ "virtio_gpu" ];
     boot.initrd.kernelModules = [ "virtio_gpu" ];

     # Ship halmasuit + halmasuit-luks + the full GLES runtime closure
     # into the initramfs. `boot.initrd.systemd.storePaths` follows
     # each entry's transitive closure, so naming the package roots
     # is enough.
     #
     # xkeyboard-config is a dlopen target xkbcommon resolves through a
     # compile-time baked-in path; it's not a regular link dep, so the
     # halmasuit binary closure misses it and add_keyboard fails with
     # "Cannot load XKB rules 'evdev'". Smithay's `add_keyboard` is
     # called regardless of the initramfs/rootfs path (the seat is one
     # initialization site above the DRM branching), so xkbcommon needs
     # its data files reachable before any post-pivot Wayland client.
     boot.initrd.systemd.storePaths = [
       "${cfg.package}/bin/halmasuit"
       "${cfg.luks.package}/bin/halmasuit-luks"
       "${pkgs.mesa}"
       "${pkgs.libglvnd}"
       "${pkgs.xkeyboard-config}"
     ] ++ lib.optionals (cfg.wallpaper != null) [
       # Wallpaper assets must be in the initramfs closure so the
       # wallpaper plane can composite from frame 0 (G1/R3 — no
       # pre-client solid phase). `cfg.wallpaper.source` is the
       # primary asset; the JSON config file is what halmasuit reads
       # via HALMASUIT_WALLPAPER_CONFIG. For video wallpapers, the
       # decoder binary and fallback image too.
       "${cfg.wallpaper.source}"
       "${wallpaperConfigFile}"
     ] ++ lib.optionals (cfg.wallpaper != null && cfg.wallpaper.type == "video") [
       "${cfg.decoder.package}/bin/halmasuit-decoder"
     ] ++ lib.optionals (
       cfg.wallpaper != null
       && cfg.wallpaper.type == "video"
       && cfg.wallpaper.fallback != null
     ) [
       "${cfg.wallpaper.fallback}"
     ];

     # The Phase B unit. Registered ONLY in initramfs systemd; the
     # rootfs side will observe the surviving PID via /proc but not
     # manage a unit for it.
     boot.initrd.systemd.services.halmasuit = {
       description = "halmasuit (Phase B: initramfs survival)";
       wantedBy    = [ "initrd.target" ];
       after       = [ "systemd-modules-load.service" "systemd-udev-settle.service" ];
       # The pivot kill spree (systemd-shutdown's killall) runs
       # BEFORE initrd-switch-root.service. We must be wantedBy
       # initrd.target so we start before the pivot, AND we must be
       # ordered before initrd-switch-root.service so we're registered
       # by the time the kill spree fires.
       before      = [ "initrd-switch-root.service" ];
       unitConfig = {
         DefaultDependencies = false;
         IgnoreOnIsolate     = true;
         # THE survival mechanism. Belongs in [Unit], not [Service]
         # (RESEARCH.md L131-136: load-fragment-gperf.gperf.in maps
         # Unit.SurviveFinalKillSignal, NOT Service.*; misplacement
         # is silently dropped as "Unknown key"). The VM test
         # asserts both placement and effect.
         SurviveFinalKillSignal = "yes";
       };
       serviceConfig = {
         Type           = "simple";
         # No `Group=`: the rootfs `enable` unit pins the egid via
         # `Group = cfg.greeterGroup` (line ~601) because rootfs NSS
         # resolves the name to a gid. The initramfs systemd has no
         # NSS / no /etc/group (the user-database auto-creation runs
         # in stage 2 activation, long after this unit starts), so
         # both name- and numeric-form `Group=` fail 216/GROUP. The
         # gid half of `drop_privileges` is instead pinned by
         # halmasuit consulting `HALMASUIT_COMPOSITOR_GID` and
         # calling `setresgid(target_gid, …)` directly — set below.
         # ExecStartPre sets up two paths halmasuit needs before its
         # main() reaches them, in the initramfs context:
         #
         # 1. `/run/opengl-driver` symlink. Mesa's GBM loader has a
         #    baked-in search path `/run/opengl-driver/lib/gbm/<drv>_gbm.so`
         #    that ignores `LIBGL_DRIVERS_PATH`. The rootfs systemd
         #    activation script (hardware.graphics.enable) builds the
         #    symlink farm at rootfs boot; in initramfs that activation
         #    never runs. Pointing at `${pkgs.mesa}` satisfies the loader
         #    because its `lib/gbm/dri_gbm.so` is in the initramfs
         #    storePaths closure.
         # 2. `/run/halmasuit/`. The Wayland socket lives here per
         #    XDG_RUNTIME_DIR. Created by mkdir rather than
         #    `RuntimeDirectory=` — `RuntimeDirectory=` makes systemd
         #    consider this unit eligible for the rootfs's
         #    `initrd-cleanup.service` sweep, which sends SIGTERM
         #    post-pivot and breaks halmasuit's survival (the probe in
         #    drm-master-probe-phase2 doesn't use `RuntimeDirectory=`
         #    and survives cleanly).
         ExecStartPre = [
           "${pkgs.coreutils}/bin/ln -sfn ${pkgs.mesa} /run/opengl-driver"
           "${pkgs.coreutils}/bin/mkdir -p /run/halmasuit"
         ];
         ExecStart      = lib.getExe cfg.package;
         Restart        = "no";
         # `file:/dev/kmsg` for the same shutdown-liveness reason as
         # the rootfs unit (see the long comment in the `enable`
         # branch above): halmasuit writes liveness lines on stdout;
         # `file:/dev/kmsg` has systemd open the device directly so
         # the fd survives journald death and the rootfs pivot.
         StandardOutput = "file:/dev/kmsg";
         StandardError  = "journal";
         # Cross-pivot per-process-root divergence: at switch_root
         # halmasuit's process-root diverges from rootfs systemd's
         # despite sharing a mount-namespace ID — halmasuit's `/`
         # post-pivot is essentially empty (no /etc/passwd, no
         # /nix/store, no /run/systemd/ask-password/). Two design
         # choices follow from this and are visible elsewhere in
         # this file + crates/halmasuit/src/main.rs:
         #
         #  - Sockets that must be reachable cross-mount-ns (greetd
         #    listener + broker connect) bind/connect via ABSTRACT
         #    Linux socket names (`@halmasuit-greetd`,
         #    `@halmasuit-session`) — abstract sockets live in the
         #    NETWORK namespace which halmasuit + rootfs share.
         #  - To exec the greeter, read /run/systemd/ask-password/,
         #    and consult /etc/passwd, halmasuit calls
         #    `RequestRootFd` on the broker pre-drop; the broker
         #    sends `/proc/self/root` via SCM_RIGHTS; halmasuit
         #    `fchdir + chroot`s into rootfs's process-root before
         #    binding the post-pivot listener, spawning the greeter,
         #    and dropping privileges. See the `PivotPhase` state
         #    machine in crates/halmasuit/src/main.rs (the
         #    `try_connect_and_request_root_fd` / `try_recv_root_fd`
         #    / `apply_chroot_to_root_fd` helpers drive one
         #    non-blocking step per tick) and the
         #    `serve_root_fd_request` SO_PEERCRED gate in
         #    crates/halmasuit-session/src/broker.rs.
         #
         # `tests/full-boot-flash.nix` is the end-to-end gate.
       };
       environment = {
         RUST_LOG        = cfg.logLevel;
         XDG_RUNTIME_DIR = "/run/halmasuit";
         # Bind sockets as ABSTRACT Linux sockets (kernel-namespace-
         # scoped, no filesystem inode) because filesystem-bound
         # sockets aren't visible cross-mount-namespace at the
         # switch_root boundary. Abstract sockets live in the NETWORK
         # namespace which halmasuit + rootfs share — so rootfs
         # greeters CAN connect via the abstract name AND halmasuit
         # CAN reach the broker socket bound by rootfs systemd's
         # `halmasuit-session.socket` unit. See the cross-pivot
         # docstring on the unit's serviceConfig above.
         # The greetd socket is bound POST-CHROOT (run_post_pivot_setup
         # calls setup_greetd_listener after halmasuit chroots into
         # rootfs's view), so a filesystem path works the same way it
         # does in the rootfs `enable` deployment. We DON'T use an
         # abstract socket here: greetd clients (Quickshell / DMS) call
         # `connect(2)` with the env value verbatim and don't interpret
         # a leading '@' as the abstract namespace — pointing them at
         # `@halmasuit-greetd` silently fails the connect and the
         # greeter never asks the broker to authenticate. The BROKER
         # socket above stays abstract because halmasuit reaches it
         # FROM the initramfs net-ns (PrivateNetwork=false; same
         # net-ns) before the chroot happens.
         HALMASUIT_GREETD_SOCKET  = "/run/halmasuit/greetd.sock";
         HALMASUIT_BROKER_SOCKET  = "@halmasuit-session";
         # Phase B v2: greeter-identity fields halmasuit consults when
         # `User::from_uid` fails because /etc/passwd isn't visible in
         # the surviving initramfs process-root. The values must match
         # the system users the module auto-creates above. The gid is
         # sourced from the GREETER GROUP's actual gid (resolved
         # through the module config), NOT `cfg.greeterUid` — those
         # happen to share a value via the auto-created group's
         # `gid = lib.mkDefault cfg.greeterUid` default, but an
         # operator who overrides `greeterGroup` to a pre-existing
         # group with a different gid would otherwise ship the wrong
         # bytes to halmasuit's setresgid call.
         HALMASUIT_GREETER_GID    = toString config.users.groups.${cfg.greeterGroup}.gid;
         HALMASUIT_GREETER_NAME   = cfg.greeterUser;
         HALMASUIT_GREETER_HOME   = "/var/empty";

         # Group ownership for the bound `/run/halmasuit/wayland-0` socket
         # (Phase B fromInitrd path). The rootfs `enable` unit pins this
         # via systemd's `Group = cfg.greeterGroup` directive on the
         # service (process egid at bind time → file gid). Initramfs
         # systemd can't carry `Group=` (no NSS pre-pivot — `Group=` fails
         # 216/GROUP), so halmasuit reads this env and `fchown`s the
         # socket explicitly after bind. Without this the file ends up
         # `root:root` and the greeter (running as the greeter uid in the
         # greeter group, mode 0660) hits EACCES on `connect(2)`.
         HALMASUIT_WAYLAND_GROUP_GID = toString config.users.groups.${cfg.greeterGroup}.gid;

         # PAM/auth surface for the post-pivot greeter. The initramfs
         # phase skips greetd + greeter spawn + privilege drop (no
         # users / no /etc/passwd before the pivot); these env vars
         # become live when `run_post_pivot_setup` runs in
         # `crates/halmasuit/src/main.rs` shortly after the pivot-poll
         # timer detects `/etc/initrd-release` disappearing.
         HALMASUIT_GREETER_UID    = toString cfg.greeterUid;
         HALMASUIT_COMPOSITOR_UID = toString cfg.compositorUid;
         # Pin the post-drop gid explicitly. Without this halmasuit
         # falls back to `getegid()` which inherits PID1's gid 0 in
         # initramfs (Group= can't be set on initramfs units — no
         # NSS pre-pivot). The compositor user's primary group is
         # `cfg.greeterGroup`; the actual gid comes from the module
         # config (not `cfg.greeterUid`, which is a uid that happens
         # to share its numeric value with the auto-created group's
         # gid by default — an operator overriding `greeterGroup` to
         # a pre-existing group breaks that coincidence).
         HALMASUIT_COMPOSITOR_GID = toString config.users.groups.${cfg.greeterGroup}.gid;
         HALMASUIT_PAM_SERVICE    = cfg.pamService;

         # Mesa runtime. /run/opengl-driver symlink is created by the
         # ExecStartPre above; LD_LIBRARY_PATH carries libglvnd's
         # libEGL/libGL dispatch and Mesa's libgallium.
         # llvmpipe is forced for the VM-test virtio-gpu-pci substrate
         # (matches the rootfs unit's LIBGL_ALWAYS_SOFTWARE=1).
         LIBGL_ALWAYS_SOFTWARE = "1";
         LD_LIBRARY_PATH       = "${pkgs.libglvnd}/lib:${pkgs.mesa}/lib";
       } // lib.optionalAttrs (cfg.greeterCommand != null) {
         # Greeter binary halmasuit fork+execs post-pivot.
         HALMASUIT_GREETER_COMMAND = cfg.greeterCommand;
       } // wallpaperEnv;
     };

     # halmasuit-luks: the systemd password-agent Wayland client.
     # Registered as a separate initramfs unit ordered AFTER halmasuit
     # (so the Wayland socket exists by the time halmasuit-luks tries
     # to connect). NOT SurviveFinalKillSignal — halmasuit-luks exits
     # cleanly when /etc/initrd-release disappears (= pivot done; the
     # rootfs systemd-cryptsetup agent takes over from here for any
     # rootfs LUKS volumes). Conceptually replaceable by any other
     # systemd password-agent implementation; the user can override
     # `services.halmasuit.luks.package`.
     # When passphraseFile is set, halmasuit-luks runs in
     # non-interactive responder mode (--passphrase-from PATH).
     # halmasuit's Wayland socket is not used, so the Wayland-readiness
     # ordering relaxes to a no-op (the agent ignores WAYLAND_DISPLAY
     # and reads the passphrase file directly).
     boot.initrd.systemd.contents = lib.mkIf (cfg.luks.passphraseFile != null) {
       "/etc/halmasuit-luks-passphrase".source = cfg.luks.passphraseFile;
     };

     boot.initrd.systemd.services.halmasuit-luks = {
       description = "halmasuit-luks (Phase B: LUKS prompt Wayland client)";
       wantedBy    = [ "initrd.target" ];
       after       = if cfg.luks.passphraseFile != null then [] else [ "halmasuit.service" ];
       requires    = if cfg.luks.passphraseFile != null then [] else [ "halmasuit.service" ];
       before      = [ "initrd-switch-root.service" ];
       # NOT before=cryptsetup.target: halmasuit-luks is a
       # long-running agent that loops until /etc/initrd-release
       # disappears. Ordering it before cryptsetup.target would
       # block cryptsetup units from starting (the target can't be
       # reached until its `before`-orderers exit), wedging boot.
       # The agent + cryptsetup races are race-FREE: systemd-cryptsetup
       # writes the ask-file then waits on a socket response; the
       # agent polls the dir at 200ms cadence. Whichever started
       # first, the response arrives.
       unitConfig = {
         DefaultDependencies = false;
         IgnoreOnIsolate     = true;
       };
       serviceConfig = {
         Type           = "simple";
         ExecStart =
           if cfg.luks.passphraseFile != null
           then "${lib.getExe cfg.luks.package} --passphrase-from /etc/halmasuit-luks-passphrase"
           else lib.getExe cfg.luks.package;
         # The agent is allowed to die and respawn — Restart=on-failure
         # lets a wedged xkbcommon load or transient Wayland connect
         # error recover. The boot succeeds anyway if no LUKS volume
         # needs unlocking (the agent watches an empty directory).
         Restart        = "on-failure";
         RestartSec     = "1s";
         StandardOutput = "journal";
         StandardError  = "journal";
       };
       environment = {
         RUST_LOG          = cfg.logLevel;
         # Connect to halmasuit's Wayland socket. halmasuit binds at
         # /run/halmasuit/wayland-0 under XDG_RUNTIME_DIR. Ignored in
         # non-interactive mode.
         XDG_RUNTIME_DIR   = "/run/halmasuit";
         WAYLAND_DISPLAY   = "wayland-0";
       };
     };
   })
  ];
}
