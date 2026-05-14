// SPDX-License-Identifier: GPL-2.0
/*
 * relay_repro.c — minimal reproducer for relay_switch_subbuf() race
 *
 * The bug
 * -------
 * relay_switch_subbuf() increments subbufs_produced and fires smp_mb()
 * *before* writing buf->data = new and buf->padding[new_subbuf] = 0.  A
 * concurrent reader on another CPU can therefore see the new production
 * count while buf->data still points to the old subbuf and
 * buf->padding[new_subbuf] still holds the previous fill's padding.
 * relay_file_read_subbuf_avail() then computes avail from the stale
 * padding and copies old records out of the recycled subbuf — producing
 * duplicate seq values in the output stream.
 *
 * How to use
 * ----------
 *   make && insmod relay_repro.ko
 *   # On another terminal (CPU 1):
 *   ./relay_repro_client /sys/kernel/debug/relay_repro/buf0
 *   rmmod relay_repro
 *
 * Records are 24 bytes; 4096 / 24 = 170 records + 16 bytes of padding.
 * Non-zero padding makes the stale-avail calculation differ from the
 * correct one, which is what exposes the duplicate.
 */
#include <linux/module.h>
#include <linux/relay.h>
#include <linux/debugfs.h>
#include <linux/kthread.h>
#include <linux/atomic.h>
#include <linux/slab.h>

#define REPRO_SUBBUF_SIZE  4096U
#define REPRO_N_SUBBUFS    4U
#define REPRO_MAGIC        0xDEADBEEFU

/*
 * 24-byte record: 4096 / 24 = 170 full records, leaving 16 bytes of
 * padding per subbuf (non-zero, which is required to trigger the race).
 */
struct repro_rec {
	__le32 magic;
	__le32 seq;
	__le64 timestamp_ns;
	__le64 fill;          /* padding to reach 24 bytes */
};

static_assert(sizeof(struct repro_rec) == 24,
	      "struct repro_rec must be exactly 24 bytes");

static struct rchan       *repro_chan;
static struct dentry      *repro_dir;
static struct task_struct *writer_task;

/* -------------------------------------------------------------------------
 * Relay callbacks
 * ---------------------------------------------------------------------- */

static struct dentry *repro_create_buf_file(const char *filename,
					    struct dentry *parent,
					    umode_t mode,
					    struct rchan_buf *buf,
					    int *is_global)
{
	*is_global = 0;
	return debugfs_create_file(filename, mode, parent, buf,
				   &relay_file_operations);
}

static int repro_remove_buf_file(struct dentry *d)
{
	debugfs_remove(d);
	return 0;
}

/* No-overwrite: drop new records when the ring is full. */
static int repro_subbuf_start(struct rchan_buf *buf, void *subbuf,
			      void *prev_subbuf)
{
	if (relay_buf_full(buf)) {
		pr_warn_ratelimited("relay_repro: buffer full, dropping record\n");
		return 0;
	}
	return 1;
}

static const struct rchan_callbacks repro_cb = {
	.subbuf_start    = repro_subbuf_start,
	.create_buf_file = repro_create_buf_file,
	.remove_buf_file = repro_remove_buf_file,
};

/* -------------------------------------------------------------------------
 * Writer kthread — tight loop, no sleep, maximises race probability
 * ---------------------------------------------------------------------- */

static int writer_fn(void *unused)
{
	u32 seq = 0;

	while (!kthread_should_stop()) {
		struct repro_rec *r;
		unsigned long flags;

		local_irq_save(flags);
		r = relay_reserve(repro_chan, sizeof(*r));
		if (r) {
			r->magic        = cpu_to_le32(REPRO_MAGIC);
			r->seq          = cpu_to_le32(seq++);
			r->timestamp_ns = cpu_to_le64(ktime_get_ns());
			r->fill         = 0;
		}
		local_irq_restore(flags);

		cpu_relax();
	}
	return 0;
}

/* -------------------------------------------------------------------------
 * Module init / exit
 * ---------------------------------------------------------------------- */

static int __init relay_repro_init(void)
{
	int err;

	if (num_online_cpus() < 2) {
		pr_warn("relay_repro: need at least 2 CPUs for the race\n");
		return -ENODEV;
	}

	repro_dir = debugfs_create_dir("relay_repro", NULL);
	if (IS_ERR_OR_NULL(repro_dir))
		return -ENOMEM;

	repro_chan = relay_open("buf", repro_dir,
				REPRO_SUBBUF_SIZE, REPRO_N_SUBBUFS,
				&repro_cb, NULL);
	if (!repro_chan) {
		err = -ENOMEM;
		goto err_dir;
	}

	/*
	 * Pin writer to CPU 0; run the reader on CPU 1.  Adjacent CPUs
	 * share cache, so store propagation is fast — the window between
	 * smp_mb() and buf->data = new is wide enough to be hit regularly.
	 */
	writer_task = kthread_create(writer_fn, NULL, "relay_repro_writer");
	if (IS_ERR(writer_task)) {
		err = PTR_ERR(writer_task);
		writer_task = NULL;
		goto err_chan;
	}
	kthread_bind(writer_task, 0);
	wake_up_process(writer_task);

	pr_info("relay_repro: loaded — read /sys/kernel/debug/relay_repro/buf0\n"
		"  writer pinned to CPU 0; pin reader to CPU 1\n"
		"  subbuf=%u B  n_subbufs=%u  record=%zu B  padding=%zu B\n",
		REPRO_SUBBUF_SIZE, REPRO_N_SUBBUFS,
		sizeof(struct repro_rec),
		(size_t)REPRO_SUBBUF_SIZE %  sizeof(struct repro_rec));
	return 0;

err_chan:
	relay_close(repro_chan);
err_dir:
	debugfs_remove_recursive(repro_dir);
	return err;
}

static void __exit relay_repro_exit(void)
{
	if (writer_task)
		kthread_stop(writer_task);
	relay_close(repro_chan);
	debugfs_remove_recursive(repro_dir);
	pr_info("relay_repro: unloaded\n");
}

module_init(relay_repro_init);
module_exit(relay_repro_exit);
MODULE_LICENSE("GPL");
MODULE_DESCRIPTION("Reproducer for relay_switch_subbuf smp_mb ordering bug");
MODULE_AUTHOR("Thore Sommer");
