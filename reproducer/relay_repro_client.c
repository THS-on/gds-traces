/*
 * relay_repro_client.c — read relay_repro/buf0 and detect duplicate records
 *
 * A duplicate manifests as a seq value less than or equal to the previous
 * one: after the ring wraps, the stale first-fill bytes of the recycled
 * subbuf are re-served, producing seq values far below the current cursor.
 *
 * Usage:
 *   ./relay_repro_client [/sys/kernel/debug/relay_repro/buf0]
 *
 * Pin the process to CPU 1 (where the kernel writer is on CPU 0) to
 * maximise contention with relay_switch_subbuf().
 */
#define _GNU_SOURCE
#include <endian.h>
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <poll.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

#define REPRO_MAGIC  UINT32_C(0xDEADBEEF)
#define RECORD_SIZE  24
#define READ_BUF     (256 * 1024)

struct repro_rec {
	uint32_t magic;
	uint32_t seq;
	uint64_t timestamp_ns;
	uint64_t fill;
} __attribute__((packed));

_Static_assert(sizeof(struct repro_rec) == RECORD_SIZE, "size mismatch");

static volatile int stop_flag;
static void on_sigint(int s) { (void)s; stop_flag = 1; }

int main(int argc, char *argv[])
{
	const char *path = argc > 1 ? argv[1]
				    : "/sys/kernel/debug/relay_repro/buf0";
	int fd;
	uint8_t *buf;
	uint64_t file_off = 0;
	uint32_t last_seq    = UINT32_MAX; /* sentinel: no record seen yet */
	uint64_t last_toggle = UINT64_MAX; /* matches last_seq sentinel   */
	uint64_t total = 0, dups = 0;

	signal(SIGINT,  on_sigint);
	signal(SIGTERM, on_sigint);

	/* Pin to CPU 1 so the writer on CPU 0 and reader share the same
	 * cache domain — store propagation is fast and the race window is hit
	 * regularly.                                                           */
	cpu_set_t cpuset;
	CPU_ZERO(&cpuset);
	CPU_SET(1, &cpuset);
	if (sched_setaffinity(0, sizeof(cpuset), &cpuset) < 0)
		perror("sched_setaffinity (continuing anyway)");

	fd = open(path, O_RDONLY | O_NONBLOCK);
	if (fd < 0) { perror("open"); return 1; }

	buf = malloc(READ_BUF);
	if (!buf) { perror("malloc"); return 1; }

	printf("Reading %s  (Ctrl-C to stop)\n", path);
	fflush(stdout);

	while (!stop_flag) {
		struct pollfd pfd = { .fd = fd, .events = POLLIN };
		int ret = poll(&pfd, 1, 100);
		if (ret < 0) {
			if (errno == EINTR) continue;
			perror("poll"); break;
		}
		if (ret == 0 || !(pfd.revents & POLLIN))
			continue;

		ssize_t n = read(fd, buf, READ_BUF);

		if (n > 0 && n % RECORD_SIZE != 0)
			fprintf(stderr, "warning: read %" PRIdMAX
				" bytes — not a multiple of %d (record_size)\n",
				(intmax_t)n, RECORD_SIZE);

		if (n == 0 || (n < 0 && errno == EAGAIN))
			continue;
		if (n < 0) { perror("read"); break; }

		uint8_t *p   = buf;
		uint8_t *end = buf + n;

		while (p + RECORD_SIZE <= end) {
			struct repro_rec *r = (struct repro_rec *)p;
			uint32_t magic  = le32toh(r->magic);
			uint32_t seq    = le32toh(r->seq);
			uint64_t toggle = le64toh(r->fill);
			uint64_t off    = file_off + (uint64_t)(p - buf);

			if (magic != REPRO_MAGIC) {
				fprintf(stderr, "bad magic 0x%08" PRIx32
					" at offset %" PRIu64 "\n",
					magic, off);
				p++;
				continue;
			}

			if (last_seq != UINT32_MAX && seq <= last_seq &&
			    toggle == last_toggle) {
				printf("DUP  off=%-14" PRIu64
				       "  seq=%-10" PRIu32
				       "  prev=%" PRIu32 "\n",
				       off, seq, last_seq);
				dups++;
			}
			last_seq    = seq;
			last_toggle = toggle;
			total++;
			p += RECORD_SIZE;
		}

		file_off += (uint64_t)n;

		if (total > 0 && total % 1000000 == 0)
			printf("  %" PRIu64 " records  %" PRIu64 " dups\n",
			       total, dups);
	}

	printf("\nTotal: %" PRIu64 " records  %" PRIu64 " duplicates\n",
	       total, dups);
	free(buf);
	close(fd);
	return dups > 0 ? 1 : 0;
}
