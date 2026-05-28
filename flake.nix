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
            # libpam is for the halmasuit-session broker's FFI — the
            # SOLE libpam surface in the workspace (Epic #1 R14; Epic #5
            # replaced the pam-sys dep with a hand-rolled `unsafe extern
            # "C"` block in `halmasuit_session::pam_sys` that links
            # libpam.so.0 directly via `#[link(name = "pam")]`).
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
              # FFmpeg headers + libs for the halmasuit-decoder
              # subsystem (Epic #12). rsmpeg's link_system_ffmpeg
              # feature probes pkg-config for libavformat /
              # libavcodec / libavutil / libswscale at build time.
              # Without ffmpeg-headless here, devShell `cargo check`
              # of anything depending on rsmpeg fails at the
              # pkg-config probe. Production halmasuit-decoder has
              # its own derivation below with this in buildInputs.
              ffmpeg-headless
            ];

            # libclang is required ONLY by the dev-deps-only pam-sys
            # parity audit lever (`crates/halmasuit-session/tests/pam_ffi_parity.rs`,
            # Epic #5): pam-sys lives in [dev-dependencies] of
            # halmasuit-session and runs bindgen at build time when
            # compiled by `cargo test`. Production builds use the
            # hand-rolled `halmasuit_session::pam_sys` FFI module — they
            # link `-lpam` directly with zero bindgen / clang-sys /
            # libclang involvement (and the production Nix packages
            # below DO NOT include libclang in their nativeBuildInputs).
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
              # libclang + bindgen wiring for the pam-sys parity test
              # (dev-deps-only). bindgen invokes clang directly,
              # bypassing NIX_CFLAGS_COMPILE; point it at PAM + glibc
              # headers so pam-sys's build.rs finds
              # <security/pam_appl.h> and its transitive <unistd.h>.
              export LIBCLANG_PATH="${pkgs.llvmPackages.libclang.lib}/lib"
              export BINDGEN_EXTRA_CLANG_ARGS="-I${pkgs.pam}/include -I${pkgs.glibc.dev}/include"
              mkdir -p "$CARGO_HOME" "$RUSTUP_HOME"
            '';
          };
        });

      packages = forEachSystem (system:
        let
          pkgs               = pkgsFor system;
          rustPlatform       = rustPlatformFor pkgs;
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
            #
            # libclang is NOT in this list: the compositor binary has
            # no bindgen consumer in its dep graph (verified via
            # `cargo tree -p halmasuit --edges normal,build`). The
            # privileged broker's libpam FFI is hand-rolled in
            # `halmasuit_session::pam_sys` and the compositor never
            # links libpam at all (CLAUDE.md: "No PAM in the
            # compositor's address space").
            nativeBuildInputs = [ pkgs.pkg-config ];
            # Runtime + link deps:
            # - libxkbcommon: smithay needs it for keymap handling.
            # - wayland: smithay's protocol scanner.
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
            #
            # NOT included: libpam. The compositor has no PAM in its
            # address space (CLAUDE.md hard rule); the privileged
            # broker (`halmasuit-session`) is the sole libpam consumer
            # and has its own derivation below with `pkgs.pam` in its
            # buildInputs. `cargo tree -p halmasuit | grep -i pam` is
            # empty.
            buildInputs       = [
              pkgs.libxkbcommon
              pkgs.wayland
              pkgs.libgbm
              pkgs.libGL
              pkgs.seatd
              pkgs.libinput
              pkgs.udev
            ];
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
          # halmasuit-vt-test-client — Epic #71 R1.4 VM-test client
          # for the broker↔compositor VT-switching IPC dance. Built
          # with the pinned `rustPlatform` (1.95) because it depends
          # on `halmasuit-session-ipc` which uses the workspace MSRV.
          # Same pattern as `halmasuit-vm-client` (the greetd VM
          # client) — both are test-only binaries staged here so
          # tests/*.nix consume a prebuilt package, not their own
          # rustPlatform derivation (nixpkgs's default rustc is below
          # the workspace MSRV).
          halmasuit-vt-test-client = rustPlatform.buildRustPackage {
            pname   = "halmasuit-vt-test-client";
            version = "0.1.0";
            src     = ./.;
            cargoLock = {
              lockFile = ./Cargo.lock;
              allowBuiltinFetchGit = true;
            };
            cargoBuildFlags = [ "-p" "halmasuit-vt-test-client" ];
            doCheck = false;
            meta = {
              description = "halmasuit Epic #71 R1.4 VT-switching VM test client";
              license     = pkgs.lib.licenses.asl20;
              mainProgram = "halmasuit-vt-test-client";
            };
          };

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

          # halmasuit-luks — Phase B systemd password-agent Wayland
          # client. Spawned by initramfs systemd alongside halmasuit
          # in the `services.halmasuit.fromInitrd.enable` deployment;
          # watches /run/systemd/ask-password/ for LUKS unlock
          # requests, prompts the user via a fullscreen xdg_toplevel
          # over halmasuit's wayland socket, writes responses to the
          # agent socket. Replaceable by any other implementation of
          # the systemd password-agent protocol.
          #
          # nativeBuildInputs/buildInputs: smithay-client-toolkit's
          # xkbcommon-sys build script probes pkg-config for
          # libxkbcommon (same as halmasuit's own build).
          halmasuit-luks = rustPlatform.buildRustPackage {
            pname   = "halmasuit-luks";
            version = "0.1.0";
            src     = ./.;
            cargoLock = {
              lockFile = ./Cargo.lock;
              allowBuiltinFetchGit = true;
            };
            cargoBuildFlags   = [ "-p" "halmasuit-luks" ];
            nativeBuildInputs = [ pkgs.pkg-config ];
            buildInputs       = [ pkgs.libxkbcommon ];
            doCheck = false;
            meta = {
              description = "Phase B systemd password-agent Wayland client for halmasuit";
              license     = pkgs.lib.licenses.asl20;
              mainProgram = "halmasuit-luks";
            };
          };

          # halmasuit-session — the socket-activated privileged
          # PAM-lifecycle broker (Epic #1 R6). The sole libpam-linking
          # crate in the workspace (R14), now via the hand-rolled
          # `halmasuit_session::pam_sys` FFI module (Epic #5). Production
          # links `-lpam` directly — no bindgen, no clang-sys, no
          # libclang at build time. pam-sys is a [dev-dependencies]
          # audit lever for the parity test only (`doCheck = false`
          # below means this derivation never sees dev-deps).
          halmasuit-session = rustPlatform.buildRustPackage {
            pname   = "halmasuit-session";
            version = "0.1.0";
            src     = ./.;
            cargoLock = {
              lockFile = ./Cargo.lock;
              allowBuiltinFetchGit = true;
            };
            cargoBuildFlags   = [ "-p" "halmasuit-session" "--bin" "halmasuit-session" ];
            nativeBuildInputs = [ pkgs.pkg-config ];
            buildInputs       = [ pkgs.pam ];
            doCheck = false;
            meta = {
              description = "halmasuit socket-activated privileged PAM-lifecycle broker";
              license     = pkgs.lib.licenses.asl20;
              mainProgram = "halmasuit-session";
            };
          };

          # halmasuit-decoder — sandboxed video-decoder subprocess
          # (Epic #12). Forked by halmasuit at runtime via
          # DecoderRelay; lives in a private user/net/mount namespace
          # under PR_SET_NO_NEW_PRIVS + rlimits. Links FFmpeg (LGPL,
          # dynamic) via rsmpeg's link_system_ffmpeg feature. NOT
          # --enable-gpl; h264 via stock libavcodec, AV1 via libdav1d.
          #
          # Bindgen-free production (Epic #12 task #28 / Epic #5
          # commitment): the rsmpeg → rusty_ffmpeg build.rs uses the
          # checked-in `ffmpeg_binding.rs` via `FFMPEG_BINDING_PATH`
          # instead of running bindgen at build time. libclang is
          # therefore NOT in this derivation's nativeBuildInputs —
          # matches the anti-pattern "NO libclang in nativeBuildInputs
          # (production must stay bindgen-free)" and Epic #5's
          # commitment to a libclang-free production closure.
          #
          # Regenerating ffmpeg_binding.rs (when ffmpeg-headless pins
          # bump):  just regenerate-decoder-bindings  (runs cargo
          # build inside the devShell with libclang, captures the
          # generated binding.rs from target/, copies it back). The
          # file lives in crates/halmasuit-decoder/ffmpeg_binding.rs
          # and is checked in like a generated lockfile.
          halmasuit-decoder = rustPlatform.buildRustPackage {
            pname   = "halmasuit-decoder";
            version = "0.1.0";
            src     = ./.;
            cargoLock = {
              lockFile = ./Cargo.lock;
              allowBuiltinFetchGit = true;
            };
            cargoBuildFlags   = [ "-p" "halmasuit-decoder" ];
            nativeBuildInputs = [ pkgs.pkg-config ];
            buildInputs       = [ pkgs.ffmpeg-headless ];
            env = {
              # Use the checked-in pre-generated bindings; this makes
              # rusty_ffmpeg's build.rs skip its bindgen invocation
              # entirely (no libclang needed at build time).
              FFMPEG_BINDING_PATH = "${./crates/halmasuit-decoder/ffmpeg_binding.rs}";
            };
            doCheck = false;
            meta = {
              description = "halmasuit sandboxed video-decoder subprocess";
              license     = pkgs.lib.licenses.asl20;
              mainProgram = "halmasuit-decoder";
            };
          };

          # halmasuit-session-pam-testdriver — test-only driver for the
          # real-PAM gate (Epic #1 R12). Reaches libpam via
          # halmasuit-session's hand-rolled FFI (Epic #5); no bindgen
          # at build time, no libclang.
          halmasuit-session-pam-testdriver = rustPlatform.buildRustPackage {
            pname   = "halmasuit-session-pam-testdriver";
            version = "0.1.0";
            src     = ./.;
            cargoLock = {
              lockFile = ./Cargo.lock;
              allowBuiltinFetchGit = true;
            };
            cargoBuildFlags   = [ "-p" "halmasuit-session-pam-testdriver" ];
            nativeBuildInputs = [ pkgs.pkg-config ];
            buildInputs       = [ pkgs.pam ];
            doCheck = false;
            meta = {
              description = "halmasuit-session real-PAM VM-gate driver (test-only)";
              license     = pkgs.lib.licenses.asl20;
              mainProgram = "halmasuit-session-pam-testdriver";
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

          # halmasuit-shutdown-probe — research probe for Epic #47 R2
          # (wallpaper continuity to kernel halt). Three-phase probe;
          # Phase 0 only landed today. Lean closure (signalfd + libc),
          # no smithay / no DRM. Phase 2 (when it lands) will need a
          # DRM-aware build variant analogous to drm-master-probe-phase4.
          halmasuit-shutdown-probe = rustPlatform.buildRustPackage {
            pname   = "halmasuit-shutdown-probe";
            version = "0.1.0";
            src     = ./.;
            cargoLock = {
              lockFile = ./Cargo.lock;
              allowBuiltinFetchGit = true;
            };
            cargoBuildFlags = [ "-p" "halmasuit-shutdown-probe" ];
            doCheck = false; # NixOS VM test is the actual test
            meta = {
              description = "Phase 0 research probe — SurviveFinalKillSignal=yes on rootfs shutdown kill spree (Epic #47 R2)";
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
          # composites layer-shell clients.
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

          # halmasuit-subsurface-test-client — exercises halmasuit's
          # wl_compositor commit-aggregation contract (R3): an
          # xdg_toplevel parent + a sync wl_subsurface child, driven
          # through a deterministic commit sequence the regression
          # test asserts against.
          halmasuit-subsurface-test-client = rustPlatform.buildRustPackage {
            pname   = "halmasuit-subsurface-test-client";
            version = "0.1.0";
            src     = ./.;
            cargoLock = {
              lockFile = ./Cargo.lock;
              allowBuiltinFetchGit = true;
            };
            cargoBuildFlags = [ "-p" "halmasuit-subsurface-test-client" ];
            nativeBuildInputs = [ pkgs.pkg-config ];
            buildInputs       = [ pkgs.libxkbcommon pkgs.wayland ];
            doCheck = false;
            meta = {
              description = "wl_subsurface sync-semantics test client (R3)";
              license     = pkgs.lib.licenses.asl20;
              mainProgram = "halmasuit-subsurface-test-client";
            };
          };

          # halmasuit-deferred-configure-test-client — observes the
          # protocol-level timing of halmasuit's initial
          # xdg_surface.configure (R4): raw wayland-client (no SCTK
          # Window), drives a deterministic two-phase observation
          # (pre-commit, post-commit) emitting two stderr markers the
          # VM test asserts against.
          halmasuit-deferred-configure-test-client = rustPlatform.buildRustPackage {
            pname   = "halmasuit-deferred-configure-test-client";
            version = "0.1.0";
            src     = ./.;
            cargoLock = {
              lockFile = ./Cargo.lock;
              allowBuiltinFetchGit = true;
            };
            cargoBuildFlags = [ "-p" "halmasuit-deferred-configure-test-client" ];
            nativeBuildInputs = [ pkgs.pkg-config ];
            buildInputs       = [ pkgs.wayland ];
            doCheck = false;
            meta = {
              description = "xdg-shell deferred-configure timing observer (R4)";
              license     = pkgs.lib.licenses.asl20;
              mainProgram = "halmasuit-deferred-configure-test-client";
            };
          };

          # halmasuit-popup-test-client — observes the geometry the
          # compositor forwards on xdg_popup.configure (R5
          # PopupManager-driven positioner pipeline). Raw protocol;
          # creates xdg_toplevel + xdg_popup with a deliberate
          # positioner and emits POPUP_CONFIGURE: x=..y=..w=..h=..
          halmasuit-popup-test-client = rustPlatform.buildRustPackage {
            pname   = "halmasuit-popup-test-client";
            version = "0.1.0";
            src     = ./.;
            cargoLock = {
              lockFile = ./Cargo.lock;
              allowBuiltinFetchGit = true;
            };
            cargoBuildFlags = [ "-p" "halmasuit-popup-test-client" ];
            nativeBuildInputs = [ pkgs.pkg-config ];
            buildInputs       = [ pkgs.wayland ];
            doCheck = false;
            meta = {
              description = "xdg-shell popup positioner / geometry observer (R5)";
              license     = pkgs.lib.licenses.asl20;
              mainProgram = "halmasuit-popup-test-client";
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
      # pkgs.halmasuit-session` resolutions work. Consumers apply this once
      # (`nixpkgs.overlays = [ halmasuit.overlays.default ];`) and then
      # `services.halmasuit.enable = true` works without further wiring.
      overlays.default = final: _prev: {
        halmasuit         = self.packages.${final.stdenv.hostPlatform.system}.halmasuit;
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
          halmasuit-session    = self.packages.x86_64-linux.halmasuit-session;
        };
        # Epic #12 task 10: end-to-end video wallpaper gate. Real
        # h264, real rsmpeg, real sandbox; asserts decoder spawn,
        # crash-recovery respawn within budget, budget-exhaustion
        # fallback, AND login-flash continuity under video wallpaper.
        visual-wallpaper-video = import ./tests/visual-wallpaper-video.nix {
          system            = "x86_64-linux";
          inherit nixpkgs;
          halmasuit         = self.packages.x86_64-linux.halmasuit;
          halmasuit-session = self.packages.x86_64-linux.halmasuit-session;
          halmasuit-decoder = self.packages.x86_64-linux.halmasuit-decoder;
        };
        halmasuit-vm = import ./tests/halmasuit-vm.nix {
          system    = "x86_64-linux";
          inherit nixpkgs;
          halmasuit            = self.packages.x86_64-linux.halmasuit;
          halmasuit-session    = self.packages.x86_64-linux.halmasuit-session;
          halmasuit-vm-client  = self.packages.x86_64-linux.halmasuit-vm-client;
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
        # Epic #1 FLAGSHIP gate (headline): ONE pam_handle_t spans
        # auth→session — real pam_unix, pam_mount-equivalent authtok
        # continuity across phases, getgrouplist-MERGE, Amendment-A1.3
        # env survival.
        session-onehandle = import ./tests/session-onehandle.nix {
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
        # Epic #47 R2 Phase 0 probe: SurviveFinalKillSignal=yes on the
        # rootfs side. drm-master-probe-phase2 already proved this for
        # the boot pivot; this proves it for the shutdown pivot.
        halmasuit-shutdown-probe-phase0 = import ./tests/halmasuit-shutdown-probe-phase0.nix {
          system = "x86_64-linux";
          inherit nixpkgs;
        };
        # Epic #47 R2 Phase 1 probe: same-PID survival across
        # systemd-shutdown's pivot to /run/initramfs. Adds
        # boot.initrd.systemd.shutdownRamfs.storePaths so the probe
        # binary lives in the post-pivot tmpfs view.
        halmasuit-shutdown-probe-phase1 = import ./tests/halmasuit-shutdown-probe-phase1.nix {
          system = "x86_64-linux";
          inherit nixpkgs;
        };
        # Epic #47 R2 Phase 2 probe (THE risky one): DRM master fd
        # survival across the shutdownRamfs pivot. No documented
        # prior art for a graphics process doing this. If this
        # passes, production wiring is unblocked. If it fails, fall
        # back to the partial-scope alternative.
        halmasuit-shutdown-probe-phase2 = import ./tests/halmasuit-shutdown-probe-phase2.nix {
          system = "x86_64-linux";
          inherit nixpkgs;
        };
        # Epic #71 Phase 0 probe: validate that an unprivileged process
        # can call TIOCSCTTY + VT_RELDISP on an inherited VT fd without
        # holding CAP_SYS_TTY_CONFIG. The production broker-passes-fd
        # design assumes this works; the probe answers the question
        # empirically before production commits.
        vt-probe-phase0 = import ./tests/vt-probe-phase0.nix {
          system = "x86_64-linux";
          inherit nixpkgs;
        };
        # Epic #71 R1.4: end-to-end VT-switching round-trip. Drives
        # the full broker↔compositor IPC dance against the real
        # halmasuit-session broker + live kernel VT subsystem.
        # Asserts kernel /sys/class/tty/tty0/active actually changed
        # (not just the protocol completing).
        vt-switch-roundtrip = import ./tests/vt-switch-roundtrip.nix {
          system = "x86_64-linux";
          inherit nixpkgs;
          halmasuit                = self.packages.x86_64-linux.halmasuit;
          halmasuit-session        = self.packages.x86_64-linux.halmasuit-session;
          halmasuit-vt-test-client = self.packages.x86_64-linux.halmasuit-vt-test-client;
        };
        # Epic #71 R-honest.1: org.halmasuit.Compositor1 live-value
        # gate. Asserts GetFrameCounter strictly increases over DBus
        # (render path feeds the same Arc the surface reads) — the
        # regression gate against the R3.3 "always 0" stub.
        compositor1-dbus = import ./tests/compositor1-dbus.nix {
          system = "x86_64-linux";
          inherit nixpkgs;
          halmasuit         = self.packages.x86_64-linux.halmasuit;
          halmasuit-session = self.packages.x86_64-linux.halmasuit-session;
        };
        # Epic #71 R1.4: master-drop timeout invariant (systemd
        # #21388 regression gate). Broker MUST FAIL the request on
        # the 5s watchdog and MUST NOT fire VT_ACTIVATE. Asserted
        # at kernel-state level: /sys/class/tty/tty0/active stays
        # on tty1 even after the protocol reports Rejected.
        vt-switch-master-drop-timeout = import ./tests/vt-switch-master-drop-timeout.nix {
          system = "x86_64-linux";
          inherit nixpkgs;
          halmasuit                = self.packages.x86_64-linux.halmasuit;
          halmasuit-session        = self.packages.x86_64-linux.halmasuit-session;
          halmasuit-vt-test-client = self.packages.x86_64-linux.halmasuit-vt-test-client;
        };
        # Phase B (initramfs survival): real halmasuit binary in
        # boot.initrd.systemd.services with SurviveFinalKillSignal=yes,
        # asserts PID + DRM-master + Wayland-socket continuity across
        # switch_root, single NDJSON stream observable post-pivot.
        initrd-survival = import ./tests/initrd-survival.nix {
          system            = "x86_64-linux";
          inherit nixpkgs;
          halmasuit         = self.packages.x86_64-linux.halmasuit;
          halmasuit-luks    = self.packages.x86_64-linux.halmasuit-luks;
          halmasuit-session = self.packages.x86_64-linux.halmasuit-session;
        };
        # Phase B hard gate: real LUKS-backed VM, real PAM auth via
        # halmasuit-vm-client over the abstract @halmasuit-greetd
        # socket, full survival + chroot + greeter + auth → session.
        full-boot-flash = import ./tests/full-boot-flash.nix {
          system              = "x86_64-linux";
          inherit nixpkgs;
          halmasuit           = self.packages.x86_64-linux.halmasuit;
          halmasuit-luks      = self.packages.x86_64-linux.halmasuit-luks;
          halmasuit-session   = self.packages.x86_64-linux.halmasuit-session;
          halmasuit-vm-client = self.packages.x86_64-linux.halmasuit-vm-client;
        };
        # Phase B LUKS unlock gate: real cryptsetup, real
        # systemd-cryptsetup ask-password producer, real
        # systemd password-agent wire. halmasuit-luks runs in
        # non-interactive responder mode (--passphrase-from PATH)
        # and answers the ask-file; the LUKS volume actually
        # unlocks. Isolates the wire contract from the Wayland UI
        # path (which is exercised by the full deployment shape).
        luks-unlock = import ./tests/luks-unlock.nix {
          system         = "x86_64-linux";
          inherit nixpkgs;
          halmasuit-luks = self.packages.x86_64-linux.halmasuit-luks;
        };
        # Phase B kernel-handoff-to-session pixmap continuity gate.
        # The Plymouth-removability proof: extends the same
        # exact-stream no-flash mechanism the rootfs visual-* family
        # uses (frame_audit build + frame_rendered events +
        # assert_no_flash_stream) to the boot-from-initrd timeline.
        # Consumes halmasuit-debug, same as the visual-* checks.
        visual-initrd-pixmap = import ./tests/visual-initrd-pixmap.nix {
          system              = "x86_64-linux";
          inherit nixpkgs;
          halmasuit           = self.packages.x86_64-linux.halmasuit-debug;
          halmasuit-luks      = self.packages.x86_64-linux.halmasuit-luks;
          halmasuit-session   = self.packages.x86_64-linux.halmasuit-session;
          halmasuit-vm-client = self.packages.x86_64-linux.halmasuit-vm-client;
          ssimulacra2-cli     = self.packages.x86_64-linux.ssimulacra2-cli;
        };
        # Epic #35 Phase B golden-boot — first cell of the matrix:
        # LUKS side-volume × image wallpaper. Real DankGreeter
        # driven by machine.send_chars; real niri as the
        # broker-launched session; per-variant per-scene goldens.
        visual-phase-b-side-image = import ./tests/visual-phase-b-side-image.nix {
          system              = "x86_64-linux";
          inherit nixpkgs nix-config;
          halmasuit-debug     = self.packages.x86_64-linux.halmasuit-debug;
          halmasuit-luks      = self.packages.x86_64-linux.halmasuit-luks;
          halmasuit-session   = self.packages.x86_64-linux.halmasuit-session;
          halmasuit-vm-client = self.packages.x86_64-linux.halmasuit-vm-client;
          ssimulacra2-cli     = self.packages.x86_64-linux.ssimulacra2-cli;
        };
        # Epic #35 cell (side, shader): same shape, animated GLSL
        # fragment-shader wallpaper (tests/fixtures/wallpaper-shader.glsl).
        visual-phase-b-side-shader = import ./tests/visual-phase-b-side-shader.nix {
          system              = "x86_64-linux";
          inherit nixpkgs nix-config;
          halmasuit-debug     = self.packages.x86_64-linux.halmasuit-debug;
          halmasuit-luks      = self.packages.x86_64-linux.halmasuit-luks;
          halmasuit-session   = self.packages.x86_64-linux.halmasuit-session;
          halmasuit-vm-client = self.packages.x86_64-linux.halmasuit-vm-client;
          ssimulacra2-cli     = self.packages.x86_64-linux.ssimulacra2-cli;
        };
        # Epic #35 cell (side, video): same shape, real h264 (ffmpeg-built
        # testsrc), looping, with a PNG fallback. Exercises the
        # halmasuit-decoder sandbox + DecoderRelay through the fromInitrd
        # path on top of the rest of the Phase B end-to-end arc.
        # Epic #35 cell (enc, image): LUKS rootfs (not a side volume).
        # Same arc, dual-boot specialisation pattern (cf.
        # nixos/tests/systemd-initrd-luks-password.nix): first boot
        # luksFormats /dev/vdb + `bootctl set-default cryptroot`,
        # second boot enters the specialisation; halmasuit-luks
        # responds to the cryptroot-mount ask-password prompt.
        visual-phase-b-enc-image = import ./tests/visual-phase-b-enc-image.nix {
          system              = "x86_64-linux";
          inherit nixpkgs nix-config;
          halmasuit-debug     = self.packages.x86_64-linux.halmasuit-debug;
          halmasuit-luks      = self.packages.x86_64-linux.halmasuit-luks;
          halmasuit-session   = self.packages.x86_64-linux.halmasuit-session;
          halmasuit-vm-client = self.packages.x86_64-linux.halmasuit-vm-client;
          ssimulacra2-cli     = self.packages.x86_64-linux.ssimulacra2-cli;
        };
        # Epic #35 cell (enc, shader): LUKS rootfs + GLSL shader wallpaper.
        visual-phase-b-enc-shader = import ./tests/visual-phase-b-enc-shader.nix {
          system              = "x86_64-linux";
          inherit nixpkgs nix-config;
          halmasuit-debug     = self.packages.x86_64-linux.halmasuit-debug;
          halmasuit-luks      = self.packages.x86_64-linux.halmasuit-luks;
          halmasuit-session   = self.packages.x86_64-linux.halmasuit-session;
          halmasuit-vm-client = self.packages.x86_64-linux.halmasuit-vm-client;
          ssimulacra2-cli     = self.packages.x86_64-linux.ssimulacra2-cli;
        };
        # Epic #35 cell (enc, video): LUKS rootfs + h264 video wallpaper.
        # Final matrix cell.
        visual-phase-b-enc-video = import ./tests/visual-phase-b-enc-video.nix {
          system              = "x86_64-linux";
          inherit nixpkgs nix-config;
          halmasuit-debug     = self.packages.x86_64-linux.halmasuit-debug;
          halmasuit-decoder   = self.packages.x86_64-linux.halmasuit-decoder;
          halmasuit-luks      = self.packages.x86_64-linux.halmasuit-luks;
          halmasuit-session   = self.packages.x86_64-linux.halmasuit-session;
          halmasuit-vm-client = self.packages.x86_64-linux.halmasuit-vm-client;
          ssimulacra2-cli     = self.packages.x86_64-linux.ssimulacra2-cli;
        };
        visual-phase-b-side-video = import ./tests/visual-phase-b-side-video.nix {
          system              = "x86_64-linux";
          inherit nixpkgs nix-config;
          halmasuit-debug     = self.packages.x86_64-linux.halmasuit-debug;
          halmasuit-decoder   = self.packages.x86_64-linux.halmasuit-decoder;
          halmasuit-luks      = self.packages.x86_64-linux.halmasuit-luks;
          halmasuit-session   = self.packages.x86_64-linux.halmasuit-session;
          halmasuit-vm-client = self.packages.x86_64-linux.halmasuit-vm-client;
          ssimulacra2-cli     = self.packages.x86_64-linux.ssimulacra2-cli;
        };
        # Visual gates consume `halmasuit-debug` (frame_audit on): the
        # capture path is the in-process `Snapshot()` D-Bus method,
        # not QMP screendump. Structural tests above stay on the
        # production `halmasuit` package.
        visual-halmasuit-clear = import ./tests/visual-halmasuit-clear.nix {
          system = "x86_64-linux";
          inherit nixpkgs;
          halmasuit       = self.packages.x86_64-linux.halmasuit-debug;
          halmasuit-session = self.packages.x86_64-linux.halmasuit-session;
          ssimulacra2-cli = self.packages.x86_64-linux.ssimulacra2-cli;
        };
        visual-halmasuit-layer = import ./tests/visual-halmasuit-layer.nix {
          system = "x86_64-linux";
          inherit nixpkgs;
          halmasuit                        = self.packages.x86_64-linux.halmasuit-debug;
          halmasuit-session                  = self.packages.x86_64-linux.halmasuit-session;
          halmasuit-layer-shell-test-client = self.packages.x86_64-linux.halmasuit-layer-shell-test-client;
          ssimulacra2-cli                  = self.packages.x86_64-linux.ssimulacra2-cli;
        };
        visual-halmasuit-splash = import ./tests/visual-halmasuit-splash.nix {
          system = "x86_64-linux";
          inherit nixpkgs;
          halmasuit        = self.packages.x86_64-linux.halmasuit-debug;
          halmasuit-session  = self.packages.x86_64-linux.halmasuit-session;
          ssimulacra2-cli  = self.packages.x86_64-linux.ssimulacra2-cli;
        };
        visual-backdrop = import ./tests/visual-backdrop.nix {
          system = "x86_64-linux";
          inherit nixpkgs;
          halmasuit                         = self.packages.x86_64-linux.halmasuit-debug;
          halmasuit-session                   = self.packages.x86_64-linux.halmasuit-session;
          halmasuit-layer-shell-test-client = self.packages.x86_64-linux.halmasuit-layer-shell-test-client;
          ssimulacra2-cli                   = self.packages.x86_64-linux.ssimulacra2-cli;
        };
        # Epic layer F1: real xdg_toplevel composited fullscreen
        # over halmasuit's internal wallpaper plane.
        visual-halmasuit-toplevel = import ./tests/visual-halmasuit-toplevel.nix {
          system = "x86_64-linux";
          inherit nixpkgs;
          halmasuit                      = self.packages.x86_64-linux.halmasuit-debug;
          halmasuit-session                = self.packages.x86_64-linux.halmasuit-session;
          halmasuit-toplevel-test-client = self.packages.x86_64-linux.halmasuit-toplevel-test-client;
          ssimulacra2-cli                = self.packages.x86_64-linux.ssimulacra2-cli;
        };
        # Epic layer F2: greetd-driven greeter→session foreground
        # swap; no-flash continuity across the REAL transition.
        visual-foreground = import ./tests/visual-foreground.nix {
          system = "x86_64-linux";
          inherit nixpkgs;
          halmasuit                         = self.packages.x86_64-linux.halmasuit-debug;
          halmasuit-session                   = self.packages.x86_64-linux.halmasuit-session;
          halmasuit-layer-shell-test-client = self.packages.x86_64-linux.halmasuit-layer-shell-test-client;
          halmasuit-toplevel-test-client    = self.packages.x86_64-linux.halmasuit-toplevel-test-client;
          halmasuit-vm-client               = self.packages.x86_64-linux.halmasuit-vm-client;
          ssimulacra2-cli                   = self.packages.x86_64-linux.ssimulacra2-cli;
        };
        # Epic G-layer R2/R4: the REAL niri as the broker-launched
        # session (niri-flake pinned via nix-config; unpatched).
        visual-niri-session = import ./tests/visual-niri-session.nix {
          system = "x86_64-linux";
          inherit nixpkgs nix-config;
          halmasuit                         = self.packages.x86_64-linux.halmasuit-debug;
          halmasuit-session                 = self.packages.x86_64-linux.halmasuit-session;
          halmasuit-layer-shell-test-client = self.packages.x86_64-linux.halmasuit-layer-shell-test-client;
          halmasuit-vm-client               = self.packages.x86_64-linux.halmasuit-vm-client;
          ssimulacra2-cli                   = self.packages.x86_64-linux.ssimulacra2-cli;
        };
        # Epic #47 R1 hard gate: login → SIGKILL niri → broker-respawn
        # greeter (NEW pid) → second login. Uses the same direct-niri
        # session command pattern as visual-niri-session (bypassing
        # niri-session's dbus dep) so the two-key swap_gate actually
        # reaches Swapped under headless rendering.
        visual-logout-respawn = import ./tests/visual-logout-respawn.nix {
          system = "x86_64-linux";
          inherit nixpkgs nix-config;
          halmasuit                         = self.packages.x86_64-linux.halmasuit-debug;
          halmasuit-session                 = self.packages.x86_64-linux.halmasuit-session;
          halmasuit-layer-shell-test-client = self.packages.x86_64-linux.halmasuit-layer-shell-test-client;
          halmasuit-vm-client               = self.packages.x86_64-linux.halmasuit-vm-client;
          ssimulacra2-cli                   = self.packages.x86_64-linux.ssimulacra2-cli;
        };
        # Epic #47 R2.1 hard gate: SIGTERM-arming + graceful tear-down.
        # halmasuit ignores SIGTERM during the boot pivot (shutdown_armed
        # = false in fromInitrd mode until Phase::RootfsReady), then
        # honors it as the real shutdown signal post-arming. This test
        # exercises the rootfs-only path (shutdown_armed=true at start)
        # and asserts wallpaper-only recomposite + clean exit + no
        # flash across the tear-down.
        visual-shutdown-tear-down = import ./tests/visual-shutdown-tear-down.nix {
          system = "x86_64-linux";
          inherit nixpkgs nix-config;
          halmasuit                         = self.packages.x86_64-linux.halmasuit-debug;
          halmasuit-session                 = self.packages.x86_64-linux.halmasuit-session;
          halmasuit-layer-shell-test-client = self.packages.x86_64-linux.halmasuit-layer-shell-test-client;
          halmasuit-vm-client               = self.packages.x86_64-linux.halmasuit-vm-client;
          ssimulacra2-cli                   = self.packages.x86_64-linux.ssimulacra2-cli;
        };
        # Epic #47 R2.2 hard gate: production halmasuit survives the
        # rootfs→shutdownRamfs pivot under an actual `systemctl
        # poweroff`. Pivot survival was probe-validated in
        # halmasuit-shutdown-probe-phase{1,2}; this test exercises
        # the production binary (with the broker-launched greeter,
        # the real DRM backend, the SurviveFinalKillSignal unit
        # directive, and the systemd.shutdownRamfs.storePaths
        # wiring) and asserts the same PID emits liveness lines
        # AFTER the post-pivot `shutdown[1]:` marker.
        visual-shutdown-pivot-survival = import ./tests/visual-shutdown-pivot-survival.nix {
          system = "x86_64-linux";
          inherit nixpkgs nix-config;
          halmasuit                         = self.packages.x86_64-linux.halmasuit-debug;
          halmasuit-session                 = self.packages.x86_64-linux.halmasuit-session;
          halmasuit-layer-shell-test-client = self.packages.x86_64-linux.halmasuit-layer-shell-test-client;
          halmasuit-vm-client               = self.packages.x86_64-linux.halmasuit-vm-client;
        };
        # Epic #61 R3.4: image cell of the wallpaper-shutdown-survival
        # matrix. Pairs with pivot-survival (shader cell, has phash-
        # progression + frame-counter advancing assertions). Image is
        # static, so this cell asserts only the survival invariants
        # (PID continuity, no coredump, liveness past pivot marker).
        visual-shutdown-image = import ./tests/visual-shutdown-image.nix {
          system = "x86_64-linux";
          inherit nixpkgs nix-config;
          halmasuit                         = self.packages.x86_64-linux.halmasuit-debug;
          halmasuit-session                 = self.packages.x86_64-linux.halmasuit-session;
          halmasuit-layer-shell-test-client = self.packages.x86_64-linux.halmasuit-layer-shell-test-client;
          halmasuit-vm-client               = self.packages.x86_64-linux.halmasuit-vm-client;
          ssimulacra2-cli                   = self.packages.x86_64-linux.ssimulacra2-cli;
        };
        # Epic #61 R3.5: video cell of the wallpaper-shutdown-survival
        # matrix. Asserts the full animation invariants (frame counter
        # advances + phash progression) plus the shared survival
        # invariants. Drives the halmasuit-decoder relay path end-to-
        # end through the shutdown sequence.
        visual-shutdown-video = import ./tests/visual-shutdown-video.nix {
          system = "x86_64-linux";
          inherit nixpkgs nix-config;
          halmasuit                         = self.packages.x86_64-linux.halmasuit-debug;
          halmasuit-decoder                 = self.packages.x86_64-linux.halmasuit-decoder;
          halmasuit-session                 = self.packages.x86_64-linux.halmasuit-session;
          halmasuit-layer-shell-test-client = self.packages.x86_64-linux.halmasuit-layer-shell-test-client;
          halmasuit-vm-client               = self.packages.x86_64-linux.halmasuit-vm-client;
        };
        # R13 forcing function (the reason this epic exists): the
        # real DMS DankGreeter (Quickshell/Qt6 + greeter-niri) as
        # halmasuit's greeter. Scaffolded at epic #2 close (8925ca5);
        # turned on once R2 + the rest of the Phase A/B contracts
        # landed in this convergence epic.
        visual-dankgreeter = import ./tests/visual-dankgreeter.nix {
          system = "x86_64-linux";
          inherit nixpkgs nix-config;
          halmasuit         = self.packages.x86_64-linux.halmasuit-debug;
          halmasuit-session = self.packages.x86_64-linux.halmasuit-session;
          ssimulacra2-cli   = self.packages.x86_64-linux.ssimulacra2-cli;
        };
        # R13(b) GATE: real DMS DankGreeter as halmasuit's greeter,
        # real keystrokes → broker → real pam_unix → session_opened.
        # The end-to-end chain through the upstream client we
        # actually deploy with on gnomon.
        visual-dankgreeter-auth = import ./tests/visual-dankgreeter-auth.nix {
          system = "x86_64-linux";
          inherit nixpkgs nix-config;
          halmasuit         = self.packages.x86_64-linux.halmasuit-debug;
          halmasuit-session = self.packages.x86_64-linux.halmasuit-session;
          ssimulacra2-cli   = self.packages.x86_64-linux.ssimulacra2-cli;
        };
        # R12 (GTK4 half): real GTK4 wayland client as halmasuit's
        # greeter. Qt6 is covered by visual-dankgreeter (Quickshell);
        # this proves the parallel GTK4 path through the same
        # halmasuit registry surface.
        visual-gtk4-smoke = import ./tests/visual-gtk4-smoke.nix {
          system = "x86_64-linux";
          inherit nixpkgs;
          halmasuit         = self.packages.x86_64-linux.halmasuit-debug;
          halmasuit-session = self.packages.x86_64-linux.halmasuit-session;
          ssimulacra2-cli   = self.packages.x86_64-linux.ssimulacra2-cli;
        };
        # Convergence epic R2: wl_surface.frame callbacks fire so
        # Mesa-EGL clients don't wedge in dri2_wl_surface_throttle.
        visual-frame-callbacks = import ./tests/visual-frame-callbacks.nix {
          system = "x86_64-linux";
          inherit nixpkgs nix-config;
          halmasuit         = self.packages.x86_64-linux.halmasuit-debug;
          halmasuit-session = self.packages.x86_64-linux.halmasuit-session;
          ssimulacra2-cli   = self.packages.x86_64-linux.ssimulacra2-cli;
        };
        # Convergence epic R3: sync wl_subsurface commits are
        # aggregated to the parent atomic state, NOT applied
        # immediately (smithay smallvil pattern).
        visual-sync-subsurface = import ./tests/visual-sync-subsurface.nix {
          system = "x86_64-linux";
          inherit nixpkgs;
          halmasuit                        = self.packages.x86_64-linux.halmasuit-debug;
          halmasuit-session                = self.packages.x86_64-linux.halmasuit-session;
          halmasuit-subsurface-test-client = self.packages.x86_64-linux.halmasuit-subsurface-test-client;
          ssimulacra2-cli                  = self.packages.x86_64-linux.ssimulacra2-cli;
        };
        # Convergence epic R4: halmasuit defers the initial
        # xdg_surface.configure to the commit handler (per xdg-shell
        # spec: configure is sent in response to the client's first
        # wl_surface.commit, not eagerly at xdg_toplevel creation).
        visual-deferred-configure = import ./tests/visual-deferred-configure.nix {
          system = "x86_64-linux";
          inherit nixpkgs;
          halmasuit                                 = self.packages.x86_64-linux.halmasuit-debug;
          halmasuit-session                         = self.packages.x86_64-linux.halmasuit-session;
          halmasuit-deferred-configure-test-client  = self.packages.x86_64-linux.halmasuit-deferred-configure-test-client;
          ssimulacra2-cli                           = self.packages.x86_64-linux.ssimulacra2-cli;
        };
        # Convergence epic R5: smithay PopupManager + positioner-driven
        # xdg_popup geometry (no more zero-rect default configure).
        visual-popup = import ./tests/visual-popup.nix {
          system = "x86_64-linux";
          inherit nixpkgs;
          halmasuit                  = self.packages.x86_64-linux.halmasuit-debug;
          halmasuit-session          = self.packages.x86_64-linux.halmasuit-session;
          halmasuit-popup-test-client = self.packages.x86_64-linux.halmasuit-popup-test-client;
          ssimulacra2-cli            = self.packages.x86_64-linux.ssimulacra2-cli;
        };
        # Amendment A5.6: poll-only leader pidfd backstop — SCM_RIGHTS
        # worker→broker→compositor armed + fires on leader exit.
        visual-pidfd-revert = import ./tests/visual-pidfd-revert.nix {
          system = "x86_64-linux";
          inherit nixpkgs;
          halmasuit                         = self.packages.x86_64-linux.halmasuit-debug;
          halmasuit-session                 = self.packages.x86_64-linux.halmasuit-session;
          halmasuit-layer-shell-test-client = self.packages.x86_64-linux.halmasuit-layer-shell-test-client;
          halmasuit-toplevel-test-client    = self.packages.x86_64-linux.halmasuit-toplevel-test-client;
          halmasuit-vm-client               = self.packages.x86_64-linux.halmasuit-vm-client;
        };
        # Amendment A5: two-key flash-free swap ORDERING + revert
        # (headless event-stream proof; pixel proof is visual-foreground).
        visual-revert = import ./tests/visual-revert.nix {
          system = "x86_64-linux";
          inherit nixpkgs;
          halmasuit                         = self.packages.x86_64-linux.halmasuit-debug;
          halmasuit-session                 = self.packages.x86_64-linux.halmasuit-session;
          halmasuit-layer-shell-test-client = self.packages.x86_64-linux.halmasuit-layer-shell-test-client;
          halmasuit-toplevel-test-client    = self.packages.x86_64-linux.halmasuit-toplevel-test-client;
          halmasuit-vm-client               = self.packages.x86_64-linux.halmasuit-vm-client;
        };
        # Epic layer E2: real keystroke → libinput → wl_seat →
        # focused client. Production halmasuit (input is core).
        halmasuit-input = import ./tests/halmasuit-input.nix {
          system = "x86_64-linux";
          inherit nixpkgs;
          halmasuit                         = self.packages.x86_64-linux.halmasuit;
          halmasuit-session                   = self.packages.x86_64-linux.halmasuit-session;
          halmasuit-layer-shell-test-client = self.packages.x86_64-linux.halmasuit-layer-shell-test-client;
        };
      };
    };
}
