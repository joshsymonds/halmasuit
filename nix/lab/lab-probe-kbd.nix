{ stdenv }:

let
  # Harness over the real lab-probe-kbd.c. checkPhase prepends a line that
  # includes the real source with main renamed, then appends this body. It
  # records every uinput ioctl, drives the request handler through pipes, and
  # checks the FIFO and notify surfaces against a real filesystem and socket.
  harness = builtins.toFile "lab-probe-kbd-test.c" ''
    #include <errno.h>
    #include <fcntl.h>
    #include <limits.h>
    #include <stdarg.h>
    #include <stdbool.h>
    #include <stddef.h>
    #include <stdio.h>
    #include <stdlib.h>
    #include <string.h>
    #include <sys/socket.h>
    #include <sys/stat.h>
    #include <sys/un.h>
    #include <unistd.h>
    #include <linux/input.h>
    #include <linux/uinput.h>

    #define COUNT(a) (sizeof(a) / sizeof((a)[0]))

    /* Literal codes from the probe's key table, independent of the C source. */
    static const int literal_codes[26] = {
            30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49,
            24, 25, 16, 19, 31, 20, 22, 47, 17, 45, 21, 44,
    };
    #define LITERAL_ENTER 28
    #define LITERAL_BACKSPACE 14

    static int checks;
    static int failures;

    static void expect(bool ok, const char *file, int line, const char *text)
    {
            checks++;
            if (!ok) {
                    failures++;
                    fprintf(stderr, "%s:%d: failed: %s\n", file, line, text);
            }
    }
    #define CHECK(cond) expect((cond), __FILE__, __LINE__, #cond)

    static void must(bool ok, const char *what)
    {
            if (!ok) {
                    fprintf(stderr, "harness: %s\n", what);
                    exit(2);
            }
    }

    /* Records every ioctl the holder makes against /dev/uinput. */
    struct ioctl_call {
            unsigned long req;
            int arg;
            struct uinput_setup setup;
    };
    #define MAX_IOCTLS 64
    static struct ioctl_call ioctls[MAX_IOCTLS];
    static size_t nioctls;

    int __wrap_ioctl(int fd, unsigned long req, ...)
    {
            struct ioctl_call *c;
            va_list ap;

            (void)fd;
            if (nioctls == MAX_IOCTLS) {
                    errno = ENOSPC;
                    return -1;
            }
            c = &ioctls[nioctls++];
            c->req = req;
            va_start(ap, req);
            if (req == UI_DEV_SETUP)
                    c->setup = *va_arg(ap, struct uinput_setup *);
            else if (req != UI_DEV_CREATE)
                    c->arg = va_arg(ap, int);
            va_end(ap);
            return 0;
    }

    static int req_w = -1, req_r = -1, rep_r = -1, rep_w = -1, up_r = -1, up_w = -1;

    static void open_pipes(void)
    {
            int req[2], rep[2], up[2];

            must(pipe2(req, O_CLOEXEC) == 0, "pipe2 request");
            must(pipe2(rep, O_CLOEXEC | O_NONBLOCK) == 0, "pipe2 reply");
            must(pipe2(up, O_CLOEXEC | O_NONBLOCK) == 0, "pipe2 uinput");
            req_r = req[0];
            req_w = req[1];
            rep_r = rep[0];
            rep_w = rep[1];
            up_r = up[0];
            up_w = up[1];
    }

    struct events {
            struct input_event ev[512];
            size_t n;
    };

    static void send_request(const char *bytes, size_t len)
    {
            must(write(req_w, bytes, len) == (ssize_t)len, "write request");
    }

    static void read_reply(char *buf, size_t cap)
    {
            ssize_t n = read(rep_r, buf, cap - 1);

            buf[n > 0 ? (size_t)n : 0] = '\0';
            if (n <= 0)
                    buf[0] = '\0';
    }

    static void drain_events(struct events *out)
    {
            out->n = 0;
            while (out->n < COUNT(out->ev) &&
                   read(up_r, &out->ev[out->n], sizeof(struct input_event)) ==
                           (ssize_t)sizeof(struct input_event))
                    out->n++;
    }

    static void exchange(const char *line, size_t len, char *reply, size_t cap,
                         struct events *ev)
    {
            send_request(line, len);
            serve_one(req_r, rep_w, up_w);
            read_reply(reply, cap);
            drain_events(ev);
    }

    static size_t expected_presses(const char *username, int *out)
    {
            size_t n = 0;
            const char *p;

            for (int i = 0; i < 32; i++)
                    out[n++] = LITERAL_BACKSPACE;
            if (username[0] != '\0') {
                    for (p = username; *p != '\0'; p++)
                            out[n++] = literal_codes[*p - 'a'];
                    out[n++] = LITERAL_ENTER;
            }
            for (p = "wrongprobe"; *p != '\0'; p++)
                    out[n++] = literal_codes[*p - 'a'];
            out[n++] = LITERAL_ENTER;
            return n;
    }

    /* Each press is press, SYN, release, SYN, in order, with no extra events. */
    static void check_typed(const struct events *ev, const char *username)
    {
            int want[128];
            size_t nwant = expected_presses(username, want);

            CHECK(ev->n == nwant * 4);
            for (size_t i = 0; i < nwant && i * 4 + 3 < ev->n; i++) {
                    const struct input_event *g = &ev->ev[i * 4];

                    CHECK(g[0].type == EV_KEY && g[0].code == want[i] && g[0].value == 1);
                    CHECK(g[1].type == EV_SYN && g[1].code == SYN_REPORT && g[1].value == 0);
                    CHECK(g[2].type == EV_KEY && g[2].code == want[i] && g[2].value == 0);
                    CHECK(g[3].type == EV_SYN && g[3].code == SYN_REPORT && g[3].value == 0);
            }
    }

    static bool expected_key(int code)
    {
            if (code == LITERAL_ENTER || code == LITERAL_BACKSPACE)
                    return true;
            for (size_t i = 0; i < 26; i++) {
                    if (literal_codes[i] == code)
                            return true;
            }
            return false;
    }

    static void test_key_mapping(void)
    {
            const char *alphabet = "abcdefghijklmnopqrstuvwxyz";

            for (size_t i = 0; i < 26; i++)
                    CHECK(letter_code(alphabet[i]) == literal_codes[i]);
    }

    static void test_device_setup(void)
    {
            bool seen[KEY_MAX + 1] = { false };
            struct uinput_setup setup;

            nioctls = 0;
            create_keyboard(up_w);
            CHECK(nioctls == 31);
            if (nioctls != 31)
                    return;
            CHECK(ioctls[0].req == UI_SET_EVBIT && ioctls[0].arg == EV_KEY);
            for (size_t i = 1; i <= 28; i++) {
                    int code = ioctls[i].arg;

                    CHECK(ioctls[i].req == UI_SET_KEYBIT);
                    CHECK(code >= 0 && code <= KEY_MAX);
                    if (code < 0 || code > KEY_MAX)
                            continue;
                    CHECK(expected_key(code));
                    CHECK(!seen[code]);
                    seen[code] = true;
            }
            CHECK(ioctls[29].req == UI_DEV_SETUP);
            setup = ioctls[29].setup;
            CHECK(setup.id.bustype == BUS_VIRTUAL);
            CHECK(setup.id.vendor == 0x0001);
            CHECK(setup.id.product == 0x0001);
            CHECK(setup.id.version == 0x0001);
            CHECK(strcmp(setup.name, "lab-probe keyboard") == 0);
            CHECK(ioctls[30].req == UI_DEV_CREATE);
    }

    static void test_accepted(void)
    {
            static const struct {
                    const char *line;
                    const char *user;
                    const char *reply;
            } cases[] = {
                    { "\n", "", "ok 43\n" },
                    { "probeuser\n", "probeuser", "ok 53\n" },
                    { "a\n", "a", "ok 45\n" },
                    { "abcdefghijklmnopqrstuvwxyzabcdef\n",
                      "abcdefghijklmnopqrstuvwxyzabcdef", "ok 76\n" },
            };

            for (size_t i = 0; i < COUNT(cases); i++) {
                    char reply[64];
                    struct events ev;

                    exchange(cases[i].line, strlen(cases[i].line), reply, sizeof reply, &ev);
                    CHECK(strcmp(reply, cases[i].reply) == 0);
                    check_typed(&ev, cases[i].user);
            }
    }

    static void test_rejected(void)
    {
            static const char *const bad_lines[] = {
                    "PROBE\n", "probe1\n", "pro/be\n", "pro be\n",
            };
            char longline[128];
            char reply[64];
            struct events ev;

            for (size_t i = 0; i < COUNT(bad_lines); i++) {
                    exchange(bad_lines[i], strlen(bad_lines[i]), reply, sizeof reply, &ev);
                    CHECK(strcmp(reply, "error bad username\n") == 0);
                    CHECK(ev.n == 0);
            }

            memset(longline, 'a', 33);
            longline[33] = '\n';
            exchange(longline, 34, reply, sizeof reply, &ev);
            CHECK(strcmp(reply, "error username too long\n") == 0);
            CHECK(ev.n == 0);

            memset(longline, 'a', 100);
            longline[100] = '\n';
            exchange(longline, 101, reply, sizeof reply, &ev);
            CHECK(strcmp(reply, "error username too long\n") == 0);
            CHECK(ev.n == 0);
    }

    /* An over-long line is discarded through its newline, so the next line is a fresh request. */
    static void test_overlong_discarded(void)
    {
            char buf[128];
            char reply[64];
            struct events ev;

            memset(buf, 'b', 100);
            buf[100] = '\n';
            buf[101] = '\n';
            send_request(buf, 102);
            serve_one(req_r, rep_w, up_w);
            read_reply(reply, sizeof reply);
            CHECK(strcmp(reply, "error username too long\n") == 0);
            drain_events(&ev);
            CHECK(ev.n == 0);

            serve_one(req_r, rep_w, up_w);
            read_reply(reply, sizeof reply);
            CHECK(strcmp(reply, "ok 43\n") == 0);
            drain_events(&ev);
            check_typed(&ev, "");
    }

    static void test_fifos(void)
    {
            char base[] = "./lab-probe-kbd-XXXXXX";
            char dir[PATH_MAX];
            char path[2 * PATH_MAX];
            struct stat st;
            int rq = -1;
            int rp = -1;
            int f;

            must(mkdtemp(base) != NULL, "mkdtemp");
            snprintf(dir, sizeof dir, "%s/kbd", base);
            open_request_fifos(dir, &rq, &rp);
            CHECK(rq >= 0 && rp >= 0);
            CHECK(stat(dir, &st) == 0 && S_ISDIR(st.st_mode) && (st.st_mode & 0777) == 0700);
            snprintf(path, sizeof path, "%s/request", dir);
            CHECK(stat(path, &st) == 0 && S_ISFIFO(st.st_mode) && (st.st_mode & 0777) == 0600);
            CHECK(rq >= 0 && (fcntl(rq, F_GETFL) & O_ACCMODE) == O_RDWR);
            snprintf(path, sizeof path, "%s/reply", dir);
            CHECK(stat(path, &st) == 0 && S_ISFIFO(st.st_mode) && (st.st_mode & 0777) == 0600);
            CHECK(rp >= 0 && (fcntl(rp, F_GETFL) & O_ACCMODE) == O_RDWR);
            if (rq >= 0)
                    close(rq);
            if (rp >= 0)
                    close(rp);

            /* Existing FIFOs from an earlier holder are replaced. */
            rq = -1;
            rp = -1;
            open_request_fifos(dir, &rq, &rp);
            CHECK(rq >= 0 && rp >= 0);
            snprintf(path, sizeof path, "%s/request", dir);
            CHECK(stat(path, &st) == 0 && S_ISFIFO(st.st_mode) && (st.st_mode & 0777) == 0600);
            CHECK(rq >= 0 && (fcntl(rq, F_GETFL) & O_ACCMODE) == O_RDWR);
            if (rq >= 0)
                    close(rq);
            if (rp >= 0)
                    close(rp);

            /* A stale regular file where a FIFO belongs is replaced. */
            must(unlink(path) == 0, "unlink request fifo");
            f = open(path, O_CREAT | O_WRONLY | O_TRUNC, 0600);
            must(f >= 0, "create stale request");
            must(write(f, "stale", 5) == 5, "write stale request");
            close(f);
            rq = -1;
            rp = -1;
            open_request_fifos(dir, &rq, &rp);
            CHECK(rq >= 0 && rp >= 0);
            CHECK(stat(path, &st) == 0 && S_ISFIFO(st.st_mode) && (st.st_mode & 0777) == 0600);
            if (rq >= 0)
                    close(rq);
            if (rp >= 0)
                    close(rp);
    }

    static void test_notify(void)
    {
            char cwd[PATH_MAX];
            char path[2 * PATH_MAX];
            char buf[32];
            struct sockaddr_un addr;
            ssize_t n;
            int s;

            must(getcwd(cwd, sizeof cwd) != NULL, "getcwd");
            snprintf(path, sizeof path, "%s/notify.sock", cwd);

            s = socket(AF_UNIX, SOCK_DGRAM | SOCK_CLOEXEC, 0);
            must(s >= 0, "socket");
            memset(&addr, 0, sizeof addr);
            addr.sun_family = AF_UNIX;
            memcpy(addr.sun_path, path, strlen(path));
            unlink(path);
            must(bind(s, (struct sockaddr *)&addr,
                      offsetof(struct sockaddr_un, sun_path) + strlen(path) + 1) == 0,
                 "bind file socket");
            setenv("NOTIFY_SOCKET", path, 1);
            notify_ready();
            n = recv(s, buf, sizeof buf, MSG_DONTWAIT);
            CHECK(n == 8 && memcmp(buf, "READY=1\n", 8) == 0);
            close(s);
            unlink(path);

            s = socket(AF_UNIX, SOCK_DGRAM | SOCK_CLOEXEC, 0);
            must(s >= 0, "socket");
            memset(&addr, 0, sizeof addr);
            addr.sun_family = AF_UNIX;
            memcpy(addr.sun_path + 1, "lab-probe-kbd-test", strlen("lab-probe-kbd-test"));
            must(bind(s, (struct sockaddr *)&addr,
                      offsetof(struct sockaddr_un, sun_path) + 1 +
                              strlen("lab-probe-kbd-test")) == 0,
                 "bind abstract socket");
            setenv("NOTIFY_SOCKET", "@lab-probe-kbd-test", 1);
            notify_ready();
            n = recv(s, buf, sizeof buf, MSG_DONTWAIT);
            CHECK(n == 8 && memcmp(buf, "READY=1\n", 8) == 0);
            close(s);

            /* Unset NOTIFY_SOCKET skips the send and returns. */
            unsetenv("NOTIFY_SOCKET");
            notify_ready();
    }

    int main(void)
    {
            open_pipes();
            test_key_mapping();
            test_device_setup();
            test_accepted();
            test_rejected();
            test_overlong_discarded();
            test_fifos();
            test_notify();
            printf("lab-probe-kbd: %d checks, %d failed\n", checks, failures);
            return failures == 0 ? EXIT_SUCCESS : EXIT_FAILURE;
    }
  '';
in

stdenv.mkDerivation {
  pname = "lab-probe-kbd";
  version = "0";

  src = ./lab-probe-kbd.c;

  dontUnpack = true;
  dontConfigure = true;

  doCheck = true;

  buildPhase = ''
    runHook preBuild
    $CC -Wall -Wextra -Werror -O2 $src -o lab-probe-kbd
    runHook postBuild
  '';

  checkPhase = ''
    runHook preCheck
    {
      printf '#define _GNU_SOURCE\n#define main lab_probe_kbd_main\n#include "%s"\n#undef main\n' "$src"
      cat ${harness}
    } > lab-probe-kbd-test.c
    $CC -Wall -Wextra -Werror -O2 lab-probe-kbd-test.c -Wl,--wrap=ioctl -o lab-probe-kbd-test
    ./lab-probe-kbd-test
    runHook postCheck
  '';

  installPhase = ''
    runHook preInstall
    install -Dm755 lab-probe-kbd $out/bin/lab-probe-kbd
    runHook postInstall
  '';

  meta = {
    description = "uinput keyboard holder for the gnomon lab's early-boot probe";
    mainProgram = "lab-probe-kbd";
  };
}
