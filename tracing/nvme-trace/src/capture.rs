use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsFd;

use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};

static SIGINT_SEEN: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone, Eq, PartialEq)]
struct TraceFile {
    cpu: u32,
    path: PathBuf,
}

#[derive(Debug)]
struct CaptureStats {
    cpu: u32,
    path: PathBuf,
    bytes: u64,
}

pub(crate) fn capture(
    controller: &str,
    trace_dir_arg: Option<PathBuf>,
    out_dir: &Path,
    drain_ms: u64,
) -> Result<()> {
    let trace_dir = trace_dir_arg
        .unwrap_or_else(|| PathBuf::from("/sys/kernel/debug/nvme_trace").join(controller));
    let enable_path = trace_dir.join("enable");
    let trace_files = discover_trace_files(&trace_dir)
        .with_context(|| format!("discovering trace files under {}", trace_dir.display()))?;

    if trace_files.is_empty() {
        bail!("no trace<cpu> files found under {}", trace_dir.display());
    }

    fs::create_dir_all(out_dir)
        .with_context(|| format!("creating output directory {}", out_dir.display()))?;
    install_sigint_handler()?;

    let stop_readers = Arc::new(AtomicBool::new(false));
    let mut handles = Vec::with_capacity(trace_files.len());
    for trace_file in trace_files {
        let out_path = output_file_for_cpu(out_dir, trace_file.cpu);
        let stop = Arc::clone(&stop_readers);
        handles.push(thread::spawn(move || {
            capture_trace_file(trace_file.cpu, trace_file.path, out_path, stop)
        }));
    }

    write_enable(&enable_path, true)
        .with_context(|| format!("enabling tracing via {}", enable_path.display()))?;
    eprintln!("capturing; press Ctrl-C to stop");

    while !SIGINT_SEEN.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(100));
    }

    let disable_result = write_enable(&enable_path, false)
        .with_context(|| format!("disabling tracing via {}", enable_path.display()));
    thread::sleep(Duration::from_millis(drain_ms));
    stop_readers.store(true, Ordering::SeqCst);

    let mut stats = Vec::new();
    let mut join_errors = Vec::new();
    for handle in handles {
        match handle.join() {
            Ok(Ok(stat)) => stats.push(stat),
            Ok(Err(err)) => join_errors.push(err),
            Err(_) => join_errors.push(anyhow!("capture reader thread panicked")),
        }
    }

    disable_result?;
    if let Some(err) = join_errors.into_iter().next() {
        return Err(err);
    }

    stats.sort_by_key(|stat| stat.cpu);
    for stat in stats {
        eprintln!(
            "cpu{}: wrote {} bytes to {}",
            stat.cpu,
            stat.bytes,
            stat.path.display()
        );
    }

    Ok(())
}

fn capture_trace_file(
    cpu: u32,
    trace_path: PathBuf,
    out_path: PathBuf,
    stop: Arc<AtomicBool>,
) -> Result<CaptureStats> {
    let mut input = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&trace_path)
        .with_context(|| format!("opening relay file {}", trace_path.display()))?;
    let mut output = File::create(&out_path)
        .with_context(|| format!("creating output file {}", out_path.display()))?;
    let mut buf = [0_u8; 128 * 1024];
    let mut bytes = 0_u64;

    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }

        let mut pfds = [PollFd::new(input.as_fd(), PollFlags::POLLIN)];
        match poll(&mut pfds, PollTimeout::try_from(100u16).unwrap()) {
            Ok(0) => continue,
            Ok(_) if !pfds[0].revents().is_some_and(|r| r.contains(PollFlags::POLLIN)) => {
                continue;
            }
            Ok(_) => {}
            Err(nix::errno::Errno::EINTR) => continue,
            Err(e) => {
                return Err(anyhow::Error::from(e))
                    .with_context(|| format!("polling relay file {}", trace_path.display()));
            }
        }

        match input.read(&mut buf) {
            Ok(0) => {}
            Ok(n) => {
                output
                    .write_all(&buf[..n])
                    .with_context(|| format!("writing {}", out_path.display()))?;
                bytes += n as u64;
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {}
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("reading relay file {}", trace_path.display()));
            }
        }
    }

    output
        .flush()
        .with_context(|| format!("flushing {}", out_path.display()))?;
    Ok(CaptureStats {
        cpu,
        path: out_path,
        bytes,
    })
}

fn discover_trace_files(trace_dir: &Path) -> Result<Vec<TraceFile>> {
    let mut files = Vec::new();
    for entry in
        fs::read_dir(trace_dir).with_context(|| format!("reading {}", trace_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if let Some(cpu) = parse_trace_cpu(name) {
            files.push(TraceFile { cpu, path });
        }
    }
    files.sort_by_key(|file| file.cpu);
    Ok(files)
}

fn parse_trace_cpu(name: &str) -> Option<u32> {
    let suffix = name.strip_prefix("trace")?;
    if suffix.is_empty() || !suffix.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    suffix.parse().ok()
}

fn output_file_for_cpu(out_dir: &Path, cpu: u32) -> PathBuf {
    out_dir.join(format!("cpu{cpu}.bin"))
}

fn write_enable(enable_path: &Path, enabled: bool) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .open(enable_path)
        .with_context(|| format!("opening {}", enable_path.display()))?;
    file.write_all(if enabled { b"1\n" } else { b"0\n" })
        .with_context(|| format!("writing {}", enable_path.display()))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_trace_cpu_names() {
        assert_eq!(parse_trace_cpu("trace0"), Some(0));
        assert_eq!(parse_trace_cpu("trace123"), Some(123));
        assert_eq!(parse_trace_cpu("trace"), None);
        assert_eq!(parse_trace_cpu("traceabc"), None);
        assert_eq!(parse_trace_cpu("enable"), None);
    }

    #[test]
    fn maps_output_file_for_cpu() {
        assert_eq!(
            output_file_for_cpu(Path::new("traces/nvme0"), 7),
            PathBuf::from("traces/nvme0/cpu7.bin")
        );
    }
}
