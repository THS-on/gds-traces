//! ftrace-based NVMe PCI trace capture.
//!
//! Reads per-CPU ring buffer pages from
//! `/sys/kernel/debug/tracing/per_cpu/cpuN/trace_pipe_raw`, converts
//! `nvme_pci_submit` and `nvme_pci_complete` trace events into our existing
//! binary record format, and writes them to the caller-supplied output.
//!
//! # Setup sequence
//! 1. `nop` tracer, all events disabled, tracing off
//! 2. Size the per-CPU ring buffer
//! 3. `nooverwrite` mode: drop *new* events on overflow, preserve old data
//! 4. Clear leftover data
//! 5. Read event IDs and field offsets from the ftrace format files
//! 6. Apply optional `ctrl_id==N` filter
//! 7. Enable our two events, then `tracing_on = 1`
//!
//! # Cleanup sequence
//! 1. `tracing_on = 0`, disable events
//! 2. Drain remaining pages from reader threads
//! 3. Read per-CPU `stats` files; report and return the total overrun count
//! 4. Restore `overwrite` mode and default buffer size, clear trace

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use nvme_trace::{
    COMPLETE_LEN, NVME_TRACE_COMPLETE, NVME_TRACE_MAGIC, NVME_TRACE_SUBMIT, NVME_TRACE_VERSION,
    SUBMIT_FIXED_LEN,
};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Size of a raw ring-buffer page as returned by `trace_pipe_raw`.
pub const PAGE_SIZE: usize = 4096;

/// Byte offset within a page where event data begins (after 8-byte timestamp
/// and 8-byte commit counter).
const PAGE_DATA_OFFSET: usize = 16;

// `ring_buffer_event` type_len sentinel values.
const RINGBUF_TYPE_TIME_EXTEND: u8 = 29;
const RINGBUF_TYPE_TIME_STAMP: u8 = 30;
const RINGBUF_TYPE_PADDING: u8 = 31;

// ---------------------------------------------------------------------------
// Field-offset tables (read from ftrace format files at startup)
// ---------------------------------------------------------------------------

/// Byte offsets of `nvme_pci_submit` fields within the event data blob
/// (the blob includes the 8-byte `trace_entry` common header).
#[derive(Debug, Clone)]
pub struct SubmitFormat {
    pub ctrl_id: usize,
    pub qid: usize,
    pub cid: usize,
    pub data_len: usize,
    pub meta_len: usize,
    pub use_sgl: usize,
    pub single_segment: usize,
    pub sqe: usize,
    /// Offset of the `__data_loc` u32 that encodes descriptor offset+length.
    pub descriptors_dataloc: usize,
}

/// Byte offsets of `nvme_pci_complete` fields within the event data blob.
#[derive(Debug, Clone)]
pub struct CompleteFormat {
    pub ctrl_id: usize,
    pub qid: usize,
    pub cid: usize,
    pub result: usize,
    pub sq_head: usize,
    pub sq_id: usize,
    pub status: usize,
    pub retries: usize,
}

// ---------------------------------------------------------------------------
// FtraceCapture
// ---------------------------------------------------------------------------

/// Manages the lifetime of one ftrace capture session.
///
/// Constructed by [`FtraceCapture::setup`]; the caller must call [`stop`],
/// [`check_overruns`], and [`cleanup`] in that order after capture ends.
pub struct FtraceCapture {
    pub tracing_dir: PathBuf,
    /// Numeric ftrace event ID for `nvme_pci_submit`.
    pub submit_id: u16,
    /// Numeric ftrace event ID for `nvme_pci_complete`.
    pub complete_id: u16,
    pub submit_fmt: SubmitFormat,
    pub complete_fmt: CompleteFormat,
}

impl FtraceCapture {
    /// Configure ftrace for NVMe PCI tracing.
    pub fn setup(
        tracing_dir: &Path,
        ctrl_id_filter: Option<u32>,
        buffer_mb: u64,
    ) -> Result<Self> {
        let td = tracing_dir.to_path_buf();
        let ncpus = num_online_cpus(&td)?;

        write_file(&td.join("current_tracer"), "nop")?;
        write_file(&td.join("events/enable"), "0")?;
        write_file(&td.join("tracing_on"), "0")?;

        let per_cpu_kb = (buffer_mb * 1024) / ncpus as u64;
        write_file(&td.join("buffer_size_kb"), &per_cpu_kb.to_string())?;

        // Drop new events when full; preserve already-captured data.
        write_file(&td.join("trace_options"), "nooverwrite")?;

        // Clear any stale data from a previous session.
        write_file(&td.join("trace"), "")?;

        // Read event metadata before enabling, so the format files exist.
        let submit_id = read_event_id(&td, "nvme", "nvme_pci_submit")?;
        let complete_id = read_event_id(&td, "nvme", "nvme_pci_complete")?;
        let submit_fmt = parse_submit_format(&td)?;
        let complete_fmt = parse_complete_format(&td)?;

        if let Some(id) = ctrl_id_filter {
            let filter = format!("ctrl_id=={id}");
            write_file(&td.join("events/nvme/nvme_pci_submit/filter"), &filter)?;
            write_file(&td.join("events/nvme/nvme_pci_complete/filter"), &filter)?;
        }

        write_file(&td.join("events/nvme/nvme_pci_submit/enable"), "1")?;
        write_file(&td.join("events/nvme/nvme_pci_complete/enable"), "1")?;

        Ok(FtraceCapture {
            tracing_dir: td,
            submit_id,
            complete_id,
            submit_fmt,
            complete_fmt,
        })
    }

    /// Start tracing.  Call after all reader threads are spawned.
    pub fn start(&self) -> Result<()> {
        write_file(&self.tracing_dir.join("tracing_on"), "1")
    }

    /// Stop tracing and disable our events.  Call before signalling readers
    /// to stop so no new events enter the ring buffer during drain.
    pub fn stop(&self) -> Result<()> {
        write_file(&self.tracing_dir.join("tracing_on"), "0")?;
        write_file(
            &self.tracing_dir.join("events/nvme/nvme_pci_submit/enable"),
            "0",
        )?;
        write_file(
            &self.tracing_dir.join("events/nvme/nvme_pci_complete/enable"),
            "0",
        )
    }

    /// Sum `overrun` counts from all per-CPU stats files.  Must be called
    /// after reader threads have drained and exited so all pages are consumed.
    pub fn check_overruns(&self) -> Result<u64> {
        let mut total = 0u64;
        let per_cpu = self.tracing_dir.join("per_cpu");
        for entry in
            fs::read_dir(&per_cpu).with_context(|| format!("reading {}", per_cpu.display()))?
        {
            let entry = entry?;
            let stats = entry.path().join("stats");
            if let Ok(content) = fs::read_to_string(&stats) {
                for line in content.lines() {
                    if let Some(v) = line.strip_prefix("overrun:") {
                        if let Ok(n) = v.trim().parse::<u64>() {
                            total += n;
                        }
                    }
                }
            }
        }
        Ok(total)
    }

    /// Restore ftrace to a clean state.  Errors are logged but not fatal.
    pub fn cleanup(&self) {
        let _ = write_file(&self.tracing_dir.join("trace_options"), "overwrite");
        let _ = write_file(&self.tracing_dir.join("buffer_size_kb"), "1408");
        let _ = write_file(
            &self.tracing_dir.join("events/nvme/nvme_pci_submit/filter"),
            "0",
        );
        let _ = write_file(
            &self.tracing_dir.join("events/nvme/nvme_pci_complete/filter"),
            "0",
        );
        let _ = write_file(&self.tracing_dir.join("trace"), "");
    }

    /// Path of the blocking raw ring-buffer pipe for the given CPU.
    pub fn pipe_path(&self, cpu: u32) -> PathBuf {
        self.tracing_dir
            .join(format!("per_cpu/cpu{cpu}/trace_pipe_raw"))
    }

    /// Sorted list of online CPU indices (from the `per_cpu/` directory).
    pub fn online_cpus(&self) -> Result<Vec<u32>> {
        discover_cpus(&self.tracing_dir)
    }

    /// Parse one 4096-byte ring-buffer page and append converted binary
    /// records to `out`.  Returns the number of NVMe events extracted.
    pub fn parse_page(&self, page: &[u8; PAGE_SIZE], out: &mut Vec<u8>) -> usize {
        parse_page_events(
            page,
            self.submit_id,
            self.complete_id,
            &self.submit_fmt,
            &self.complete_fmt,
            out,
        )
    }
}

// ---------------------------------------------------------------------------
// ftrace file I/O helpers
// ---------------------------------------------------------------------------

fn write_file(path: &Path, value: &str) -> Result<()> {
    let mut f = OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    f.write_all(value.as_bytes())
        .with_context(|| format!("writing {}", path.display()))
}

fn read_event_id(tracing_dir: &Path, system: &str, event: &str) -> Result<u16> {
    let path = tracing_dir.join(format!("events/{system}/{event}/id"));
    let s = fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    s.trim()
        .parse::<u16>()
        .with_context(|| format!("parsing event ID from {}", path.display()))
}

fn num_online_cpus(tracing_dir: &Path) -> Result<u64> {
    discover_cpus(tracing_dir).map(|v| v.len() as u64)
}

fn discover_cpus(tracing_dir: &Path) -> Result<Vec<u32>> {
    let per_cpu = tracing_dir.join("per_cpu");
    let mut cpus = Vec::new();
    for entry in
        fs::read_dir(&per_cpu).with_context(|| format!("reading {}", per_cpu.display()))?
    {
        let entry = entry?;
        if let Some(n) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.strip_prefix("cpu"))
            .and_then(|s| s.parse::<u32>().ok())
        {
            cpus.push(n);
        }
    }
    if cpus.is_empty() {
        bail!("no per_cpu/cpuN directories under {}", per_cpu.display());
    }
    cpus.sort_unstable();
    Ok(cpus)
}

// ---------------------------------------------------------------------------
// Format file parsing
// ---------------------------------------------------------------------------

/// Return the byte offset of `field_name` within an ftrace event data blob,
/// as declared in an ftrace `format` file.
///
/// Lines in the format file look like:
/// ```text
///     field:int ctrl_id; offset:8; size:4; signed:1;
///     field:__data_loc u8[] descriptors; offset:98; size:4; signed:0;
/// ```
/// Array field names may have a suffix: `sqe[64]` → name "sqe".
fn format_field_offset(content: &str, field_name: &str) -> Option<usize> {
    for line in content.lines() {
        let line = line.trim();
        if !line.starts_with("field:") {
            continue;
        }
        // parts[0] = "field:TYPE… NAME"
        let mut parts = line.splitn(2, ';');
        let decl = parts.next()?.strip_prefix("field:")?;
        // Last whitespace token = "NAME" or "NAME[N]"
        let name_tok = decl.split_whitespace().last()?;
        let name = name_tok.split('[').next()?;
        if name != field_name {
            continue;
        }
        // Scan remaining semicolon-separated parts for "offset:N"
        for part in line.split(';') {
            if let Some(v) = part.trim().strip_prefix("offset:") {
                return v.trim().parse().ok();
            }
        }
    }
    None
}

fn read_format(tracing_dir: &Path, system: &str, event: &str) -> Result<String> {
    let path = tracing_dir.join(format!("events/{system}/{event}/format"));
    fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))
}

fn require_offset(content: &str, field: &str, event: &str) -> Result<usize> {
    format_field_offset(content, field)
        .with_context(|| format!("field '{field}' not found in format for '{event}'"))
}

fn parse_submit_format(tracing_dir: &Path) -> Result<SubmitFormat> {
    let c = read_format(tracing_dir, "nvme", "nvme_pci_submit")?;
    let ev = "nvme_pci_submit";
    Ok(SubmitFormat {
        ctrl_id: require_offset(&c, "ctrl_id", ev)?,
        qid: require_offset(&c, "qid", ev)?,
        cid: require_offset(&c, "cid", ev)?,
        data_len: require_offset(&c, "data_len", ev)?,
        meta_len: require_offset(&c, "meta_len", ev)?,
        use_sgl: require_offset(&c, "use_sgl", ev)?,
        single_segment: require_offset(&c, "single_segment", ev)?,
        sqe: require_offset(&c, "sqe", ev)?,
        descriptors_dataloc: require_offset(&c, "descriptors", ev)?,
    })
}

fn parse_complete_format(tracing_dir: &Path) -> Result<CompleteFormat> {
    let c = read_format(tracing_dir, "nvme", "nvme_pci_complete")?;
    let ev = "nvme_pci_complete";
    Ok(CompleteFormat {
        ctrl_id: require_offset(&c, "ctrl_id", ev)?,
        qid: require_offset(&c, "qid", ev)?,
        cid: require_offset(&c, "cid", ev)?,
        result: require_offset(&c, "result", ev)?,
        sq_head: require_offset(&c, "sq_head", ev)?,
        sq_id: require_offset(&c, "sq_id", ev)?,
        status: require_offset(&c, "status", ev)?,
        retries: require_offset(&c, "retries", ev)?,
    })
}

// ---------------------------------------------------------------------------
// Ring-buffer page parsing
// ---------------------------------------------------------------------------

/// Parse one 4096-byte page from `trace_pipe_raw` and append converted
/// binary records to `out`.  Returns the number of NVMe events extracted.
///
/// # Page layout
/// ```text
/// offset  0 .. 7   : page timestamp (u64 le, ns)
/// offset  8 .. 11  : committed byte count (u32 le)  [lower 32 bits of local_t]
/// offset 12 .. 15  : (upper 32 bits of local_t, ignored)
/// offset 16 .. end : packed ring_buffer_event records (commit bytes valid)
/// ```
///
/// Each `ring_buffer_event`:
/// ```text
/// [4 bytes: type_len(5 bits) | time_delta(27 bits)]
/// + if type_len == 0:   [4 bytes: data_len] [data_len bytes: event data]
/// + if type_len 1..=28: [type_len*4 bytes: event data]
/// + if type_len == 29 (TIME_EXTEND): [4 bytes: upper time bits]
/// + if type_len == 30 (TIME_STAMP):  [8 bytes: absolute timestamp]
/// + if type_len == 31 (PADDING):     rest of page is padding
/// ```
fn parse_page_events(
    page: &[u8; PAGE_SIZE],
    submit_id: u16,
    complete_id: u16,
    submit_fmt: &SubmitFormat,
    complete_fmt: &CompleteFormat,
    out: &mut Vec<u8>,
) -> usize {
    let page_ts = u64::from_le_bytes(page[0..8].try_into().unwrap());
    // Committed byte count: lower 32 bits of the local_t value at offset 8.
    let commit = u32::from_le_bytes(page[8..12].try_into().unwrap()) as usize;

    if commit == 0 || commit > PAGE_SIZE - PAGE_DATA_OFFSET {
        return 0;
    }

    let data = &page[PAGE_DATA_OFFSET..PAGE_DATA_OFFSET + commit];
    let mut pos = 0;
    let mut ts = page_ts;
    let mut count = 0;

    while pos + 4 <= data.len() {
        let hdr = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap());
        let type_len = (hdr & 0x1f) as u8;
        let time_delta = (hdr >> 5) as u64;
        pos += 4;

        match type_len {
            RINGBUF_TYPE_PADDING => break,

            RINGBUF_TYPE_TIME_STAMP => {
                if pos + 8 > data.len() {
                    break;
                }
                ts = u64::from_le_bytes(data[pos..pos + 8].try_into().unwrap());
                pos += 8;
            }

            RINGBUF_TYPE_TIME_EXTEND => {
                if pos + 4 > data.len() {
                    break;
                }
                let upper = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap()) as u64;
                pos += 4;
                ts = ts.wrapping_add((upper << 27) | time_delta);
            }

            0 => {
                // type_len == 0: next word is data length
                if pos + 4 > data.len() {
                    break;
                }
                let event_len = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
                pos += 4;
                ts = ts.wrapping_add(time_delta);
                if event_len == 0 || pos + event_len > data.len() {
                    break;
                }
                count += dispatch_event(
                    &data[pos..pos + event_len],
                    ts,
                    submit_id,
                    complete_id,
                    submit_fmt,
                    complete_fmt,
                    out,
                );
                pos += event_len;
            }

            n => {
                // type_len 1..=28: data_len = type_len * 4
                let event_len = n as usize * 4;
                ts = ts.wrapping_add(time_delta);
                if pos + event_len > data.len() {
                    break;
                }
                count += dispatch_event(
                    &data[pos..pos + event_len],
                    ts,
                    submit_id,
                    complete_id,
                    submit_fmt,
                    complete_fmt,
                    out,
                );
                pos += event_len;
            }
        }
    }

    count
}

/// Check the event type and encode it if it matches one of our events.
fn dispatch_event(
    event_data: &[u8],
    timestamp_ns: u64,
    submit_id: u16,
    complete_id: u16,
    submit_fmt: &SubmitFormat,
    complete_fmt: &CompleteFormat,
    out: &mut Vec<u8>,
) -> usize {
    if event_data.len() < 2 {
        return 0;
    }
    let event_type = u16::from_le_bytes(event_data[0..2].try_into().unwrap());

    if event_type == submit_id {
        if let Some(rec) = encode_submit(event_data, timestamp_ns, submit_fmt) {
            out.extend_from_slice(&rec);
            return 1;
        }
    } else if event_type == complete_id {
        if let Some(rec) = encode_complete(event_data, timestamp_ns, complete_fmt) {
            out.extend_from_slice(&rec);
            return 1;
        }
    }
    0
}

// ---------------------------------------------------------------------------
// Event-to-binary-record encoding
// ---------------------------------------------------------------------------

/// Convert a `nvme_pci_submit` event data blob into our relay-compatible
/// binary record format.  Returns `None` if the data is too short.
fn encode_submit(data: &[u8], timestamp_ns: u64, fmt: &SubmitFormat) -> Option<Vec<u8>> {
    // All fixed fields must be accessible.
    let min = fmt
        .descriptors_dataloc
        .checked_add(4)
        .filter(|&end| end <= data.len())?;
    let _ = min;
    fmt.sqe.checked_add(64).filter(|&end| end <= data.len())?;

    let ctrl_id =
        i32::from_le_bytes(data[fmt.ctrl_id..fmt.ctrl_id + 4].try_into().ok()?) as u8;
    let qid = i32::from_le_bytes(data[fmt.qid..fmt.qid + 4].try_into().ok()?) as u8;
    let cid = u16::from_le_bytes(data[fmt.cid..fmt.cid + 2].try_into().ok()?);
    let data_len =
        u32::from_le_bytes(data[fmt.data_len..fmt.data_len + 4].try_into().ok()?);
    let meta_len =
        u32::from_le_bytes(data[fmt.meta_len..fmt.meta_len + 4].try_into().ok()?);
    let use_sgl = data[fmt.use_sgl] != 0;
    let single_segment = data[fmt.single_segment] != 0;
    let sqe = &data[fmt.sqe..fmt.sqe + 64];

    // `__data_loc` encodes (len << 16) | offset, where offset is from the
    // start of the event data blob (including the 8-byte trace_entry header).
    let dataloc = u32::from_le_bytes(
        data[fmt.descriptors_dataloc..fmt.descriptors_dataloc + 4]
            .try_into()
            .ok()?,
    );
    let desc_off = (dataloc & 0xffff) as usize;
    let desc_len = (dataloc >> 16) as usize;
    let descriptors = if desc_len > 0 && desc_off + desc_len <= data.len() {
        &data[desc_off..desc_off + desc_len]
    } else {
        &[]
    };

    let nr_entries: u16 = if use_sgl {
        (descriptors.len() / 16) as u16 // sizeof(nvme_sgl_desc) = 16
    } else {
        (descriptors.len() / 8) as u16 // sizeof(__le64) = 8
    };

    let total_len = SUBMIT_FIXED_LEN + descriptors.len();
    let mut rec = Vec::with_capacity(total_len);

    // Header (24 bytes)
    rec.extend_from_slice(&NVME_TRACE_MAGIC.to_le_bytes());
    rec.push(NVME_TRACE_VERSION);
    rec.push(NVME_TRACE_SUBMIT);
    rec.extend_from_slice(&(total_len as u16).to_le_bytes());
    rec.extend_from_slice(&timestamp_ns.to_le_bytes());
    rec.extend_from_slice(&0u32.to_le_bytes()); // seq = 0 (ftrace has no per-device counter)
    rec.push(ctrl_id);
    rec.push(qid);
    rec.extend_from_slice(&cid.to_le_bytes());

    // Submit payload (76 bytes fixed)
    rec.extend_from_slice(sqe);
    rec.extend_from_slice(&data_len.to_le_bytes());
    rec.extend_from_slice(&meta_len.to_le_bytes());
    rec.push(u8::from(use_sgl));
    rec.push(u8::from(single_segment));
    rec.extend_from_slice(&nr_entries.to_le_bytes());

    // Variable descriptor tail
    rec.extend_from_slice(descriptors);

    Some(rec)
}

/// Convert a `nvme_pci_complete` event data blob into our binary record format.
fn encode_complete(data: &[u8], timestamp_ns: u64, fmt: &CompleteFormat) -> Option<Vec<u8>> {
    fmt.retries
        .checked_add(1)
        .filter(|&end| end <= data.len())?;

    let ctrl_id =
        i32::from_le_bytes(data[fmt.ctrl_id..fmt.ctrl_id + 4].try_into().ok()?) as u8;
    let qid = i32::from_le_bytes(data[fmt.qid..fmt.qid + 4].try_into().ok()?) as u8;
    let cid = u16::from_le_bytes(data[fmt.cid..fmt.cid + 2].try_into().ok()?);
    let result = u64::from_le_bytes(data[fmt.result..fmt.result + 8].try_into().ok()?);
    let sq_head = u16::from_le_bytes(data[fmt.sq_head..fmt.sq_head + 2].try_into().ok()?);
    let sq_id = u16::from_le_bytes(data[fmt.sq_id..fmt.sq_id + 2].try_into().ok()?);
    let status = u16::from_le_bytes(data[fmt.status..fmt.status + 2].try_into().ok()?);
    let retries = data[fmt.retries];

    let mut rec = Vec::with_capacity(COMPLETE_LEN);

    // Header (24 bytes)
    rec.extend_from_slice(&NVME_TRACE_MAGIC.to_le_bytes());
    rec.push(NVME_TRACE_VERSION);
    rec.push(NVME_TRACE_COMPLETE);
    rec.extend_from_slice(&(COMPLETE_LEN as u16).to_le_bytes());
    rec.extend_from_slice(&timestamp_ns.to_le_bytes());
    rec.extend_from_slice(&0u32.to_le_bytes()); // seq = 0
    rec.push(ctrl_id);
    rec.push(qid);
    rec.extend_from_slice(&cid.to_le_bytes());

    // Complete payload (16 bytes)
    rec.extend_from_slice(&result.to_le_bytes());
    rec.extend_from_slice(&sq_head.to_le_bytes());
    rec.extend_from_slice(&sq_id.to_le_bytes());
    rec.extend_from_slice(&status.to_le_bytes());
    rec.push(retries);
    rec.push(0); // pad

    Some(rec)
}
