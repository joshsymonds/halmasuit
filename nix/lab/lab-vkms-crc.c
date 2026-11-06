/*
 * lab-vkms-crc: vkms scanout CRC sampler for the lab VM gate.
 *
 * Opens the first /dev/dri card whose DRM driver name is "vkms", drops DRM
 * master right after opening, and samples the CRC of every CRTC through
 * debugfs (/sys/kernel/debug/dri/<minor>/crtc-<index>/crc). A CRTC is active
 * when KMS reports a valid mode and its data file opens; an inactive CRTC is
 * logged with active=0 and crc=00000000. Prints the frozen lab-vkms-crc
 * journal lines on stdout and failures on stderr.
 *
 * The debugfs crtc-<index> directory is named by the CRTC's position in the
 * KMS CRTC list, which drmModeGetResources reports in order.
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
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <time.h>
#include <unistd.h>

#include <xf86drm.h>
#include <xf86drmMode.h>

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

struct crtc {
	uint32_t id;
	uint32_t index;
	int data_fd;
	bool has_sample;
	bool active;
	bool error_reported;
	uint32_t crc;
};

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

static bool is_card_name(const char *name)
{
	const char *digits = name + strlen("card");

	if (strncmp(name, "card", strlen("card")) != 0 || *digits == '\0')
		return false;
	for (; *digits != '\0'; digits++) {
		if (*digits < '0' || *digits > '9')
			return false;
	}
	return true;
}

static bool is_vkms_fd(int fd)
{
	drmVersionPtr version = drmGetVersion(fd);
	bool match;

	if (version == NULL)
		return false;
	match = strcmp(version->name, "vkms") == 0;
	drmFreeVersion(version);
	return match;
}

/*
 * Returns the fd of the first vkms card, with its name copied into card, or
 * -1 when no vkms card is present yet. Other cards are closed unchanged.
 */
static int open_vkms_card(char *card, size_t card_len)
{
	DIR *dir;
	struct dirent *ent;
	int fd = -1;

	dir = opendir("/dev/dri");
	if (dir == NULL)
		return -1;
	while ((ent = readdir(dir)) != NULL) {
		char path[sizeof "/dev/dri/" + NAME_MAX];

		if (!is_card_name(ent->d_name))
			continue;
		snprintf(path, sizeof path, "/dev/dri/%s", ent->d_name);
		fd = open(path, O_RDWR | O_CLOEXEC);
		if (fd < 0)
			continue;
		if (is_vkms_fd(fd)) {
			snprintf(card, card_len, "%s", ent->d_name);
			break;
		}
		close(fd);
		fd = -1;
	}
	closedir(dir);
	return fd;
}

static int wait_for_vkms_card(char *card, size_t card_len)
{
	int fd;

	while ((fd = open_vkms_card(card, card_len)) < 0)
		sleep_until(add_ns(now_mono(), CARD_SCAN_PERIOD_NS));
	return fd;
}

static struct crtc *read_crtcs(int fd, size_t *count)
{
	drmModeResPtr res;
	struct crtc *crtcs;

	res = drmModeGetResources(fd);
	if (res == NULL)
		fatal("drmModeGetResources", errno);
	if (res->count_crtcs <= 0)
		fatal("vkms card reports no CRTCs", 0);

	*count = (size_t)res->count_crtcs;
	crtcs = calloc(*count, sizeof *crtcs);
	if (crtcs == NULL)
		fatal("calloc", errno);
	for (size_t i = 0; i < *count; i++) {
		crtcs[i].id = res->crtcs[i];
		crtcs[i].index = (uint32_t)i;
		crtcs[i].data_fd = -1;
	}
	drmModeFreeResources(res);
	return crtcs;
}

static bool crtc_kms_active(int fd, uint32_t id)
{
	drmModeCrtcPtr crtc = drmModeGetCrtc(fd, id);
	bool active;

	if (crtc == NULL)
		fatal("drmModeGetCrtc", errno);
	active = crtc->mode_valid != 0;
	drmModeFreeCrtc(crtc);
	return active;
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

static void sample_crtc(int card_fd, const char *card, unsigned dev_minor,
			struct crtc *c)
{
	char dir[DEBUGFS_DIR_LEN];
	char line[CRC_LINE_LEN];
	uint32_t crc;
	ssize_t n;
	int err;

	if (!crtc_kms_active(card_fd, c->id)) {
		close_data(c);
		observe(card, c, false, 0, monotonic_us());
		return;
	}

	snprintf(dir, sizeof dir, "/sys/kernel/debug/dri/%u/crtc-%" PRIu32,
		 dev_minor, c->index);
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

static _Noreturn void sample_forever(int card_fd, const char *card,
				     unsigned dev_minor, struct crtc *crtcs,
				     size_t count)
{
	for (;;) {
		for (size_t i = 0; i < count; i++)
			sample_crtc(card_fd, card, dev_minor, &crtcs[i]);
		if (ferror(stdout))
			fatal("stdout write", errno);
		sleep_until(add_ns(now_mono(), PACING_NS));
	}
}

int main(void)
{
	char card[NAME_MAX + 1];
	struct stat st;
	struct crtc *crtcs;
	size_t count;
	int card_fd;

	if (setvbuf(stdout, NULL, _IOLBF, 0) != 0)
		fatal("setvbuf", errno);
	printf("start t=%" PRIu64 "\n", monotonic_us());

	card_fd = wait_for_vkms_card(card, sizeof card);
	if (drmIsMaster(card_fd) && drmDropMaster(card_fd) != 0)
		fatal("drmDropMaster", errno);
	if (fstat(card_fd, &st) != 0)
		fatal("fstat", errno);

	crtcs = read_crtcs(card_fd, &count);
	sample_forever(card_fd, card, minor(st.st_rdev), crtcs, count);
}
