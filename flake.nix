{
  description = "halmasuit — Linux system compositor.";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

    # nix-config provides the dms-niri module — the same module gnomon uses to
    # bring up greetd + DankGreeter + niri + DMS. We import the module's file
    # path directly and supply the inputs it expects via _module.args.inputs.
    #
    # `git+file://` (not `path:`) per the user's memory rule on local flake
    # inputs. Switch to `github:joshsymonds/nix-config` once pushed.
    nix-config.url = "git+file:///home/joshsymonds/nix-config";
  };

  outputs =
    { self
    , nixpkgs
    , nix-config
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

      # NixOS VM tests. `nix flake check` exercises everything here.
      checks = forEachSystem (system: {
        smoke-boot = import ./tests/smoke-boot.nix {
          inherit system nixpkgs nix-config;
        };
      });
    };
}
