# NVMe PCI Relay Tracing

The patched Linux tree adds an optional NVMe PCI tracing path backed by
`debugfs` and the kernel `relay` subsystem. It records binary submission and
completion events for NVMe PCI I/O with low overhead compared to text tracing.

## Kernel Configuration

Enable the new Kconfig option when building the kernel:

```text
CONFIG_NVME_PCI_TRACE=y
```

The option depends on:

```text
CONFIG_BLK_DEV_NVME
CONFIG_RELAY
CONFIG_DEBUG_FS
```

The implementation lives in `linux/drivers/nvme/host/pci.c`, and the userspace
record format is defined in `linux/drivers/nvme/host/pci_trace.h`.

## Debugfs Interface

After booting the patched kernel, mount debugfs if it is not already mounted:

```sh
sudo mount -t debugfs none /sys/kernel/debug
```

Each NVMe controller gets a directory:

```text
/sys/kernel/debug/nvme_trace/nvme0/
/sys/kernel/debug/nvme_trace/nvme1/
...
```

Each directory contains:

| File | Meaning |
|------|---------|
| `enable` | Write `1` to enable tracing for this controller, `0` to disable it. |
| `trace<cpu>` | Per-CPU relay buffer files, for example `trace0`, `trace1`, ... |

The relay buffers are per CPU. Each CPU has an 8 x 256 KiB ring, so the buffer
capacity is 2 MiB per CPU and controller. The buffers are no-overwrite: if
userspace does not read fast enough, new records are dropped instead of blocking
I/O or overwriting old data.

## Capturing a Trace

Pick the controller directory first:

```sh
TRACE_DIR=/sys/kernel/debug/nvme_trace/nvme0
```

Start one reader per CPU trace file before enabling tracing. This example writes
one binary file per CPU:

```sh
mkdir -p traces/nvme0
readers=
for f in "$TRACE_DIR"/trace*; do
    cpu=${f##*trace}
    sudo cat "$f" > "traces/nvme0/cpu${cpu}.bin" &
    readers="$readers $!"
done
```

Enable tracing, run the workload, then disable tracing:

```sh
echo 1 | sudo tee "$TRACE_DIR/enable"

# Run the workload to capture here.
# Example:
sudo dd if=/dev/nvme0n1 of=/dev/null bs=4M count=256 iflag=direct

echo 0 | sudo tee "$TRACE_DIR/enable"
```

Stop the readers after tracing is disabled:

```sh
sudo kill $readers
wait $readers 2>/dev/null
```

The output files are raw binary streams. Keep the per-CPU files separate unless
your parser merges them by record timestamp or sequence number.

## Record Format

Every record starts with `struct nvme_trace_hdr` from
`linux/drivers/nvme/host/pci_trace.h`. Multi-byte fields are little-endian.
The packed header is 24 bytes.

Important header fields:

| Field | Meaning |
|-------|---------|
| `magic` | `0x4e564d45` (`"NVME"`), used to identify valid records. |
| `version` | Current format version, `1`. |
| `type` | `0` for submit, `1` for complete. |
| `len` | Total record length in bytes, including the header. |
| `timestamp_ns` | `ktime_get_ns()` timestamp captured at the trace hook. |
| `seq` | Per-controller monotonic sequence counter. |
| `ctrl_id` | NVMe controller instance, matching `nvmeN`. |
| `qid` | NVMe queue ID. |
| `cid` | NVMe command ID from the SQE or CQE. |

Use `hdr.len` to advance to the next record. Use `hdr.magic` and `hdr.version`
to resynchronize or reject partial data.

Submit records use `struct nvme_trace_submit`. They contain:

- The full 64-byte SQE copied verbatim.
- `data_len` and `meta_len`.
- `use_sgl`, which selects PRP tail entries (`0`) or SGL tail entries (`1`).
- `single_segment`, which means the SQE already contains the data pointer and no
  descriptor tail follows.
- `nr_entries`, the number of variable tail entries after the fixed struct.

Completion records use `struct nvme_trace_complete`. They contain the CQE
result, SQ head, SQ ID, status, and retry count.

The `nvme-trace print` subcommand decodes read and write SQEs. It prints named
opcodes, namespace ID, starting LBA, number of logical blocks, byte offset, and
the SQE-implied transfer size. Byte offsets use a 512-byte logical block size by
default; override it when the namespace uses a different LBA size:

```sh
cargo run -- print --block-size 4096 traces/nvme0/cpu0.bin
```

For parsers, prefer including `pci_trace.h` directly so structure packing and
field offsets stay aligned with the kernel:

```c
#include "linux/drivers/nvme/host/pci_trace.h"
```

## Correlating Events

Submission and completion events can be matched with:

```text
ctrl_id + qid + cid
```

For total ordering across per-CPU files, sort records by `timestamp_ns`.
`seq` is also per controller and monotonic, but records are written into
different CPU buffers, so readers still need to merge the files explicitly.

## Practical Notes

- Tracing is per controller. Enabling `nvme0/enable` does not trace `nvme1`.
- Start readers before writing `1` to `enable`; otherwise early records may be
  dropped once relay buffers fill.
- Disable tracing before killing readers to avoid cutting records mid-stream.
- Empty `trace<cpu>` files are normal on CPUs that did not handle submissions or
  completions.
- The API records physical PRP/SGL descriptors for replay research. Treat trace
  files as sensitive system data.
