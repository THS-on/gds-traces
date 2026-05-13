mod capture;
mod print;
mod splice;

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(about = "Capture, splice, and print NVMe PCI relay traces")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Capture {
        #[arg(long)]
        controller: String,
        #[arg(long)]
        out: PathBuf,
        #[arg(long)]
        trace_dir: Option<PathBuf>,
        #[arg(long, default_value_t = 250)]
        drain_ms: u64,
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
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Capture {
            controller,
            out,
            trace_dir,
            drain_ms,
        } => capture::capture(&controller, trace_dir, &out, drain_ms),
        Command::Splice { out, inputs } => splice::splice(&inputs, &out),
        Command::Print { block_size, inputs } => print::print_records(&inputs, block_size),
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
