use std::fs::File;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use nvme_trace::{
    ASSUMED_NVME_CTRL_PAGE_SIZE, CompleteRecord, RecordReader, SglDesc, SubmitRecord, SubmitTail,
    TraceRecord,
};

use crate::expand_input_paths;

pub(crate) fn print_records(inputs: &[PathBuf], block_size: u64) -> Result<()> {
    if block_size == 0 {
        bail!("--block-size must be greater than zero");
    }

    let input_files = expand_input_paths(inputs)?;
    if input_files.is_empty() {
        bail!("no input trace files found");
    }

    for path in input_files {
        let file = File::open(&path).with_context(|| format!("opening {}", path.display()))?;
        let mut reader = RecordReader::new(file);
        while let Some(raw) = reader
            .next_record()
            .with_context(|| format!("parsing {}", path.display()))?
        {
            println!(
                "{}",
                format_record(&path, &raw.record, raw.offset, block_size)
            );
        }
    }

    Ok(())
}

fn format_record(path: &Path, record: &TraceRecord, offset: u64, block_size: u64) -> String {
    match record {
        TraceRecord::Submit(submit) => {
            let mut line = format_submit(path, submit, offset, block_size);
            if !submit.use_sgl {
                let (prp1, prp2) = submit_prps(submit);
                line.push_str(&format!(
                    " mps={} prp1=0x{prp1:016x} prp2=0x{prp2:016x}",
                    ASSUMED_NVME_CTRL_PAGE_SIZE
                ));
            }
            line.push_str(&format!(" tail={}", format_submit_tail(&submit.tail)));
            line
        }
        TraceRecord::Complete(complete) => format_complete(path, complete, offset),
    }
}

fn format_submit(path: &Path, submit: &SubmitRecord, offset: u64, block_size: u64) -> String {
    let mode = if submit.use_sgl { "sgl" } else { "prp" };
    let opcode = NvmeOpcode::from(submit.sqe[0]);
    let mut line = format!(
        "{}:{offset} submit ts={} seq={} ctrl=nvme{} qid={} cid={} opcode={} data_len={} meta_len={} mode={} single_segment={} tail_entries={}",
        path.display(),
        submit.header.timestamp_ns,
        submit.header.seq,
        submit.header.ctrl_id,
        submit.header.qid,
        submit.header.cid,
        opcode,
        submit.data_len,
        submit.meta_len,
        mode,
        submit.single_segment,
        submit.tail.len(),
    );

    if opcode.is_read_or_write() {
        let nsid = sqe_le_u32(submit, 4);
        let slba = sqe_le_u64(submit, 40);
        let nlb = u64::from(sqe_le_u16(submit, 48)) + 1;
        let byte_offset = u128::from(slba) * u128::from(block_size);
        let sqe_data_bytes = u128::from(nlb) * u128::from(block_size);
        line.push_str(&format!(
            " nsid={} slba={} nlb={} block_size={} byte_offset={} sqe_data_bytes={}",
            nsid, slba, nlb, block_size, byte_offset, sqe_data_bytes
        ));
    }

    line
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum NvmeOpcode {
    Flush,
    Write,
    Read,
    WriteUncorrectable,
    Compare,
    WriteZeroes,
    DatasetManagement,
    Verify,
    Other(u8),
}

impl NvmeOpcode {
    fn is_read_or_write(self) -> bool {
        matches!(self, NvmeOpcode::Read | NvmeOpcode::Write)
    }
}

impl From<u8> for NvmeOpcode {
    fn from(value: u8) -> Self {
        match value {
            0x00 => NvmeOpcode::Flush,
            0x01 => NvmeOpcode::Write,
            0x02 => NvmeOpcode::Read,
            0x04 => NvmeOpcode::WriteUncorrectable,
            0x05 => NvmeOpcode::Compare,
            0x08 => NvmeOpcode::WriteZeroes,
            0x09 => NvmeOpcode::DatasetManagement,
            0x0c => NvmeOpcode::Verify,
            value => NvmeOpcode::Other(value),
        }
    }
}

impl std::fmt::Display for NvmeOpcode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NvmeOpcode::Flush => write!(f, "flush(0x00)"),
            NvmeOpcode::Write => write!(f, "write(0x01)"),
            NvmeOpcode::Read => write!(f, "read(0x02)"),
            NvmeOpcode::WriteUncorrectable => write!(f, "write_uncorrectable(0x04)"),
            NvmeOpcode::Compare => write!(f, "compare(0x05)"),
            NvmeOpcode::WriteZeroes => write!(f, "write_zeroes(0x08)"),
            NvmeOpcode::DatasetManagement => write!(f, "dataset_management(0x09)"),
            NvmeOpcode::Verify => write!(f, "verify(0x0c)"),
            NvmeOpcode::Other(value) => write!(f, "unknown(0x{value:02x})"),
        }
    }
}

fn submit_prps(submit: &SubmitRecord) -> (u64, u64) {
    (sqe_le_u64(submit, 24), sqe_le_u64(submit, 32))
}

fn sqe_le_u16(submit: &SubmitRecord, offset: usize) -> u16 {
    let bytes = submit.sqe[offset..offset + 2]
        .try_into()
        .expect("valid SQE field");
    u16::from_le_bytes(bytes)
}

fn sqe_le_u32(submit: &SubmitRecord, offset: usize) -> u32 {
    let bytes = submit.sqe[offset..offset + 4]
        .try_into()
        .expect("valid SQE field");
    u32::from_le_bytes(bytes)
}

fn sqe_le_u64(submit: &SubmitRecord, offset: usize) -> u64 {
    let bytes = submit.sqe[offset..offset + 8]
        .try_into()
        .expect("valid SQE field");
    u64::from_le_bytes(bytes)
}

fn format_submit_tail(tail: &SubmitTail) -> String {
    match tail {
        SubmitTail::Prp(entries) => format_prp_tail(entries),
        SubmitTail::Sgl(entries) => format_sgl_tail(entries),
    }
}

fn format_prp_tail(entries: &[u64]) -> String {
    let entries = entries
        .iter()
        .map(|entry| format!("0x{entry:016x}"))
        .collect::<Vec<_>>()
        .join(",");
    format!("[{entries}]")
}

fn format_sgl_tail(entries: &[SglDesc]) -> String {
    let entries = entries
        .iter()
        .map(|entry| {
            format!(
                "{{addr=0x{:016x},len={},type=0x{:02x},reserved={:02x}{:02x}{:02x}}}",
                entry.addr,
                entry.length,
                entry.desc_type,
                entry.reserved[0],
                entry.reserved[1],
                entry.reserved[2]
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    format!("[{entries}]")
}

fn format_complete(path: &Path, complete: &CompleteRecord, offset: u64) -> String {
    format!(
        "{}:{offset} complete ts={} seq={} ctrl=nvme{} qid={} cid={} result=0x{:016x} sq_head={} sq_id={} status=0x{:04x} retries={}",
        path.display(),
        complete.header.timestamp_ns,
        complete.header.seq,
        complete.header.ctrl_id,
        complete.header.qid,
        complete.header.cid,
        complete.result,
        complete.sq_head,
        complete.sq_id,
        complete.status,
        complete.retries
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use nvme_trace::{Header, SubmitRecord};

    #[test]
    fn formats_submit_tail_count() {
        let tail = SubmitTail::Prp(vec![1, 2, 3]);
        assert_eq!(tail.len(), 3);
        assert!(!tail.is_empty());
    }

    #[test]
    fn formats_prp_tail_details() {
        let record = TraceRecord::Submit(submit_record(
            SubmitTail::Prp(vec![0x1000, 0x2000]),
            0xfeed_0000,
            0xbeef_0000,
        ));

        let line = format_record(Path::new("cpu0.bin"), &record, 64, 512);

        assert!(line.contains("mps=4096"));
        assert!(line.contains("opcode=read(0x02)"));
        assert!(line.contains("nsid=1"));
        assert!(line.contains("slba=8192"));
        assert!(line.contains("nlb=8"));
        assert!(line.contains("block_size=512"));
        assert!(line.contains("byte_offset=4194304"));
        assert!(line.contains("sqe_data_bytes=4096"));
        assert!(line.contains("prp1=0x00000000feed0000"));
        assert!(line.contains("prp2=0x00000000beef0000"));
        assert!(line.contains("tail_entries=2"));
        assert!(line.contains("tail=[0x0000000000001000,0x0000000000002000]"));
    }

    #[test]
    fn formats_sgl_tail_details() {
        let record = TraceRecord::Submit(submit_record(
            SubmitTail::Sgl(vec![SglDesc {
                addr: 0x3000,
                length: 512,
                reserved: [0xaa, 0xbb, 0xcc],
                desc_type: 0x11,
            }]),
            0,
            0,
        ));

        let line = format_record(Path::new("cpu1.bin"), &record, 128, 4096);

        assert!(line.contains("mode=sgl"));
        assert!(line.contains("block_size=4096"));
        assert!(line.contains("byte_offset=33554432"));
        assert!(line.contains("sqe_data_bytes=32768"));
        assert!(!line.contains(" prp1="));
        assert!(
            line.contains("tail=[{addr=0x0000000000003000,len=512,type=0x11,reserved=aabbcc}]")
        );
    }

    fn submit_record(tail: SubmitTail, prp1: u64, prp2: u64) -> SubmitRecord {
        let use_sgl = matches!(tail, SubmitTail::Sgl(_));
        let mut sqe = [0_u8; nvme_trace::SQE_LEN];
        sqe[0] = 0x02;
        sqe[4..8].copy_from_slice(&1_u32.to_le_bytes());
        sqe[24..32].copy_from_slice(&prp1.to_le_bytes());
        sqe[32..40].copy_from_slice(&prp2.to_le_bytes());
        sqe[40..48].copy_from_slice(&8192_u64.to_le_bytes());
        sqe[48..50].copy_from_slice(&7_u16.to_le_bytes());

        SubmitRecord {
            header: Header {
                magic: nvme_trace::NVME_TRACE_MAGIC,
                version: nvme_trace::NVME_TRACE_VERSION,
                record_type: nvme_trace::NVME_TRACE_SUBMIT,
                len: 0,
                timestamp_ns: 100,
                seq: 7,
                ctrl_id: 0,
                qid: 1,
                cid: 2,
            },
            sqe,
            data_len: 4096,
            meta_len: 0,
            use_sgl,
            single_segment: false,
            nr_entries: tail.len() as u16,
            tail,
        }
    }
}
