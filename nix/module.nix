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
  brokerPeerUid = if cfg.enable then cfg.compositorUid else cfg.greeterUid;
in
{
  options.services.halmasuit = {
    enable = lib.mkEnableOption "halmasuit — Linux system compositor";

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
      type        = lib.types.nullOr lib.types.ints.unsigned;
      default     = null;
      example     = 999;
      description = ''
        UID of the greeter system user. halmasuit's greetd listener
        rejects connections whose SO_PEERCRED uid does not match — this
        is the load-bearing authorization on the auth socket.

        Must be set when `services.halmasuit.enable = true`. The matching
        system user is the responsibility of the operator: declare it
        elsewhere in your NixOS config (`users.users.<name> = { uid = …;
        … }`) and pass its uid here.
      '';
    };

    greeterGroup = lib.mkOption {
      type        = lib.types.nullOr lib.types.str;
      default     = null;
      example     = "halmasuit-greeter";
      description = ''
        If set, becomes the systemd unit's `Group=`. Files bound by
        halmasuit (greetd.sock, wayland-0) inherit this group, so a
        greeter whose primary or supplementary group matches can connect
        through the 0660 socket mode without further wiring.

        Leave `null` only for the root-only test deployment; production
        deployments must set this so a non-root greeter can connect.
      '';
    };

    compositorUid = lib.mkOption {
      type        = lib.types.nullOr lib.types.ints.unsigned;
      default     = null;
      example     = 998;
      description = ''
        UID halmasuit `setresuid`s to in-process after binding its
        sockets. The compositor system user is the responsibility of
        the operator: declare it in `users.users.<name>` and pass the
        uid here. UID 0 is rejected by the assertion below: the whole
        point of the privilege drop is to NOT run as root.

        Must be set when `services.halmasuit.enable = true`.
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

    witnessImage = lib.mkOption {
      type        = lib.types.nullOr lib.types.path;
      default     = null;
      example     = lib.literalExpression ''./branding/witness.png'';
      description = ''
        Absolute path to a PNG halmasuit composites as its bottom-most
        internal background plane from frame 0 (epic G1/R3/R6). When
        set, exported to halmasuit's unit environment as
        `HALMASUIT_WITNESS_IMAGE` and decoded by halmasuit itself at
        startup — there is no separate client. `null` runs halmasuit
        with no witness (legacy clear-only — non-visual tests).
      '';
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
  };

  config = lib.mkMerge [

   (lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = cfg.greeterUid != null;
        message   = ''
          services.halmasuit.greeterUid must be set when
          services.halmasuit.enable = true. halmasuit's greetd listener
          rejects connections whose peer uid does not match.
        '';
      }
      {
        assertion = cfg.compositorUid != null;
        message   = ''
          services.halmasuit.compositorUid must be set when
          services.halmasuit.enable = true. halmasuit drops privileges
          to this uid after binding its sockets.
        '';
      }
      {
        assertion = (cfg.compositorUid or 0) != 0;
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
    # seatd: the root device broker libseat connects to. halmasuit
    # acquires its DRM (and, layer E2, libinput) fds through a
    # LibSeatSession instead of self-issuing SET_MASTER — the
    # privilege posture validated by drm-master-probe Phase 4 (seatd
    # owns master; halmasuit never does). Required for ALL halmasuit
    # deployments now, not just a test.
    services.seatd.enable = true;

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

    # Default PAM service file — gives us unixAuth-backed pam_unix +
    # pam_env + pam_limits, the stack the privileged halmasuit-session
    # broker authenticates against and the conventional starting stack
    # for greeters. Operators wanting custom modules disable
    # installPamConfig and declare security.pam.services.<name>
    # themselves.
    security.pam.services = lib.mkIf cfg.installPamConfig {
      ${cfg.pamService} = {};
    };

    # No setuid wrapper: the compositor execs no privilege-drop helper.
    # Session launch is the privileged halmasuit-session broker
    # forking-then-dropping a non-setuid child (Epic #1 R7/R15) — see
    # the broker unit below. There is no setuid inode in the closure.

    systemd.services.halmasuit = {
      description = "halmasuit — Linux system compositor";
      wantedBy    = [ "multi-user.target" ];
      # seatd must be up before halmasuit so `LibSeatSession::new()`
      # can reach the seatd socket while halmasuit is still root
      # (pre-privilege-drop). `requires` so a seatd failure fails
      # halmasuit loudly rather than silently losing the GPU.
      #
      # `halmasuit-session.socket` ordered before us so the broker's
      # SOCK_SEQPACKET listening socket is bound (PID 1 owns it) by the
      # time the compositor relays its first greeter auth to it (Epic
      # #1 R3). NOT `requires`: the broker is socket-activated and
      # idle-exits — only the socket need exist, not a running service.
      after       = [ "local-fs.target" "seatd.service" "halmasuit-session.socket" ];
      requires    = [ "seatd.service" ];

      serviceConfig = {
        Type           = "simple";
        ExecStart      = lib.getExe cfg.package;
        # `on-failure` rather than `always`: a clean Shutdown event
        # followed by exit 0 is a deliberate poweroff path, not a crash
        # to recover from.
        Restart        = "on-failure";
        RestartSec     = "1s";
        # Capture stderr only; halmasuit emits its NDJSON event stream
        # there. stdout stays silent for now.
        StandardOutput = "null";
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
      } // lib.optionalAttrs (cfg.greeterGroup != null) {
        # Process egid → inherited by Unix sockets bound under
        # RuntimeDirectory. Members of this group can connect through
        # halmasuit's 0660 sockets without further wiring.
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
        # Force libseat's seatd backend. halmasuit runs as a system
        # service with no logind session, so libseat's autodetect
        # (logind → seatd → builtin) is ambiguous; pin it.
        LIBSEAT_BACKEND = "seatd";
      } // lib.optionalAttrs (cfg.greeterCommand != null) {
        # Greeter binary halmasuit fork+execs at startup as the
        # greeter user. See `services.halmasuit.greeterCommand`.
        HALMASUIT_GREETER_COMMAND = cfg.greeterCommand;
      } // lib.optionalAttrs (cfg.witnessImage != null) {
        # Decoded by halmasuit at startup as the internal witness
        # plane. See `services.halmasuit.witnessImage`. String
        # interpolation (NOT `toString`) so a path literal is realized
        # into the store and is guaranteed present at this path in
        # halmasuit's closure — `witnessImage` is self-sufficient, not
        # reliant on some other unit pulling the file in. An absolute
        # runtime-path string interpolates to itself unchanged.
        HALMASUIT_WITNESS_IMAGE = "${cfg.witnessImage}";
      };
    };
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
   # Provisioned whenever the compositor is enabled (`cfg.enable`) OR
   # the broker is requested on its own (`cfg.session.enable`): the
   # compositor's only auth path is relaying to this broker (Epic #1
   # R3/A4), so it is mandatory infrastructure, not an opt-in add-on.
   (lib.mkIf (cfg.enable || cfg.session.enable) {
     systemd.sockets."halmasuit-session" = {
       description = "halmasuit-session privileged PAM-lifecycle broker socket";
       wantedBy    = [ "sockets.target" ];
       socketConfig = {
         # SOCK_SEQPACKET: the broker's wire codec is one logical
         # message per datagram (matches halmasuit-greetd's framing).
         ListenSequentialPacket = "/run/halmasuit-session.sock";
         # The binary owns the accept loop and the global single slot
         # (Epic R5 / Amendment A2.1) — NOT one instance per
         # connection.
         Accept = false;
         # SO_PEERCRED in the broker is the load-bearing authorization
         # (only the HALMASUIT_BROKER_PEER_UID relay peer may drive
         # auth). The socket mode
         # is defence-in-depth only; the compositor will broker greeter
         # connections through a tighter SocketUser/SocketGroup once
         # the G-layer lands. Until then a permissive mode lets the
         # gated VM client connect; the SO_PEERCRED check still refuses
         # any non-greeter peer.
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
       };
     };

     assertions = [
       {
         assertion = cfg.greeterUid != null;
         message   = ''
           services.halmasuit.session.enable requires
           services.halmasuit.greeterUid to be set: the broker's
           SO_PEERCRED relay-peer gate rejects every connection whose
           peer uid is not the authorized relay peer (the compositor
           when services.halmasuit.enable, else the greeter uid).
         '';
       }
     ];
   })
  ];
}
