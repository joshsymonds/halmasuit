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

      # Rust target triple for the musl-static build of
      # halmasuit-spawn. The setuid-root helper MUST be statically
      # linked: a dynamic glibc binary dlopens NSS modules *inside the
      # privileged process* before the privilege drop (initgroups(3) →
      # nss). musl resolves /etc/group itself with no dlopen, so the
      # privileged window touches no plugin code. This is the F3 /
      # ARCHITECTURE.md "statically linked" hardening invariant; the
      # build enforces it instead of merely asserting it in a comment.
      muslRustTargetFor = system: {
        "x86_64-linux"  = "x86_64-unknown-linux-musl";
        "aarch64-linux" = "aarch64-unknown-linux-musl";
      }.${system};

      # rustPlatform whose stdenv targets static musl, for
      # halmasuit-spawn only. The pinned rust-overlay toolchain is
      # reused (single source of truth with rustup) with the musl
      # rust-std added; pkgsStatic supplies the musl/static stdenv that
      # buildRustPackage's build+install hooks key off.
      rustPlatformStaticFor = system:
        let
          pkgs      = pkgsFor system;
          toolchain = (rustToolchainFor pkgs).override {
            targets = [ (muslRustTargetFor system) ];
          };
          # A real cross stdenv: buildPlatform stays gnu (so cargo
          # build scripts / proc-macros compile and *run* natively on
          # the builder) while hostPlatform is musl (so the shipped
          # binary links static musl). pkgsStatic conflates the two and
          # breaks the external toolchain's build scripts.
          crossPkgs = {
            "x86_64-linux"  = pkgs.pkgsCross.musl64;
            "aarch64-linux" = pkgs.pkgsCross.aarch64-multiplatform-musl;
          }.${system};
        in
        crossPkgs.makeRustPlatform {
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

            # Native libraries smithay links against. smithay's
            # `wayland_frontend` minimum pulls in libxkbcommon for keymap
            # handling; later features (`backend_libinput`, `backend_drm`,
            # `renderer_gl`) will add libinput, libgbm, libegl, libdrm.
            # libpam is for halmasuit-pam's FFI (pam-sys links against
            # libpam.so.0 via `links = "pam"` in its Cargo.toml).
            buildInputs = with pkgs; [
              libxkbcommon
              wayland
              pam
            ];

            # bindgen (used transitively by pam-sys at build time) needs
            # libclang.so available; LIBCLANG_PATH points it at the right
            # one. Without this, `cargo build` panics inside clang-sys's
            # build script when it can't find libclang.
            nativeBuildInputs = [ pkgs.llvmPackages.libclang ];

            shellHook = ''
              export CARGO_HOME="$PWD/.cargo-home"
              export RUSTUP_HOME="$PWD/.rustup-home"
              export PATH="$CARGO_HOME/bin:$PATH"
              export LIBCLANG_PATH="${pkgs.llvmPackages.libclang.lib}/lib"
              # bindgen invokes clang directly, bypassing NIX_CFLAGS_COMPILE.
              # Point it at PAM + glibc headers so pam-sys's build.rs
              # finds <security/pam_appl.h> and its transitive <unistd.h>.
              export BINDGEN_EXTRA_CLANG_ARGS="-I${pkgs.pam}/include -I${pkgs.glibc.dev}/include"
              mkdir -p "$CARGO_HOME" "$RUSTUP_HOME"
            '';
          };
        });

      packages = forEachSystem (system:
        let
          pkgs               = pkgsFor system;
          rustPlatform       = rustPlatformFor pkgs;
          rustPlatformStatic = rustPlatformStaticFor system;
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
            # smithay is pinned to a git rev. allowBuiltinFetchGit lets
            # nixpkgs's rustPlatform clone the rev at build time via
            # builtins.fetchGit rather than requiring a pre-computed
            # outputHashes entry. Acceptable for a project that already
            # uses a git smithay pin throughout.
            cargoLock = {
              lockFile = ./Cargo.lock;
              allowBuiltinFetchGit = true;
            };
            cargoBuildFlags    = [ "-p" "halmasuit" ];
            # Native deps:
            # - pkg-config: smithay's build script probes for libwayland.
            # - llvmPackages.libclang: bindgen (transitively via pam-sys)
            #   runs clang at build time. The dev shell exports this via
            #   shellHook; rustPlatform.buildRustPackage has its own
            #   sandboxed env, so we duplicate the wiring here.
            nativeBuildInputs = [ pkgs.pkg-config pkgs.llvmPackages.libclang ];
            # Runtime deps:
            # - libxkbcommon: smithay needs it for keymap handling.
            # - wayland: smithay's protocol scanner.
            # - pam: pam-sys links against libpam.so.0 at runtime via
            #   `links = "pam"` in its Cargo.toml.
            buildInputs       = [ pkgs.libxkbcommon pkgs.wayland pkgs.pam ];
            # bindgen invokes clang directly (bypassing NIX_CFLAGS_COMPILE).
            # Mirror the shellHook so pam-sys's build.rs finds
            # <security/pam_appl.h> and its transitive <unistd.h>.
            env = {
              LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
              BINDGEN_EXTRA_CLANG_ARGS =
                "-I${pkgs.pam}/include -I${pkgs.glibc.dev}/include";
            };
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

          # halmasuit-vm-client — tiny greetd-protocol test client.
          # Shipped as a separate Nix package so VM tests can install
          # it via environment.systemPackages and drive halmasuit's
          # greetd socket from the testScript.
          halmasuit-vm-client = rustPlatform.buildRustPackage {
            pname   = "halmasuit-vm-client";
            version = "0.1.0";
            src     = ./.;
            cargoLock = {
              lockFile = ./Cargo.lock;
              allowBuiltinFetchGit = true;
            };
            cargoBuildFlags = [ "-p" "halmasuit-vm-client" ];
            doCheck = false;
            meta = {
              description = "halmasuit greetd-protocol test client";
              license     = pkgs.lib.licenses.asl20;
              mainProgram = "halmasuit-vm-client";
            };
          };

          # halmasuit-spawn — setuid-root privilege-drop helper. Shipped
          # as a separate Nix package so the production NixOS module can
          # wrap it with security.wrappers (setuid bit) and the VM test
          # can install + invoke it as a real setuid binary.
          halmasuit-spawn = rustPlatformStatic.buildRustPackage {
            pname   = "halmasuit-spawn";
            version = "0.1.0";
            src     = ./.;
            cargoLock = {
              lockFile = ./Cargo.lock;
              allowBuiltinFetchGit = true;
            };
            cargoBuildFlags    = [ "-p" "halmasuit-spawn" ];
            doCheck = false; # VM test is the deployment-side gate
            # crt-static is already implied by the *-musl target, but
            # set it explicitly so a future stdenv/toolchain change
            # can't silently produce a dynamically-linked setuid helper.
            # Scope it to the musl *host* target only — a global
            # RUSTFLAGS would also static-PIE the gnu *build-platform*
            # build scripts (libc/nix), which then SIGSEGV on the
            # builder.
            "CARGO_TARGET_${pkgs.lib.toUpper (builtins.replaceStrings [ "-" ] [ "_" ] (muslRustTargetFor system))}_RUSTFLAGS" =
              "-C target-feature=+crt-static";
            # Build-enforced hardening invariant: a statically linked
            # ELF has no PT_INTERP program header. If one appears, the
            # binary would dlopen NSS inside the privileged process —
            # fail the build rather than ship the regression. This is
            # what makes "statically linked" a gate, not a comment (F3).
            postInstall = ''
              if "$READELF" -l "$out/bin/halmasuit-spawn" | grep -qw INTERP; then
                echo "halmasuit-spawn: PT_INTERP present — not statically linked." >&2
                echo "The setuid-root helper must be static (F3 / ARCHITECTURE.md)." >&2
                exit 1
              fi
            '';
            meta = {
              description = "halmasuit setuid-root privilege-drop helper (static musl)";
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
            cargoLock = {
              lockFile = ./Cargo.lock;
              allowBuiltinFetchGit = true;
            };
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
      # the NixOS module's `default = pkgs.halmasuit` / `default =
      # pkgs.halmasuit-spawn` resolutions work. Consumers apply this once
      # (`nixpkgs.overlays = [ halmasuit.overlays.default ];`) and then
      # `services.halmasuit.enable = true` works without further wiring.
      overlays.default = final: _prev: {
        halmasuit       = self.packages.${final.stdenv.hostPlatform.system}.halmasuit;
        halmasuit-spawn = self.packages.${final.stdenv.hostPlatform.system}.halmasuit-spawn;
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
          system               = "x86_64-linux";
          inherit nixpkgs;
          halmasuit            = self.packages.x86_64-linux.halmasuit;
          halmasuit-spawn      = self.packages.x86_64-linux.halmasuit-spawn;
        };
        halmasuit-vm = import ./tests/halmasuit-vm.nix {
          system    = "x86_64-linux";
          inherit nixpkgs;
          halmasuit            = self.packages.x86_64-linux.halmasuit;
          halmasuit-spawn      = self.packages.x86_64-linux.halmasuit-spawn;
          halmasuit-vm-client  = self.packages.x86_64-linux.halmasuit-vm-client;
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
