{
  description = "halmasuit — Linux system compositor.";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

    # nix-config provides the dms-niri module — the same module gnomon uses
    # to bring up greetd + DankGreeter + niri + DMS. We import the module's
    # file path directly and supply the inputs it expects via specialArgs.
    # Pinned to `main`: layer G proves halmasuit hosts the user's ACTUAL
    # forked stack (not upstream) — epic req 18, decided 2026-05-15. The
    # user's DMS/niri integration work lives on `josh/integration` branches
    # of the *DMS and niri repos*, consumed transitively via nix-config's
    # own inputs (niri-flake / the joshsymonds/niri-quality-of-life niri
    # branch ref) — not a nix-config branch.
    nix-config.url = "github:joshsymonds/nix-config/main";

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
      # rust-std added; a `pkgsCross.*` cross stdenv (see `crossPkgs`
      # below) supplies the musl/static host stdenv that
      # buildRustPackage's build+install hooks key off. (pkgsStatic
      # was tried and rejected — it conflates build/host; see the
      # `crossPkgs` comment.)
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
            # handling; `backend_gbm` + `renderer_gl` pull in libgbm
            # (Mesa) at link time. libEGL is dynamically loaded at runtime
            # via libloading; not a build-time link dep. libdrm comes
            # transitively via drm-rs / gbm-sys.
            # libpam is for halmasuit-pam's FFI (pam-sys links against
            # libpam.so.0 via `links = "pam"` in its Cargo.toml).
            buildInputs = with pkgs; [
              libxkbcommon
              wayland
              pam
              libgbm
              libGL
              # libseat.pc / libseat.so for drm-master-probe's `phase4`
              # feature (libseat-sys). `just check` runs clippy
              # `--all-features`, which enables phase4, so the devShell
              # needs libseat at build time. Provided by seatd.
              seatd
              # libinput.pc for smithay's backend_libinput (input-sys)
              # under the same feature.
              libinput
            ];

            # bindgen (used transitively by pam-sys at build time) needs
            # libclang.so available; LIBCLANG_PATH points it at the right
            # one. Without this, `cargo build` panics inside clang-sys's
            # build script when it can't find libclang.
            #
            # pkg-config is needed by smithay-client-toolkit (and
            # transitively xkbcommon-sys) at build time to find
            # libxkbcommon's pkg-config metadata. halmasuit's
            # buildRustPackage derivation already has it via its own
            # nativeBuildInputs; the devShell needs it explicitly for
            # `cargo build` in the worktree.
            nativeBuildInputs = [ pkgs.llvmPackages.libclang pkgs.pkg-config ];

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
            # Runtime + link deps:
            # - libxkbcommon: smithay needs it for keymap handling.
            # - wayland: smithay's protocol scanner.
            # - pam: pam-sys links against libpam.so.0 at runtime via
            #   `links = "pam"` in its Cargo.toml.
            # - libgbm: smithay's `backend_gbm` + `renderer_gl` link
            #   against libgbm.so via gbm-sys at build time.
            # - libGL (libglvnd): provides `libEGL.so.1` which smithay
            #   dlopens at runtime via libloading. Adding it to
            #   buildInputs ensures `rustPlatform.buildRustPackage`
            #   sets RPATH so the dlopen succeeds without relying on
            #   LD_LIBRARY_PATH propagation.
            # - seatd: libseat-sys links libseat (smithay
            #   backend_session_libseat) — seatd brokers DRM/input
            #   fds; halmasuit no longer self-SET_MASTERs (epic E /
            #   drm-master-probe Phase 4).
            # - libinput + libxkbcommon: input-sys links them
            #   (smithay backend_libinput/backend_udev; input-sys
            #   hardcodes -lxkbcommon). udev: libudev for seat-scoped
            #   device discovery.
            buildInputs       = [
              pkgs.libxkbcommon
              pkgs.wayland
              pkgs.pam
              pkgs.libgbm
              pkgs.libGL
              pkgs.seatd
              pkgs.libinput
              pkgs.udev
            ];
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
            # smithay dlopens libEGL.so.1 (and libGL.so.1) at runtime via
            # `libloading`. Because nothing in halmasuit's link graph
            # actually references those libs, Nix's linker drops them from
            # RPATH despite libGL being in buildInputs. Add libGL's lib
            # dir to RPATH explicitly with patchelf so the runtime dlopen
            # succeeds without LD_LIBRARY_PATH propagation.
            postFixup = ''
              patchelf --add-rpath "${pkgs.libGL}/lib" $out/bin/halmasuit
            '';
            meta = {
              description = "halmasuit Linux system compositor (v2 Phase A spine)";
              license     = pkgs.lib.licenses.asl20;
              mainProgram = "halmasuit";
            };
          };

          # halmasuit-debug — halmasuit built with the `frame_audit`
          # Cargo feature: per-frame GPU readback + `analyze()` +
          # `Event::FrameRendered` emission (and, next task, the
          # `Snapshot()` D-Bus method). Visual VM tests consume THIS;
          # the production `halmasuit` package has none of it (Epic #1
          # req 7/14). Same derivation as `halmasuit` (all the
          # EGL/pam/RPATH wiring is inherited) plus the feature flag;
          # the binary is still named `halmasuit`, so the postFixup
          # patchelf target and the NixOS module's ExecStart are
          # unchanged.
          halmasuit-debug = self.packages.${system}.halmasuit.overrideAttrs (old: {
            pname = "halmasuit-debug";
            cargoBuildFlags = old.cargoBuildFlags ++ [ "--features" "frame_audit" ];
          });

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

          # halmasuit-session — the socket-activated privileged
          # PAM-lifecycle broker (Epic #1 R6). Links pam-sys (the SOLE
          # libpam surface, R14) so it needs the same bindgen + libpam
          # wiring as the testdriver, but none of smithay's
          # wayland/GL/seatd stack.
          halmasuit-session = rustPlatform.buildRustPackage {
            pname   = "halmasuit-session";
            version = "0.1.0";
            src     = ./.;
            cargoLock = {
              lockFile = ./Cargo.lock;
              allowBuiltinFetchGit = true;
            };
            cargoBuildFlags   = [ "-p" "halmasuit-session" "--bin" "halmasuit-session" ];
            nativeBuildInputs = [ pkgs.pkg-config pkgs.llvmPackages.libclang ];
            buildInputs       = [ pkgs.pam ];
            env = {
              LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
              BINDGEN_EXTRA_CLANG_ARGS =
                "-I${pkgs.pam}/include -I${pkgs.glibc.dev}/include";
            };
            doCheck = false;
            meta = {
              description = "halmasuit socket-activated privileged PAM-lifecycle broker";
              license     = pkgs.lib.licenses.asl20;
              mainProgram = "halmasuit-session";
            };
          };

          # halmasuit-session-pam-testdriver — test-only driver for the
          # real-PAM gate (Epic #1 R12). Links pam-sys (via
          # halmasuit-session) so it needs the same bindgen + libpam
          # build wiring as `halmasuit` (clang for pam-sys's build.rs,
          # pam headers, libpam.so.0 to link) — but NOT smithay's
          # wayland/GL/seatd stack (halmasuit-session has none of it).
          halmasuit-session-pam-testdriver = rustPlatform.buildRustPackage {
            pname   = "halmasuit-session-pam-testdriver";
            version = "0.1.0";
            src     = ./.;
            cargoLock = {
              lockFile = ./Cargo.lock;
              allowBuiltinFetchGit = true;
            };
            cargoBuildFlags   = [ "-p" "halmasuit-session-pam-testdriver" ];
            nativeBuildInputs = [ pkgs.pkg-config pkgs.llvmPackages.libclang ];
            buildInputs       = [ pkgs.pam ];
            env = {
              LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
              BINDGEN_EXTRA_CLANG_ARGS =
                "-I${pkgs.pam}/include -I${pkgs.glibc.dev}/include";
            };
            doCheck = false;
            meta = {
              description = "halmasuit-session real-PAM VM-gate driver (test-only)";
              license     = pkgs.lib.licenses.asl20;
              mainProgram = "halmasuit-session-pam-testdriver";
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
            env = {
              "CARGO_TARGET_${pkgs.lib.toUpper (builtins.replaceStrings [ "-" ] [ "_" ] (muslRustTargetFor system))}_RUSTFLAGS" =
                "-C target-feature=+crt-static";
            };
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

          # drm-master-probe-phase4 — the SAME probe built with the
          # `phase4` cargo feature (libseat/seatd survival across
          # setresuid). Separate package so the phase-0–3 tests keep
          # the lean DRM-only closure (no smithay/libseat/libinput);
          # mirrors the halmasuit/halmasuit-debug split. libseat-sys /
          # input-sys link libseat/libinput via pkg-config.
          drm-master-probe-phase4 = rustPlatform.buildRustPackage {
            pname   = "drm-master-probe-phase4";
            version = "0.1.0";
            src     = ./.;
            cargoLock = {
              lockFile = ./Cargo.lock;
              allowBuiltinFetchGit = true;
            };
            cargoBuildFlags   = [ "-p" "drm-master-probe" "--features" "phase4" ];
            nativeBuildInputs = [ pkgs.pkg-config ];
            # libseat (seatd), libudev (udev), libinput, and
            # libxkbcommon (input-sys links it). Mirrors the devShell
            # set that links `--features phase4` cleanly.
            buildInputs       = [
              pkgs.seatd
              pkgs.libinput
              pkgs.libxkbcommon
              pkgs.udev
              pkgs.libgbm
            ];
            doCheck = false; # the NixOS VM test (drm-master-probe-phase4) is the test
            meta = {
              description = "Phase 4 research probe — libseat/seatd session survival across setresuid";
              license     = pkgs.lib.licenses.asl20;
            };
          };

          # halmasuit-layer-shell-test-client — throwaway sctk-based
          # wl_client that binds wlr-layer-shell BACKGROUND, paints a
          # known solid color via wl_shm, holds. Used by
          # tests/visual-halmasuit-layer.nix to verify halmasuit
          # composites layer-shell clients. Retired once halmasuit-splash
          # exists (B.4).
          halmasuit-layer-shell-test-client = rustPlatform.buildRustPackage {
            pname   = "halmasuit-layer-shell-test-client";
            version = "0.1.0";
            src     = ./.;
            cargoLock = {
              lockFile = ./Cargo.lock;
              allowBuiltinFetchGit = true;
            };
            cargoBuildFlags = [ "-p" "halmasuit-layer-shell-test-client" ];
            nativeBuildInputs = [ pkgs.pkg-config ];
            buildInputs       = [ pkgs.libxkbcommon pkgs.wayland ];
            doCheck = false;
            meta = {
              description = "Layer-shell test client for halmasuit B.3 visual gate";
              license     = pkgs.lib.licenses.asl20;
              mainProgram = "halmasuit-layer-shell-test-client";
            };
          };

          # halmasuit-toplevel-test-client — throwaway sctk xdg_toplevel
          # client (fullscreen solid colour) exercising halmasuit's F1
          # xdg-shell compositing path.
          halmasuit-toplevel-test-client = rustPlatform.buildRustPackage {
            pname   = "halmasuit-toplevel-test-client";
            version = "0.1.0";
            src     = ./.;
            cargoLock = {
              lockFile = ./Cargo.lock;
              allowBuiltinFetchGit = true;
            };
            cargoBuildFlags = [ "-p" "halmasuit-toplevel-test-client" ];
            nativeBuildInputs = [ pkgs.pkg-config ];
            buildInputs       = [ pkgs.libxkbcommon pkgs.wayland ];
            doCheck = false;
            meta = {
              description = "xdg_toplevel test client for halmasuit F1 visual gate";
              license     = pkgs.lib.licenses.asl20;
              mainProgram = "halmasuit-toplevel-test-client";
            };
          };

          # halmasuit-splash — the real system background wl_client.
          # wgpu (GL backend) renders the HALMASUIT_SPLASH_IMAGE PNG
          # fullscreen on a wlr-layer-shell BACKGROUND surface. wgpu
          # and wayland-client(dlopen) dlopen libEGL/libGL/libwayland
          # at runtime; like halmasuit those are dropped from RPATH
          # because nothing link-references them, so re-add them with
          # patchelf (same treatment as the halmasuit package).
          halmasuit-splash = rustPlatform.buildRustPackage {
            pname   = "halmasuit-splash";
            version = "0.1.0";
            src     = ./.;
            cargoLock = {
              lockFile = ./Cargo.lock;
              allowBuiltinFetchGit = true;
            };
            cargoBuildFlags   = [ "-p" "halmasuit-splash" ];
            nativeBuildInputs = [ pkgs.pkg-config ];
            buildInputs       = [
              pkgs.libxkbcommon
              pkgs.wayland
              pkgs.libGL
            ];
            doCheck = false;
            postFixup = ''
              patchelf --add-rpath "${pkgs.libGL}/lib:${pkgs.wayland}/lib" \
                $out/bin/halmasuit-splash
            '';
            meta = {
              description = "halmasuit system background (wgpu PNG layer-shell BACKGROUND client)";
              license     = pkgs.lib.licenses.asl20;
              mainProgram = "halmasuit-splash";
            };
          };

          # ssimulacra2_rs — pure-Rust port of the SSIMULACRA2
          # perceptual image-diff metric. Used by visual VM tests as
          # the golden-comparison engine. Chosen over the C++
          # libjxl-tools ssimulacra2 (nixpkgs build is broken in our
          # pin due to libhwy/gtest C++14 mismatch) and over Kornel's
          # dssim (not in nixpkgs; would need its own custom
          # derivation). buildNoDefaultFeatures = true skips the heavy
          # video-decoder deps; we only compare PNGs.
          #
          # See PLAN.md / epic Task #1 Requirement #9 for the choice
          # rationale.
          ssimulacra2-cli = rustPlatform.buildRustPackage rec {
            pname   = "ssimulacra2_rs";
            version = "0.5.2";
            src     = pkgs.fetchCrate {
              inherit pname version;
              hash = "sha256-p9NERnuLz1FLx/JBsWIEa6ZJg9zno2DIArn96igVBzQ=";
            };
            cargoHash = "sha256-c0rRiLYJSkLoOrOodnSvKWzCfEQz7Yxy2QKfPa5aVfw=";
            buildNoDefaultFeatures = true;
            doCheck = false;
            meta = {
              description = "Pure-Rust ssimulacra2 perceptual image-diff metric (CLI)";
              homepage    = "https://github.com/rust-av/ssimulacra2_bin";
              license     = pkgs.lib.licenses.bsd2;
              mainProgram = "ssimulacra2_rs";
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
        halmasuit         = self.packages.${final.stdenv.hostPlatform.system}.halmasuit;
        halmasuit-spawn   = self.packages.${final.stdenv.hostPlatform.system}.halmasuit-spawn;
        halmasuit-session = self.packages.${final.stdenv.hostPlatform.system}.halmasuit-session;
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
        # Epic #1 R12: first real-PAM gate. run_pam_auth against the
        # real libpam stack with the real test user — NO mock.
        run-pam-auth = import ./tests/run-pam-auth.nix {
          system = "x86_64-linux";
          inherit nixpkgs;
          halmasuit-session-pam-testdriver =
            self.packages.x86_64-linux.halmasuit-session-pam-testdriver;
        };
        # Epic #1 R5/R6 + Amendment A2: the socket-activated broker
        # posture gate (no standing root when idle, on-demand
        # activation, idle-exit + re-activation, evict-old reachable
        # from the event loop) — real pam_unix, NO mock.
        session-r5r6 = import ./tests/session-r5r6.nix {
          system = "x86_64-linux";
          inherit nixpkgs;
          halmasuit-session = self.packages.x86_64-linux.halmasuit-session;
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
        drm-master-probe-phase4 = import ./tests/drm-master-probe-phase4.nix {
          system = "x86_64-linux";
          inherit nixpkgs;
        };
        # Visual gates consume `halmasuit-debug` (frame_audit on): the
        # capture path is the in-process `Snapshot()` D-Bus method,
        # not QMP screendump. Structural tests above stay on the
        # production `halmasuit` package.
        visual-halmasuit-clear = import ./tests/visual-halmasuit-clear.nix {
          system = "x86_64-linux";
          inherit nixpkgs;
          halmasuit       = self.packages.x86_64-linux.halmasuit-debug;
          halmasuit-spawn = self.packages.x86_64-linux.halmasuit-spawn;
          ssimulacra2-cli = self.packages.x86_64-linux.ssimulacra2-cli;
        };
        visual-halmasuit-layer = import ./tests/visual-halmasuit-layer.nix {
          system = "x86_64-linux";
          inherit nixpkgs;
          halmasuit                        = self.packages.x86_64-linux.halmasuit-debug;
          halmasuit-spawn                  = self.packages.x86_64-linux.halmasuit-spawn;
          halmasuit-layer-shell-test-client = self.packages.x86_64-linux.halmasuit-layer-shell-test-client;
          ssimulacra2-cli                  = self.packages.x86_64-linux.ssimulacra2-cli;
        };
        visual-halmasuit-splash = import ./tests/visual-halmasuit-splash.nix {
          system = "x86_64-linux";
          inherit nixpkgs;
          halmasuit        = self.packages.x86_64-linux.halmasuit-debug;
          halmasuit-spawn  = self.packages.x86_64-linux.halmasuit-spawn;
          halmasuit-splash = self.packages.x86_64-linux.halmasuit-splash;
          ssimulacra2-cli  = self.packages.x86_64-linux.ssimulacra2-cli;
        };
        visual-backdrop = import ./tests/visual-backdrop.nix {
          system = "x86_64-linux";
          inherit nixpkgs;
          halmasuit                         = self.packages.x86_64-linux.halmasuit-debug;
          halmasuit-spawn                   = self.packages.x86_64-linux.halmasuit-spawn;
          halmasuit-splash                  = self.packages.x86_64-linux.halmasuit-splash;
          halmasuit-layer-shell-test-client = self.packages.x86_64-linux.halmasuit-layer-shell-test-client;
          ssimulacra2-cli                   = self.packages.x86_64-linux.ssimulacra2-cli;
        };
        # Epic layer F1: real xdg_toplevel composited fullscreen
        # over the splash background.
        visual-halmasuit-toplevel = import ./tests/visual-halmasuit-toplevel.nix {
          system = "x86_64-linux";
          inherit nixpkgs;
          halmasuit                      = self.packages.x86_64-linux.halmasuit-debug;
          halmasuit-spawn                = self.packages.x86_64-linux.halmasuit-spawn;
          halmasuit-splash               = self.packages.x86_64-linux.halmasuit-splash;
          halmasuit-toplevel-test-client = self.packages.x86_64-linux.halmasuit-toplevel-test-client;
          ssimulacra2-cli                = self.packages.x86_64-linux.ssimulacra2-cli;
        };
        # Epic layer F2: greetd-driven greeter→session foreground
        # swap; no-flash continuity across the REAL transition.
        visual-foreground = import ./tests/visual-foreground.nix {
          system = "x86_64-linux";
          inherit nixpkgs;
          halmasuit                         = self.packages.x86_64-linux.halmasuit-debug;
          halmasuit-spawn                   = self.packages.x86_64-linux.halmasuit-spawn;
          halmasuit-splash                  = self.packages.x86_64-linux.halmasuit-splash;
          halmasuit-layer-shell-test-client = self.packages.x86_64-linux.halmasuit-layer-shell-test-client;
          halmasuit-toplevel-test-client    = self.packages.x86_64-linux.halmasuit-toplevel-test-client;
          halmasuit-vm-client               = self.packages.x86_64-linux.halmasuit-vm-client;
          ssimulacra2-cli                   = self.packages.x86_64-linux.ssimulacra2-cli;
        };
        # Epic layer E2: real keystroke → libinput → wl_seat →
        # focused client. Production halmasuit (input is core).
        halmasuit-input = import ./tests/halmasuit-input.nix {
          system = "x86_64-linux";
          inherit nixpkgs;
          halmasuit                         = self.packages.x86_64-linux.halmasuit;
          halmasuit-spawn                   = self.packages.x86_64-linux.halmasuit-spawn;
          halmasuit-layer-shell-test-client = self.packages.x86_64-linux.halmasuit-layer-shell-test-client;
        };
      };
    };
}
