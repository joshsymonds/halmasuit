# NixOS module for halmasuit — Linux system compositor.
#
# Phase A *minimum* shape: one systemd unit that runs the halmasuit binary
# and pipes stderr to journald. No PAM service file, no setuid bit on
# halmasuit-spawn, no `compositor` / `greeter` users, no Plymouth /
# greetd replacement yet — those land in later tasks alongside the code
# that needs them.
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

    logLevel = lib.mkOption {
      type        = lib.types.str;
      default     = "info";
      example     = "debug,halmasuit::event=info";
      description = ''
        Value passed as `RUST_LOG`. halmasuit consumes this via
        `tracing-subscriber::EnvFilter`; the default keeps event-stream
        output at INFO, which is what the introspection sink emits at.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
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
        # Hardening minimums. Looser than the eventual `compositor` user
        # posture but already restricts the obvious abuse paths.
        NoNewPrivileges = true;
        ProtectSystem   = "strict";
        ProtectHome     = true;
        PrivateTmp      = true;
      };

      environment = {
        RUST_LOG = cfg.logLevel;
      };
    };
  };
}
