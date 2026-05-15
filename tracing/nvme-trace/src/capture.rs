use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};

use crate::ftrace::{FtraceCapture, PAGE_SIZE};

static SIGINT_SEEN: AtomicBool = AtomicBool::new(false);

#[derive(Debug)]
struct CaptureStats {
    cpu: u32,
    path: PathBuf,
    bytes_written: u64,
    events_captured: u64,
}

/// Capture NVMe PCI trace events via ftrace's per-CPU `trace_pipe_raw` files.
///
/// # Arguments
/// * `ctrl_id_filter` — if `Some(N)`, only record events for controller N
/// * `buffer_mb`      — total ring-buffer size in MiB (split across CPUs)
/// * `drain_ms`       — milliseconds to drain remaining pages after Ctrl-C
/// * `tracing_dir`    — override `/sys/kernel/debug/tracing` (for testing)
/// * `out_dir`        — directory to write `cpu{N}.bin` output files
pub(crate) fn capture(
    ctrl_id_filter: Option<u32>,
    buffer_mb: u64,
    drain_ms: u64,
    tracing_dir: Option<PathBuf>,
    out_dir: &Path,
) -> Result<()> {
    let tracing_dir = tracing_dir
        .unwrap_or_else(|| PathBuf::from("/sys/kernel/debug/tracing"));

    let ftrace = Arc::new(
        FtraceCapture::setup(&tracing_dir, ctrl_id_filter, buffer_mb)
            .context("setting up ftrace")?,
    );

    let cpus = ftrace.online_cpus()?;
    if cpus.is_empty() {
        bail!("no online CPUs found");
    }

    fs::create_dir_all(out_dir)
        .with_context(|| format!("creating output directory {}", out_dir.display()))?;
    install_sigint_handler()?;

    let stop = Arc::new(AtomicBool::new(false));
    let mut handles = Vec::with_capacity(cpus.len());
    for cpu in &cpus {
        let cpu = *cpu;
        let pipe = ftrace.pipe_path(cpu);
        let out = out_dir.join(format!("cpu{cpu}.bin"));
        let stop = Arc::clone(&stop);
        let ftrace = Arc::clone(&ftrace);
        handles.push(thread::spawn(move || {
            read_cpu_pipe(cpu, pipe, out, stop, ftrace)
        }));
    }

    // Start tracing only after all reader threads are ready.
    ftrace.start().context("enabling tracing_on")?;
    eprintln!("capturing; press Ctrl-C to stop");

    while !SIGINT_SEEN.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(100));
    }

    // Stop new events first, then let threads drain the remaining pages.
    let stop_result = ftrace.stop().context("disabling tracing_on");
    thread::sleep(Duration::from_millis(drain_ms));
    stop.store(true, Ordering::SeqCst);

    let mut stats = Vec::new();
    let mut join_errors = Vec::new();
    for handle in handles {
        match handle.join() {
            Ok(Ok(s)) => stats.push(s),
            Ok(Err(e)) => join_errors.push(e),
            Err(_) => join_errors.push(anyhow!("capture thread panicked")),
        }
    }

    stop_result?;
    for err in &join_errors {
        eprintln!("capture error: {err:#}");
    }
    if let Some(err) = join_errors.into_iter().next() {
        return Err(err);
    }

    // Check for dropped events; fail if any were lost.
    let overruns = ftrace.check_overruns().context("reading overrun stats")?;
    ftrace.cleanup();

    stats.sort_by_key(|s| s.cpu);
    for s in &stats {
        eprintln!(
            "cpu{}: {} events, {} bytes → {}",
            s.cpu,
            s.events_captured,
            s.bytes_written,
            s.path.display()
        );
    }

    if overruns > 0 {
        eprintln!("WARNING: {overruns} events were dropped (ring buffer overrun)");
        bail!("trace incomplete: {overruns} overrun events");
    }

    Ok(())
}

/// Per-CPU reader thread: reads raw ring-buffer pages from `trace_pipe_raw`,
/// parses them into binary records, and writes the records to `out_path`.
fn read_cpu_pipe(
    cpu: u32,
    pipe_path: PathBuf,
    out_path: PathBuf,
    stop: Arc<AtomicBool>,
    ftrace: Arc<FtraceCapture>,
) -> Result<CaptureStats> {
    let mut input = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&pipe_path)
        .with_context(|| format!("opening {}", pipe_path.display()))?;
    let mut output = File::create(&out_path)
        .with_context(|| format!("creating {}", out_path.display()))?;

    let mut page = [0u8; PAGE_SIZE];
    let mut records = Vec::new();
    let mut bytes_written = 0u64;
    let mut events_captured = 0u64;

    loop {
        if stop.load(Ordering::SeqCst) {
            // Drain any remaining pages before exiting.
            drain_pipe(&mut input, &mut output, &ftrace, &mut records,
                       &mut bytes_written, &mut events_captured, &out_path)?;
            break;
        }

        let mut pfds = [PollFd::new(input.as_fd(), PollFlags::POLLIN)];
        match poll(&mut pfds, PollTimeout::try_from(100u16).unwrap()) {
            Ok(0) => continue,
            Ok(_) if !pfds[0].revents().is_some_and(|r| r.contains(PollFlags::POLLIN)) => {
                continue
            }
            Ok(_) => {}
            Err(nix::errno::Errno::EINTR) => continue,
            Err(e) => {
                return Err(anyhow::Error::from(e))
                    .with_context(|| format!("polling {}", pipe_path.display()))
            }
        }

        if let Some((n, b)) = read_one_page(&mut input, &mut page, &ftrace, &mut records)? {
            output
                .write_all(&records)
                .with_context(|| format!("writing {}", out_path.display()))?;
            bytes_written += b;
            events_captured += n;
        }
    }

    output
        .flush()
        .with_context(|| format!("flushing {}", out_path.display()))?;
    Ok(CaptureStats {
        cpu,
        path: out_path,
        bytes_written,
        events_captured,
    })
}

/// Try to read one PAGE_SIZE chunk.  Returns `Some((events, bytes_of_records))`
/// on success, `None` on EAGAIN / EWOULDBLOCK.
fn read_one_page(
    input: &mut impl Read,
    page: &mut [u8; PAGE_SIZE],
    ftrace: &FtraceCapture,
    records: &mut Vec<u8>,
) -> Result<Option<(u64, u64)>> {
    // `trace_pipe_raw` delivers complete 4096-byte pages; accumulate until full.
    let mut total_read = 0;
    while total_read < PAGE_SIZE {
        match input.read(&mut page[total_read..]) {
            Ok(0) => {
                if total_read == 0 {
                    return Ok(None);
                }
                break; // partial page — process what we have
            }
            Ok(n) => total_read += n,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                if total_read == 0 {
                    return Ok(None);
                }
                break;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
    }

    records.clear();
    let n = ftrace.parse_page(page, records);
    let b = records.len() as u64;
    Ok(Some((n as u64, b)))
}

/// After the stop signal, keep reading until `trace_pipe_raw` returns EAGAIN.
fn drain_pipe(
    input: &mut impl Read,
    output: &mut File,
    ftrace: &FtraceCapture,
    records: &mut Vec<u8>,
    bytes_written: &mut u64,
    events_captured: &mut u64,
    out_path: &Path,
) -> Result<()> {
    let mut page = [0u8; PAGE_SIZE];
    loop {
        match read_one_page(input, &mut page, ftrace, records)? {
            None => break,
            Some((n, b)) => {
                output
                    .write_all(records)
                    .with_context(|| format!("writing {}", out_path.display()))?;
                *bytes_written += b;
                *events_captured += n;
            }
        }
    }
    Ok(())
}

fn install_sigint_handler() -> Result<()> {
    SIGINT_SEEN.store(false, Ordering::SeqCst);
    let previous = unsafe {
        libc::signal(
            libc::SIGINT,
            handle_sigint as *const () as libc::sighandler_t,
        )
    };
    if previous == libc::SIG_ERR {
        bail!("failed to install SIGINT handler");
    }
    Ok(())
}

extern "C" fn handle_sigint(_: libc::c_int) {
    SIGINT_SEEN.store(true, Ordering::SeqCst);
}
