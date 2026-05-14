use std::fs::{File, OpenOptions};
use std::io::{self, Cursor, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::sync::Arc;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use binrw::{BinRead, BinWrite, BinWriterExt};
use clap::Parser;
use nix::fcntl::OFlag;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sched::{CpuSet, sched_setaffinity};
use nix::unistd::Pid;
use signal_hook::consts::signal::{SIGINT, SIGTERM};
use signal_hook::flag;

// Wire format: magic(4) + seq(4) + timestamp_ns(8) + fill(8) = 24 bytes
const RECORD_SIZE: usize = 24;
const READ_BUF: usize = 256 * 1024;

#[derive(BinRead, Debug)]
#[br(little, magic = 0xDEAD_BEEFu32)]
struct ReproRec {
    seq: u32,
    _timestamp_ns: u64,
    fill: u64,
}

#[derive(BinWrite)]
#[bw(little)]
struct ChunkHeader {
    timestamp_ns: u64,
    read_size: u64,
}

#[derive(Parser, Debug)]
#[command(about = "Read relay_repro/buf0 and detect duplicate records")]
struct Cli {
    /// Pin to this CPU
    #[arg(short = 'c', long, default_value_t = 1)]
    cpu: usize,

    /// Busy-loop reads instead of poll(2)
    #[arg(short = 'n', long)]
    no_poll: bool,

    /// Write raw read data to this log file (binary, with per-read headers)
    #[arg(long)]
    log: Option<PathBuf>,

    /// Path to relay buffer
    #[arg(default_value = "/sys/kernel/debug/relay_repro/buf0")]
    path: PathBuf,
}

fn hexdump(buf: &[u8]) {
    for (i, chunk) in buf.chunks(16).enumerate() {
        eprint!("{:04x}:", i * 16);
        for b in chunk {
            eprint!(" {:02x}", b);
        }
        for _ in chunk.len()..16 {
            eprint!("   ");
        }
        eprint!("  |");
        for &b in chunk {
            eprint!(
                "{}",
                if b.is_ascii_graphic() || b == b' ' {
                    b as char
                } else {
                    '.'
                }
            );
        }
        eprintln!("|");
    }
}

fn timestamp_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

// Writer thread: receives (timestamp_ns, raw_bytes) and writes them with a
// 16-byte header (ChunkHeader) before each chunk.
fn log_writer(rx: mpsc::Receiver<(u64, Vec<u8>)>, log_path: PathBuf) -> Result<()> {
    let mut file = File::create(&log_path)
        .with_context(|| format!("creating log file {}", log_path.display()))?;
    for (ts, data) in rx {
        file.write_le(&ChunkHeader { timestamp_ns: ts, read_size: data.len() as u64 })
            .with_context(|| format!("writing log header to {}", log_path.display()))?;
        file.write_all(&data)
            .with_context(|| format!("writing log data to {}", log_path.display()))?;
    }
    file.flush()
        .with_context(|| format!("flushing {}", log_path.display()))
}

fn run(cli: &Cli, stop: Arc<AtomicBool>) -> Result<u64> {
    let mut input = OpenOptions::new()
        .read(true)
        .custom_flags(OFlag::O_NONBLOCK.bits())
        .open(&cli.path)
        .with_context(|| format!("opening {}", cli.path.display()))?;

    // Optional async logger: sender is None when --log is not given.
    let (log_tx, log_handle): (Option<SyncSender<(u64, Vec<u8>)>>, _) =
        if let Some(ref log_path) = cli.log {
            let (tx, rx) = mpsc::sync_channel::<(u64, Vec<u8>)>(64);
            let path = log_path.clone();
            let handle = thread::spawn(move || log_writer(rx, path));
            (Some(tx), Some(handle))
        } else {
            (None, None)
        };

    let mut buf = vec![0u8; READ_BUF];
    let mut last_seq: Option<u32> = None;
    let mut last_toggle: Option<u64> = None;
    let mut total: u64 = 0;
    let mut dups: u64 = 0;
    let mut last_report: u64 = 0;

    println!(
        "Reading {}  cpu={}  mode={}  (Ctrl-C to stop)",
        cli.path.display(),
        cli.cpu,
        if cli.no_poll { "no-poll" } else { "poll" }
    );

    while !stop.load(Ordering::SeqCst) {
        if !cli.no_poll {
            let mut pfds = [PollFd::new(input.as_fd(), PollFlags::POLLIN)];
            match poll(&mut pfds, PollTimeout::try_from(100u16).unwrap()) {
                Ok(0) => continue,
                Ok(_)
                    if !pfds[0]
                        .revents()
                        .is_some_and(|r| r.contains(PollFlags::POLLIN)) =>
                {
                    continue
                }
                Ok(_) => {}
                Err(nix::errno::Errno::EINTR) => continue,
                Err(e) => {
                    return Err(anyhow::Error::from(e))
                        .with_context(|| format!("polling {}", cli.path.display()));
                }
            }
        }

        let n = match input.read(&mut buf) {
            Ok(0) => continue,
            Ok(n) => n,
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                eprintln!("warning: read returned WouldBlock unexpectedly");
                continue;
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {
                eprintln!("warning: read interrupted");
                continue;
            }
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("reading {}", cli.path.display()));
            }
        };

        // Send raw bytes to logger (non-blocking; dropped if channel is full).
        if let Some(ref tx) = log_tx {
            let _ = tx.try_send((timestamp_ns(), buf[..n].to_vec()));
        }

        let corrupt = n % RECORD_SIZE != 0;
        if corrupt {
            eprintln!(
                "warning: read {} bytes — not a multiple of {} (record_size)",
                n, RECORD_SIZE
            );
            hexdump(&buf[..n]);
        }

        let mut offset = 0usize;
        while offset + RECORD_SIZE <= n {
            let slice = &buf[offset..offset + RECORD_SIZE];
            match ReproRec::read(&mut Cursor::new(slice)) {
                Err(_) => {
                    eprintln!(
                        "bad magic 0x{:08x} at offset {}",
                        u32::from_le_bytes(slice[..4].try_into().unwrap()),
                        offset
                    );
                    hexdump(&buf[..n]);
                    offset += 1;
                    continue;
                }
                Ok(rec) => {
                    let is_dup = last_seq
                        .is_some_and(|ls| rec.seq <= ls && last_toggle == Some(rec.fill));
                    if is_dup {
                        println!(
                            "DUP  off={:<14}  seq={:<10}  prev={}",
                            offset,
                            rec.seq,
                            last_seq.unwrap()
                        );
                        dups += 1;
                        hexdump(&buf[..n]);
                    }
                    last_seq = Some(rec.seq);
                    last_toggle = Some(rec.fill);
                    total += 1;
                    offset += RECORD_SIZE;
                }
            }
        }

        if total - last_report >= 1_000_000 {
            println!("  {} records  {} dups", total, dups);
            last_report = total;
        }
    }

    // Drop sender so the writer thread drains and exits.
    drop(log_tx);
    if let Some(handle) = log_handle {
        handle
            .join()
            .map_err(|_| anyhow::anyhow!("log writer thread panicked"))??;
    }

    println!("\nTotal: {} records  {} duplicates", total, dups);
    Ok(dups)
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let mut cpuset = CpuSet::new();
    cpuset.set(cli.cpu).context("building CPU set")?;
    if let Err(err) = sched_setaffinity(Pid::from_raw(0), &cpuset) {
        eprintln!("sched_setaffinity (continuing anyway): {}", err);
    }

    let stop = Arc::new(AtomicBool::new(false));
    flag::register(SIGINT, Arc::clone(&stop)).context("installing SIGINT handler")?;
    flag::register(SIGTERM, Arc::clone(&stop)).context("installing SIGTERM handler")?;

    let dups = run(&cli, stop)?;
    std::process::exit(if dups > 0 { 1 } else { 0 });
}
