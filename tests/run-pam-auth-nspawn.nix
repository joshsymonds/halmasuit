# tests/run-pam-auth-nspawn.nix — same real-PAM gate as
# run-pam-auth.nix, packaged for systemd-nspawn instead of
# pkgs.testers.runNixOSTest.
#
# Produces a NixOS toplevel (no kernel, no qemu-vm config) the
# tests/lib/nspawn-rig.sh script can boot. The test-driver script
# runs inside the container and exits 0 on pass, non-zero on fail.
#
# Build via:
#   nix build --no-link --print-out-paths \
#     .#packages.x86_64-linux.run-pam-auth-nspawn-toplevel
#
# Run via:
#   just check-nspawn-pam-auth

{
  system ? "x86_64-linux",
  nixpkgs,
  halmasuit-session-pam-testdriver,
}:

let
  pkgs = import nixpkgs { inherit system; };

  nixos = (nixpkgs.lib.nixosSystem {
    inherit system;
    modules = [
      ./lib/test-user.nix
      ({ config, lib, pkgs, ... }: {
        # Minimal stage-2 init posture for nspawn:
        # - No bootloader, no kernel (nspawn uses host's kernel)
        # - Container-shaped filesystem layout (no /boot, no swap)
        boot.isContainer = true;

        # Match the original VM test's pam_unix-only stack exactly.
        # PAM module referenced by absolute store path so resolution
        # is path-deterministic. Real auth, no mock.
        security.pam.services.halmasuit-pam-test.text = ''
          auth     required ${pkgs.pam}/lib/security/pam_unix.so
          account  required ${pkgs.pam}/lib/security/pam_unix.so
        '';

        # Test user identity — UID 1000 + GID 1000, password "test".
        # Matches tests/lib/test-user.nix's expectation that the gid
        # is >= the broker's UID floor.
        users.groups.test = { gid = 1000; };
        users.users.test = { group = "test"; };

        # Install the testdriver so the in-container test script can
        # invoke it via PATH.
        environment.systemPackages = [ halmasuit-session-pam-testdriver ];

        # Ensure NSS / shadow are wired for real pam_unix resolution.
        # NixOS defaults are fine — listed explicitly for clarity.
        users.mutableUsers = false;

        # The in-container test driver. Exits 0 if all assertions
        # pass, non-zero otherwise. Mirrors run-pam-auth.nix's
        # Python testScript end-to-end.
        environment.etc."run-pam-auth-test.sh" = {
          mode = "0755";
          text = ''
            #!/bin/sh
            set -eu

            # Wait briefly for the broker's PAM service file to be
            # available (NixOS-activation should be done by the time
            # multi-user.target is up, but defensive).
            if [ ! -f /etc/pam.d/halmasuit-pam-test ]; then
              echo "FAIL: /etc/pam.d/halmasuit-pam-test missing"
              exit 1
            fi

            # 1. Correct password → identity OK.
            printf 'test' > /tmp/pw
            out=$(halmasuit-session-pam-testdriver halmasuit-pam-test test /tmp/pw)
            case "$out" in
              *"OK user=test uid=1000 gid=1000"*)
                echo "PASS: in-process pam_unix correct-password"
                ;;
              *)
                echo "FAIL: in-process pam_unix expected 'OK user=test uid=1000 gid=1000', got: $out"
                exit 1
                ;;
            esac

            # 2. Wrong password → fail closed (exit non-zero).
            printf 'definitely-not-the-password' > /tmp/bad
            if halmasuit-session-pam-testdriver halmasuit-pam-test test /tmp/bad 2>/dev/null; then
              echo "FAIL: wrong password unexpectedly accepted"
              exit 1
            fi
            echo "PASS: in-process pam_unix wrong-password fail-closed"

            # 3. Through the ephemeral privileged fork (R4).
            out=$(halmasuit-session-pam-testdriver halmasuit-pam-test test /tmp/pw --via-fork)
            case "$out" in
              *"OK user=test uid=1000 gid=1000"*)
                echo "PASS: via-fork pam_unix correct-password"
                ;;
              *)
                echo "FAIL: via-fork expected 'OK user=test uid=1000 gid=1000', got: $out"
                exit 1
                ;;
            esac

            # 4. Wrong password via-fork → fail closed.
            if halmasuit-session-pam-testdriver halmasuit-pam-test test /tmp/bad --via-fork 2>/dev/null; then
              echo "FAIL: via-fork wrong password unexpectedly accepted"
              exit 1
            fi
            echo "PASS: via-fork pam_unix wrong-password fail-closed"

            # 5. No orphan worker survives.
            halmasuit-session-pam-testdriver halmasuit-pam-test test /tmp/pw --via-fork >/dev/null
            if pgrep -f halmasuit-session-pam-testdriver >/dev/null; then
              echo "FAIL: orphan testdriver worker survived"
              exit 1
            fi
            echo "PASS: no orphan worker"

            # 6. Real R5 evict-old via the demo subcommand.
            out=$(halmasuit-session-pam-testdriver halmasuit-pam-test test /tmp/pw --evict-demo)
            case "$out" in
              *"EVICT_DEMO unauthorized_refused inflight_untouched=true"*)
                ;;
              *)
                echo "FAIL: evict-demo unauthorized refusal not observed: $out"
                exit 1
                ;;
            esac
            case "$out" in
              *"evicted_sigkill=true pid_changed=true"*)
                ;;
              *)
                echo "FAIL: evict-demo authorized SIGKILL+fresh-worker not observed: $out"
                exit 1
                ;;
            esac
            case "$out" in
              *"OK user=test uid=1000 gid=1000"*)
                ;;
              *)
                echo "FAIL: evict-demo post-evict fresh auth did not succeed: $out"
                exit 1
                ;;
            esac
            case "$out" in
              *"fresh_reaped_ok=true"*)
                ;;
              *)
                echo "FAIL: evict-demo fresh worker not reaped: $out"
                exit 1
                ;;
            esac
            if pgrep -f halmasuit-session-pam-testdriver >/dev/null; then
              echo "FAIL: orphan testdriver worker survived evict-demo"
              exit 1
            fi
            echo "PASS: R5 evict-demo (unauthorized refused, authorized SIGKILL, fresh auth, no orphan)"

            echo ""
            echo "run-pam-auth-nspawn: ALL ASSERTIONS PASSED"
          '';
        };

        system.stateVersion = "25.05";
      })
    ];
  });
in
nixos.config.system.build.toplevel
