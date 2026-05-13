use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use nvme_trace::splice_named_streams;

use crate::expand_input_paths;

pub(crate) fn splice(inputs: &[PathBuf], out: &Path) -> Result<()> {
    let input_files = expand_input_paths(inputs)?;
    if input_files.is_empty() {
        bail!("no input trace files found");
    }

    if let Some(parent) = out.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating output directory {}", parent.display()))?;
    }

    let mut streams = Vec::with_capacity(input_files.len());
    for path in input_files {
        let file = File::open(&path).with_context(|| format!("opening {}", path.display()))?;
        streams.push((path.display().to_string(), file));
    }

    let mut output = File::create(out).with_context(|| format!("creating {}", out.display()))?;
    let written = splice_named_streams(streams, &mut output, |warning| {
        eprintln!("warning: {warning}");
    })?;
    output
        .flush()
        .with_context(|| format!("flushing {}", out.display()))?;
    eprintln!("wrote {written} bytes to {}", out.display());
    Ok(())
}
