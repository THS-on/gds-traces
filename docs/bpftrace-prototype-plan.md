# bpftrace NVMe Prototype — Phased Plan

This document breaks the bpftrace prototype (called for in
[nvme-bpf-tracing-plan.md](nvme-bpf-tracing-plan.md)) into eight self-contained
phases. Each phase produces one `.bt` file in `tracing/bpftrace/` and has its own
validation criteria. Implement and validate one phase before writing the next.

## Kernel Context (post-v7.0-rc1)

| Item | Detail |
|------|--------|
| Single-request path | `nvme_queue_rq()` → `nvme_prep_rq()` → `nvme_sq_copy_cmd()` |
| Batch path | `nvme_queue_rqs()` → `nvme_submit_cmds()` |
| `nvme_queue_rq` signature | `(struct blk_mq_hw_ctx *hctx, const struct blk_mq_queue_data *bd)` |
| `nvme_submit_cmds` signature | `(struct nvme_queue *nvmeq, struct rq_list *rqlist)` |
| `nvme_complete_rq` signature | `(struct request *req)` |
| `nvme_complete_batch_req` signature | `(struct request *req)` |
| Device filter path | `req->q->disk->disk_name` (block namespace name, e.g. `nvme0n1`) |
| `nvme_sq_copy_cmd` | static inline — NOT a valid fentry target |
| Module for `pci.c` functions | `nvme` |
| Module for `core.c` functions | `nvme_core` |
| `iod` from request | `blk_mq_rq_to_pdu(req)` = `req + 1` (pointer arithmetic) |
| PRP vs SGL | `iod->cmd.common.flags & 0xC0` — non-zero means SGL |
| `NVME_MAX_NR_DESCRIPTORS` | 5 (`pci.c` line 44) |

Relevant `struct nvme_iod` fields (`pci.c` lines 287–303):

```c
struct nvme_iod {
    struct nvme_request  req;           // nvme status lives here at completion
    struct nvme_command  cmd;           // full NVMe command, including dptr
    u8                   flags;
    u8                   nr_descriptors;
    size_t               total_len;
    struct dma_iova_state dma_state;
    void                *descriptors[5]; // PRP list pages or SGL descriptor pages
    ...
};
```

`struct nvme_sgl_desc` layout (`include/linux/nvme.h` lines 1025–1030):

```
offset  size  field
     0     8  addr
     8     4  length
    12     3  rsvd
    15     1  type   — upper nibble: 0x0=data, 0x2=segment, 0x3=last segment
```

Total size: 16 bytes. Up to 256 entries fit in one 4 KiB descriptor page.

## Target Kernel Requirement

The tracing scripts are validated on the NVMe test server, not necessarily on
the workstation that holds this repository. Do not treat the absence of
`nvme_submit_cmds` on the local system as a failed prototype check.

The repository includes
`patches/linux-nvme-submit-cmds-noinline.patch`, which changes
`nvme_submit_cmds()` to `static noinline`. The target server must boot a kernel
with that patch applied so `bpftrace` can attach
`fentry:nvme:nvme_submit_cmds`. Without the patch, the compiler may inline the
helper and leave no stable fentry target for the batch path.

## Script Location

All `.bt` files go in `tracing/bpftrace/`:

```
tracing/bpftrace/
  phase1-attach.bt
  phase2-request-id.bt
  phase3-nvme-fields.bt
  phase4-correlation.bt
  phase5-nvme-device-filter.bt
  phase6-dptr-metadata.bt
  phase7-segment-addresses-prp.bt
  phase7-segment-addresses-sgl.bt
  phase8-batch.bt
```

---

## Phase 1 — Probe Attachment

**File:** `tracing/bpftrace/phase1-attach.bt`

**Status:** Implemented.

Attach all five target probes and print a one-liner per fire. The only goal is to
confirm that BTF is present for both modules and that fentry/fexit can attach.

```bpftrace
#!/usr/bin/env bpftrace

BEGIN { printf("probes attached\n"); }

fentry:nvme:nvme_queue_rq              { printf("queue_rq       cpu=%d\n", cpu); }
fexit:nvme:nvme_queue_rq               { printf("queue_rq_exit  ret=%llu\n", (uint64)retval); }
fentry:nvme:nvme_submit_cmds           { printf("submit_cmds    cpu=%d\n", cpu); }
fentry:nvme_core:nvme_complete_rq      { printf("complete_rq    cpu=%d\n", cpu); }
fentry:nvme_core:nvme_complete_batch_req { printf("complete_batch cpu=%d\n", cpu); }
```

**Validation:**
- On the patched target server, the script loads without error.
- All five probe lines appear after
  `dd if=/dev/disk/by-id/nvme-KIOXIA_KCMYXRUG3T84_4FB0A0CT0LM3_1 of=/dev/null count=1`.
- No "failed to attach" messages.

**If `nvme_queue_rq` is not found:** check `lsmod | grep nvme` and
`ls /sys/kernel/btf/nvme`.

**If `nvme_submit_cmds` is not found:** confirm the test server is running the
kernel built with `patches/linux-nvme-submit-cmds-noinline.patch`. This symbol is
not expected to be available on every development machine.

---

## Phase 2 — Request Identity

**File:** `tracing/bpftrace/phase2-request-id.bt`

**Status:** Implemented.

Capture the `struct request *` pointer (the correlation key between submit and
completion), plus process context. Handle both current PCI NVMe submission paths:
single-request `nvme_queue_rq()` and batched `nvme_submit_cmds()`. Filter the
single-request `fexit` to successful submissions only (`retval == 0`).

```bpftrace
#!/usr/bin/env bpftrace

BEGIN {
    printf("capturing NVMe request identities\n");
}

fexit:nvme:nvme_queue_rq / retval == 0 / {
    $req = (uint64)args->bd->rq;
    printf("submit         req=0x%llx ts=%llu cpu=%d pid=%d comm=%s\n",
           $req, nsecs, cpu, pid, comm);
}

fentry:nvme:nvme_submit_cmds {
    $rq = args->rqlist->head;

    unroll(64) {
        if ($rq == 0) {
            return;
        }

        $req = (uint64)$rq;
        printf("batch_submit   req=0x%llx ts=%llu cpu=%d pid=%d comm=%s\n",
               $req, nsecs, cpu, pid, comm);
        $rq = (struct request *)$rq->rq_next;
    }

    if ($rq != 0) {
        printf("batch_submit   truncated after 64 requests\n");
    }
}

fentry:nvme_core:nvme_complete_rq {
    $req = (uint64)args->req;
    printf("complete       req=0x%llx ts=%llu cpu=%d\n",
           $req, nsecs, cpu);
}

fentry:nvme_core:nvme_complete_batch_req {
    $req = (uint64)args->req;
    printf("complete_batch req=0x%llx ts=%llu cpu=%d\n",
           $req, nsecs, cpu);
}
```

**Validation:**
- For a quiet single-request workload, each `submit` line is followed by exactly
  one `complete` or `complete_batch` line with the same `req` value.
- For a batched workload, each `batch_submit` line is followed by exactly one
  `complete_batch` line with the same `req` value.
- Request pointer is non-zero and changes between requests.
- No `batch_submit truncated after 64 requests` line appears during the phase 2
  validation workload.

**Findings:**
- Incidental `smartctl` traffic validated the single-request path:
  `submit` and `complete` used the same `struct request *` pointer.
- The by-id `dd` workload used the current batched path and produced a matching
  `batch_submit` / `complete_batch` pair:
  `req=0xff46323b8ad40000`.
- The observed batched request latency was `504308 ns`, computed from
  `1688724634248 - 1688724129940`.
- Phase 2 therefore validates `struct request *` as the correlation key for both
  `nvme_queue_rq()` and `nvme_submit_cmds()` submission paths.

---

## Phase 3 — NVMe Command Fields

**File:** `tracing/bpftrace/phase3-nvme-fields.bt`

**Status:** Implemented.

Access `struct nvme_iod` via `req + 1` (pointer arithmetic equivalent of
`blk_mq_rq_to_pdu`). Read opcode, NSID, SLBA, and transfer length for NVMe
read/write commands. Handle both the single-request and batched submission paths;
the by-id `dd` workload may use `nvme_submit_cmds()` on the current test kernel.

```bpftrace
#!/usr/bin/env bpftrace

BEGIN {
    printf("capturing NVMe command fields\n");
}

fexit:nvme:nvme_queue_rq / retval == 0 / {
    $req  = args->bd->rq;
    $iod  = (struct nvme_iod *)($req + 1);
    $op   = $iod->cmd.common.opcode;
    if ($op != 0x01 && $op != 0x02) {
        return;
    }

    $ns   = $iod->cmd.common.nsid;
    $slba = $iod->cmd.rw.slba;
    $len  = (uint64)$req->__data_len;

    printf("submit req=0x%llx op=0x%02x nsid=%u slba=%llu len=%llu\n",
           (uint64)$req, $op, $ns, $slba, $len);
}

fentry:nvme:nvme_submit_cmds {
    $rq = args->rqlist->head;

    unroll(64) {
        if ($rq == 0) {
            return;
        }

        $iod = (struct nvme_iod *)($rq + 1);
        $op  = $iod->cmd.common.opcode;

        if ($op == 0x01 || $op == 0x02) {
            $ns   = $iod->cmd.common.nsid;
            $slba = $iod->cmd.rw.slba;
            $len  = (uint64)$rq->__data_len;

            printf("batch_submit req=0x%llx op=0x%02x nsid=%u slba=%llu len=%llu\n",
                   (uint64)$rq, $op, $ns, $slba, $len);
        }

        $rq = (struct request *)$rq->rq_next;
    }

    if ($rq != 0) {
        printf("batch_submit truncated after 64 requests\n");
    }
}
```

**Validation:**
- Use direct I/O for validation so the block-device page cache cannot satisfy the
  workload without issuing an NVMe command.
- Use a 128 KiB transfer so `len=131072` is easy to distinguish from incidental
  4 KiB background I/O.
- `dd if=/dev/disk/by-id/nvme-KIOXIA_KCMYXRUG3T84_4FB0A0CT0LM3_1 of=/dev/null bs=128K count=1 iflag=direct`:
  one `submit` or `batch_submit` line has opcode = `0x02` (read), NSID = 1,
  SLBA = 0, `len` = 131072.
- Offset read:
  `dd if=/dev/disk/by-id/nvme-KIOXIA_KCMYXRUG3T84_4FB0A0CT0LM3_1 of=/dev/null bs=128K count=1 skip=256 iflag=direct`
  reads at byte offset 32 MiB. Expected SLBA is
  `33554432 / logical_block_size`, where `logical_block_size` is reported by
  `blockdev --getss /dev/disk/by-id/nvme-KIOXIA_KCMYXRUG3T84_4FB0A0CT0LM3_1`;
  for a 512-byte namespace this is
  `slba=65536`, with `len=131072`.
- `dd if=/dev/zero of=/dev/disk/by-id/nvme-KIOXIA_KCMYXRUG3T84_4FB0A0CT0LM3_1 bs=128K count=1 oflag=direct`:
  one `submit` or `batch_submit` line has opcode = `0x01` (write).
- `len` matches the requested byte count.
- Admin commands such as `op=0x06` are ignored because `cmd.rw.slba` is only
  meaningful for read/write commands.

**Findings:**
- The by-id `dd` workload used the batched submission path, so the expected
  validation lines are `batch_submit`, not `submit`.
- The direct 128 KiB read from offset 0 produced:
  `batch_submit req=0xff46323b8b110000 op=0x02 nsid=1 slba=0 len=131072`.
- The direct 128 KiB read at `skip=256` produced:
  `batch_submit req=0xff46323b8b488000 op=0x02 nsid=1 slba=65536 len=131072`.
- The direct 128 KiB write to offset 0 produced:
  `batch_submit req=0xff46323b8a6c0000 op=0x01 nsid=1 slba=0 len=131072`.
- Phase 3 therefore validates read/write opcode extraction, namespace ID, SLBA,
  request length, and batched-path command-field extraction.

---

## Phase 4 — Submit/Completion Correlation and Latency

**File:** `tracing/bpftrace/phase4-correlation.bt`

**Status:** Implemented.

Store the submit timestamp in a BPF map keyed by request pointer. At completion,
look up the entry, compute latency, emit, and delete. Count unmatched completions.
Both submission paths and both `nvme_complete_rq` and
`nvme_complete_batch_req` must be handled.

```bpftrace
#!/usr/bin/env bpftrace

BEGIN {
    printf("correlating NVMe submit/completion latency\n");
    @unmatched = 0;
    @batch_truncated = 0;
}

fexit:nvme:nvme_queue_rq / retval == 0 / {
    $req = (uint64)args->bd->rq;
    @submit_ts[$req] = nsecs;
}

fentry:nvme:nvme_submit_cmds {
    $rq = args->rqlist->head;
    $ts = nsecs;

    unroll(64) {
        if ($rq == 0) {
            return;
        }

        $req = (uint64)$rq;
        @submit_ts[$req] = $ts;
        $rq = (struct request *)$rq->rq_next;
    }

    if ($rq != 0) {
        @batch_truncated++;
        printf("batch_submit truncated after 64 requests\n");
    }
}

fentry:nvme_core:nvme_complete_rq {
    $req = (uint64)args->req;
    $ts  = @submit_ts[$req];
    if ($ts != 0) {
        printf("complete       req=0x%llx lat_ns=%llu\n", $req, nsecs - $ts);
        $ignore = delete(@submit_ts, $req);
    } else {
        @unmatched++;
    }
}

fentry:nvme_core:nvme_complete_batch_req {
    $req = (uint64)args->req;
    $ts  = @submit_ts[$req];
    if ($ts != 0) {
        printf("complete_batch req=0x%llx lat_ns=%llu\n", $req, nsecs - $ts);
        $ignore = delete(@submit_ts, $req);
    } else {
        @unmatched++;
    }
}

END {
    printf("unmatched completions: %llu\n", @unmatched);
    printf("batch submit truncations: %llu\n", @batch_truncated);
}
```

**Validation:**
- Every submit event produces a matching completion event.
- `@unmatched` = 0 for a workload started after the script.
- Latencies are positive and in the microsecond-to-millisecond range.
- No `batch_submit truncated after 64 requests` line appears during the phase 4
  validation workload.

---

## Phase 5 — NVMe Device Filtering

**File:** `tracing/bpftrace/phase5-nvme-device-filter.bt`

**Status:** Implemented.

Add an explicit allow-list for the NVMe block devices that should be traced. The
target server has multiple NVMe devices, but prototype validation should only
record the subset under test. Filter both submission and completion probes using
`req->q->disk->disk_name`; this avoids recording completions for requests that
were intentionally ignored at submission time.

The kernel sees the namespace block name (`nvme0n1`, `nvme1n1`, ...), not a
`/dev/disk/by-id/` symlink. Resolve the selected by-id path to its kernel block
name before editing the device-name predicates. For multiple target devices,
expand each predicate into an explicit boolean allow-list, such as
`str($disk->disk_name) != "nvme0n1" && str($disk->disk_name) != "nvme1n1"` on
early returns.

```bpftrace
#!/usr/bin/env bpftrace

BEGIN {
    printf("correlating latency for selected NVMe devices\n");
    printf("edit device-name predicates before running on the target server\n");

    @target_disk = "nvme1n1";
    @target_submits = 0;
    @unmatched = 0;
    @batch_truncated = 0;
}

fexit:nvme:nvme_queue_rq / retval == 0 / {
    $req = args->bd->rq;
    $queue = $req->q;
    if ($queue == 0) {
        return;
    }

    $disk = $queue->disk;
    if ($disk == 0) {
        return;
    }

    if (str($disk->disk_name) != str(@target_disk)) {
        printf("skip_submit    disk=%s\n", str($disk->disk_name));
        return;
    }

    $id = (uint64)$req;
    @submit_ts[$id] = nsecs;
    @target_submits++;
    printf("submit         disk=%s req=0x%llx ts=%llu\n",
           str($disk->disk_name), $id, nsecs);
}

fentry:nvme:nvme_submit_cmds {
    $rq = args->rqlist->head;
    $ts = nsecs;

    unroll(64) {
        if ($rq == 0) {
            return;
        }

        $queue = $rq->q;
        if ($queue != 0) {
            $disk = $queue->disk;
            if ($disk != 0) {
                if (str($disk->disk_name) == str(@target_disk)) {
                    $id = (uint64)$rq;
                    @submit_ts[$id] = $ts;
                    @target_submits++;
                    printf("batch_submit   disk=%s req=0x%llx ts=%llu\n",
                           str($disk->disk_name), $id, $ts);
                } else {
                    printf("skip_batch     disk=%s\n", str($disk->disk_name));
                }
            }
        }

        $rq = (struct request *)$rq->rq_next;
    }

    if ($rq != 0) {
        @batch_truncated++;
        printf("batch_submit truncated after 64 requests\n");
    }
}

fentry:nvme_core:nvme_complete_rq {
    $req = args->req;
    $queue = $req->q;
    if ($queue == 0) {
        return;
    }

    $disk = $queue->disk;
    if ($disk == 0) {
        return;
    }

    if (str($disk->disk_name) != str(@target_disk)) {
        printf("skip_complete  disk=%s\n", str($disk->disk_name));
        return;
    }

    $id = (uint64)$req;
    $ts = @submit_ts[$id];
    if ($ts != 0) {
        printf("complete       disk=%s req=0x%llx lat_ns=%llu\n",
               str($disk->disk_name), $id, nsecs - $ts);
        $ignore = delete(@submit_ts, $id);
    } else {
        @unmatched++;
    }
}

fentry:nvme_core:nvme_complete_batch_req {
    $req = args->req;
    $queue = $req->q;
    if ($queue == 0) {
        return;
    }

    $disk = $queue->disk;
    if ($disk == 0) {
        return;
    }

    if (str($disk->disk_name) != str(@target_disk)) {
        printf("skip_complete  disk=%s\n", str($disk->disk_name));
        return;
    }

    $id = (uint64)$req;
    $ts = @submit_ts[$id];
    if ($ts != 0) {
        printf("complete_batch disk=%s req=0x%llx lat_ns=%llu\n",
               str($disk->disk_name), $id, nsecs - $ts);
        $ignore = delete(@submit_ts, $id);
    } else {
        @unmatched++;
    }
}

END {
    printf("target submits: %llu\n", @target_submits);
    printf("unmatched target completions: %llu\n", @unmatched);
    printf("batch submit truncations: %llu\n", @batch_truncated);
}
```

**Validation:**
- Resolve the by-id validation device to its kernel disk name and add that name
  to each device-name predicate.
- Direct I/O against an allow-listed disk prints `submit`/`batch_submit` and
  matching completion lines that include only the allow-listed `disk=...` value.
- Direct I/O against a non-allow-listed NVMe disk produces no submit or
  completion output.
- `@unmatched` remains 0 for a workload started after the script.
- No `batch_submit truncated after 64 requests` line appears during the phase 5
  validation workload.

---

## Phase 6 — Data Pointer Metadata

**File:** `tracing/bpftrace/phase6-dptr-metadata.bt`

**Status:** Implemented.

Read the data pointer mode (PRP vs SGL), descriptor count, and total transfer length
without yet reading segment addresses. `cmd.common.flags & 0xC0` is non-zero for SGL
(bits 6–7 = `NVME_CMD_SGL_ALL`). Includes device-name filtering. The single-request path uses a split fentry/fexit
design: `fentry` checks the disk name and marks qualifying requests in a BPF map;
`fexit` reads the IOD fields using only the map lookup with no `str()` calls.
This is necessary because the BPF stack is limited to 512 bytes and a `fexit` probe
already saves the function arguments and return value onto the stack — adding two
`str()` buffers (64 bytes each) inside the same probe overflows it.

For the batch path, the disk is checked once on the first request before the
`unroll(64)` loop — all requests in a single `nvme_submit_cmds` call share the same
queue and disk, so a per-iteration `str()` comparison is both unnecessary and causes
a BPF branch-range overflow.

The target disk name is a `#define` compile-time constant (`TARGET_DISK`) rather than
a global map variable. Using a map (`@target_disk = "nvme1n1"` in `BEGIN`) causes the
BPF verifier to reject the program with "stack too large", because every probe that
reads the map must budget stack space for the string buffer. A `#define` has zero
stack cost and is substituted by the preprocessor before the BPF bytecode is
generated.

```bpftrace
#!/usr/bin/env bpftrace

#define TARGET_DISK "nvme1n1"

BEGIN {
    printf("capturing NVMe dptr metadata for device %s\n", TARGET_DISK);
}

// Mark target-disk requests at fentry (req->q->disk is set before the function runs).
// str() comparisons are kept out of fexit to stay within the 512-byte BPF stack limit.
fentry:nvme:nvme_queue_rq {
    $req  = args->bd->rq;
    $disk = $req->q->disk;
    if ($disk != 0 && str($disk->disk_name) == TARGET_DISK) {
        @target_rqs[(uint64)$req] = 1;
    }
}

// At fexit the command is fully prepared.  Only process marked requests.
fexit:nvme:nvme_queue_rq {
    $req = (uint64)args->bd->rq;
    if (@target_rqs[$req] == 0) {
        return;
    }
    $ignore = delete(@target_rqs, $req);
    if (retval != 0) {
        return;
    }

    $iod   = (struct nvme_iod *)($req + 1);
    $flags = $iod->cmd.common.flags;
    $ndesc = (uint64)$iod->nr_descriptors;
    $tlen  = $iod->total_len;
    printf("submit req=0x%llx mode=%s nr_desc=%llu total_len=%lu\n",
           $req, ($flags & 0xC0) ? "SGL" : "PRP", $ndesc, $tlen);
}

fentry:nvme:nvme_submit_cmds {
    $rq = args->rqlist->head;
    if ($rq == 0) {
        return;
    }

    // All requests in one nvme_submit_cmds call share the same queue and disk.
    // Check once on the first request to keep str() out of the unrolled loop.
    $disk = $rq->q->disk;
    if ($disk == 0 || str($disk->disk_name) != TARGET_DISK) {
        return;
    }

    unroll(64) {
        if ($rq == 0) {
            return;
        }

        $iod   = (struct nvme_iod *)($rq + 1);
        $flags = $iod->cmd.common.flags;
        $ndesc = (uint64)$iod->nr_descriptors;
        $tlen  = $iod->total_len;
        printf("batch_submit req=0x%llx mode=%s nr_desc=%llu total_len=%lu\n",
               (uint64)$rq, ($flags & 0xC0) ? "SGL" : "PRP", $ndesc, $tlen);

        $rq = (struct request *)$rq->rq_next;
    }

    if ($rq != 0) {
        printf("batch_submit truncated after 64 requests\n");
    }
}
```

**Validation:**
- Edit `TARGET_DISK` at the top of the script to the kernel disk name of the target
  device before running on the target server.
- PCI NVMe uses PRP for all standard workloads on this setup.
- `total_len` matches `__data_len` from the request.
- `nr_descriptors` = 0 for a 4 KiB request; ≥ 1 for a 1 MiB request.
- Direct I/O against a non-target NVMe disk produces no output.

**Findings:**
- Using a global map `@target_disk = "nvme1n1"` in `BEGIN` caused the BPF verifier to
  reject the program with "stack too large" before any probe fired. Every probe that
  reads the map must reserve stack space for the string buffer; combined with the
  existing `fexit` frame usage this exceeded the 512-byte limit. Replacing the map
  with `#define TARGET_DISK "nvme1n1"` (a preprocessor constant) eliminated the issue
  entirely — the constant is inlined at compile time with zero stack cost.
- bpftrace does not support C-style adjacent string literal concatenation in `printf`
  format strings (i.e. `"prefix " TARGET_DISK "\n"` fails to parse). Use `%s` with
  the constant as an argument: `printf("... %s\n", TARGET_DISK)`.
- String comparisons against a `#define` string constant work without wrapping the
  constant in `str()`: `str($disk->disk_name) == TARGET_DISK` is correct; the
  `str()` call is only needed for kernel memory pointers, not for string literals.

---

## Phase 7 — PRP/SGL Address Capture

**Files:**
- `tracing/bpftrace/phase7-segment-addresses-prp.bt`
- `tracing/bpftrace/phase7-segment-addresses-sgl.bt`

**Status:** Implemented.

Read actual segment addresses. PRP and SGL live in separate scripts because
bpftrace supports preprocessor definitions in the preamble and runtime
`if`/`else`, but not C-style compile-time `#if` blocks around later probe
definitions.

For PRP: emit the request direction, SLBA, disk byte offset, total byte count,
`prp1`, `prp2`, and `nr_desc`, then derive per-segment source, destination, and
size. For writes, the source is host DMA and the destination is the disk offset;
for reads, the source and destination are reversed. `prp1` is the first data
page and may be unaligned. If the remaining transfer fits in one controller page,
`prp2` is a second data page. Otherwise `prp2` is the DMA address of a PRP list,
and `iod->descriptors[]` holds the kernel virtual addresses of those PRP-list
pages. Each PRP-list entry is a little-endian 64-bit DMA address; with a 4 KiB
controller page the last entry is reserved for chaining, so each list page holds
511 data entries. The bpftrace prototype previews a bounded number of entries per
descriptor page.

For SGL: decode the inline `dptr.sgl` descriptor and, when it is a segment header
(type upper nibble = `0x2`/`0x3`), walk the data entries in each
`iod->descriptors[]` page using raw pointer arithmetic (each entry is 16 bytes).

High-level decode sketch; the runnable scripts keep the PRP and SGL branches in
separate files:

```bpftrace
#!/usr/bin/env bpftrace

fexit:nvme:nvme_queue_rq / retval == 0 / {
    $req   = args->bd->rq;
    $iod   = (struct nvme_iod *)($req + 1);
    $flags = $iod->cmd.common.flags;

    if (($flags & 0xC0) == 0) {
        // PRP mode. Keep $req as a typed struct request * when deriving iod:
        // casting to uint64 before "$req + 1" increments by one byte instead
        // of one struct request and corrupts fields such as nr_descriptors.
        $prp1    = (uint64)$iod->cmd.common.dptr.prp1;
        $prp2    = (uint64)$iod->cmd.common.dptr.prp2;
        $ndesc   = (uint64)$iod->nr_descriptors;
        $bytes   = (uint64)$iod->total_len;
        $disk    = ((uint64)$req->__sector) << 9;
        $seg_len = 4096 - ($prp1 & 4095);
        if ($seg_len > $bytes) { $seg_len = $bytes; }

        printf("PRP req=0x%llx disk_off=0x%llx bytes=%llu prp1=0x%llx prp2=0x%llx nr_desc=%llu\n",
               (uint64)$req, $disk, $bytes, $prp1, $prp2, $ndesc);
        printf("  seg[0] src/dst depend on opcode size=%llu via=prp1\n", $seg_len);

        // If bytes - seg_len <= 4096, prp2 is data. Otherwise prp2 is the DMA
        // address of a PRP-list page, while descriptors[i] are kernel virtual
        // addresses of list pages containing __le64 data-page DMA addresses.
        if ($ndesc > 0) {
            $pg = (uint64)$iod->descriptors[0];
            $dma = *(uint64 *)$pg;
            printf("  seg[1] dma=0x%llx via=prp_list[0][0]\n", $dma);
        }

    } else {
        // SGL mode
        $sgl_addr = $iod->cmd.common.dptr.sgl.addr;
        $sgl_len  = $iod->cmd.common.dptr.sgl.length;
        $sgl_type = $iod->cmd.common.dptr.sgl.type;
        $type_hi  = ($sgl_type >> 4) & 0xF;
        printf("SGL req=0x%llx dptr: addr=0x%llx len=%u type=0x%02x\n",
               (uint64)$req, $sgl_addr, $sgl_len, $sgl_type);

        if ($type_hi == 0) {
            printf("  (single inline SGL entry)\n");
        } else {
            // Segment header: walk data entries in iod->descriptors[] pages.
            // Each nvme_sgl_desc is 16 bytes: addr@0 (8B), length@8 (4B), type@15 (1B).
            // Entries with type upper nibble != 0 are segment links, not data.
            // Print up to 8 entries per page; pages 2–4 follow the same pattern.
            $ndesc = (uint64)$iod->nr_descriptors;

            if ($ndesc > 0) {
                $pg = (uint64)$iod->descriptors[0];
                printf("  sgl_page[0]:\n");
                $e = $pg +   0; if ((*(uint8 *)($e+15) >> 4) == 0) { printf("    [0] addr=0x%llx len=%u\n", *(uint64 *)$e, *(uint32 *)($e+8)); }
                $e = $pg +  16; if ((*(uint8 *)($e+15) >> 4) == 0) { printf("    [1] addr=0x%llx len=%u\n", *(uint64 *)$e, *(uint32 *)($e+8)); }
                $e = $pg +  32; if ((*(uint8 *)($e+15) >> 4) == 0) { printf("    [2] addr=0x%llx len=%u\n", *(uint64 *)$e, *(uint32 *)($e+8)); }
                $e = $pg +  48; if ((*(uint8 *)($e+15) >> 4) == 0) { printf("    [3] addr=0x%llx len=%u\n", *(uint64 *)$e, *(uint32 *)($e+8)); }
                $e = $pg +  64; if ((*(uint8 *)($e+15) >> 4) == 0) { printf("    [4] addr=0x%llx len=%u\n", *(uint64 *)$e, *(uint32 *)($e+8)); }
                $e = $pg +  80; if ((*(uint8 *)($e+15) >> 4) == 0) { printf("    [5] addr=0x%llx len=%u\n", *(uint64 *)$e, *(uint32 *)($e+8)); }
                $e = $pg +  96; if ((*(uint8 *)($e+15) >> 4) == 0) { printf("    [6] addr=0x%llx len=%u\n", *(uint64 *)$e, *(uint32 *)($e+8)); }
                $e = $pg + 112; if ((*(uint8 *)($e+15) >> 4) == 0) { printf("    [7] addr=0x%llx len=%u\n", *(uint64 *)$e, *(uint32 *)($e+8)); }
            }
            if ($ndesc > 1) {
                $pg = (uint64)$iod->descriptors[1];
                printf("  sgl_page[1]:\n");
                $e = $pg +   0; if ((*(uint8 *)($e+15) >> 4) == 0) { printf("    [0] addr=0x%llx len=%u\n", *(uint64 *)$e, *(uint32 *)($e+8)); }
                $e = $pg +  16; if ((*(uint8 *)($e+15) >> 4) == 0) { printf("    [1] addr=0x%llx len=%u\n", *(uint64 *)$e, *(uint32 *)($e+8)); }
                $e = $pg +  32; if ((*(uint8 *)($e+15) >> 4) == 0) { printf("    [2] addr=0x%llx len=%u\n", *(uint64 *)$e, *(uint32 *)($e+8)); }
                $e = $pg +  48; if ((*(uint8 *)($e+15) >> 4) == 0) { printf("    [3] addr=0x%llx len=%u\n", *(uint64 *)$e, *(uint32 *)($e+8)); }
                $e = $pg +  64; if ((*(uint8 *)($e+15) >> 4) == 0) { printf("    [4] addr=0x%llx len=%u\n", *(uint64 *)$e, *(uint32 *)($e+8)); }
                $e = $pg +  80; if ((*(uint8 *)($e+15) >> 4) == 0) { printf("    [5] addr=0x%llx len=%u\n", *(uint64 *)$e, *(uint32 *)($e+8)); }
                $e = $pg +  96; if ((*(uint8 *)($e+15) >> 4) == 0) { printf("    [6] addr=0x%llx len=%u\n", *(uint64 *)$e, *(uint32 *)($e+8)); }
                $e = $pg + 112; if ((*(uint8 *)($e+15) >> 4) == 0) { printf("    [7] addr=0x%llx len=%u\n", *(uint64 *)$e, *(uint32 *)($e+8)); }
            }
            // Pages 2–4 follow the identical pattern (descriptors[2..4]).
        }
    }
}
```

**Design notes:**
- Uses the fentry/fexit split pattern from phase 6 for the single-request path:
  `fentry` marks target-disk requests in `@target_rqs`; `fexit` reads the DMA
  addresses without any `str()` call (keeps the fexit stack within 512 bytes).
- The batch path checks the disk once on the first request before
  `unroll(BATCH_UNROLL)` to avoid `str()` inside the loop (branch-range
  overflow risk, phase 6 lesson).
- Phase 7 address scripts define `BATCH_UNROLL` as a compile-time constant, set
  to 32. PRP address capture with `unroll(64)` generated enough code that the
  kernel rejected the fentry program; 32 keeps the prototype loadable while still
  covering normal validation batches. Phase 8 remains the dedicated batch-stress
  script.
- SGL per-entry walking is omitted from the batch `unroll(BATCH_UNROLL)` path; only
  `dptr.sgl` header fields are printed there. Full entry walking is available
  from the single-request `fexit` path.
- bpftrace local variables must keep one type within a probe. Do not reuse a name
  such as `$disk` for both `struct gendisk *` and a `uint64` disk offset; use
  distinct names such as `$gdisk` and `$disk`.
- On Linux 7.x with bpftrace 0.25, `--verbose` can fail with `-28` if the
  verifier log buffer is too small. If the only kernel log line is
  `processed ... insns`, rerun without `--verbose` or use a larger buffer, e.g.
  `sudo env BPFTRACE_LOG_SIZE=16777216 bpftrace --verbose ./tracing/bpftrace/phase7-segment-addresses-prp.bt`.

**Validation:**
- 4 KiB PRP read: `prp1` non-zero and page-aligned (low 12 bits = 0), `prp2` = 0,
  `nr_desc` = 0.
- 1 MiB PRP read or write: `prp1` and `prp2` both non-zero, `nr_desc` = 1 on the
  current driver, and `prp2` is the DMA address of the PRP list page rather than
  a data page.
- For a 1 MiB page-aligned write with 512-byte logical blocks, observed output was
  internally consistent: `slba=4120576` matched `disk_off=0x7dc00000`, PRP1
  covered 4096 bytes, the PRP-list remainder was `1048576 - 4096 = 1044480`
  bytes, and the previewed PRP-list entries advanced by 4 KiB in both host DMA
  address and disk destination.
- `nr_desc` must be bounded by `NVME_MAX_NR_DESCRIPTORS` from the PCI driver
  (5 in the local `linux/drivers/nvme/host/pci.c`). Values such as `nr_desc=72`
  indicate the script is reading `struct nvme_iod` from the wrong address.
- SGL mode: `dptr.sgl.type` upper nibble = 0 for small inline, 2/3 for segment list.
- SGL data entries stop printing when type upper nibble flips to 2/3 (link entries).

**Findings:**
- The local PCI driver is the source of truth for PRP layout:
  `NVME_MAX_NR_DESCRIPTORS` is 5, `PRPS_PER_PAGE` is 511, `struct nvme_iod`
  contains `total_len`, `descriptors[5]`, `dma_vecs`, and `nr_dma_vecs`, and
  `nvme_pci_setup_data_prp()` writes PRP-list virtual addresses into
  `iod->descriptors[]`.
- `blk_mq_rq_to_pdu(req)` is implemented as `req + 1`. In bpftrace this only has
  the same meaning if the variable is still a typed `struct request *`. If it is
  first converted to `uint64`, `+ 1` advances one byte and the resulting
  `struct nvme_iod *` is invalid.
- Disk byte offsets can be derived from the block layer request as
  `req->__sector << 9`; NVMe `rw.slba` is already in namespace logical-block
  units, so the two match directly only when the namespace LBA size is 512 bytes.

---

## Phase 8 — Batch Path

**File:** `tracing/bpftrace/phase8-batch.bt`

**Status:** Implemented.

Capture batched requests submitted via `nvme_submit_cmds`. Walk the `rq_list` linked
list via `req->rq_next`, bounded with `unroll()` (required by the BPF verifier).

Track batch submissions and correlate with `nvme_complete_batch_req`. Uses
`#define TARGET_DISK` for device filtering and a single pre-loop disk check (same
patterns as phases 5–7). Includes completion latency correlation and END summary.

**Design notes:**
- `#define TARGET_DISK` avoids the stack-too-large verifier error that a global
  map string causes (phase 6 lesson).
- Single `str()` check on `args->rqlist->head` before `unroll(64)` keeps the loop
  free of string operations (phase 6 branch-range lesson).
- Timestamp is captured once before the loop and stored per-request so that
  latency reflects submission time, not the per-request iteration timestamp.
- Only `nvme_complete_batch_req` is tracked here; requests on this disk should
  not appear in `nvme_complete_rq` under the batch workload.

**Validation:**
- Under `inv elbencho.run`, multiple `batch_submit` lines appear per
  `nvme_submit_cmds` invocation.
- Batched requests do NOT appear in `fexit:nvme:nvme_queue_rq` output (they go
  through `nvme_queue_rqs` → `nvme_submit_cmds` instead).
- Each `batch_submit` line is followed by exactly one `complete_batch` line with
  the same `req` value.
- `@unmatched` = 0 for a workload started after the script.
- No `batch_submit truncated after 64 requests` line appears during the phase 8
  validation workload.

---

## Workloads

| Phase | Workload |
|-------|----------|
| 1–2, 4 | `dd if=/dev/disk/by-id/nvme-KIOXIA_KCMYXRUG3T84_4FB0A0CT0LM3_1 of=/dev/null bs=4096 count=1 iflag=direct` |
| 3 | `dd if=/dev/disk/by-id/nvme-KIOXIA_KCMYXRUG3T84_4FB0A0CT0LM3_1 of=/dev/null bs=128K count=1 iflag=direct`, plus `skip=256` for the known-offset SLBA read, plus `dd if=/dev/zero of=/dev/disk/by-id/nvme-KIOXIA_KCMYXRUG3T84_4FB0A0CT0LM3_1 bs=128K count=1 oflag=direct` to test write opcode |
| 5 | Resolve the by-id validation disk to its `nvmeXnY` block name, add it to the device-name predicates, then run the 4 KiB direct read above while optionally issuing I/O to a non-target NVMe disk to confirm it is suppressed |
| 6 | The 4 KiB direct read above, plus `dd if=/dev/disk/by-id/nvme-KIOXIA_KCMYXRUG3T84_4FB0A0CT0LM3_1 of=/dev/null bs=1M count=1 iflag=direct` to compare descriptor counts |
| 7 | `dd if=/dev/disk/by-id/nvme-KIOXIA_KCMYXRUG3T84_4FB0A0CT0LM3_1 of=/dev/null bs=1M count=1 iflag=direct` to trigger a PRP list |
| 8 | `inv elbencho.run` (existing task) to stress the batch path |
