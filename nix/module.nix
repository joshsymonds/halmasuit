# NixOS module for halmasuit — Linux system compositor.
#
# Phase A shape: one systemd unit running the halmasuit binary that
# hosts the greetd listener (greetd.sock) and the Wayland listener
# (wayland-0) under /run/halmasuit/. Greeters authorize via SO_PEERCRED:
# only the configured `greeterUid` may speak the greetd protocol.
#
# Today the unit runs as root (User= unset). The Phase A epic requires
# non-root *by completion*; the privilege drop lands when the first
# privileged code path (DRM master, Wayland socket in /run) lands and
# wants to refuse it. Keeping User= explicit-absent here makes the audit
# trail obvious.

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
      type        = lib.types.nullOr lib.types.int;
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
    ];

    # Default PAM service file — gives us unixAuth-backed pam_unix +
    # pam_env + pam_limits, which is what halmasuit-pam exercises in
    # the VM test and is the conventional starting stack for greeters.
    # Operators wanting custom modules disable installPamConfig and
    # declare security.pam.services.<name> themselves.
    security.pam.services = lib.mkIf cfg.installPamConfig {
      ${cfg.pamService} = {};
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
        # RuntimeDirectory creates /run/halmasuit/ with the unit's UID
        # (currently root; future `compositor` user inherits ownership
        # automatically when User= is set). The Wayland socket lives at
        # /run/halmasuit/wayland-0 — smithay's ListeningSocketSource
        # places the socket at $XDG_RUNTIME_DIR/<name>.
        RuntimeDirectory     = "halmasuit";
        RuntimeDirectoryMode = "0755";
        # Hardening minimums. Looser than the eventual `compositor` user
        # posture but already restricts the obvious abuse paths. Each
        # directive below is free for Phase A's userspace-only work; some
        # (notably MemoryDenyWriteExecute, RestrictNamespaces) will need
        # auditable relaxations when DRM/Wayland/smithay's GL backend land.
        NoNewPrivileges        = true;
        ProtectSystem          = "strict";
        ProtectHome            = true;
        PrivateTmp             = true;
        ProtectKernelTunables  = true;
        ProtectKernelModules   = true;
        ProtectKernelLogs      = true;
        ProtectControlGroups   = true;
        RestrictNamespaces     = true;
        RestrictRealtime       = true;
        RestrictSUIDSGID       = true;
        LockPersonality        = true;
        MemoryDenyWriteExecute = true;
        SystemCallArchitectures = "native";
        SystemCallFilter       = [ "@system-service" ];
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
        # Resolved path to halmasuit-spawn. Production deployments that
        # deprivilege halmasuit itself will override this to point at a
        # security.wrappers setuid wrapper; for the root-running Phase A
        # unit the binary is invoked directly.
        HALMASUIT_SPAWN_BIN   = "${cfg.spawnPackage}/bin/halmasuit-spawn";
      };
    };
  };
}
