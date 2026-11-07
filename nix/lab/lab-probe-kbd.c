/*
 * lab-probe-kbd: uinput keyboard holder for the gnomon lab's early-boot probe.
 *
 * Creates the virtual keyboard "lab-probe keyboard" once and holds it for the
 * life of the boot. Each request line on the request FIFO makes it type the
 * probe sequence, and each request gets one reply line on the reply FIFO.
 *
 *   request: <username>\n                 0 to 32 bytes of a-z; the fixed
 *                                         password is typed
 *            <username> <password>\n      one space separates the fields; the
 *                                         username may be empty, the password
 *                                         is 1 to 64 bytes of a-z and is typed
 *                                         in place of the fixed password
 *   reply:   ok <n>\n                     n is the number of key presses typed
 *            error <reason>\n
 *
 * The holder never echoes or logs request bytes, and wipes its request buffers
 * after each request.
 *
 * /dev/uinput and both FIFOs are opened once and kept for the life of the
 * process. Paths are never reopened: after switch-root the holder's root is the
 * initramfs root, where /run and /dev/uinput no longer resolve.
 */
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <linux/input.h>
#include <linux/uinput.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <sys/un.h>
#include <time.h>
#include <unistd.h>

#define ARRAY_SIZE(a) (sizeof(a) / sizeof((a)[0]))

#define DEVICE_NAME "lab-probe keyboard"
#define DEVICE_VENDOR 0x0001
#define DEVICE_PRODUCT 0x0001
#define DEVICE_VERSION 0x0001

#define REQUEST_DIR "/run/lab-probe-kbd"
#define USERNAME_MAX 32
#define PASSWORD_MAX 64
#define CLEAR_PRESSES 32
#define KEY_PACE_NS (20L * 1000000L)

static const char FIXED_PASSWORD[] = "wrongprobe";
static const char READY[] = "READY=1\n";

static const int letter_codes[26] = {
	KEY_A, KEY_B, KEY_C, KEY_D, KEY_E, KEY_F, KEY_G, KEY_H, KEY_I,
	KEY_J, KEY_K, KEY_L, KEY_M, KEY_N, KEY_O, KEY_P, KEY_Q, KEY_R,
	KEY_S, KEY_T, KEY_U, KEY_V, KEY_W, KEY_X, KEY_Y, KEY_Z,
};

/*
 * One request as read from the request FIFO. A zero password_len means the
 * line named no password, and the fixed password is typed.
 */
struct request {
	char username[USERNAME_MAX];
	char password[PASSWORD_MAX];
	size_t username_len;
	size_t password_len;
};

enum request_field { USERNAME_FIELD, PASSWORD_FIELD, EXTRA_FIELD };

static _Noreturn void fatal(const char *what, int err)
{
	if (err != 0)
		fprintf(stderr, "lab-probe-kbd: %s: %s\n", what, strerror(err));
	else
		fprintf(stderr, "lab-probe-kbd: %s\n", what);
	exit(EXIT_FAILURE);
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

static void pause_key(void)
{
	struct timespec ts = { 0, KEY_PACE_NS };

	while (nanosleep(&ts, &ts) != 0) {
		if (errno != EINTR)
			fatal("nanosleep", errno);
	}
}

static void emit(int fd, uint16_t type, uint16_t code, int32_t value)
{
	struct input_event ev;
	int err;

	memset(&ev, 0, sizeof ev);
	ev.type = type;
	ev.code = code;
	ev.value = value;
	err = write_all(fd, &ev, sizeof ev);
	if (err != 0)
		fatal("write /dev/uinput", err);
}

/* One key press as the probe types it: press, SYN, release, SYN, then a pause. */
static void tap(int fd, int code)
{
	emit(fd, EV_KEY, (uint16_t)code, 1);
	emit(fd, EV_SYN, SYN_REPORT, 0);
	emit(fd, EV_KEY, (uint16_t)code, 0);
	emit(fd, EV_SYN, SYN_REPORT, 0);
	pause_key();
}

static int letter_code(char c)
{
	if (c < 'a' || c > 'z')
		fatal("no key for character", 0);
	return letter_codes[c - 'a'];
}

static size_t type_text(int fd, const char *text, size_t len)
{
	for (size_t i = 0; i < len; i++)
		tap(fd, letter_code(text[i]));
	return len;
}

/*
 * Types the probe: clear the field with BackSpace, then the username and Enter
 * when there is one, then the password and Enter. The password is the one the
 * request named, or the fixed password when it named none. Returns the number
 * of key presses typed.
 */
static size_t type_probe(int fd, const struct request *req)
{
	size_t typed = 0;

	for (size_t i = 0; i < CLEAR_PRESSES; i++) {
		tap(fd, KEY_BACKSPACE);
		typed++;
	}
	if (req->username_len > 0) {
		typed += type_text(fd, req->username, req->username_len);
		tap(fd, KEY_ENTER);
		typed++;
	}
	if (req->password_len > 0)
		typed += type_text(fd, req->password, req->password_len);
	else
		typed += type_text(fd, FIXED_PASSWORD, strlen(FIXED_PASSWORD));
	tap(fd, KEY_ENTER);
	typed++;
	return typed;
}

static void set_key_bit(int fd, int code)
{
	if (ioctl(fd, UI_SET_KEYBIT, code) < 0)
		fatal("UI_SET_KEYBIT", errno);
}

static void create_keyboard(int fd)
{
	struct uinput_setup setup;

	if (ioctl(fd, UI_SET_EVBIT, EV_KEY) < 0)
		fatal("UI_SET_EVBIT", errno);
	for (size_t i = 0; i < ARRAY_SIZE(letter_codes); i++)
		set_key_bit(fd, letter_codes[i]);
	set_key_bit(fd, KEY_ENTER);
	set_key_bit(fd, KEY_BACKSPACE);

	memset(&setup, 0, sizeof setup);
	setup.id.bustype = BUS_VIRTUAL;
	setup.id.vendor = DEVICE_VENDOR;
	setup.id.product = DEVICE_PRODUCT;
	setup.id.version = DEVICE_VERSION;
	snprintf(setup.name, sizeof setup.name, "%s", DEVICE_NAME);
	if (ioctl(fd, UI_DEV_SETUP, &setup) < 0)
		fatal("UI_DEV_SETUP", errno);
	if (ioctl(fd, UI_DEV_CREATE) < 0)
		fatal("UI_DEV_CREATE", errno);
}

static void join_path(char *out, size_t size, const char *dir, const char *leaf)
{
	int n = snprintf(out, size, "%s/%s", dir, leaf);

	if (n < 0 || (size_t)n >= size)
		fatal("path too long", ENAMETOOLONG);
}

/* Replaces whatever is at path with a fresh FIFO. */
static void make_fifo(const char *path)
{
	if (unlink(path) != 0 && errno != ENOENT)
		fatal("unlink stale fifo", errno);
	if (mkfifo(path, 0600) != 0)
		fatal("mkfifo", errno);
}

/*
 * Creates dir (0700) and its request and reply FIFOs (0600), then opens both
 * O_RDWR so that opening never blocks and reads never see EOF.
 */
static void open_request_fifos(const char *dir, int *request_fd, int *reply_fd)
{
	char request[PATH_MAX];
	char reply[PATH_MAX];

	if (mkdir(dir, 0700) != 0 && errno != EEXIST)
		fatal("mkdir", errno);
	join_path(request, sizeof request, dir, "request");
	join_path(reply, sizeof reply, dir, "reply");
	make_fifo(request);
	make_fifo(reply);
	*request_fd = open(request, O_RDWR | O_CLOEXEC);
	if (*request_fd < 0)
		fatal("open request fifo", errno);
	*reply_fd = open(reply, O_RDWR | O_CLOEXEC);
	if (*reply_fd < 0)
		fatal("open reply fifo", errno);
}

/*
 * Reads one request line into req and returns NULL when it is valid, or the
 * fixed reason it is rejected. A rejected line is consumed through its newline.
 * When a line has several faults, the reason is the first of these that applies:
 * username too long, bad username, too many fields, password too long,
 * bad password, missing password.
 */
static const char *read_request(int fd, struct request *req)
{
	enum request_field field = USERNAME_FIELD;
	bool user_long = false;
	bool user_bad = false;
	bool too_many = false;
	bool pass_long = false;
	bool pass_bad = false;

	req->username_len = 0;
	req->password_len = 0;
	for (;;) {
		char c;
		ssize_t r = read(fd, &c, 1);

		if (r < 0) {
			if (errno == EINTR)
				continue;
			fatal("read request", errno);
		}
		if (r == 0)
			fatal("request fifo closed", 0);
		if (c == '\n')
			break;
		if (c == ' ') {
			if (field == USERNAME_FIELD) {
				field = PASSWORD_FIELD;
			} else {
				too_many = true;
				field = EXTRA_FIELD;
			}
			continue;
		}
		switch (field) {
		case USERNAME_FIELD:
			if (req->username_len == USERNAME_MAX) {
				user_long = true;
				break;
			}
			if (c < 'a' || c > 'z')
				user_bad = true;
			req->username[req->username_len++] = c;
			break;
		case PASSWORD_FIELD:
			if (req->password_len == PASSWORD_MAX) {
				pass_long = true;
				break;
			}
			if (c < 'a' || c > 'z')
				pass_bad = true;
			req->password[req->password_len++] = c;
			break;
		case EXTRA_FIELD:
			break;
		}
	}
	if (user_long)
		return "username too long";
	if (user_bad)
		return "bad username";
	if (too_many)
		return "too many fields";
	if (pass_long)
		return "password too long";
	if (pass_bad)
		return "bad password";
	if (field == PASSWORD_FIELD && req->password_len == 0)
		return "missing password";
	return NULL;
}

static void serve_one(int request_fd, int reply_fd, int uinput_fd, struct request *req)
{
	char reply[64];
	const char *reason = read_request(request_fd, req);
	int n;
	int err;

	if (reason != NULL)
		n = snprintf(reply, sizeof reply, "error %s\n", reason);
	else
		n = snprintf(reply, sizeof reply, "ok %zu\n",
			     type_probe(uinput_fd, req));
	explicit_bzero(req, sizeof *req);
	err = write_all(reply_fd, reply, (size_t)n);
	if (err != 0)
		fatal("write reply", err);
}

/*
 * Sends READY=1 to the socket named by NOTIFY_SOCKET, if set. A leading '@'
 * names an abstract socket.
 */
static void notify_ready(void)
{
	const char *path = getenv("NOTIFY_SOCKET");
	struct sockaddr_un addr;
	size_t n;
	socklen_t alen;
	int fd;

	if (path == NULL || path[0] == '\0')
		return;
	n = strlen(path);
	if (n >= sizeof addr.sun_path)
		fatal("NOTIFY_SOCKET too long", ENAMETOOLONG);

	memset(&addr, 0, sizeof addr);
	addr.sun_family = AF_UNIX;
	if (path[0] == '@') {
		memcpy(addr.sun_path + 1, path + 1, n - 1);
		alen = offsetof(struct sockaddr_un, sun_path) + n;
	} else {
		memcpy(addr.sun_path, path, n);
		alen = offsetof(struct sockaddr_un, sun_path) + n + 1;
	}

	fd = socket(AF_UNIX, SOCK_DGRAM | SOCK_CLOEXEC, 0);
	if (fd < 0)
		fatal("socket", errno);
	if (sendto(fd, READY, sizeof READY - 1, 0, (struct sockaddr *)&addr, alen) < 0)
		fatal("sendto NOTIFY_SOCKET", errno);
	close(fd);
}

int main(void)
{
	struct request req = { { 0 }, { 0 }, 0, 0 };
	int uinput_fd;
	int request_fd;
	int reply_fd;

	uinput_fd = open("/dev/uinput", O_WRONLY | O_NONBLOCK | O_CLOEXEC);
	if (uinput_fd < 0)
		fatal("open /dev/uinput", errno);
	create_keyboard(uinput_fd);
	open_request_fifos(REQUEST_DIR, &request_fd, &reply_fd);
	notify_ready();
	for (;;)
		serve_one(request_fd, reply_fd, uinput_fd, &req);
}
