# Deployment-side gate for v2 Phase A's introspection trio.
#
# Boots a vanilla NixOS machine with `services.halmasuit.enable = true` and
# asserts the lifecycle events documented in halmasuit-introspect's Event
# schema land in journald and that the unit transitions cleanly through
# start → run → stop.
#
# Sibling tests:
# - crates/halmasuit/tests/lifecycle.rs verifies the in-process shape (real
#   tracing-subscriber + calloop + signalfd against the cargo-built binary).
# - This test verifies the deployment shape (NixOS module + systemd unit +
#   journald + tracing-subscriber JSON envelope all stacked end-to-end).
#
# Mutation check (do this once, locally, when modifying this test or the
# binary it gates): comment out one emit() call in
# crates/halmasuit/src/main.rs, rebuild, and confirm this test goes RED with
# a useful failure message.

{
  system,
  nixpkgs,
  halmasuit,
}:

let
  pkgs = import nixpkgs { inherit system; };
in
pkgs.testers.runNixOSTest {
  name = "halmasuit-introspect";

  nodes.machine =
    { ... }:
    {
      imports = [ ../nix/module.nix ];

      services.halmasuit = {
        enable  = true;
        package = halmasuit;
      };

      # Phase A doesn't touch graphics, DRM, or input yet — keep the VM
      # minimal. Subsequent tasks will need virtio-gpu-pci etc.
      virtualisation = {
        memorySize = 512;
        cores      = 1;
        diskSize   = 1024;
      };
    };

  testScript = ''
    import json

    machine.start()
    machine.wait_for_unit("multi-user.target")
    machine.wait_for_unit("halmasuit.service")

    # Give halmasuit a beat to emit Started + PhaseEntered. The unit reaches
    # `wait_for_unit` once systemd considers it `active`; the first events
    # are emitted within microseconds of process start but journald has its
    # own ingestion latency, so wait for the second event explicitly before
    # asserting.
    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF 'phase_entered'",
        timeout=30,
    )

    def captured_events():
        """
        Read journald's record-per-line view of halmasuit's MESSAGE field
        and decode the two-layer JSON: the tracing-subscriber envelope, then
        the halmasuit-introspect inner JSON inside `fields.json`.
        Returns a list of (envelope_dict, inner_dict) tuples.
        """
        raw = machine.succeed(
            "journalctl -u halmasuit -o cat --no-pager"
        )
        out = []
        for line in raw.splitlines():
            line = line.strip()
            if not line:
                continue
            try:
                outer = json.loads(line)
            except Exception:
                continue
            if outer.get("target") != "halmasuit::event":
                continue
            inner_str = outer.get("fields", {}).get("json")
            if not inner_str:
                continue
            try:
                inner = json.loads(inner_str)
            except Exception:
                continue
            out.append((outer, inner))
        return out

    def find(events, event_name):
        return next((inner for (_, inner) in events if inner.get("event") == event_name), None)

    events = captured_events()
    assert events, (
        "no halmasuit::event-targeted records captured. "
        "Raw journal: " + machine.succeed("journalctl -u halmasuit -o cat --no-pager")
    )

    # Assertion 1: Started carries a numeric pid and the crate version.
    started = find(events, "started")
    assert started is not None, f"no started event captured. Events: {events}"
    assert isinstance(started.get("pid"), int) and started["pid"] > 0, (
        f"started.pid must be positive int, got: {started}"
    )
    assert started.get("version") == "0.1.0", (
        f"started.version must match Cargo.toml, got: {started}"
    )

    # Assertion 2: PhaseEntered with phase = init.
    phase_entered = find(events, "phase_entered")
    assert phase_entered is not None, f"no phase_entered event captured. Events: {events}"
    assert phase_entered.get("phase") == "init", (
        f"phase_entered.phase must be 'init' in Phase A, got: {phase_entered}"
    )

    # Assertion 3: SIGTERM via systemctl stop produces Shutdown with
    # reason = signal_term, and the unit transitions to inactive (not failed).
    machine.succeed("systemctl stop halmasuit")
    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF 'signal_term'",
        timeout=30,
    )

    events = captured_events()
    shutdown = find(events, "shutdown")
    assert shutdown is not None, f"no shutdown event captured. Events: {events}"
    assert shutdown.get("reason") == "signal_term", (
        f"shutdown.reason must be 'signal_term' after systemctl stop (which sends SIGTERM), "
        f"got: {shutdown}"
    )

    # Assertion 4: clean exit, not failed-state.
    active_state = machine.succeed(
        "systemctl show -p ActiveState --value halmasuit.service"
    ).strip()
    assert active_state == "inactive", (
        f"halmasuit.service must be inactive (clean stop) after systemctl stop, "
        f"got ActiveState={active_state!r}"
    )

    # Assertion 5: tracing-subscriber envelope carries our target. This
    # protects against accidental loss of the `target:` attribute in emit().
    for (envelope, _) in events:
        assert envelope.get("target") == "halmasuit::event", (
            f"envelope must carry halmasuit::event target, got: {envelope}"
        )

    print(f"halmasuit-introspect: {len(events)} matching events captured, all assertions PASSED")
  '';
}
