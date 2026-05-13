{
  description = "halmasuit — Linux system compositor.";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

    # nix-config provides the dms-niri module — the same module gnomon uses
    # to bring up greetd + DankGreeter + niri + DMS. We import the module's
    # file path directly and supply the inputs it expects via specialArgs.
    nix-config.url = "github:joshsymonds/nix-config";
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

          # Phase 0 research probe: validates userspace DRM master
          # persistence from rootfs boot through multi-user.target.
          # Built as a Nix package so the NixOS VM test can install it.
          # Not production code — halmasuit-kms is the v2 home for DRM
          # ownership.
          drm-master-probe = pkgs.rustPlatform.buildRustPackage {
            pname   = "drm-master-probe";
            version = "0.1.0";
            src     = ./.;
            cargoLock.lockFile = ./Cargo.lock;
            cargoBuildFlags    = [ "-p" "drm-master-probe" ];
            doCheck = false; # NixOS VM test is the actual test
            meta = {
              description = "Phase 0 research probe — DRM master persistence (halmasuit v2 de-risking)";
              license     = pkgs.lib.licenses.asl20;
            };
          };
        });

      # NixOS VM tests run on Linux only. Limited to x86_64-linux because
      # nixpkgs.testers.runNixOSTest requires a build host matching the test
      # architecture; emitting aarch64-linux checks from an x86_64 evaluator
      # would either invoke qemu-aarch64 user-mode emulation (multi-minute
      # slowdown) or fail outright. Add aarch64-linux here when we have a
      # native runner for it.
      checks.x86_64-linux = {
        smoke-boot = import ./tests/smoke-boot.nix {
          system = "x86_64-linux";
          inherit nixpkgs nix-config;
        };
        login-flash = import ./tests/login-flash.nix {
          system = "x86_64-linux";
          inherit nixpkgs nix-config;
        };
        drm-master-probe = import ./tests/drm-master-probe.nix {
          system = "x86_64-linux";
          inherit nixpkgs;
        };
        drm-master-probe-phase1 = import ./tests/drm-master-probe-phase1.nix {
          system = "x86_64-linux";
          inherit nixpkgs;
        };
      };
    };
}
