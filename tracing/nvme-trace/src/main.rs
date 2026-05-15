mod analyze;
mod capture;
mod ftrace;
mod print;
mod splice;

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(about = "Capture, splice, and print NVMe PCI ftrace captures")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Capture {
        /// Filter events to a single controller (sets `ctrl_id==N` ftrace filter).
        /// Omit to capture all controllers.
        #[arg(long)]
        ctrl_id: Option<u32>,
        /// Total ftrace ring-buffer size in MiB, split across CPUs.
        #[arg(long, default_value_t = 512)]
        buffer_mb: u64,
        /// Milliseconds to drain remaining pages after Ctrl-C.
        #[arg(long, default_value_t = 250)]
        drain_ms: u64,
        /// Override the ftrace directory (default: /sys/kernel/debug/tracing).
        #[arg(long)]
        tracing_dir: Option<PathBuf>,
        /// Directory to write per-CPU output files (cpu0.bin, cpu1.bin, …).
        #[arg(long)]
        out: PathBuf,
    },
    Splice {
        #[arg(long)]
        out: PathBuf,
        #[arg(required = true)]
        inputs: Vec<PathBuf>,
    },
    Print {
        #[arg(long, default_value_t = 512)]
        block_size: u64,
        #[arg(required = true)]
        inputs: Vec<PathBuf>,
    },
    Analyze {
        #[command(subcommand)]
        command: AnalyzeCommand,
    },
}

#[derive(Debug, Subcommand)]
enum AnalyzeCommand {
    Throughput {
        #[arg(long)]
        scale: String,
        #[arg(required = true)]
        inputs: Vec<PathBuf>,
    },
    QueueDepth {
        #[arg(long)]
        scale: String,
        #[arg(required = true)]
        inputs: Vec<PathBuf>,
    },
    QueueDepthPercent {
        #[arg(required = true)]
        inputs: Vec<PathBuf>,
    },
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Capture {
            ctrl_id,
            buffer_mb,
            drain_ms,
            tracing_dir,
            out,
        } => capture::capture(ctrl_id, buffer_mb, drain_ms, tracing_dir, &out),
        Command::Splice { out, inputs } => splice::splice(&inputs, &out),
        Command::Print { block_size, inputs } => print::print_records(&inputs, block_size),
        Command::Analyze { command } => match command {
            AnalyzeCommand::Throughput { scale, inputs } => analyze::throughput(&inputs, &scale),
            AnalyzeCommand::QueueDepth { scale, inputs } => analyze::queue_depth(&inputs, &scale),
            AnalyzeCommand::QueueDepthPercent { inputs } => analyze::queue_depth_percent(&inputs),
        },
    }
}

pub(crate) fn expand_input_paths(inputs: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for input in inputs {
        if input.is_dir() {
            files.extend(expand_trace_dir(input)?);
        } else {
            files.push(input.clone());
        }
    }
    files.sort_by_key(|path| sort_input_path(path));
    Ok(files)
}

fn expand_trace_dir(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if parse_cpu_bin(name).is_some() {
            files.push(path);
        }
    }
    files.sort_by_key(|path| sort_input_path(path));
    Ok(files)
}

fn sort_input_path(path: &Path) -> (Option<u32>, String) {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    (parse_cpu_bin(name), path.display().to_string())
}

fn parse_cpu_bin(name: &str) -> Option<u32> {
    let suffix = name.strip_prefix("cpu")?.strip_suffix(".bin")?;
    if suffix.is_empty() || !suffix.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    suffix.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cpu_bin_names() {
        assert_eq!(parse_cpu_bin("cpu0.bin"), Some(0));
        assert_eq!(parse_cpu_bin("cpu42.bin"), Some(42));
        assert_eq!(parse_cpu_bin("cpu.bin"), None);
        assert_eq!(parse_cpu_bin("trace0"), None);
        assert_eq!(parse_cpu_bin("spliced.bin"), None);
    }
}
