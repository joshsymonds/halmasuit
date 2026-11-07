/*
 * lab-probe-type: root-only relay from stdin to the lab keyboard holder.
 *
 * Reads one request line from stdin and forwards it byte-exact to the holder's
 * request FIFO, then prints the holder's reply line on stdout. The holder
 * validates the request; this command does not. Nothing of the request is
 * logged or printed, and every error line carries a fixed reason.
 *
 *   stdin:   <request>\n    at most 128 bytes before the newline; nothing after it is read
 *   stdout:  <holder reply line>, or error <reason>
 *
 * Exit status is 0 only when the holder's reply starts with "ok ".
 */
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <poll.h>
#include <signal.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/types.h>
#include <time.h>
#include <unistd.h>

#define REQUEST_DIR "/run/lab-probe-kbd"
#define REQUEST_MAX 128
#define RESULT_MAX 128
#define REPLY_TIMEOUT_MS 30000

/* Writes error <reason> as the result line and returns the exit status. */
static int fail(char *result, size_t size, const char *reason)
{
	snprintf(result, size, "error %s", reason);
	return EXIT_FAILURE;
}

/* Returns 0, or the errno that stopped the write. */
static int write_all(int fd, const void *buf, size_t len)
{
	const char *p = buf;

	while (len > 0) {
		ssize_t n = write(fd, p, len);

		if (n < 0) {
			if (errno == EINTR)
				continue;
			return errno;
		}
		if (n == 0)
			return EIO;
		p += n;
		len -= (size_t)n;
	}
	return 0;
}

static bool join_path(char *out, size_t size, const char *dir, const char *leaf)
{
	int n = snprintf(out, size, "%s/%s", dir, leaf);

	return n >= 0 && (size_t)n < size;
}

/*
 * Reads stdin one byte at a time through the first newline, so nothing after
 * it is consumed. Returns NULL with *len set to the line length including the
 * newline, or the fixed reason the line is refused.
 */
static const char *read_line(int fd, char line[REQUEST_MAX + 1], size_t *len)
{
	size_t n = 0;

	for (;;) {
		char c;
		ssize_t r = read(fd, &c, 1);

		if (r < 0) {
			if (errno == EINTR)
				continue;
			return "read stdin";
		}
		if (r == 0)
			return "no newline";
		if (c == '\n') {
			line[n++] = c;
			*len = n;
			return NULL;
		}
		if (n == REQUEST_MAX)
			return "line too long";
		line[n++] = c;
	}
}

/* Drops replies left in fd by an earlier request. */
static void discard_replies(int fd)
{
	char scratch[RESULT_MAX];

	for (;;) {
		ssize_t r = read(fd, scratch, sizeof scratch);

		if (r < 0 && errno == EINTR)
			continue;
		if (r <= 0)
			break;
	}
}

/*
 * Reads the holder's reply into result up to its first newline, waiting at
 * most timeout_ms in total. Returns the exit status for the reply.
 */
static int await_reply(int fd, int timeout_ms, char *result, size_t size)
{
	struct timespec start;
	size_t n = 0;

	if (clock_gettime(CLOCK_MONOTONIC, &start) != 0)
		return fail(result, size, "clock");
	for (;;) {
		struct timespec now;
		struct pollfd pfd = { .fd = fd, .events = POLLIN };
		long elapsed_ms;
		int ready;
		ssize_t r;
		char *end;

		if (clock_gettime(CLOCK_MONOTONIC, &now) != 0)
			return fail(result, size, "clock");
		elapsed_ms = (now.tv_sec - start.tv_sec) * 1000L +
			     (now.tv_nsec - start.tv_nsec) / 1000000L;
		if (elapsed_ms >= timeout_ms)
			return fail(result, size, "no holder reply");
		ready = poll(&pfd, 1, (int)(timeout_ms - elapsed_ms));
		if (ready < 0) {
			if (errno == EINTR)
				continue;
			return fail(result, size, "poll reply");
		}
		if (ready == 0)
			return fail(result, size, "no holder reply");
		r = read(fd, result + n, size - 1 - n);
		if (r < 0) {
			if (errno == EAGAIN || errno == EINTR)
				continue;
			return fail(result, size, "read reply");
		}
		if (r == 0)
			return fail(result, size, "no holder reply");
		end = memchr(result + n, '\n', (size_t)r);
		if (end != NULL) {
			*end = '\0';
			return strncmp(result, "ok ", 3) == 0 ? EXIT_SUCCESS : EXIT_FAILURE;
		}
		n += (size_t)r;
		if (n == size - 1)
			return fail(result, size, "reply too long");
	}
}

/*
 * Writes the request line to the holder under dir and awaits its reply. The
 * reply FIFO is opened before the request is written so the reply cannot be
 * lost. The request is written once and nothing else is sent.
 */
static int forward(const char *line, size_t len, const char *dir, int timeout_ms,
		   char *result, size_t size)
{
	char request_path[PATH_MAX];
	char reply_path[PATH_MAX];
	int request_fd;
	int reply_fd;
	int err;
	int status;

	if (!join_path(request_path, sizeof request_path, dir, "request") ||
	    !join_path(reply_path, sizeof reply_path, dir, "reply"))
		return fail(result, size, "path too long");
	reply_fd = open(reply_path, O_RDONLY | O_NONBLOCK | O_CLOEXEC);
	if (reply_fd < 0)
		return fail(result, size, errno == ENOENT ? "no holder" : "open reply");
	discard_replies(reply_fd);
	request_fd = open(request_path, O_WRONLY | O_NONBLOCK | O_CLOEXEC);
	if (request_fd < 0) {
		err = errno;
		close(reply_fd);
		return fail(result, size,
			    (err == ENOENT || err == ENXIO) ? "no holder" : "open request");
	}
	err = write_all(request_fd, line, len);
	close(request_fd);
	if (err != 0) {
		close(reply_fd);
		return fail(result, size, "write request");
	}
	status = await_reply(reply_fd, timeout_ms, result, size);
	close(reply_fd);
	return status;
}

/*
 * Relays one request line from in_fd to the holder under dir and writes the
 * single result line into result. Returns the exit status. The request buffer
 * is wiped before return on every path.
 */
static int relay(uid_t euid, int in_fd, const char *dir, int timeout_ms, char *result,
		 size_t result_size)
{
	char line[REQUEST_MAX + 1];
	size_t len = 0;
	const char *reason;
	int status;

	if (euid != 0)
		return fail(result, result_size, "not root");
	reason = read_line(in_fd, line, &len);
	if (reason != NULL)
		status = fail(result, result_size, reason);
	else
		status = forward(line, len, dir, timeout_ms, result, result_size);
	explicit_bzero(line, sizeof line);
	return status;
}

int main(int argc, char **argv)
{
	char result[RESULT_MAX];
	int status;

	(void)argv;
	if (argc != 1)
		status = fail(result, sizeof result, "usage");
	else if (signal(SIGPIPE, SIG_IGN) == SIG_ERR)
		status = fail(result, sizeof result, "signal");
	else
		status = relay(geteuid(), STDIN_FILENO, REQUEST_DIR, REPLY_TIMEOUT_MS, result,
			       sizeof result);
	if (puts(result) == EOF || fflush(stdout) == EOF)
		return EXIT_FAILURE;
	return status;
}
