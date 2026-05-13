{
  description = "halmasuit — Linux system compositor.";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

    # nix-config provides the dms-niri module — the same module gnomon uses
    # to bring up greetd + DankGreeter + niri + DMS. We import the module's
    # file path directly and supply the inputs it expects via specialArgs.
    nix-config.url = "github:joshsymonds/nix-config";

    # rust-toolchain.toml is the single source of truth for halmasuit's
    # toolchain. rust-overlay reads it so Nix builds compile with the same
    # rustc that rustup uses locally. Without this, nixpkgs's rustc lags
    # the workspace's pinned channel (today: nixpkgs ships 1.94.1, we want
    # 1.95 for edition 2024 + recent stable features).
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    { self
    , nixpkgs
    , nix-config
    , rust-overlay
    }:
    let
      forEachSystem = nixpkgs.lib.genAttrs [
        "x86_64-linux"
        "aarch64-linux"
      ];

      # Construct pkgs with the rust-overlay applied so `pkgs.rust-bin` is
      # available everywhere we build Rust artifacts. Use this in place of
      # `nixpkgs.legacyPackages.${system}` whenever a Rust build is involved.
      pkgsFor = system: import nixpkgs {
        inherit system;
        overlays = [ rust-overlay.overlays.default ];
      };

      # Toolchain derived from rust-toolchain.toml. Single source of truth
      # shared with rustup.
      rustToolchainFor = pkgs:
        pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;

      # rustPlatform with the pinned toolchain. Use this for halmasuit and
      # any other v2 production crate that needs the 1.95 minimum.
      rustPlatformFor = pkgs:
        let toolchain = rustToolchainFor pkgs; in
        pkgs.makeRustPlatform {
          cargo = toolchain;
          rustc = toolchain;
        };
    in
    {
      devShells = forEachSystem (system:
        let
          pkgs = pkgsFor system;
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
          pkgs         = pkgsFor system;
          rustPlatform = rustPlatformFor pkgs;
        in
        {
          # `nix build` with no attribute builds the compositor.
          default = self.packages.${system}.halmasuit;

          # halmasuit compositor binary. Built with the rust-toolchain.toml-pinned
          # toolchain via rust-overlay so the workspace's 1.95 MSRV is satisfied
          # regardless of which rustc nixpkgs currently ships.
          halmasuit = rustPlatform.buildRustPackage {
            pname   = "halmasuit";
            version = "0.1.0";
            src     = ./.;
            cargoLock.lockFile = ./Cargo.lock;
            cargoBuildFlags    = [ "-p" "halmasuit" ];
            # Integration tests spawn the binary and send POSIX signals; the
            # Nix sandbox doesn't permit that cleanly. `just check` is the
            # canonical gate; the NixOS VM test (next task) is the deployment-side gate.
            doCheck = false;
            meta = {
              description = "halmasuit Linux system compositor (v2 Phase A spine)";
              license     = pkgs.lib.licenses.asl20;
              mainProgram = "halmasuit";
            };
          };

          # halmasuit-spawn — setuid-root privilege-drop helper. Shipped
          # as a separate Nix package so the production NixOS module can
          # wrap it with security.wrappers (setuid bit) and the VM test
          # can install + invoke it as a real setuid binary.
          halmasuit-spawn = rustPlatform.buildRustPackage {
            pname   = "halmasuit-spawn";
            version = "0.1.0";
            src     = ./.;
            cargoLock.lockFile = ./Cargo.lock;
            cargoBuildFlags    = [ "-p" "halmasuit-spawn" ];
            doCheck = false; # VM test is the deployment-side gate
            meta = {
              description = "halmasuit setuid-root privilege-drop helper";
              license     = pkgs.lib.licenses.asl20;
              mainProgram = "halmasuit-spawn";
            };
          };

          # Phase 0 research probe: validates userspace DRM master
          # persistence from rootfs boot through multi-user.target.
          # Built as a Nix package so the NixOS VM test can install it.
          # Not production code — halmasuit-kms is the v2 home for DRM
          # ownership. Builds under the same pinned toolchain as halmasuit;
          # the probe's Cargo.toml MSRV (1.87) is a code-level claim, not
          # a build requirement.
          drm-master-probe = rustPlatform.buildRustPackage {
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

      # NixOS modules halmasuit exports. Consumers (a user's nix-config, the
      # gnomon host config, VM tests) import these.
      nixosModules.halmasuit = ./nix/module.nix;

      # Overlay exposing halmasuit-related packages under their bare names so
      # the NixOS module's default = pkgs.halmasuit resolves. Consumers apply
      # this once (`nixpkgs.overlays = [ halmasuit.overlays.default ];`) and
      # then services.halmasuit.enable = true works without further wiring.
      overlays.default = final: _prev: {
        halmasuit = self.packages.${final.stdenv.hostPlatform.system}.halmasuit;
      };

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
        halmasuit-introspect = import ./tests/halmasuit-introspect.nix {
          system    = "x86_64-linux";
          inherit nixpkgs;
          halmasuit = self.packages.x86_64-linux.halmasuit;
        };
        halmasuit-spawn = import ./tests/halmasuit-spawn.nix {
          system          = "x86_64-linux";
          inherit nixpkgs;
          halmasuit-spawn = self.packages.x86_64-linux.halmasuit-spawn;
        };
        drm-master-probe = import ./tests/drm-master-probe.nix {
          system = "x86_64-linux";
          inherit nixpkgs;
        };
        drm-master-probe-phase1 = import ./tests/drm-master-probe-phase1.nix {
          system = "x86_64-linux";
          inherit nixpkgs;
        };
        drm-master-probe-phase2 = import ./tests/drm-master-probe-phase2.nix {
          system = "x86_64-linux";
          inherit nixpkgs;
        };
        drm-master-probe-phase3 = import ./tests/drm-master-probe-phase3.nix {
          system = "x86_64-linux";
          inherit nixpkgs;
        };
      };
    };
}
