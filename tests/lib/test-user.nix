# Shared "intentionally insecure test user" module for halmasuit's NixOS
# VM tests. The test user has password "test", uid 1000, sudo without
# password, and is in wheel/video/input — all required so the test
# driver can drive auth + inspect process state.
#
# This module exists so future tests don't copy-paste the user config and
# silently inherit an unsafe posture without noticing.
{ ... }:
{
  users.users.test = {
    isNormalUser = true;
    password = "test";
    uid = 1000;
    extraGroups = [ "wheel" "video" "input" ];
  };

  # Test driver invokes sudo on the VM to inspect process state without
  # interactive prompting. Test VMs are throwaway; safe here only.
  security.sudo.wheelNeedsPassword = false;
}
