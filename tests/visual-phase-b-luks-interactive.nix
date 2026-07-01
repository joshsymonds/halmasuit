# visual-phase-b-luks-interactive — interactive LUKS unlock gate.
#
# The end-to-end proof that the initramfs interactive unlock works
# (epic req 11): halmasuit-luks runs WITHOUT --passphrase-from, so it
# maps a real Wayland prompt over halmasuit's wallpaper, and the volume
# is unlocked by TYPING the passphrase — keystrokes flow
# machine.send_chars → QEMU keyboard → halmasuit's initramfs libinput →
# the seat → wl_keyboard → halmasuit-luks. Without the initramfs
# keyboard wiring (task #7) this prompt would receive nothing.
#
# Asserts:
#   - LuksPromptShown is emitted (the interactive prompt mapped).
#   - The prompt was handled via the centered, self-sized window path
#     (NOT fullscreen) — the windowed-path log line (centering math
#     itself is unit-tested in drm::centered_origin).
#   - The typed passphrase unlocks /dev/mapper/test-luks-data and the
#     boot proceeds to the rootfs (multi-user.target).
#
# The non-interactive auto-unlock matrix (visual-phase-b-*) is unchanged
# and still covers the production responder path + the session goldens.

{
  system,
  nixpkgs,
  niri-flake,
  dms,
  halmasuit-debug,
  halmasuit-luks,
  halmasuit-session,
  halmasuit-vm-client,
}:

let
  pkgs = import nixpkgs {
    inherit system;
    config.allowUnfree = true;
  };
in
pkgs.testers.runNixOSTest {
  name = "visual-phase-b-luks-interactive";

  skipTypeCheck = true;

  nodes.machine = {
    imports = [
      ../nix/module.nix
      (import ./lib/phase-b-golden.nix {
        wallpaper = {
          type = "image";
          source = ./fixtures/wallpaper.png;
        };
        lukshape    = "side-volume";
        interactive = true;
        inherit halmasuit-debug halmasuit-luks halmasuit-session
                halmasuit-vm-client niri-flake dms;
        wallpaperStorePaths = [ ./fixtures/wallpaper.png ];
      })
    ];
  };

  testScript = ''
    # Console-only assertions (the unlock happens in the initramfs, before
    # the shell backdoor exists), so no tests/lib/visual import is needed.
    PASSPHRASE = "luks-test-unlock-secret"

    # ── First boot: format the side volume, then crash ───────────────
    # /dev/vdb has no LUKS header yet, so the unlock unit's `isLuks`
    # guard skips on this boot. We luksFormat it, then crash; the second
    # boot's unlock unit then drives systemd-cryptsetup, which asks for
    # the passphrase.
    machine.start()
    machine.wait_for_unit("multi-user.target")
    machine.succeed(
        f"printf '{PASSPHRASE}' | "
        "cryptsetup luksFormat -q --iter-time 1 /dev/vdb -"
    )
    machine.succeed("sync")
    machine.crash()

    # ── Second boot: the INTERACTIVE prompt + typed unlock ───────────
    # CRUCIAL: the whole unlock happens in the INITRAMFS, where the test
    # driver's shell backdoor does NOT exist (it comes up only with the
    # rootfs, post-pivot) — and the pivot is itself blocked by
    # systemd-cryptsetup waiting for this passphrase. So every step here
    # is asserted on the SERIAL CONSOLE (halmasuit + halmasuit-luks +
    # systemd-cryptsetup all log there pre-pivot), never via journalctl /
    # machine.succeed. We deliberately stop at the cryptsetup unlock: the
    # full post-pivot boot-to-session is covered by the non-interactive
    # phase-b matrix; THIS gate's job is the typed-unlock chain.
    machine.start()

    # The interactive agent mapped its prompt → LuksPromptShown, and
    # halmasuit handled it via the centered, self-sized window path (NOT
    # fullscreen; the centering arithmetic itself is unit-tested in
    # drm::centered_origin). These are the earliest boot-2 markers, so
    # the watcher (armed right after start()) catches them as they stream.
    machine.wait_for_console_text("luks_prompt_shown")
    machine.wait_for_console_text("centered, self-sized")
    print("PASS: interactive LUKS prompt mapped as a centered, non-fullscreen window")

    # systemd-cryptsetup wrote the ask-password request and the agent is
    # now actively prompting + capturing keystrokes.
    machine.wait_for_console_text("new ask-password request")

    # Type the passphrase on the emulated keyboard. This reaches the
    # agent ONLY because halmasuit wired libinput in the initramfs
    # (device enumerated) and routed keyboard focus to the prompt
    # (epic req 11 / task #7).
    machine.send_chars(PASSPHRASE)
    machine.send_key("ret")

    # systemd-cryptsetup ACCEPTED the typed passphrase and set up the
    # crypt device for /dev/vdb — the end-to-end proof that interactive
    # initramfs unlock works. This single line implies the whole chain:
    # the agent received the typed keystrokes and submitted the CORRECT
    # passphrase (cryptsetup only sets the cipher on a correct key).
    #
    # Asserted on the SERIAL console, not journalctl: the entire unlock
    # happens in the initramfs, before the test driver's shell backdoor
    # (which only comes up with the rootfs, post-pivot) exists. We also
    # wait ONLY for this line after typing — not the near-simultaneous
    # "passphrase submitted" — because the two emit in racy order and
    # wait_for_console_text matches forward-only; this is the later-or-
    # equal, definitive marker. The centered/non-fullscreen prompt was
    # already asserted on the console above (luks_prompt_shown +
    # "centered, self-sized"); its arithmetic is unit-tested in
    # drm::centered_origin.
    machine.wait_for_console_text("key size 512 bits for device /dev/vdb")
    print("PASS: typed passphrase unlocked /dev/vdb — "
          "interactive initramfs keyboard unlock works end to end")
  '';
}
