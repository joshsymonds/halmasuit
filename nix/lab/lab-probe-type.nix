{ stdenv }:

let
  # Harness over the real lab-probe-type.c. checkPhase prepends a line that
  # includes the real source with main renamed, then appends this body. It
  # runs relay against a fake holder: forked children own the request and
  # reply FIFOs under a scratch directory, so the relay's FIFO, stdin, and
  # timeout behavior is exercised for real. Only the holder is faked.
  harness = builtins.toFile "lab-probe-type-test.c" ''
    #include <errno.h>
    #include <fcntl.h>
    #include <limits.h>
    #include <signal.h>
    #include <stdbool.h>
    #include <stdio.h>
    #include <stdlib.h>
    #include <string.h>
    #include <sys/stat.h>
    #include <sys/types.h>
    #include <sys/wait.h>
    #include <unistd.h>

    #define HOLDER_WAIT_MS 5000
    #define PROBE_TIMEOUT_MS 100
    #define BUF_MAX 256

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

    static void die_sys(const char *what)
    {
            perror(what);
            exit(2);
    }

    static void path_in(char *out, size_t size, const char *dir, const char *leaf)
    {
            int n = snprintf(out, size, "%s/%s", dir, leaf);

            must(n >= 0 && (size_t)n < size, "path too long");
    }

    static void make_fifo(const char *path)
    {
            if (unlink(path) != 0 && errno != ENOENT)
                    die_sys("unlink");
            if (mkfifo(path, 0600) != 0)
                    die_sys("mkfifo");
    }

    /* Creates dir with fresh request and reply FIFOs and no capture file. */
    static void make_fixture(const char *dir)
    {
            char request[PATH_MAX], reply[PATH_MAX], capture[PATH_MAX];

            if (mkdir(dir, 0700) != 0 && errno != EEXIST)
                    die_sys("mkdir");
            path_in(request, sizeof request, dir, "request");
            path_in(reply, sizeof reply, dir, "reply");
            path_in(capture, sizeof capture, dir, "capture");
            make_fifo(request);
            make_fifo(reply);
            if (unlink(capture) != 0 && errno != ENOENT)
                    die_sys("unlink capture");
    }

    /* Returns the read end of a pipe holding data, with the write end closed. */
    static int input_from(const char *data)
    {
            size_t len = strlen(data);
            int fds[2];

            if (pipe(fds) != 0)
                    die_sys("pipe");
            if (write(fds[1], data, len) != (ssize_t)len)
                    die_sys("write input");
            close(fds[1]);
            return fds[0];
    }

    /* Reads fd until EOF or until it would block, NUL-terminates buf, returns the length. */
    static size_t drain(int fd, char *buf, size_t size)
    {
            size_t n = 0;

            while (n + 1 < size) {
                    ssize_t r = read(fd, buf + n, size - 1 - n);

                    if (r < 0 && errno == EINTR)
                            continue;
                    if (r <= 0)
                            break;
                    n += (size_t)r;
            }
            buf[n] = '\0';
            return n;
    }

    /* Reads path without blocking. A missing path reads as empty. */
    static size_t slurp(const char *path, char *buf, size_t size)
    {
            int fd = open(path, O_RDONLY | O_NONBLOCK);
            size_t n;

            if (fd < 0) {
                    if (errno != ENOENT)
                            die_sys("open");
                    buf[0] = '\0';
                    return 0;
            }
            n = drain(fd, buf, size);
            close(fd);
            return n;
    }

    /*
     * Fake holder: captures the first request line into capture, replies, and
     * then waits to be killed. It never reads past the first newline.
     */
    static void holder_child(int request_fd, int reply_fd, const char *capture, const char *reply)
    {
            char line[BUF_MAX];
            size_t n = 0;
            int fd;

            for (;;) {
                    char c;
                    ssize_t r = read(request_fd, &c, 1);

                    if (r < 0 && errno == EINTR)
                            continue;
                    if (r != 1 || n == sizeof line)
                            _exit(3);
                    line[n++] = c;
                    if (c == '\n')
                            break;
            }
            fd = open(capture, O_WRONLY | O_CREAT | O_TRUNC, 0600);
            if (fd < 0 || write(fd, line, n) != (ssize_t)n)
                    _exit(4);
            close(fd);
            if (write(reply_fd, reply, strlen(reply)) != (ssize_t)strlen(reply))
                    _exit(5);
            for (;;)
                    pause();
    }

    struct outcome {
            int status;
            char result[128];
            char forwarded[BUF_MAX];
            char unread[BUF_MAX];
    };

    /*
     * Runs relay as euid against a fake holder under dir. reply is the holder's
     * answer, or NULL for a holder that never answers. stale, when set, is
     * written to the reply FIFO before the relay runs. forwarded holds every
     * byte that reached the request FIFO, and unread the stdin bytes left over.
     */
    static void run_case(struct outcome *o, uid_t euid, const char *dir, const char *input,
                         const char *stale, const char *reply, int timeout_ms)
    {
            char request_path[PATH_MAX], reply_path[PATH_MAX], capture_path[PATH_MAX];
            int request_fd, reply_fd, in;
            pid_t holder = -1;
            size_t n;

            make_fixture(dir);
            path_in(request_path, sizeof request_path, dir, "request");
            path_in(reply_path, sizeof reply_path, dir, "reply");
            path_in(capture_path, sizeof capture_path, dir, "capture");
            request_fd = open(request_path, O_RDWR);
            reply_fd = open(reply_path, O_RDWR);
            if (request_fd < 0 || reply_fd < 0)
                    die_sys("open fixture");
            if (stale != NULL && write(reply_fd, stale, strlen(stale)) != (ssize_t)strlen(stale))
                    die_sys("write stale reply");
            if (reply != NULL) {
                    holder = fork();
                    if (holder < 0)
                            die_sys("fork");
                    if (holder == 0)
                            holder_child(request_fd, reply_fd, capture_path, reply);
            }
            in = input_from(input);
            o->status = relay(euid, in, dir, timeout_ms, o->result, sizeof o->result);
            drain(in, o->unread, sizeof o->unread);
            close(in);
            n = slurp(capture_path, o->forwarded, sizeof o->forwarded);
            n += slurp(request_path, o->forwarded + n, sizeof o->forwarded - n);
            if (holder > 0) {
                    int st;

                    (void)kill(holder, SIGKILL);
                    must(waitpid(holder, &st, 0) == holder, "waitpid holder");
            }
            close(request_fd);
            close(reply_fd);
    }

    static void test_non_root_refuses(void)
    {
            struct outcome o;

            run_case(&o, 1000, "t-non-root", "alpha\n", NULL, "ok 0\n", HOLDER_WAIT_MS);
            CHECK(o.status == 1);
            CHECK(strcmp(o.result, "error not root") == 0);
            CHECK(strcmp(o.unread, "alpha\n") == 0);
            CHECK(o.forwarded[0] == '\0');
    }

    static void test_forwards_first_line_only(void)
    {
            struct outcome o;

            run_case(&o, 0, "t-forward", "alpha\nbeta\n", NULL, "ok 3\n", HOLDER_WAIT_MS);
            CHECK(o.status == 0);
            CHECK(strcmp(o.result, "ok 3") == 0);
            CHECK(strcmp(o.forwarded, "alpha\n") == 0);
            CHECK(strcmp(o.unread, "beta\n") == 0);
    }

    static void test_discards_stale_reply(void)
    {
            struct outcome o;

            run_case(&o, 0, "t-stale", "gamma\n", "ok 99\n", "ok 4\n", HOLDER_WAIT_MS);
            CHECK(o.status == 0);
            CHECK(strcmp(o.result, "ok 4") == 0);
            CHECK(strcmp(o.forwarded, "gamma\n") == 0);
    }

    static void test_holder_error_reply_sets_status(void)
    {
            struct outcome o;

            run_case(&o, 0, "t-holder-error", "alpha\n", NULL, "error bad username\n", HOLDER_WAIT_MS);
            CHECK(o.status == 1);
            CHECK(strcmp(o.result, "error bad username") == 0);
    }

    static void test_over_long_refused(void)
    {
            char input[256];
            struct outcome o;

            memset(input, 'a', 129);
            input[129] = '\n';
            input[130] = '\0';
            run_case(&o, 0, "t-over-long", input, NULL, "ok 0\n", HOLDER_WAIT_MS);
            CHECK(o.status == 1);
            CHECK(strcmp(o.result, "error line too long") == 0);
            CHECK(o.forwarded[0] == '\0');
            CHECK(strstr(o.result, "aaaa") == NULL);
    }

    static void test_eof_without_newline_refused(void)
    {
            struct outcome o;

            run_case(&o, 0, "t-eof", "alpha", NULL, "ok 0\n", HOLDER_WAIT_MS);
            CHECK(o.status == 1);
            CHECK(strcmp(o.result, "error no newline") == 0);
            CHECK(o.forwarded[0] == '\0');
            CHECK(strstr(o.result, "alpha") == NULL);
    }

    static void test_missing_holder(void)
    {
            char result[128];
            int in;
            int status;

            make_fixture("t-no-reader");
            in = input_from("alpha\n");
            status = relay(0, in, "t-no-reader", HOLDER_WAIT_MS, result, sizeof result);
            close(in);
            CHECK(status == 1);
            CHECK(strcmp(result, "error no holder") == 0);
            CHECK(strstr(result, "alpha") == NULL);

            in = input_from("alpha\n");
            status = relay(0, in, "t-absent", HOLDER_WAIT_MS, result, sizeof result);
            close(in);
            CHECK(status == 1);
            CHECK(strcmp(result, "error no holder") == 0);
    }

    static void test_reply_timeout(void)
    {
            struct outcome o;

            run_case(&o, 0, "t-timeout", "alpha\n", NULL, NULL, PROBE_TIMEOUT_MS);
            CHECK(o.status == 1);
            CHECK(strcmp(o.result, "error no holder reply") == 0);
            CHECK(strcmp(o.forwarded, "alpha\n") == 0);
            CHECK(strstr(o.result, "alpha") == NULL);
    }

    static void test_usage_refused(void)
    {
            char *argv[] = { "lab-probe-type", "extra", NULL };
            char out[64];
            int saved = dup(STDOUT_FILENO);
            int fd = open("t-usage.out", O_WRONLY | O_CREAT | O_TRUNC, 0600);
            int status;

            must(saved >= 0 && fd >= 0, "usage capture");
            must(dup2(fd, STDOUT_FILENO) >= 0, "redirect stdout");
            close(fd);
            status = lab_probe_type_main(2, argv);
            fflush(stdout);
            must(dup2(saved, STDOUT_FILENO) >= 0, "restore stdout");
            close(saved);
            CHECK(status == 1);
            CHECK(slurp("t-usage.out", out, sizeof out) == 12);
            CHECK(strcmp(out, "error usage\n") == 0);
    }

    int main(void)
    {
            test_non_root_refuses();
            test_forwards_first_line_only();
            test_discards_stale_reply();
            test_holder_error_reply_sets_status();
            test_over_long_refused();
            test_eof_without_newline_refused();
            test_missing_holder();
            test_reply_timeout();
            test_usage_refused();

            if (failures != 0) {
                    fprintf(stderr, "lab-probe-type: %d of %d checks failed\n", failures, checks);
                    return 1;
            }
            printf("lab-probe-type: %d checks passed\n", checks);
            return 0;
    }
  '';
in

stdenv.mkDerivation {
  pname = "lab-probe-type";
  version = "0";

  src = ./lab-probe-type.c;

  dontUnpack = true;
  dontConfigure = true;

  doCheck = true;

  buildPhase = ''
    runHook preBuild
    $CC -Wall -Wextra -Werror -O2 $src -o lab-probe-type
    runHook postBuild
  '';

  checkPhase = ''
    runHook preCheck
    {
      printf '#define _GNU_SOURCE\n#define main lab_probe_type_main\n#include "%s"\n#undef main\n' "$src"
      cat ${harness}
    } > lab-probe-type-test.c
    $CC -Wall -Wextra -Werror -O2 lab-probe-type-test.c -o lab-probe-type-test
    ./lab-probe-type-test
    runHook postCheck
  '';

  installPhase = ''
    runHook preInstall
    install -Dm755 lab-probe-type $out/bin/lab-probe-type
    runHook postInstall
  '';

  meta = {
    description = "root-only relay from stdin to the lab keyboard holder's request FIFO";
    mainProgram = "lab-probe-type";
  };
}
