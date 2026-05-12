# halmasuit

A Linux **system compositor** — one long-lived display-server process that
owns the graphics hardware from the moment the user-space graphical stack
starts until the system shuts down, hosting a normal Wayland window manager
(niri) and its shell (DMS) as nested clients. Eliminates the visible flash
that exists today between greetd's greeter and the user's desktop session.

See **[ARCHITECTURE.md](ARCHITECTURE.md)** for design.

## Status

v1: test infrastructure under construction. No compositor code yet.

## Build / develop

Requires Nix with flakes enabled.

```bash
nix develop          # enter the dev shell (rust + cargo tooling + qemu)
just check           # lint + test
just test-vm         # NixOS VM tests (stub until v1 lands)
```

## License

Apache-2.0
