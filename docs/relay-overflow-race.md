# relay.c: TOCTOU overflow race in `relay_file_read`

## Background

The kernel relay subsystem (`kernel/relay.c`) implements a per-CPU ring buffer
used for high-frequency tracing.  The ring is divided into `n_subbufs` fixed-
size slots.  The producer (typically running in interrupt context) writes
records into the current slot and calls `relay_switch_subbuf()` when it is
full, advancing `subbufs_produced`.  The consumer calls `read(2)` on the relay
file; internally `relay_file_read()` calls `relay_file_read_avail()` to check
for data, computes how many bytes are available in the current slot, then
copies them to user space.

### Key state

```
struct rchan_buf {
    size_t subbufs_produced;   /* incremented by producer on every slot switch */
    size_t subbufs_consumed;   /* incremented by consumer after reading a slot  */
    size_t bytes_consumed;     /* byte offset within the current consumed slot  */
    size_t offset;             /* byte offset within the current produced slot  */
    void  *data;               /* pointer to the current produced slot          */
    size_t padding[];          /* per-slot: unused bytes at the end             */
};
```

Physical slot index for any absolute subbuf number N:

```
physical_slot = N % n_subbufs
```

---

## The bug: TOCTOU between overflow detection and `copy_to_user`

### Overflow detection in `relay_file_read_avail()`

When the producer is more than one full ring ahead of the consumer
(`subbufs_produced - subbufs_consumed >= n_subbufs`), the consumer has been
lapped.  `relay_file_read_avail()` handles this by fast-forwarding the
consumer:

```c
/* kernel/relay.c – relay_file_read_avail() */
size_t produced = READ_ONCE(buf->subbufs_produced);   /* snapshot A */
...
if (unlikely(produced - consumed >= n_subbufs)) {
    produced = READ_ONCE(buf->subbufs_produced);       /* snapshot B */
    consumed = produced - n_subbufs + 1;               /* fast-forward */
    buf->subbufs_consumed = consumed;
    buf->bytes_consumed   = 0;
}
```

After the fast-forward, the consumer is positioned at absolute subbuf
`produced_B - n_subbufs + 1`, which maps to physical slot:

```
(produced_B - n_subbufs + 1) % n_subbufs
    = (produced_B + 1) % n_subbufs          [since -n_subbufs ≡ 0 mod n_subbufs]
```

The producer's **next** subbuf switch will be to subbuf `produced_B + 1`,
which maps to:

```
(produced_B + 1) % n_subbufs
```

**The consumer was placed on exactly the same physical slot as the producer's
next write.**

### The race window

`relay_file_read()` calls `relay_file_read_avail()`, then separately calls
`relay_file_read_subbuf_avail()` to size the copy, then does `copy_to_user()`.
None of this holds any lock against the producer.  The producer runs freely in
interrupt context, even on the same CPU.

```
Consumer (process context)          Producer (interrupt context)
──────────────────────────────      ────────────────────────────
relay_file_read_avail()
  produced_B = 7  (snapshot B)
  consumed   = 4  (fast-forward)    [still writing subbuf 7 → slot 3]
  → read from physical slot 0

relay_file_read_start_pos()  → 0
relay_file_read_subbuf_avail()
  avail = subbuf_size               [slot 0 is complete, slot 3 ≠ slot 0]

                                    relay_switch_subbuf()
                                      subbufs_produced → 8
                                      → NOW writing subbuf 8 → slot 0 !!

copy_to_user(slot 0, avail)         writing records into slot 0 from offset 0
  reads bytes [0 .. avail)              ↕  concurrent ↕
```

---

## Walkthrough example

### Setup

```
n_subbufs  = 4
subbuf_size = S bytes (e.g. 1 MiB)
RECORD_SIZE = 24 bytes (magic + fill + seq + timestamp)
```

### Ring buffer layout (physical slots 0–3)

Each column is one physical slot.  Rows show the history of which absolute
subbuf occupied it.

```
slot:         0       1       2       3
            ┌───┐   ┌───┐   ┌───┐   ┌───┐
round 0     │ 0 │   │ 1 │   │ 2 │   │ 3 │
            ├───┤   ├───┤   ├───┤   ├───┤
round 1     │ 4 │   │ 5 │   │ 6 │   │ 7 │  ← produced=7, consumed=3
            ├───┤   ├───┤   ├───┤   ├───┤
round 2     │ 8 │   │ 9 │   │10 │   │11 │  ← producer will write here next
            └───┘   └───┘   └───┘   └───┘
             ↑
             consumer fast-forwards here (consumed=4)
             producer's NEXT write also goes here (subbuf 8)
```

### State just before the race

```
subbufs_produced  = 7   (subbufs 0–6 complete; writing subbuf 7 → slot 3)
subbufs_consumed  = 3   (subbufs 0–2 consumed)

overflow check: 7 – 3 = 4 = n_subbufs  → triggered
fast-forward  : consumed = 7 – 4 + 1 = 4  → physical slot 0
```

Slot 0 currently holds subbuf 4 data (written in round 1):

```
slot 0 (subbuf 4):
┌──────────────────────────────────────────────────────────────┐
│ rec[0] seq=400 │ rec[1] seq=401 │ … │ rec[N-1] seq=400+N-1  │
└──────────────────────────────────────────────────────────────┘
```

### The race unfolds

The consumer begins `copy_to_user(slot 0, subbuf_size)`.
Meanwhile the producer finishes subbuf 7 and calls `relay_switch_subbuf()`:

```
subbufs_produced → 8
buf->data        → slot 0   (wraps around)
buf->offset      → 0
```

The producer starts writing subbuf 8 records into slot 0 from offset 0:

```
slot 0 during copy_to_user:

offset   0        M*24       subbuf_size
         ├────────┼──────────────────────┤
producer │subbuf 8│  (not yet written)   │  writing →
consumer │        copy_to_user reading → │
         └────────┴──────────────────────┘
```

When the dust settles the consumer has read:

```
[0 .. M*24)      → subbuf 8 records  seq = 800, 801, …, 800+M-1   VALID
[M*24 .. S)      → subbuf 4 records  seq = 400, 401, …              STALE
```

### What the reader sees

The client code tracks `last_seq` and fires on `rec.seq <= last_seq` with the
same `fill` value:

```
…
rec seq=800  ✓
rec seq=801  ✓
rec seq=802  ✓
…
rec seq=800+M-1  ✓
rec seq=400   DUP  (seq <= 800+M-1, same fill)
rec seq=401   DUP
…
```

Valid monotone records at the front, stale older records at the tail —
exactly the observed symptom.

---

## Why naive fixes do not work

### Fix attempt 1 – `continue` on guard failure

```c
if (unlikely(READ_ONCE(buf->subbufs_produced) - buf->subbufs_consumed
             >= buf->chan->n_subbufs))
    continue;   /* re-enter the loop */
```

The guard detects the race and re-enters the loop, which calls
`relay_file_read_avail()` again.  This fast-forwards `consumed` to
`produced_new - n_subbufs + 1`.  But if the producer is running at interrupt
rate on the **same CPU**, it fires between the fast-forward and the guard
re-check, advancing `produced_new` by 1 each time.  The condition is
perpetually true:

```
(produced_new + 1) - (produced_new - n_subbufs + 1) = n_subbufs  ≥ n_subbufs
```

The consumer spins forever holding `inode_lock`.  **Livelock.**

### Fix attempt 2 – inline re-sync, then fall through

```c
if (unlikely(READ_ONCE(buf->subbufs_produced) - buf->subbufs_consumed
             >= buf->chan->n_subbufs)) {
    relay_file_read_avail(buf);          /* re-sync consumed */
    read_start = relay_file_read_start_pos(buf);
    avail      = relay_file_read_subbuf_avail(read_start, buf);
    …
}
copy_to_user(…);
```

This avoids the livelock, but after `relay_file_read_avail()` runs,
`consumed` is again at `produced_new - n_subbufs + 1` — the same position as
the producer's next write.  The race window between the re-sync and
`copy_to_user` is shorter, but the **structural condition is identical** to
the original bug.

```
relay_file_read_avail()         → consumed = P – n + 1  (slot X)
  …
copy_to_user(slot X, avail)     ← producer writes subbuf P+1 → slot X
```

**The original race is reintroduced.**

### Root cause summary

After any fast-forward of the form `consumed = produced - n_subbufs + 1`,
the consumer occupies exactly the slot the producer will write to next.
A single producer subbuf switch is all it takes to create the collision.
Closing this race fully requires either:

1. preventing the producer from overwriting a slot the consumer holds
   (e.g. a spinlock or read-side critical section that blocks the
   producer's `relay_switch_subbuf()`), or
2. a post-copy generation check that detects whether the slot was
   overwritten while `copy_to_user` was running and discards the result.
