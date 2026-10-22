/*
 * lab-crc: per-head NVIDIA scanout CRC sampler for the gnomon lab.
 *
 * Opens only the card whose driver is nvidia, drops DRM master right after
 * opening, and samples the private DRM_IOCTL_NVIDIA_GET_CRTC_CRC32_V2 for
 * every active CRTC. Prints the frozen lab-crc journal lines on stdout.
 */
#include <ctype.h>
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
#include <sys/ioctl.h>
#include <time.h>
#include <unistd.h>

#include <drm.h>
#include <xf86drm.h>
#include <xf86drmMode.h>

#define DRM_NVIDIA_GET_CRTC_CRC32_V2 (DRM_COMMAND_BASE + 0x0c)

struct drm_nvidia_crc_tap {
	uint32_t value;
	uint8_t supported;
	uint8_t pad0;
	uint16_t pad1;
};

struct drm_nvidia_get_crtc_crc32_v2_params {
	uint32_t crtc_id;
	struct drm_nvidia_crc_tap comp;
	struct drm_nvidia_crc_tap rg;
	struct drm_nvidia_crc_tap out;
};

_Static_assert(sizeof(struct drm_nvidia_get_crtc_crc32_v2_params) == 28,
	       "drm_nvidia_get_crtc_crc32_v2_params must match the 28-byte ioctl size");

#define DRM_IOCTL_NVIDIA_GET_CRTC_CRC32_V2 \
	DRM_IOWR(DRM_NVIDIA_GET_CRTC_CRC32_V2, struct drm_nvidia_get_crtc_crc32_v2_params)

#define NS_PER_SEC 1000000000L
#define CARD_SCAN_PERIOD_NS (100L * 1000000L)
#define PACING_NS (2L * 1000000L)
#define ACTIVITY_PERIOD_NS (1000L * 1000000L)

struct head {
	uint32_t id;
	bool active;
	bool has_sample;
	bool error_reported;
	uint32_t value[3];
	bool supported[3];
};

static _Noreturn void fatal(const char *what, int err)
{
	if (err != 0)
		fprintf(stderr, "lab-crc: %s: %s\n", what, strerror(err));
	else
		fprintf(stderr, "lab-crc: %s\n", what);
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

static bool before(struct timespec a, struct timespec b)
{
	return a.tv_sec < b.tv_sec || (a.tv_sec == b.tv_sec && a.tv_nsec < b.tv_nsec);
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
		if (!isdigit((unsigned char)*digits))
			return false;
	}
	return true;
}

/*
 * A card whose sysfs driver link is missing or unreadable is not yet the
 * nvidia card, so it is treated the same as a card bound to another driver.
 */
static bool is_nvidia_card(const char *card)
{
	char link[sizeof "/sys/class/drm/" + NAME_MAX + sizeof "/device/driver"];
	char target[256];
	ssize_t n;
	const char *base;

	snprintf(link, sizeof link, "/sys/class/drm/%s/device/driver", card);
	n = readlink(link, target, sizeof target - 1);
	if (n < 0)
		return false;
	target[n] = '\0';
	base = strrchr(target, '/');
	base = (base != NULL) ? base + 1 : target;
	return strcmp(base, "nvidia") == 0;
}

/*
 * Returns the fd of the first nvidia card that opens, with its name copied
 * into card, or -1 when no nvidia card is openable yet.
 */
static int open_nvidia_card(char *card, size_t card_len)
{
	DIR *dir;
	struct dirent *ent;
	int fd = -1;

	dir = opendir("/dev/dri");
	if (dir == NULL)
		return -1;
	while ((ent = readdir(dir)) != NULL) {
		char path[sizeof "/dev/dri/" + NAME_MAX];

		if (!is_card_name(ent->d_name) || !is_nvidia_card(ent->d_name))
			continue;
		snprintf(path, sizeof path, "/dev/dri/%s", ent->d_name);
		fd = open(path, O_RDWR | O_CLOEXEC);
		if (fd >= 0) {
			snprintf(card, card_len, "%s", ent->d_name);
			break;
		}
	}
	closedir(dir);
	return fd;
}

static int wait_for_nvidia_card(char *card, size_t card_len)
{
	int fd;

	while ((fd = open_nvidia_card(card, card_len)) < 0)
		sleep_until(add_ns(now_mono(), CARD_SCAN_PERIOD_NS));
	return fd;
}

static struct head *read_heads(int fd, size_t *count)
{
	drmModeResPtr res;
	struct head *heads;

	res = drmModeGetResources(fd);
	if (res == NULL)
		fatal("drmModeGetResources", errno);
	if (res->count_crtcs <= 0)
		fatal("nvidia card reports no CRTCs", 0);

	*count = (size_t)res->count_crtcs;
	heads = calloc(*count, sizeof *heads);
	if (heads == NULL)
		fatal("calloc", errno);
	for (size_t i = 0; i < *count; i++)
		heads[i].id = res->crtcs[i];
	drmModeFreeResources(res);
	return heads;
}

static void refresh_activity(int fd, struct head *heads, size_t count)
{
	for (size_t i = 0; i < count; i++) {
		drmModeCrtcPtr crtc = drmModeGetCrtc(fd, heads[i].id);

		if (crtc == NULL)
			fatal("drmModeGetCrtc", errno);
		heads[i].active = crtc->mode_valid != 0;
		drmModeFreeCrtc(crtc);
	}
}

static void sample_head(int fd, const char *card, struct head *h)
{
	struct drm_nvidia_get_crtc_crc32_v2_params p;
	uint32_t value[3];
	bool supported[3];
	bool changed;
	uint64_t t;
	int rc;

	memset(&p, 0, sizeof p);
	p.crtc_id = h->id;
	do {
		rc = ioctl(fd, DRM_IOCTL_NVIDIA_GET_CRTC_CRC32_V2, &p);
	} while (rc < 0 && errno == EINTR);
	t = monotonic_us();

	if (rc < 0) {
		int err = errno;

		if (!h->error_reported) {
			printf("error card=%s crtc=%" PRIu32 " t=%" PRIu64 " errno=%d\n",
			       card, h->id, t, err);
			h->error_reported = true;
		}
		/* The card fd is gone: the device was removed or never valid. */
		if (err == ENODEV || err == ENXIO || err == EBADF)
			fatal("nvidia card fd lost", err);
		return;
	}

	value[0] = p.comp.value;
	value[1] = p.rg.value;
	value[2] = p.out.value;
	supported[0] = p.comp.supported != 0;
	supported[1] = p.rg.supported != 0;
	supported[2] = p.out.supported != 0;

	changed = !h->has_sample;
	for (size_t i = 0; i < 3; i++) {
		if (value[i] != h->value[i] || supported[i] != h->supported[i])
			changed = true;
	}

	h->error_reported = false;
	h->has_sample = true;
	memcpy(h->value, value, sizeof value);
	memcpy(h->supported, supported, sizeof supported);
	if (!changed)
		return;

	printf("sample card=%s crtc=%" PRIu32 " t=%" PRIu64
	       " comp=%08" PRIx32 " rg=%08" PRIx32 " out=%08" PRIx32
	       " comp_sup=%d rg_sup=%d out_sup=%d\n",
	       card, h->id, t, value[0], value[1], value[2],
	       supported[0], supported[1], supported[2]);
}

int main(void)
{
	char card[NAME_MAX + 1];
	struct head *heads;
	size_t count;
	struct timespec next_refresh = { 0, 0 };
	int fd;

	if (setvbuf(stdout, NULL, _IOLBF, 0) != 0)
		fatal("setvbuf", errno);
	printf("start t=%" PRIu64 "\n", monotonic_us());

	fd = wait_for_nvidia_card(card, sizeof card);
	if (drmIsMaster(fd) && drmDropMaster(fd) != 0)
		fatal("drmDropMaster", errno);

	heads = read_heads(fd, &count);

	for (;;) {
		struct timespec round_start = now_mono();

		if (!before(round_start, next_refresh)) {
			refresh_activity(fd, heads, count);
			next_refresh = add_ns(round_start, ACTIVITY_PERIOD_NS);
		}
		for (size_t i = 0; i < count; i++) {
			if (heads[i].active)
				sample_head(fd, card, &heads[i]);
		}
		if (ferror(stdout))
			fatal("stdout write", errno);
		sleep_until(add_ns(now_mono(), PACING_NS));
	}
}
