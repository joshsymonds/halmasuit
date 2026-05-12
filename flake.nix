{
  description = "halmasuit — Linux system compositor.";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
  };

  outputs =
    { self
    , nixpkgs
    }:
    let
      forEachSystem = nixpkgs.lib.genAttrs [
        "x86_64-linux"
        "aarch64-linux"
      ];
    in
    {
      devShells = forEachSystem (system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          default = pkgs.mkShell {
            name = "halmasuit-dev";
            packages = with pkgs; [
              # Rust toolchain — rustup respects rust-toolchain.toml at the repo root.
              rustup

              # Cargo workspace tooling.
              cargo-nextest
              cargo-llvm-cov
              cargo-deny
              cargo-machete
              cargo-mutants
              typos

              # Build / dev tooling.
              just
              nixfmt

              # NixOS VM test substrate (used by `just test-vm` once the test crate lands).
              qemu_kvm

              # General CLI niceties used by recipes.
              jq
              git
            ];

            shellHook = ''
              export CARGO_HOME="$PWD/.cargo-home"
              export RUSTUP_HOME="$PWD/.rustup-home"
              export PATH="$CARGO_HOME/bin:$PATH"
              mkdir -p "$CARGO_HOME" "$RUSTUP_HOME"
            '';
          };
        });

      packages = forEachSystem (system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          # v1 placeholder. The compositor binary builds in v2.
          default = pkgs.runCommand "halmasuit-placeholder" { } ''
            mkdir -p $out
            echo "halmasuit v1: test infrastructure only" > $out/README
          '';
        });

      # NixOS VM tests land here in the next task. `nix flake check` will
      # exercise everything under `checks`.
      checks = forEachSystem (_system: { });
    };
}
