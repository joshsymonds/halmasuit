# tests/visual-qt6-greeter-auth.nix — R13(b) G3 keystroke auth arc.
#
# Round 4b diagnostic isolated R13(b)'s blockage to Qt's QtWayland
# platform plugin or Quickshell's WlrLayershell integration on
# wlr-layer-shell surfaces (halmasuit's libinput-to-wl_keyboard chain
# is verified working end-to-end). The DMS+layer-shell path can't be
# driven without patching Qt/Quickshell, which CLAUDE.md forbids.
#
# This test takes the orthogonal axis: keep the SAME load-bearing
# R13(b) assertion (Qt6 keystrokes → broker → real pam_unix → real
# niri session) but use an **xdg-toplevel-backed** Qt6 client instead
# of DMS+wlr-layer-shell. Qt's QtWayland fully supports xdg-toplevel
# input — the layer-shell-keyboard upstream gap doesn't apply.
#
# The greeter is a minimal Qt6 Widgets app (QLineEdit for username +
# password, QPushButton for login). On submit it spawns
# halmasuit-vm-client to drive the greetd `create_session` +
# `post_auth_message_response` + `start_session` flow over halmasuit's
# relay socket → privileged broker → real pam_unix. halmasuit's
# tracked-greeter handling kills the Qt6 greeter on broker
# `SessionOpened`; the session leader executes niri.
#
# Asserts the R13(b) chain:
#   - Qt6 process maps an xdg-toplevel (halmasuit's existing toplevel
#     focus-follows-foreground path handles it).
#   - send_chars + send_key + send_key into the Qt6 LineEdits.
#   - halmasuit-vm-client subprocess auths against broker → niri.
#   - halmasuit foreground transitions greeter → session.
#   - halmasuit PID continuous across the swap.
#   - assert_no_flash_stream over the whole continuum.

{
  system,
  nixpkgs,
  nix-config,
  halmasuit,
  halmasuit-session,
  halmasuit-vm-client,
  ssimulacra2-cli,
}:

let
  pkgs = import nixpkgs {
    inherit system;
    config.allowUnfree = true;
  };
  niri = nix-config.inputs.niri-flake.packages.${system}.niri-unstable;

  # Minimal niri config (mirrors visual-niri-session.nix's
  # multi-line KDL format; inline `{ }` parses as KDL error in this
  # niri rev).
  niriConfig = pkgs.writeText "niri-config.kdl" ''
    input {
        keyboard {
            xkb {
            }
        }
    }

    output "*" {
    }

    layout {
    }

    animations {
        off
    }
  '';

  # The Qt6-toplevel session command. niri-as-session runs as the
  # authed user; halmasuit's broker fork-drops to alice (uid 1001).
  sessionCmd = pkgs.writeShellScript "halmasuit-qt6-niri-session" ''
    export XDG_RUNTIME_DIR=/run/halmasuit-niri
    export WAYLAND_DISPLAY=/run/halmasuit/wayland-0
    export LIBGL_ALWAYS_SOFTWARE=1
    export GALLIUM_DRIVER=llvmpipe
    export MESA_LOADER_DRIVER_OVERRIDE=llvmpipe
    export LIBGL_DRI3_DISABLE=1
    exec ${niri}/bin/niri --config ${niriConfig}
  '';

  # The Qt6 Widgets greeter. Single C++ file, compiled with Qt6 cmake.
  # Uses xdg-toplevel (QtWayland default — NOT wlr-layer-shell), so
  # the Round-4b blockage doesn't apply: Qt's standard wl_keyboard
  # handling on xdg-toplevel is well-supported and exercised by every
  # ordinary Qt6 desktop app.
  qt6GreeterSrc = pkgs.writeText "halmasuit-qt6-greeter.cpp" ''
    #include <QApplication>
    #include <QDebug>
    #include <QFile>
    #include <QLineEdit>
    #include <QProcess>
    #include <QPushButton>
    #include <QVBoxLayout>
    #include <QWidget>
    #include <iostream>

    int main(int argc, char *argv[]) {
        QApplication app(argc, argv);
        QWidget w;
        w.setWindowTitle("halmasuit-qt6-greeter");
        w.resize(1280, 800);
        w.setStyleSheet("background-color: #14001f; color: white;");

        auto *layout = new QVBoxLayout(&w);
        layout->setContentsMargins(400, 300, 400, 300);
        layout->setSpacing(20);
        auto *user = new QLineEdit(&w);
        user->setPlaceholderText("Username");
        user->setStyleSheet("background:#1a1024;color:white;padding:8px;font-size:18px;");
        auto *pass = new QLineEdit(&w);
        pass->setPlaceholderText("Password");
        pass->setEchoMode(QLineEdit::Password);
        pass->setStyleSheet("background:#1a1024;color:white;padding:8px;font-size:18px;");
        auto *btn = new QPushButton("Log in", &w);
        btn->setStyleSheet(
            "background:#5e3aa6;color:white;padding:10px;font-size:18px;");
        layout->addWidget(user);
        layout->addWidget(pass);
        layout->addWidget(btn);

        QObject::connect(user, &QLineEdit::textChanged, [](const QString& s) {
            std::cerr << "QT_USER_INPUT: " << s.toStdString() << std::endl;
            std::cerr.flush();
        });
        QObject::connect(pass, &QLineEdit::textChanged, [](const QString& s) {
            std::cerr << "QT_PASS_INPUT_LEN: " << s.length() << std::endl;
            std::cerr.flush();
        });

        auto doLogin = [&]() {
            std::cerr << "QT_LOGIN_TRIGGERED" << std::endl;
            std::cerr.flush();
            QFile pwf("/run/halmasuit-greeter/.qt-pw");
            if (pwf.open(QIODevice::WriteOnly)) {
                pwf.write(pass->text().toUtf8());
                pwf.close();
                pwf.setPermissions(QFile::ReadOwner | QFile::WriteOwner);
            }
            std::cerr << "QT_SPAWN_VM_CLIENT user=" << user->text().toStdString()
                      << std::endl;
            std::cerr.flush();
            int rc = QProcess::execute(
                "halmasuit-vm-client",
                {"full-auth", "/run/halmasuit/greetd.sock",
                 user->text(),
                 "--password-file", "/run/halmasuit-greeter/.qt-pw",
                 "--cmd", "${sessionCmd}",
                 "--timeout", "20"});
            std::cerr << "QT_VM_CLIENT_DONE rc=" << rc << std::endl;
            std::cerr.flush();
            QFile::remove("/run/halmasuit-greeter/.qt-pw");
            QApplication::quit();
        };
        QObject::connect(btn, &QPushButton::clicked, doLogin);
        QObject::connect(pass, &QLineEdit::returnPressed, doLogin);
        QObject::connect(user, &QLineEdit::returnPressed,
                         [&]() { pass->setFocus(); });

        w.show();
        user->setFocus();
        std::cerr << "QT_GREETER_READY" << std::endl;
        std::cerr.flush();
        return app.exec();
    }
  '';

  # Minimal build via runCommand: g++ + Qt6 headers/libs, no qmake/
  # wrapQtAppsHook complexity. The greeterLauncher below sets the Qt
  # plugin path (QT_QPA_PLATFORM_PLUGIN_PATH) explicitly so we don't
  # need wrapQtAppsHook's runtime wrapping.
  qt6Greeter = pkgs.runCommand "halmasuit-qt6-greeter" {
    nativeBuildInputs = [ pkgs.gcc ];
    buildInputs = [ pkgs.qt6.qtbase ];
  } ''
    mkdir -p $out/bin
    QT_HEADERS=${pkgs.qt6.qtbase}/include
    QT_LIBS=${pkgs.qt6.qtbase}/lib
    g++ -std=c++17 -fPIC \
      -I$QT_HEADERS \
      -I$QT_HEADERS/QtCore \
      -I$QT_HEADERS/QtGui \
      -I$QT_HEADERS/QtWidgets \
      -L$QT_LIBS \
      -Wl,-rpath,$QT_LIBS \
      -lQt6Core -lQt6Gui -lQt6Widgets \
      -o $out/bin/halmasuit-qt6-greeter \
      ${qt6GreeterSrc}
  '';

  greeterLauncher = pkgs.writeShellScript "halmasuit-qt6-greeter-launcher" ''
    export XDG_RUNTIME_DIR=/run/halmasuit-greeter
    export WAYLAND_DISPLAY=/run/halmasuit/wayland-0
    export GREETD_SOCK=/run/halmasuit/greetd.sock
    export QT_QPA_PLATFORM=wayland
    # Qt6 platform plugin path (the unwrapped qt6Greeter binary needs
    # explicit pointers to the wayland-platform plugin + dependencies).
    export QT_PLUGIN_PATH=${pkgs.qt6.qtwayland}/lib/qt-6/plugins:${pkgs.qt6.qtbase}/lib/qt-6/plugins
    export QML2_IMPORT_PATH=${pkgs.qt6.qtbase}/lib/qt-6/qml
    export LIBGL_ALWAYS_SOFTWARE=1
    export GALLIUM_DRIVER=llvmpipe
    export MESA_LOADER_DRIVER_OVERRIDE=llvmpipe
    export LIBGL_DRI3_DISABLE=1
    export HOME=/run/halmasuit-greeter
    export XDG_CACHE_HOME=$HOME/.cache
    mkdir -p "$XDG_CACHE_HOME"
    exec ${qt6Greeter}/bin/halmasuit-qt6-greeter
  '';
in
pkgs.testers.runNixOSTest {
  name = "visual-qt6-greeter-auth";

  skipTypeCheck = true;

  node.specialArgs = { inputs = nix-config.inputs // { inherit nix-config; }; };

  interactive.nodes.machine = { ... }: {
    virtualisation.qemu.options = [
      "-device virtio-vga-gl"
      "-display gtk,gl=on"
    ];
  };

  nodes.machine =
    { config, lib, pkgs, ... }:
    {
      imports = [
        ../nix/module.nix
        ./lib/test-user.nix
        nix-config.inputs.niri-flake.nixosModules.niri
      ];

      programs.niri.enable = true;
      programs.niri.package = niri;

      services.halmasuit = {
        enable          = true;
        package         = halmasuit;
        session.package = halmasuit-session;
        greeterUid      = 999;
        greeterGroup    = "halmasuit-greeter";
        compositorUid   = 998;
        witnessImage    = ./fixtures/witness.png;
        greeterCommand  = "${greeterLauncher}";
      };

      users.users.alice = {
        isNormalUser = true;
        uid          = 1001;
        group        = "alice";
        password     = "testpassword";
        extraGroups  = [ "halmasuit-greeter" ];
      };
      users.groups.alice.gid = 1001;

      users.users.halmasuit-greeter = {
        isSystemUser = true;
        uid          = 999;
        group        = "halmasuit-greeter";
      };
      users.groups.halmasuit-greeter.gid = 999;
      users.users.halmasuit-compositor = {
        isSystemUser = true;
        uid          = 998;
        group        = "halmasuit-greeter";
      };

      # halmasuit-vm-client is the greetd client the Qt6 greeter spawns.
      environment.systemPackages = [ halmasuit-vm-client ];

      systemd.tmpfiles.rules = [
        "d /run/hsnap 0777 root root -"
        "d /run/halmasuit-greeter 0700 halmasuit-greeter halmasuit-greeter -"
        # niri's runtime dir, owned by alice.
        "d /run/halmasuit-niri 0700 alice alice -"
      ];
      systemd.services.halmasuit.serviceConfig.ReadWritePaths = [ "/run/hsnap" ];

      virtualisation = {
        memorySize = 4096;
        cores      = 4;
        diskSize   = 8192;
        qemu.options = [
          "-vga none"
          "-device virtio-gpu-pci"
        ];
      };
    };

  testScript = ''
    import os
    import sys
    import time

    sys.path.insert(0, "${./lib}")
    os.environ["PATH"] = "${ssimulacra2-cli}/bin:" + os.environ.get("PATH", "")
    os.environ.setdefault("GOLDENS_DIR", "${./goldens}")

    import visual

    def fg_events():
        return [
            e["to"] for e in visual.introspect_events(machine)
            if e["event"] == "foreground_changed"
        ]

    machine.start()
    machine.wait_for_unit("multi-user.target")
    machine.wait_for_unit("seatd.service")
    machine.wait_for_unit("halmasuit.service")

    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF scanout_active", timeout=30
    )

    # Qt6 greeter process is up.
    machine.wait_until_succeeds(
        "pgrep -f halmasuit-qt6-greeter", timeout=30
    )

    # Qt6 maps an xdg-toplevel (halmasuit's existing toplevel
    # focus-follows-foreground path handles it; no layer-shell-keyboard
    # blockage).
    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF 'xdg_toplevel mapped as fullscreen foreground'",
        timeout=60,
    )

    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -qF foreground_changed", timeout=60
    )
    assert "greeter" in fg_events(), f"expected greeter; got {fg_events()}"

    # ── G3 keystroke arc ───────────────────────────────────────────
    halmasuit_pid = machine.succeed(
        "systemctl show -p MainPID --value halmasuit.service"
    ).strip()

    # Wait for Qt6 to fully initialize (it prints QT_GREETER_READY when
    # the QApplication event loop starts and the username QLineEdit
    # has setFocus()).
    machine.wait_until_succeeds(
        "journalctl | grep -qF QT_GREETER_READY", timeout=60
    )
    # Sleep an additional second for halmasuit's commit-driven
    # focus-follows-foreground (maybe_focus_foreground_toplevel) to
    # fire on Qt6's first buffer commit, so wl_keyboard.enter has
    # landed before we begin send_chars.
    time.sleep(2)

    machine.send_chars("alice")
    machine.send_key("tab")
    machine.send_chars("testpassword")
    machine.send_key("ret")

    # halmasuit-vm-client (spawned by the Qt6 greeter) drives the
    # full greetd auth flow; on success the broker forks the session
    # leader (niri) and halmasuit's foreground transitions.
    machine.wait_until_succeeds(
        "journalctl -u halmasuit | grep -c 'foreground_changed' | grep -q '^[2-9]'",
        timeout=120,
    )
    events = fg_events()
    assert events[:2] == ["greeter", "session"], (
        f"R13b foreground ordering wrong (expected [greeter, session]): {events}"
    )

    # halmasuit PID continuous across the swap — load-bearing R13b.
    pid_now = machine.succeed(
        "systemctl show -p MainPID --value halmasuit.service"
    ).strip()
    assert pid_now == halmasuit_pid, (
        f"R13b violated: halmasuit restarted across greeter→session swap: "
        f"{halmasuit_pid} -> {pid_now}"
    )

    # niri is alive as the session.
    machine.wait_until_succeeds("pgrep -x niri", timeout=60)

    print(
        f"R13b: halmasuit pid {halmasuit_pid} continuous across "
        "Qt6-greeter → real-niri-session swap; full keystroke arc green"
    )

    visual.assert_no_flash_stream(machine)

    print("visual-qt6-greeter-auth: ALL ASSERTIONS PASSED")
  '';
}
