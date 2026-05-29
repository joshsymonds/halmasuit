# tests/halmasuit-live-signin.nix — Epic #35 R2 gold-standard regression
# gate for gen-400's three observed live-deploy failures.
#
# Builds on the visual-dankgreeter-auth pattern (real DMS Quickshell as
# halmasuit's greeter, real PAM through the broker, end-to-end keystroke
# arc), and adds the parts that would have CAUGHT the gen-400 deploy
# failures the headless VM tests missed:
#
#   1. PAM stack composed of `pam_u2f sufficient cue interactive=false`
#      + `pam_unix try_first_pass` — the user's gnomon stack. The e2e
#      test (Pass A/B / Epic #28) used `pam_echo + pam_unix` which is
#      the WRONG conv shape; this test uses the actual production
#      modules.
#
#   2. Wallpaper→greeter flash-free window check — asserts that NO
#      FrameRendered with `degenerate=true` lands between
#      `ClientFirstFrame{wallpaper}` and `ClientFirstFrame{overlay}`.
#      This is the assertion that would have failed on gen-400 if the
#      observed flash translated to a degenerate frame audit.
#
#   3. libEGL warning absent — Quickshell's stderr is captured into the
#      journal under the halmasuit-session unit; the gen-400 boot
#      logged three `libEGL warning: failed to get driver name for
#      fd -1` lines. This gate fails if ANY libEGL warning is present.
#
#   4. End-to-end signin completion via the same keystroke arc as
#      visual-dankgreeter-auth — proves the full chain works under the
#      U2F-augmented PAM stack, not just under bare pam_unix.
#
# pam_u2f with no registered key + interactive=false should fall-through
# to pam_unix; the test does NOT emulate hardware (would need
# u2f-host/u2fd plumbing inside the VM which isn't worth the complexity
# until the simpler stack works). The Q→Q broker-level conv sequence
# (pam_u2f cue prompt then pam_unix password prompt) is what this
# exercises.

{
  system,
  nixpkgs,
  niri-flake,
  dms,
  halmasuit,
  halmasuit-session,
  ssimulacra2-cli,
}:

let
  pkgs = import nixpkgs {
    inherit system;
    config.allowUnfree = true;
  };
  niri = niri-flake.packages.${system}.niri-unstable;
  dmsShell = dms.packages.${system}.dms-shell;
  dmsQuickshell = dms.packages.${system}.quickshell;
  testInputs = { inherit niri-flake dms; };

  # Same DMS Quickshell launch wrapper as visual-dankgreeter-auth.
  greeterCmd = pkgs.writeShellScript "halmasuit-live-signin-greeter" ''
    export XDG_RUNTIME_DIR=/run/halmasuit-greeter
    export WAYLAND_DISPLAY=/run/halmasuit/wayland-0
    export GREETD_SOCK=/run/halmasuit/greetd.sock
    export XDG_DATA_DIRS=/run/current-system/sw/share:/usr/share
    export LIBGL_ALWAYS_SOFTWARE=1
    export GALLIUM_DRIVER=llvmpipe
    export MESA_LOADER_DRIVER_OVERRIDE=llvmpipe
    export LIBGL_DRI3_DISABLE=1
    export QT_QPA_PLATFORM=wayland
    export QT_QUICK_BACKEND=software
    export QT_WAYLAND_DISABLE_WINDOWDECORATION=1
    export HOME=/run/halmasuit-greeter
    export XDG_CACHE_HOME=$HOME/.cache
    export DMS_RUN_GREETER=1
    mkdir -p "$XDG_CACHE_HOME/dms-greeter"
    exec ${dmsQuickshell}/bin/quickshell \
      -p ${dmsShell}/share/quickshell/dms
  '';

  pamModule = m: "${pkgs.pam}/lib/security/${m}.so";

  # Empty u2f authfile — pam_u2f finds no registered key for the test
  # user, so the `sufficient` clause fails-through to pam_unix. This
  # exercises the Q→Q conv shape (pam_u2f cue PROMPT_ECHO_OFF, then
  # pam_unix password PROMPT_ECHO_OFF) which the Pass A Q-G1 fix
  # SHOULD now handle correctly via the broker_relay queue.
  emptyU2fAuthfile = pkgs.writeText "halmasuit-live-signin-u2f-empty" "";
in
pkgs.testers.runNixOSTest {
  name = "halmasuit-live-signin";

  skipTypeCheck = true;

  node.specialArgs = { inputs = testInputs; };

  interactive.nodes.machine = { ... }: {
    virtualisation.qemu.options = [
      "-device virtio-vga-gl"
      "-display gtk,gl=on"
    ];
  };

  nodes.machine =
    { config, lib, pkgs, ... }:
    {
      imports = [
        ../nix/module.nix
        ./lib/test-user.nix
        niri-flake.nixosModules.niri
        dms.nixosModules.greeter
      ];

      programs.niri.enable = true;
      programs.niri.package = niri;

      programs.dank-material-shell.greeter = {
        enable = true;
        compositor.name = "niri";
      };

      services.greetd.enable = lib.mkForce false;
      services.greetd.settings.default_session.user = "halmasuit-greeter";

      services.halmasuit = {
        enable           = true;
        package          = halmasuit;
        session.package  = halmasuit-session;
        greeterUid       = 999;
        greeterGroup     = "halmasuit-greeter";
        compositorUid    = 998;
        wallpaper        = { type = "image"; source = ./fixtures/wallpaper.png; };
        greeterCommand   = "${greeterCmd}";
        # We declare a custom PAM service; suppress the module's
        # default install.
        installPamConfig = false;
      };

      # Epic #35 R2: PAM stack mirrors the user's gnomon config —
      # pam_u2f sufficient + pam_unix try_first_pass. No registered
      # U2F key (empty authfile) means pam_u2f falls-through to
      # pam_unix, exercising the Q→Q conv shape end-to-end.
      security.pam.services.halmasuit.text = ''
        auth     sufficient ${pkgs.pam_u2f}/lib/security/pam_u2f.so cue interactive=false authfile=${emptyU2fAuthfile}
        auth     required   ${pamModule "pam_unix"} try_first_pass
        account  required   ${pamModule "pam_unix"}
        session  required   ${pamModule "pam_unix"}
      '';

      environment.pathsToLink = [ "/share/wayland-sessions" ];

      users.users.alice = {
        isNormalUser = true;
        uid          = 1001;
        group        = "alice";
        password     = "testpassword";
        extraGroups  = [ "halmasuit-greeter" ];
      };
      users.groups.alice.gid = 1001;

      users.users.halmasuit-greeter = {
        isSystemUser = true;
        uid          = 999;
        group        = "halmasuit-greeter";
      };
      users.groups.halmasuit-greeter.gid = 999;
      users.users.halmasuit-compositor = {
        isSystemUser = true;
        uid          = 998;
        group        = "halmasuit-greeter";
      };

      systemd.tmpfiles.rules = [
        "d /run/hsnap 0777 root root -"
        "d /run/halmasuit-greeter 0700 halmasuit-greeter halmasuit-greeter -"
      ];
      systemd.services.halmasuit.serviceConfig.ReadWritePaths = [ "/run/hsnap" ];

      virtualisation = {
        memorySize = 4096;
        cores      = 4;
        diskSize   = 8192;
        qemu.options = [
          "-vga none"
          "-device virtio-gpu-pci"
        ];
      };
    };

  testScript = ''
    import os
    import sys
    import time

    sys.path.insert(0, "${./lib}")
    os.environ["PATH"] = "${ssimulacra2-cli}/bin:" + os.environ.get("PATH", "")
    os.environ.setdefault("GOLDENS_DIR", "${./goldens}")

    import visual

    def fg_events():
        return [
            e["to"] for e in visual.introspect_events(machine)
            if e["event"] == "foreground_changed"
        ]

    machine.start()
    machine.wait_for_unit("multi-user.target")
    machine.wait_for_unit("halmasuit.service")

    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF scanout_active", timeout=30
    )

    # DMS Quickshell running directly as halmasuit's greeter.
    machine.wait_until_succeeds("pgrep -f quickshell", timeout=60)

    # Wait for the greeter's layer surface to appear so keystrokes land
    # on a focused surface.
    machine.wait_until_succeeds(
        "journalctl -u halmasuit -o cat | grep -qF 'new layer surface'",
        timeout=30,
    )
    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF foreground_changed",
        timeout=30,
    )
    assert "greeter" in fg_events(), (
        f"expected greeter foreground; got {fg_events()}"
    )

    # ── Epic #35 R3: libEGL warnings absent ───────────────────────────
    # gen-400 logged three `libEGL warning: failed to get driver name
    # for fd -1` lines from Quickshell. Whatever the root cause, the
    # absence of these warnings is part of the regression gate.
    egl_warnings = machine.execute(
        "journalctl -u halmasuit-session.service -o cat --no-pager | "
        "grep -F 'libEGL warning' | wc -l"
    )[1].strip()
    assert egl_warnings == "0", (
        f"Epic #35 R3 violated: libEGL warnings present in greeter "
        f"Quickshell stderr (count={egl_warnings}). gen-400 saw three "
        f"`libEGL warning: failed to get driver name for fd -1` lines; "
        f"the fix removes the cause, not the visibility.\n"
        f"Sample lines:\n"
        + machine.execute(
            "journalctl -u halmasuit-session.service -o cat --no-pager | "
            "grep -F 'libEGL warning' | head -5"
        )[1]
    )
    print("PASS Epic #35 R3: libEGL warnings absent from Quickshell stderr")

    # ── Epic #35 R1: wallpaper→greeter flash-free window ──────────────
    # Assert that no FrameRendered with degenerate=true lands between
    # wallpaper CFF and overlay CFF (the greeter's first paint). This
    # is the new class of flash the gen-400 deploy surfaced.
    events = visual.introspect_events(machine)
    wallpaper_cff_idx = None
    overlay_cff_idx = None
    for i, e in enumerate(events):
        if e.get("event") == "client_first_frame":
            role = e.get("role", "")
            if role == "wallpaper" and wallpaper_cff_idx is None:
                wallpaper_cff_idx = i
            elif role == "overlay" and overlay_cff_idx is None:
                overlay_cff_idx = i
    assert wallpaper_cff_idx is not None, "no client_first_frame{wallpaper} in journal"
    assert overlay_cff_idx is not None, (
        f"no client_first_frame{{overlay}} in journal — greeter never painted? "
        f"wallpaper CFF at idx {wallpaper_cff_idx}"
    )
    degenerate_in_window = [
        e for e in events[wallpaper_cff_idx:overlay_cff_idx]
        if e.get("event") == "frame_rendered" and e.get("degenerate") is True
    ]
    assert not degenerate_in_window, (
        f"Epic #35 R1 violated: degenerate frame(s) rendered between "
        f"wallpaper CFF and overlay CFF (the greeter-spawn window). "
        f"This is the gen-400 first-greeter flash class.\n"
        f"Count: {len(degenerate_in_window)}\n"
        f"Examples: {degenerate_in_window[:3]}"
    )
    print(
        f"PASS Epic #35 R1: wallpaper→greeter window "
        f"(idx {wallpaper_cff_idx}..{overlay_cff_idx}) has no degenerate frames"
    )

    # ── Epic #35 R5: Q→Q signin via real pam_u2f + pam_unix ────────────
    halmasuit_pid = machine.succeed(
        "systemctl show -p MainPID --value halmasuit.service"
    ).strip()

    # Same keystroke arc as visual-dankgreeter-auth.
    # The user types: username "alice", then Enter (DMS switches to
    # password mode), then the password "testpassword", then Enter.
    # pam_u2f sees the cue prompt round-trip with no key registered →
    # falls through; pam_unix verifies the password.
    machine.send_chars("alice")
    machine.send_key("ret")
    time.sleep(1)
    machine.send_chars("testpassword")
    machine.send_key("ret")

    # Epic #35 R2: signin completes end-to-end via real PAM under the
    # pam_u2f+pam_unix stack — proves the Q-G1 broker_relay queue
    # actually handles Q→Q (the gen-400-deployed shape), not just D→P
    # (the Pass A/B e2e shape that originally tested the queue).
    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF session_opened",
        timeout=120,
    )

    pid_now = machine.succeed(
        "systemctl show -p MainPID --value halmasuit.service"
    ).strip()
    assert pid_now == halmasuit_pid, (
        f"R13(b) violated: halmasuit restarted during auth: "
        f"{halmasuit_pid} -> {pid_now}"
    )

    machine.succeed(
        "journalctl -u halmasuit-session.service | "
        "grep -qF 'pam_unix(halmasuit:session): session opened for user alice'"
    )

    print(
        f"PASS Epic #35 R2/R5: halmasuit pid {halmasuit_pid} continuous; "
        f"DMS keystrokes → broker → real pam_u2f (fall-through) → "
        f"pam_unix (session_opened)"
    )

    # No-flash invariant over the entire arc (existing assertion).
    visual.assert_no_flash_stream(machine)

    print(
        "halmasuit-live-signin: ALL ASSERTIONS PASSED — gen-400 "
        "regressions (libEGL warnings, wallpaper→greeter flash window, "
        "Q→Q signin) are gated end-to-end."
    )
  '';
}
