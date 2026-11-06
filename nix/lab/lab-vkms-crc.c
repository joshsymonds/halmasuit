/*
 * lab-vkms-crc: vkms scanout CRC sampler for the lab VM gate.
 *
 * Finds the vkms device through debugfs: the dri/<N> entry whose name file
 * reads "vkms" is the device. Its CRTC ids, their order, and their atomic
 * active state come from dri/<N>/state. The sampler opens no /dev/dri node, so
 * it never takes or blocks DRM master.
 *
 * A CRTC is sampled through dri/<N>/crtc-<index>/crc. An inactive CRTC is
 * logged with active=0 and crc=00000000. Prints the frozen lab-vkms-crc journal
 * lines on stdout and failures on stderr.
 *
 * The primary minor <N> is /dev/dri/card<N>, so the card name is card<N>. The
 * debugfs crtc-<index> directory is named by the CRTC's position in the KMS
 * CRTC list, which the state file prints in order.
 */
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <limits.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#define DEBUGFS_DRI "/sys/kernel/debug/dri"
#define NS_PER_SEC 1000000000L
#define CARD_SCAN_PERIOD_NS (100L * 1000000L)
#define PACING_NS (2L * 1000000L)
#define CRC_LINE_LEN 23
/*
 * The longest debugfs CRTC directory is 48 bytes (a 10-digit minor and index),
 * and the longest file path is that directory plus "/crc/control".
 */
#define DEBUGFS_DIR_LEN 64
#define DEBUGFS_PATH_LEN 256
#define DEBUGFS_NAME_LEN 256
#define STATE_LEN 65536

struct crtc {
	uint32_t id;
	uint32_t index;
	int data_fd;
	bool has_sample;
	bool kms_active;
	bool active;
	bool error_reported;
	uint32_t crc;
};

static char state_text[STATE_LEN];

static _Noreturn void fatal(const char *what, int err)
{
	if (err != 0)
		fprintf(stderr, "lab-vkms-crc: %s: %s\n", what, strerror(err));
	else
		fprintf(stderr, "lab-vkms-crc: %s\n", what);
	exit(EXIT_FAILURE);
}

static struct timespec now_mono(void)
{
	struct timespec ts;

	if (clock_gettime(CLOCK_MONOTONIC, &ts) != 0)
		fatal("clock_gettime", errno);
	return ts;
}

static uint64_t monotonic_us(void)
{
	struct timespec ts = now_mono();

	return (uint64_t)ts.tv_sec * 1000000u + (uint64_t)ts.tv_nsec / 1000u;
}

static struct timespec add_ns(struct timespec t, long ns)
{
	t.tv_nsec += ns;
	t.tv_sec += t.tv_nsec / NS_PER_SEC;
	t.tv_nsec %= NS_PER_SEC;
	return t;
}

static void sleep_until(struct timespec deadline)
{
	int rc;

	do {
		rc = clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME, &deadline, NULL);
	} while (rc == EINTR);
	if (rc != 0)
		fatal("clock_nanosleep", rc);
}

static bool is_minor_name(const char *name)
{
	if (*name == '\0')
		return false;
	for (; *name != '\0'; name++) {
		if (*name < '0' || *name > '9')
			return false;
	}
	return true;
}

/*
 * Reads the whole file into buf and NUL-terminates it. Returns the length, or
 * -1 with errno set; EFBIG means the contents do not fit in len - 1 bytes.
 */
static ssize_t read_file(const char *path, char *buf, size_t len)
{
	size_t total = 0;
	int fd, err = 0;

	fd = open(path, O_RDONLY | O_CLOEXEC);
	if (fd < 0)
		return -1;
	for (;;) {
		ssize_t n;

		if (total == len - 1) {
			err = EFBIG;
			break;
		}
		n = read(fd, buf + total, len - 1 - total);
		if (n < 0) {
			err = errno;
			break;
		}
		if (n == 0)
			break;
		total += (size_t)n;
	}
	close(fd);
	if (err != 0) {
		errno = err;
		return -1;
	}
	buf[total] = '\0';
	return (ssize_t)total;
}

static bool is_vkms_name(const char *name)
{
	size_t len = strlen("vkms");

	return strncmp(name, "vkms", len) == 0 &&
	       (name[len] == '\0' || name[len] == ' ' || name[len] == '\n');
}

/*
 * Sets minor to the dri/<N> entry whose name file reads "vkms" and returns
 * true, or returns false when no vkms device is registered yet.
 */
static bool find_vkms_minor(unsigned *minor)
{
	DIR *dir;
	struct dirent *ent;
	bool found = false;

	dir = opendir(DEBUGFS_DRI);
	if (dir == NULL)
		return false;
	while (!found && (ent = readdir(dir)) != NULL) {
		char path[sizeof DEBUGFS_DRI + NAME_MAX + sizeof "/name"];
		char name[DEBUGFS_NAME_LEN];
		unsigned long value;
		ssize_t n;

		if (!is_minor_name(ent->d_name))
			continue;
		value = strtoul(ent->d_name, NULL, 10);
		if (value > UINT_MAX)
			continue;
		snprintf(path, sizeof path, "%s/%s/name", DEBUGFS_DRI, ent->d_name);
		n = read_file(path, name, sizeof name);
		if (n < 0) {
			if (errno != ENOENT)
				fatal("read name", errno);
			continue;
		}
		if (is_vkms_name(name)) {
			*minor = (unsigned)value;
			found = true;
		}
	}
	closedir(dir);
	return found;
}

static unsigned wait_for_vkms_minor(void)
{
	unsigned minor = 0;

	while (!find_vkms_minor(&minor))
		sleep_until(add_ns(now_mono(), CARD_SCAN_PERIOD_NS));
	return minor;
}

/* Reads dri/<N>/state into state_text. Returns 0, or the errno of the read. */
static int read_state(unsigned minor)
{
	char path[DEBUGFS_PATH_LEN];

	snprintf(path, sizeof path, "%s/%u/state", DEBUGFS_DRI, minor);
	if (read_file(path, state_text, sizeof state_text) < 0)
		return errno;
	return 0;
}

/*
 * Walks the state dump. A "crtc[<id>]:" line starts a CRTC block; its
 * "\tactive=" line gives the atomic state. Any other line that does not start
 * with a tab (planes, connectors, private objects) ends the block. Returns the
 * number of CRTC blocks. With crtcs non-NULL it also fills each block's id,
 * index, and kms_active, and fails unless there are exactly count blocks, each
 * with one active line.
 */
static size_t parse_state(const char *text, struct crtc *crtcs, size_t count)
{
	const char *line = text;
	size_t blocks = 0, actives = 0;
	bool in_block = false;

	while (*line != '\0') {
		const char *next = strchr(line, '\n');
		uint32_t id;
		int value;

		if (sscanf(line, "crtc[%" SCNu32 "]", &id) == 1) {
			if (crtcs != NULL) {
				if (blocks >= count)
					fatal("state has more CRTCs than were loaded", 0);
				crtcs[blocks].id = id;
				crtcs[blocks].index = (uint32_t)blocks;
			}
			blocks++;
			in_block = true;
		} else if (line[0] != '\t') {
			in_block = false;
		} else if (in_block &&
			   sscanf(line, "\tactive=%d", &value) == 1 &&
			   (value == 0 || value == 1)) {
			actives++;
			if (crtcs != NULL)
				crtcs[blocks - 1].kms_active = value == 1;
		}

		if (next == NULL)
			break;
		line = next + 1;
	}

	if (crtcs != NULL && (blocks != count || actives != count))
		fatal("unparsable CRTC state", 0);
	return blocks;
}

static struct crtc *read_crtcs(unsigned dev_minor, size_t *count)
{
	struct crtc *crtcs;
	size_t n;
	int err;

	while ((err = read_state(dev_minor)) == ENOENT)
		sleep_until(add_ns(now_mono(), CARD_SCAN_PERIOD_NS));
	if (err != 0)
		fatal("read state", err);

	n = parse_state(state_text, NULL, 0);
	if (n == 0)
		fatal("vkms card reports no CRTCs", 0);
	crtcs = calloc(n, sizeof *crtcs);
	if (crtcs == NULL)
		fatal("calloc", errno);
	for (size_t i = 0; i < n; i++)
		crtcs[i].data_fd = -1;
	parse_state(state_text, crtcs, n);
	*count = n;
	return crtcs;
}

/*
 * Arms the CRC source and opens the data file. Returns 0 with the fd, or the
 * errno of the failing step. EIO from the data open means the CRTC's atomic
 * state is inactive.
 */
static int open_crc_data(const char *dir, int *fd_out)
{
	char path[DEBUGFS_PATH_LEN];
	const char source[] = "auto";
	ssize_t n;
	int ctl, data, err = 0;

	snprintf(path, sizeof path, "%s/crc/control", dir);
	ctl = open(path, O_WRONLY | O_CLOEXEC);
	if (ctl < 0)
		return errno;
	n = write(ctl, source, strlen(source));
	if (n < 0)
		err = errno;
	else if ((size_t)n != strlen(source))
		err = EIO;
	close(ctl);
	if (err != 0)
		return err;

	snprintf(path, sizeof path, "%s/crc/data", dir);
	data = open(path, O_RDONLY | O_NONBLOCK | O_CLOEXEC);
	if (data < 0)
		return errno;
	*fd_out = data;
	return 0;
}

static void close_data(struct crtc *c)
{
	if (c->data_fd >= 0)
		close(c->data_fd);
	c->data_fd = -1;
}

static void observe(const char *card, struct crtc *c, bool active, uint32_t crc,
		    uint64_t t)
{
	bool changed = !c->has_sample || active != c->active || crc != c->crc;

	c->has_sample = true;
	c->active = active;
	c->crc = crc;
	c->error_reported = false;
	if (!changed)
		return;

	printf("sample card=%s crtc=%" PRIu32 " t=%" PRIu64 " crc=%08" PRIx32
	       " active=%d\n",
	       card, c->id, t, crc, active ? 1 : 0);
}

static void report_error(const char *card, struct crtc *c, int err)
{
	if (c->error_reported)
		return;
	printf("error card=%s crtc=%" PRIu32 " t=%" PRIu64 " errno=%d\n",
	       card, c->id, monotonic_us(), err);
	c->error_reported = true;
}

static void sample_crtc(const char *card, unsigned dev_minor, struct crtc *c)
{
	char dir[DEBUGFS_DIR_LEN];
	char line[CRC_LINE_LEN];
	uint32_t crc;
	ssize_t n;
	int err;

	if (!c->kms_active) {
		close_data(c);
		observe(card, c, false, 0, monotonic_us());
		return;
	}

	snprintf(dir, sizeof dir, "%s/%u/crtc-%" PRIu32, DEBUGFS_DRI, dev_minor,
		 c->index);
	if (c->data_fd < 0) {
		err = open_crc_data(dir, &c->data_fd);
		if (err == EIO) {
			observe(card, c, false, 0, monotonic_us());
			return;
		}
		if (err != 0) {
			report_error(card, c, err);
			return;
		}
	}

	n = read(c->data_fd, line, sizeof line);
	if (n < 0 && errno == EAGAIN)
		return;
	if (n != CRC_LINE_LEN) {
		err = n < 0 ? errno : EIO;
		close_data(c);
		report_error(card, c, err);
		return;
	}

	/* The line is "0x<frame> 0x<crc>\n"; the frame field may read XXXXXXXXXX. */
	if (sscanf(line, "%*s 0x%" SCNx32, &crc) != 1)
		fatal("unparsable CRC line", 0);
	observe(card, c, true, crc, monotonic_us());
}

static _Noreturn void sample_forever(const char *card, unsigned dev_minor,
				     struct crtc *crtcs, size_t count)
{
	for (;;) {
		int err = read_state(dev_minor);

		if (err != 0)
			fatal("read state", err);
		parse_state(state_text, crtcs, count);
		for (size_t i = 0; i < count; i++)
			sample_crtc(card, dev_minor, &crtcs[i]);
		if (ferror(stdout))
			fatal("stdout write", errno);
		sleep_until(add_ns(now_mono(), PACING_NS));
	}
}

int main(void)
{
	char card[sizeof "card" + sizeof "4294967295"];
	struct crtc *crtcs;
	unsigned dev_minor;
	size_t count;

	if (setvbuf(stdout, NULL, _IOLBF, 0) != 0)
		fatal("setvbuf", errno);
	printf("start t=%" PRIu64 "\n", monotonic_us());

	dev_minor = wait_for_vkms_minor();
	snprintf(card, sizeof card, "card%u", dev_minor);
	crtcs = read_crtcs(dev_minor, &count);
	sample_forever(card, dev_minor, crtcs, count);
}
