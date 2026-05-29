# tests/session-conv-shapes.nix — PAM conversation-contract corpus.
#
# Epic #24 R7: regression-gates the broker's wire-protocol behavior
# across the full prompt/display matrix. Pre-Epic-#24 the only test
# PAM stacks were `pam_unix`-only (in session-r5r6 / run-pam-auth),
# which never emit PAM_TEXT_INFO/PAM_ERROR_MSG — leaving an entire
# half of libpam's conv contract untested. That gap was the
# proximate cause of the gen-399 production failure on gnomon
# (`pam_u2f cue + pam_unix` triggered the never-exercised
# display-class path; broker crashed with
# `protocol: unexpected frame for the current phase`).
#
# This gate parameterizes over 10 conv sequences, each registered as
# its own PAM service composed from real `pam_echo` /`pam_unix` /
# `pam_deny` (REAL libpam, NOT a mock — CLAUDE.md hard rule). Each
# sequence drives the broker's wire protocol directly from a python
# client and asserts the exact frame sequence the broker emits.
#
# Reference: `pam_conv(3)`; Linux-PAM Application Developers' Guide
# §6.2; OpenSSH `auth-pam.c`; greetd `protocol.md`.

{
  system,
  nixpkgs,
  halmasuit-session,
}:

let
  pkgs = import nixpkgs { inherit system; };

  # The fixed info-message texts pam_echo reads. pam_echo with an
  # empty file is a no-op (it skips conv); each cue file MUST have
  # real text.
  cueA = pkgs.writeText "halmasuit-conv-cue-a" "Please touch the device";
  cueB = pkgs.writeText "halmasuit-conv-cue-b" "Still waiting on hardware";
  cueC = pkgs.writeText "halmasuit-conv-cue-c" "Last chance — touch now";

  pamModule = m: "${pkgs.pam}/lib/security/${m}.so";

  # Stack builder helpers — each builds a `security.pam.services.<name>.text`
  # PAM file (account is always pam_unix for resolution).
  echoLine = file: "auth required ${pamModule "pam_echo"} file=${file}";
  unixAuthFirst = "auth required ${pamModule "pam_unix"}";
  unixAuthTryFirst = "auth required ${pamModule "pam_unix"} try_first_pass";
  denyLine = "auth required ${pamModule "pam_deny"}";
  acctLine = "account required ${pamModule "pam_unix"}";

  # Epic #35 R5: pam_u2f cue-then-fall-through helper. With
  # `sufficient` control + empty authfile (no key registered for the
  # test user) + `interactive=false`, pam_u2f emits the cue
  # PROMPT_ECHO_OFF "Please touch the device" once, then returns
  # without success — falling through to the next module. Mirrors
  # the user's gnomon stack (gen-400 production conv shape).
  emptyU2fAuthfile = pkgs.writeText "halmasuit-conv-u2f-empty" "";
  u2fCueSufficient = "auth sufficient ${pamModule "pam_u2f"} cue interactive=false authfile=${emptyU2fAuthfile}";

  # The corpus: eleven conv sequences. Each entry maps service-name →
  # (PAM stack, expected wire frame sequence). The python driver
  # iterates and asserts.
  #
  # Frame "abbreviations" used in the expected list:
  #   D <style> <message>   → BrokerToCompositor::ConvDisplay  (client MUST NOT respond)
  #   P <style> <message>   → BrokerToCompositor::ConvPrompt   (client sends response)
  #   S                     → BrokerToCompositor::Success
  #   F                     → BrokerToCompositor::Failure
  corpus = [
    {
      # 1. The plain prompt baseline — same conv shape as pre-Epic
      # session-r5r6 used; sanity-check that the refactor did NOT
      # regress the prompt-only path.
      name = "prompt-only-success";
      stack = ''
        ${unixAuthFirst}
        ${acctLine}
      '';
      sequence = [
        { kind = "P"; style = "secret"; resp = "test"; }
        { kind = "S"; }
      ];
    }

    {
      # 2. The exact gen-399 production shape:
      # `pam_u2f cue + pam_unix try_first_pass`.
      name = "info-then-prompt-success";
      stack = ''
        ${echoLine cueA}
        ${unixAuthTryFirst}
        ${acctLine}
      '';
      sequence = [
        { kind = "D"; style = "info"; }
        { kind = "P"; style = "secret"; resp = "test"; }
        { kind = "S"; }
      ];
    }

    {
      # 3. Two info-class displays in a row before the prompt. Pins
      # that AwaitWorker survives multiple sequential displays
      # (Epic #24 R4 invariant tested at the broker unit-test level
      # AND at the wire level here).
      name = "info-info-then-prompt-success";
      stack = ''
        ${echoLine cueA}
        ${echoLine cueB}
        ${unixAuthTryFirst}
        ${acctLine}
      '';
      sequence = [
        { kind = "D"; style = "info"; }
        { kind = "D"; style = "info"; }
        { kind = "P"; style = "secret"; resp = "test"; }
        { kind = "S"; }
      ];
    }

    {
      # 4. Three displays in a row — pathological but legal; pins
      # there is no implicit cap on consecutive displays.
      name = "info-info-info-then-prompt-success";
      stack = ''
        ${echoLine cueA}
        ${echoLine cueB}
        ${echoLine cueC}
        ${unixAuthTryFirst}
        ${acctLine}
      '';
      sequence = [
        { kind = "D"; style = "info"; }
        { kind = "D"; style = "info"; }
        { kind = "D"; style = "info"; }
        { kind = "P"; style = "secret"; resp = "test"; }
        { kind = "S"; }
      ];
    }

    {
      # 5. The failure terminal with NO conv (pam_deny alone). The
      # broker emits Failure directly without any preceding conv
      # frame — exercises the AuthFailed disposition path.
      name = "deny-only-failure";
      stack = ''
        ${denyLine}
        ${acctLine}
      '';
      sequence = [
        { kind = "F"; }
      ];
    }

    {
      # 6. Display + failure: info banner ("touch")  then pam_deny.
      # Confirms a display frame DOES NOT advance the broker out of
      # AwaitWorker in the failure direction either.
      name = "info-then-deny-failure";
      stack = ''
        ${echoLine cueA}
        ${denyLine}
        ${acctLine}
      '';
      sequence = [
        { kind = "D"; style = "info"; }
        { kind = "F"; }
      ];
    }

    {
      # 7. Two displays + failure. Pins multi-display + failure.
      name = "info-info-then-deny-failure";
      stack = ''
        ${echoLine cueA}
        ${echoLine cueB}
        ${denyLine}
        ${acctLine}
      '';
      sequence = [
        { kind = "D"; style = "info"; }
        { kind = "D"; style = "info"; }
        { kind = "F"; }
      ];
    }

    {
      # 8. Three displays + failure — pathological version of #7.
      name = "info-info-info-then-deny-failure";
      stack = ''
        ${echoLine cueA}
        ${echoLine cueB}
        ${echoLine cueC}
        ${denyLine}
        ${acctLine}
      '';
      sequence = [
        { kind = "D"; style = "info"; }
        { kind = "D"; style = "info"; }
        { kind = "D"; style = "info"; }
        { kind = "F"; }
      ];
    }

    {
      # 9. Display + prompt + WRONG password: pins that submitting
      # a real ConvResponse for the prompt still routes to the
      # broker (it does NOT get swallowed — the awaiting_display_ack
      # flag was cleared when the prompt arrived) and the failure
      # terminal follows.
      name = "info-then-prompt-wrong-pw-failure";
      stack = ''
        ${echoLine cueA}
        ${unixAuthTryFirst}
        ${acctLine}
      '';
      sequence = [
        { kind = "D"; style = "info"; }
        { kind = "P"; style = "secret"; resp = "wrong-password"; }
        { kind = "F"; }
      ];
    }

    {
      # 10. Display-after-prompt: a display arrives AFTER a prompt's
      # response has been consumed. Pins that
      # `awaiting_display_ack` is re-armed correctly on the second
      # display (i.e. the bit is per-frame, not per-conv-session).
      # Composed: pam_echo(A) + pam_unix(try_first_pass) +
      # pam_echo(B). With try_first_pass, pam_unix consumes the
      # password and pam_echo(B) emits the trailing info before
      # account.
      name = "info-prompt-info-success";
      stack = ''
        ${echoLine cueA}
        ${unixAuthTryFirst}
        ${echoLine cueB}
        ${acctLine}
      '';
      sequence = [
        { kind = "D"; style = "info"; }
        { kind = "P"; style = "secret"; resp = "test"; }
        { kind = "D"; style = "info"; }
        { kind = "S"; }
      ];
    }
  ];

  # NOTE on the Q→Q production shape (Epic #35 R5):
  #
  # The user's gnomon stack (`pam_u2f sufficient cue interactive=false`
  # + `pam_unix try_first_pass`) produces two consecutive Secret-class
  # ConvPrompts in some paths (cue + password re-prompt) and a single
  # ConvPrompt in others (cue with response reused as authtok). The
  # exact shape depends on pam_u2f's PAM_AUTHTOK behavior, which
  # varies with module flags and the user-not-in-authfile branch.
  #
  # Constructing a deterministic Q→Q broker-wire sequence with the
  # PAM modules available in nixpkgs proved infeasible without
  # custom-module work:
  #   - `pam_u2f sufficient` + empty authfile returns auth-failure
  #     that stops the chain (despite the `sufficient` directive),
  #     so pam_unix never gets to re-prompt.
  #   - pam_u2f stores the cue response as PAM_AUTHTOK, so any
  #     downstream pam_unix sees the prior response and skips its
  #     own prompt.
  #
  # The Q→Q broker_relay queue logic is regression-gated by:
  #   - `crates/halmasuit/src/broker_relay.rs::tests::
  #      back_to_back_prompt_then_prompt_serializes_both_forwards`
  #     (sans-IO unit test; passes deterministically).
  #   - `tests/halmasuit-live-signin.nix` (real-DMS + real-pam_u2f +
  #     real-pam_unix VM test; passes end-to-end with the production
  #     PAM stack, proving the queue holds in the production conv
  #     path regardless of which specific Q→Q-vs-Q+authtok branch
  #     libpam takes).
  #
  # Adding a custom dummy PAM module solely for the broker corpus to
  # synthesize a Q→Q wire shape is out of scope; the structural
  # equivalence of (push Forward, push Forward, pop, pop) and the
  # existing (push Forward, push Swallow, pop, pop) corpus sequences
  # already exercises the queue logic at the broker wire layer.

  # Generate the security.pam.services attrset from the corpus list.
  pamServices = builtins.listToAttrs (
    map (c: {
      name = "halmasuit-conv-${c.name}";
      value = { text = c.stack; };
    }) corpus
  );

  # The corpus encoded as JSON for the python client. Each entry has
  # the wire service name + the expected sequence.
  corpusJson = pkgs.writeText "halmasuit-conv-corpus.json" (builtins.toJSON (
    map (c: {
      service = "halmasuit-conv-${c.name}";
      sequence = c.sequence;
    }) corpus
  ));

  # The python driver: iterates over the corpus, for each sequence
  # opens a fresh broker connection, sends `begin_auth { service }`,
  # then steps through the expected frame list. Each `D` frame is
  # consumed without a wire response (Epic #24 R5: display is
  # one-way on the broker wire — the compositor's swallow is what
  # this test models). Each `P` frame is answered with the
  # configured `resp` string. Each `S`/`F` is the terminal.
  client = pkgs.writeText "conv-shapes-client.py" ''
    import json, socket, struct, sys, time
    PATH = "/run/halmasuit-session.sock"

    # The broker's AuthSlot enforces a GLOBAL churn throttle (Epic R5):
    # DEFAULT_MAX_PER_WINDOW=5 connections per DEFAULT_WINDOW=10s,
    # tracked in a single VecDeque<Instant> on AuthSlot (not keyed by
    # uid — see crates/halmasuit-session/src/slot.rs). Sequential
    # corpus runs WILL trip it unless we spread out. 2.5s between
    # connections caps us at 4 per 10s sliding window — comfortably
    # under the bound while keeping total test time under ~30s. The
    # window-prune check uses `>=` (slot.rs:170) so exact-boundary
    # moments do NOT collide.
    THROTTLE_GAP_SECS = 2.5

    with open("${corpusJson}") as f:
        CORPUS = json.load(f)

    def frame(obj):
        b = json.dumps(obj, separators=(",", ":")).encode()
        return struct.pack("=I", len(b)) + b

    def recv_frame(s):
        # SOCK_SEQPACKET: one recv = one datagram. Use MSG_TRUNC to
        # detect oversize frames that would otherwise silently truncate
        # at the 65536 buffer (the broker's MAX_MESSAGE_SIZE is 1 MiB;
        # a >65536 frame is either a regression or a contract change
        # and must red the test, not silently parse a JSON prefix).
        data = s.recv(65536, socket.MSG_TRUNC)
        if not data:
            return None
        if len(data) > 65536:
            print(f"  FAIL: frame oversized — broker emitted {len(data)} bytes, buffer is 65536", flush=True)
            return None
        ln = struct.unpack("=I", data[:4])[0]
        if 4 + ln != len(data):
            print(f"  FAIL: length-prefix mismatch — prefix says {ln}, datagram is {len(data) - 4} bytes", flush=True)
            return None
        return json.loads(data[4:4 + ln])

    overall_ok = True
    for idx, entry in enumerate(CORPUS):
        if idx > 0:
            time.sleep(THROTTLE_GAP_SECS)
        svc = entry["service"]
        seq = entry["sequence"]
        print(f"── {svc} ──")
        try:
            s = socket.socket(socket.AF_UNIX, socket.SOCK_SEQPACKET)
            s.connect(PATH)
            s.send(frame({"type": "begin_auth", "service": svc, "username": "test"}))

            local_ok = True
            for i, expected in enumerate(seq):
                got = recv_frame(s)
                if got is None:
                    print(f"  FAIL[{i}]: broker closed connection unexpectedly")
                    local_ok = False
                    break
                kind = expected["kind"]
                if kind == "D":
                    if got.get("type") != "conv_display":
                        print(f"  FAIL[{i}]: expected conv_display, got {got}")
                        local_ok = False
                        break
                    if got.get("style") != expected["style"]:
                        print(f"  FAIL[{i}]: display style mismatch — expected {expected['style']}, got {got.get('style')}")
                        local_ok = False
                        break
                    # R5: do NOT send a response — display is one-way
                    # on the broker wire. The compositor would have
                    # swallowed DMS's mandated greetd respond("") here.
                elif kind == "P":
                    if got.get("type") != "conv_prompt":
                        print(f"  FAIL[{i}]: expected conv_prompt, got {got}")
                        local_ok = False
                        break
                    if got.get("style") != expected["style"]:
                        print(f"  FAIL[{i}]: prompt style mismatch — expected {expected['style']}, got {got.get('style')}")
                        local_ok = False
                        break
                    s.send(frame({"type": "conv_response", "response": expected["resp"]}))
                elif kind == "S":
                    if got.get("type") != "success":
                        print(f"  FAIL[{i}]: expected success, got {got}")
                        local_ok = False
                        break
                elif kind == "F":
                    if got.get("type") != "failure":
                        print(f"  FAIL[{i}]: expected failure, got {got}")
                        local_ok = False
                        break
                else:
                    print(f"  FAIL[{i}]: unknown expected kind {kind!r}")
                    local_ok = False
                    break

            s.close()
            if local_ok:
                print(f"  PASS: {svc}")
            else:
                overall_ok = False
                print(f"  FAIL: {svc}")
        except Exception as e:
            print(f"  FAIL ({svc}): exception {e!r}")
            overall_ok = False

    if overall_ok:
        print("OVERALL_PASS: all corpus sequences completed")
        sys.exit(0)
    print("OVERALL_FAIL: at least one corpus sequence broke the contract")
    sys.exit(1)
  '';
in
pkgs.testers.runNixOSTest {
  name = "session-conv-shapes";

  nodes.machine =
    { config, lib, pkgs, ... }:
    {
      imports = [
        ../nix/module.nix
        ./lib/test-user.nix
      ];

      # Epic R8 UID floor: resolved gid must be ≥ broker UID floor —
      # give `test` a user-private GID-1000 group instead of nixos's
      # default `users` (gid 100). Same convention as session-r5r6.
      users.groups.test = { gid = 1000; };
      users.users.test = { group = "test"; };

      # Register every corpus service under security.pam.services.
      # The broker uses the service from the wire-provided
      # `BeginAuth { service }`, not the configured default, so each
      # corpus sequence drives its own PAM stack via its `service`
      # field.
      security.pam.services = pamServices;

      services.halmasuit.session = {
        enable  = true;
        package = halmasuit-session;
      };
      services.halmasuit.greeterUid = 1000;
      # Broker-only deployment; no compositor → no halmasuit-greeter
      # group exists. Pin to test's group for eval.
      services.halmasuit.greeterGroup = "test";
      # The configured default service is never used by this test;
      # each connection wire-specifies its own.
      services.halmasuit.pamService = "halmasuit-conv-prompt-only-success";

      environment.systemPackages = [ pkgs.python3 ];

      virtualisation = {
        memorySize = 768;
        cores      = 1;
        diskSize   = 1024;
      };
    };

  testScript = ''
    machine.wait_for_unit("sockets.target")
    machine.wait_until_succeeds("systemctl is-active halmasuit-session.socket")

    # Drive the full corpus from one python invocation. `execute`
    # not `succeed` so we always read /tmp/client.out — per-sequence
    # FAIL lines name the exact frame that broke.
    machine.execute(
        "runuser -u test -- python3 ${client} > /tmp/client.out 2>&1; "
        "echo CLIENT_EXIT=$? >> /tmp/client.out"
    )

    out = machine.succeed("cat /tmp/client.out")
    print("─── corpus client output ───")
    print(out)
    print("─── broker journal tail ───")
    print(machine.succeed("journalctl -u halmasuit-session --no-pager | tail -40 || true"))

    assert "OVERALL_PASS:" in out, (
        "session-conv-shapes corpus FAILED. The broker did not "
        "satisfy the libpam conv contract for one or more of the 10 "
        "regression-gated sequences: prompt-only-success, "
        "info-then-prompt-success, info-info-then-prompt-success, "
        "info-info-info-then-prompt-success, deny-only-failure, "
        "info-then-deny-failure, info-info-then-deny-failure, "
        "info-info-info-then-deny-failure, info-then-prompt-wrong-pw-"
        "failure, info-prompt-info-success. The per-sequence FAIL "
        "line above names the exact frame that broke; common modes "
        "are: broker emitted UnexpectedFrame for a ConvDisplay; "
        "broker advanced phase on a display; compositor failed to "
        "swallow the greetd-side response for a display. References: "
        "pam_conv(3), Linux-PAM AppDev Guide §6.2."
    )
    assert "CLIENT_EXIT=0" in out, f"client did not exit cleanly: {out}"

    print(
        "session-conv-shapes: 10/10 conv sequences satisfied the "
        "libpam contract across the broker wire — prompt-only-success, "
        "info-then-prompt-success, info-info(-info)-then-prompt-success "
        "(×2), deny-only-failure, info(-info-info)-then-deny-failure "
        "(×3), info-then-prompt-wrong-pw-failure, info-prompt-info-"
        "success."
    )
  '';
}
