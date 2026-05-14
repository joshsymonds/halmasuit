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
# code path runs unprivileged; the only setuid binary on the closure
# is halmasuit-spawn, wrapped via `security.wrappers` below.

{ config, lib, pkgs, ... }:

let
  cfg = config.services.halmasuit;
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

    spawnPackage = lib.mkOption {
      type        = lib.types.package;
      default     = pkgs.halmasuit-spawn;
      defaultText = lib.literalExpression "pkgs.halmasuit-spawn";
      description = ''
        Package providing the halmasuit-spawn privilege-drop helper. The
        unit invokes `''${spawnPackage}/bin/halmasuit-spawn` on session
        start to fork + setresuid into the authenticated user. Requires
        the halmasuit overlay (or a manual `pkgs.halmasuit-spawn`
        definition) for the default to resolve.
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
  };

  config = lib.mkIf cfg.enable {
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

    # Default PAM service file — gives us unixAuth-backed pam_unix +
    # pam_env + pam_limits, which is what halmasuit-pam exercises in
    # the VM test and is the conventional starting stack for greeters.
    # Operators wanting custom modules disable installPamConfig and
    # declare security.pam.services.<name> themselves.
    security.pam.services = lib.mkIf cfg.installPamConfig {
      ${cfg.pamService} = {};
    };

    # Setuid wrapper for halmasuit-spawn. After halmasuit deprivileges
    # itself, it still needs to fork+exec halmasuit-spawn to bring up
    # the authenticated user's session — halmasuit-spawn's own
    # `setresuid` requires euid==0, which the kernel grants at exec
    # time via the setuid bit on this wrapper. The real binary lives
    # in the nix store; security.wrappers writes a tiny setuid shim
    # at /run/wrappers/bin/halmasuit-spawn that re-execs it.
    security.wrappers.halmasuit-spawn = {
      owner  = "root";
      group  = "root";
      setuid = true;
      source = "${cfg.spawnPackage}/bin/halmasuit-spawn";
    };

    systemd.services.halmasuit = {
      description = "halmasuit — Linux system compositor";
      wantedBy    = [ "multi-user.target" ];
      # `after` is intentionally narrow for Phase A: nothing else needs
      # to be up. As DRM / Wayland / D-Bus integrations land, append the
      # specific units they depend on (systemd-logind.service, etc.).
      after       = [ "local-fs.target" ];

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
        # `shadow` group access lets halmasuit-pam (running as the
        # compositor uid) call `getspnam` directly on /etc/shadow
        # rather than forking the setuid `unix_chkpwd` helper.
        # The fork path is fragile: any inherited seccomp filter or
        # NNP bit on halmasuit silently disables the setuid bit on
        # the helper, leaving auth wedged with a confusing
        # "user unknown" log line. Direct shadow access is the
        # documented escape: pam_unix tries `getspnam` first and
        # only falls back to the helper on EPERM.
        SupplementaryGroups    = [ "shadow" ];
        # Hardening posture. The privilege split (halmasuit deprivileges
        # to compositorUid; halmasuit-spawn is the only setuid binary)
        # is the primary defense. The directives below are
        # defense-in-depth. We deliberately do NOT enable the systemd
        # directives that implicitly set `NoNewPrivileges=yes`
        # (MemoryDenyWriteExecute, RestrictNamespaces, RestrictRealtime,
        # SystemCallFilter, LockPersonality, ProtectKernelTunables,
        # ProtectKernelModules, RestrictSUIDSGID — see
        # systemd.exec(5)'s context_has_seccomp/no_new_privileges).
        # With NNP on, the kernel ignores the setuid bit on
        # halmasuit-spawn at exec time, breaking the session-spawn
        # handoff entirely. halmasuit-spawn itself is audit-grade
        # (microscopic, fuzzed, UID_MIN floor) and the only setuid
        # binary in the closure — that's what we're trusting instead.
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
        # Resolved path to halmasuit-spawn — the setuid wrapper
        # declared above. halmasuit (deprivileged) execs this to
        # launch user sessions.
        HALMASUIT_SPAWN_BIN = "/run/wrappers/bin/halmasuit-spawn";
      } // lib.optionalAttrs (cfg.greeterCommand != null) {
        # Greeter binary halmasuit fork+execs at startup as the
        # greeter user. See `services.halmasuit.greeterCommand`.
        HALMASUIT_GREETER_COMMAND = cfg.greeterCommand;
      };
    };
  };
}
