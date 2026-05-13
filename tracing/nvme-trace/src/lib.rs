use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::collections::HashMap;
use std::fmt;
use std::io::{self, Cursor, Read, Write};

use binrw::{BinRead, BinReaderExt};

pub const NVME_TRACE_MAGIC: u32 = 0x4e56_4d45;
pub const NVME_TRACE_VERSION: u8 = 1;
pub const NVME_TRACE_SUBMIT: u8 = 0;
pub const NVME_TRACE_COMPLETE: u8 = 1;

pub const HEADER_LEN: usize = 24;
pub const SUBMIT_FIXED_LEN: usize = 100;
pub const COMPLETE_LEN: usize = 40;
pub const SQE_LEN: usize = 64;
pub const PRP_ENTRY_LEN: usize = 8;
pub const SGL_DESC_LEN: usize = 16;

// The current Linux NVMe host path hard-codes NVME_CTRL_PAGE_SHIFT to 12 and
// programs CC.MPS from that value. The trace format does not store MPS, so the
// userspace parser must treat 4 KiB as an out-of-band format assumption.
// See linux/drivers/nvme/host/nvme.h and linux/drivers/nvme/host/core.c.
pub const ASSUMED_NVME_CTRL_PAGE_SIZE: usize = 4096;

#[derive(Debug)]
pub enum TraceError {
    Io(io::Error),
    Parse {
        offset: u64,
        err: binrw::Error,
    },
    TruncatedHeader {
        offset: u64,
        got: usize,
    },
    TruncatedPayload {
        offset: u64,
        expected: usize,
        got: usize,
    },
    InvalidMagic {
        offset: u64,
        magic: u32,
    },
    UnsupportedVersion {
        offset: u64,
        version: u8,
    },
    UnknownType {
        offset: u64,
        record_type: u8,
    },
    InvalidLength {
        offset: u64,
        record_type: u8,
        len: usize,
        reason: &'static str,
    },
}

impl fmt::Display for TraceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TraceError::Io(err) => write!(f, "{err}"),
            TraceError::Parse { offset, err } => {
                write!(f, "parse error at offset {offset}: {err}")
            }
            TraceError::TruncatedHeader { offset, got } => {
                write!(f, "truncated header at offset {offset}: got {got} bytes")
            }
            TraceError::TruncatedPayload {
                offset,
                expected,
                got,
            } => write!(
                f,
                "truncated payload at offset {offset}: expected {expected} bytes, got {got}"
            ),
            TraceError::InvalidMagic { offset, magic } => {
                write!(f, "bad magic at offset {offset}: 0x{magic:08x}")
            }
            TraceError::UnsupportedVersion { offset, version } => {
                write!(f, "unsupported version at offset {offset}: {version}")
            }
            TraceError::UnknownType {
                offset,
                record_type,
            } => write!(f, "unknown record type at offset {offset}: {record_type}"),
            TraceError::InvalidLength {
                offset,
                record_type,
                len,
                reason,
            } => write!(
                f,
                "invalid record length at offset {offset}: type={record_type} len={len}: {reason}"
            ),
        }
    }
}

impl std::error::Error for TraceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            TraceError::Io(err) => Some(err),
            TraceError::Parse { err, .. } => Some(err),
            _ => None,
        }
    }
}

impl From<io::Error> for TraceError {
    fn from(value: io::Error) -> Self {
        TraceError::Io(value)
    }
}

pub type Result<T> = std::result::Result<T, TraceError>;

#[derive(Debug, Clone, Copy, Eq, PartialEq, BinRead)]
#[br(little)]
pub struct Header {
    pub magic: u32,
    pub version: u8,
    pub record_type: u8,
    pub len: u16,
    pub timestamp_ns: u64,
    pub seq: u32,
    pub ctrl_id: u8,
    pub qid: u8,
    pub cid: u16,
}

#[derive(Debug, Clone, Eq, PartialEq, BinRead)]
#[br(little)]
pub struct SglDesc {
    pub addr: u64,
    pub length: u32,
    pub reserved: [u8; 3],
    pub desc_type: u8,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum SubmitTail {
    Prp(Vec<u64>),
    Sgl(Vec<SglDesc>),
}

impl SubmitTail {
    pub fn len(&self) -> usize {
        match self {
            SubmitTail::Prp(entries) => entries.len(),
            SubmitTail::Sgl(entries) => entries.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct SubmitRecord {
    pub header: Header,
    pub sqe: [u8; SQE_LEN],
    pub data_len: u32,
    pub meta_len: u32,
    pub use_sgl: bool,
    pub single_segment: bool,
    pub nr_entries: u16,
    pub tail: SubmitTail,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct CompleteRecord {
    pub header: Header,
    pub result: u64,
    pub sq_head: u16,
    pub sq_id: u16,
    pub status: u16,
    pub retries: u8,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum TraceRecord {
    Submit(SubmitRecord),
    Complete(CompleteRecord),
}

#[derive(Debug, BinRead)]
#[br(little)]
struct SubmitFixed {
    sqe: [u8; SQE_LEN],
    data_len: u32,
    meta_len: u32,
    #[br(map = |value: u8| value != 0)]
    use_sgl: bool,
    #[br(map = |value: u8| value != 0)]
    single_segment: bool,
    nr_entries: u16,
}

impl SubmitFixed {
    fn entry_len(&self) -> usize {
        if self.use_sgl {
            SGL_DESC_LEN
        } else {
            PRP_ENTRY_LEN
        }
    }

    fn expected_tail_len(&self) -> usize {
        self.nr_entries as usize * self.entry_len()
    }

    fn into_record(self, header: Header, tail: SubmitTail) -> SubmitRecord {
        SubmitRecord {
            header,
            sqe: self.sqe,
            data_len: self.data_len,
            meta_len: self.meta_len,
            use_sgl: self.use_sgl,
            single_segment: self.single_segment,
            nr_entries: self.nr_entries,
            tail,
        }
    }
}

#[derive(Debug, BinRead)]
#[br(little, import { use_sgl: bool, nr_entries: u16 })]
struct SubmitTailPayload {
    #[br(count = if use_sgl { 0 } else { nr_entries as usize })]
    prp_entries: Vec<u64>,
    #[br(count = if use_sgl { nr_entries as usize } else { 0 })]
    sgl_entries: Vec<SglDesc>,
}

impl SubmitTailPayload {
    fn into_tail(self, use_sgl: bool) -> SubmitTail {
        if use_sgl {
            SubmitTail::Sgl(self.sgl_entries)
        } else {
            SubmitTail::Prp(self.prp_entries)
        }
    }
}

#[derive(Debug, BinRead)]
#[br(little)]
struct CompletePayload {
    result: u64,
    sq_head: u16,
    sq_id: u16,
    status: u16,
    retries: u8,
    _reserved: u8,
}

impl CompletePayload {
    fn into_record(self, header: Header) -> CompleteRecord {
        CompleteRecord {
            header,
            result: self.result,
            sq_head: self.sq_head,
            sq_id: self.sq_id,
            status: self.status,
            retries: self.retries,
        }
    }
}

impl TraceRecord {
    pub fn header(&self) -> Header {
        match self {
            TraceRecord::Submit(record) => record.header,
            TraceRecord::Complete(record) => record.header,
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct RawRecord {
    pub header: Header,
    pub record: TraceRecord,
    pub bytes: Vec<u8>,
    pub offset: u64,
}

pub struct RecordReader<R> {
    inner: R,
    offset: u64,
}

impl<R: Read> RecordReader<R> {
    pub fn new(inner: R) -> Self {
        Self { inner, offset: 0 }
    }

    pub fn offset(&self) -> u64 {
        self.offset
    }

    pub fn next_record(&mut self) -> Result<Option<RawRecord>> {
        let offset = self.offset;
        let mut header_bytes = [0_u8; HEADER_LEN];
        let got = read_exact_or_eof(&mut self.inner, &mut header_bytes)?;
        if got == 0 {
            return Ok(None);
        }
        if got != HEADER_LEN {
            self.offset += got as u64;
            return Err(TraceError::TruncatedHeader { offset, got });
        }

        let header = parse_header(&header_bytes, offset)?;
        let len = header.len as usize;
        if len < HEADER_LEN {
            return Err(TraceError::InvalidLength {
                offset,
                record_type: header.record_type,
                len,
                reason: "record length is smaller than the header",
            });
        }

        let mut bytes = Vec::with_capacity(len);
        bytes.extend_from_slice(&header_bytes);
        bytes.resize(len, 0);

        let payload_len = len - HEADER_LEN;
        let got = read_exact_or_eof(&mut self.inner, &mut bytes[HEADER_LEN..])?;
        if got != payload_len {
            self.offset += HEADER_LEN as u64 + got as u64;
            return Err(TraceError::TruncatedPayload {
                offset,
                expected: payload_len,
                got,
            });
        }

        let record = parse_record_bytes(bytes, offset)?;
        self.offset += len as u64;
        Ok(Some(record))
    }
}

pub fn parse_record_bytes(bytes: Vec<u8>, offset: u64) -> Result<RawRecord> {
    if bytes.len() < HEADER_LEN {
        return Err(TraceError::TruncatedHeader {
            offset,
            got: bytes.len(),
        });
    }

    let header = parse_header(&bytes[..HEADER_LEN], offset)?;
    let len = header.len as usize;
    if len != bytes.len() {
        return Err(TraceError::InvalidLength {
            offset,
            record_type: header.record_type,
            len,
            reason: "header length does not match byte slice length",
        });
    }

    let record = match header.record_type {
        NVME_TRACE_SUBMIT => TraceRecord::Submit(parse_submit(header, &bytes, offset)?),
        NVME_TRACE_COMPLETE => TraceRecord::Complete(parse_complete(header, &bytes, offset)?),
        record_type => {
            return Err(TraceError::UnknownType {
                offset,
                record_type,
            });
        }
    };

    Ok(RawRecord {
        header,
        record,
        bytes,
        offset,
    })
}

pub fn record_order(a: &Header, b: &Header) -> Ordering {
    // seq is the authoritative order within a device: the kernel captures the
    // timestamp before claiming the atomic seq counter, so cross-CPU races can
    // produce a slightly-earlier timestamp paired with a higher seq number.
    // Use seq as the primary key for same-controller records; fall back to
    // timestamp only when comparing across different controllers.
    if a.ctrl_id == b.ctrl_id {
        a.seq
            .cmp(&b.seq)
            .then_with(|| a.qid.cmp(&b.qid))
            .then_with(|| a.cid.cmp(&b.cid))
    } else {
        a.timestamp_ns
            .cmp(&b.timestamp_ns)
            .then_with(|| a.ctrl_id.cmp(&b.ctrl_id))
            .then_with(|| a.seq.cmp(&b.seq))
            .then_with(|| a.qid.cmp(&b.qid))
            .then_with(|| a.cid.cmp(&b.cid))
    }
}

pub fn splice_streams<R: Read, W: Write, F: FnMut(String)>(
    inputs: Vec<(String, R)>,
    output: W,
    warn: F,
) -> Result<u64> {
    splice_named_streams(inputs, output, warn)
}

pub fn splice_named_streams<R: Read, W: Write, F: FnMut(String)>(
    inputs: Vec<(String, R)>,
    mut output: W,
    mut warn: F,
) -> Result<u64> {
    let mut names = Vec::with_capacity(inputs.len());
    let mut readers = Vec::with_capacity(inputs.len());
    for (name, reader) in inputs {
        names.push(name);
        readers.push(RecordReader::new(reader));
    }
    splice_streams_with_names(&mut readers, names, &mut output, &mut warn)
}

fn splice_streams_with_names<R: Read, W: Write, F: FnMut(String)>(
    readers: &mut [RecordReader<R>],
    names: Vec<String>,
    output: &mut W,
    warn: &mut F,
) -> Result<u64> {
    let mut heap = BinaryHeap::new();
    for (input_index, reader) in readers.iter_mut().enumerate() {
        if let Some(record) = reader.next_record()? {
            heap.push(HeapItem {
                source_name: names[input_index].clone(),
                input_index,
                record,
            });
        }
    }

    let mut written = 0_u64;
    let mut last_seq_by_ctrl = HashMap::<u8, u32>::new();

    while let Some(item) = heap.pop() {
        let header = item.record.header;
        if let Some(last_seq) = last_seq_by_ctrl.insert(header.ctrl_id, header.seq)
            && header.seq <= last_seq
        {
            warn(format!(
                "sequence is not increasing for ctrl {}: previous={} current={} source={} offset={}",
                header.ctrl_id, last_seq, header.seq, item.source_name, item.record.offset
            ));
        }

        output.write_all(&item.record.bytes)?;
        written += item.record.bytes.len() as u64;

        if let Some(next) = readers[item.input_index].next_record()? {
            heap.push(HeapItem {
                source_name: item.source_name,
                input_index: item.input_index,
                record: next,
            });
        }
    }

    Ok(written)
}

#[derive(Debug)]
struct HeapItem {
    source_name: String,
    input_index: usize,
    record: RawRecord,
}

impl Eq for HeapItem {}

impl PartialEq for HeapItem {
    fn eq(&self, other: &Self) -> bool {
        self.record.header == other.record.header
            && self.source_name == other.source_name
            && self.record.offset == other.record.offset
    }
}

impl Ord for HeapItem {
    fn cmp(&self, other: &Self) -> Ordering {
        record_order(&other.record.header, &self.record.header)
            .then_with(|| other.source_name.cmp(&self.source_name))
            .then_with(|| other.record.offset.cmp(&self.record.offset))
    }
}

impl PartialOrd for HeapItem {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn parse_header(bytes: &[u8], offset: u64) -> Result<Header> {
    let header: Header = read_bin(bytes, offset)?;

    if header.magic != NVME_TRACE_MAGIC {
        return Err(TraceError::InvalidMagic {
            offset,
            magic: header.magic,
        });
    }
    if header.version != NVME_TRACE_VERSION {
        return Err(TraceError::UnsupportedVersion {
            offset,
            version: header.version,
        });
    }

    Ok(header)
}

fn parse_submit(header: Header, bytes: &[u8], offset: u64) -> Result<SubmitRecord> {
    if bytes.len() < SUBMIT_FIXED_LEN {
        return Err(TraceError::InvalidLength {
            offset,
            record_type: header.record_type,
            len: bytes.len(),
            reason: "submit record is smaller than the fixed struct",
        });
    }

    let fixed: SubmitFixed = read_bin(&bytes[HEADER_LEN..SUBMIT_FIXED_LEN], offset)?;
    let tail_len = bytes.len() - SUBMIT_FIXED_LEN;
    if tail_len != fixed.expected_tail_len() {
        return Err(TraceError::InvalidLength {
            offset,
            record_type: header.record_type,
            len: bytes.len(),
            reason: "submit tail length does not match nr_entries",
        });
    }

    let tail: SubmitTailPayload = read_bin_args(
        &bytes[SUBMIT_FIXED_LEN..],
        offset,
        <SubmitTailPayload as BinRead>::Args::builder()
            .use_sgl(fixed.use_sgl)
            .nr_entries(fixed.nr_entries)
            .finalize(),
    )?;
    let tail = tail.into_tail(fixed.use_sgl);

    Ok(fixed.into_record(header, tail))
}

fn parse_complete(header: Header, bytes: &[u8], offset: u64) -> Result<CompleteRecord> {
    if bytes.len() != COMPLETE_LEN {
        return Err(TraceError::InvalidLength {
            offset,
            record_type: header.record_type,
            len: bytes.len(),
            reason: "completion record must be exactly the fixed struct size",
        });
    }

    let payload: CompletePayload = read_bin(&bytes[HEADER_LEN..], offset)?;
    Ok(payload.into_record(header))
}

fn read_bin<T>(bytes: &[u8], offset: u64) -> Result<T>
where
    T: for<'a> BinRead<Args<'a> = ()>,
{
    let mut reader = Cursor::new(bytes);
    reader
        .read_le()
        .map_err(|err| TraceError::Parse { offset, err })
}

fn read_bin_args<'a, T>(bytes: &[u8], offset: u64, args: T::Args<'a>) -> Result<T>
where
    T: BinRead,
{
    let mut reader = Cursor::new(bytes);
    T::read_le_args(&mut reader, args).map_err(|err| TraceError::Parse { offset, err })
}

fn read_exact_or_eof<R: Read>(reader: &mut R, mut buf: &mut [u8]) -> io::Result<usize> {
    let mut total = 0;
    while !buf.is_empty() {
        match reader.read(buf) {
            Ok(0) => return Ok(total),
            Ok(n) => {
                total += n;
                let tmp = buf;
                buf = &mut tmp[n..];
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn parses_completion_record() {
        let bytes = complete_record(100, 7, 1, 2, 3, 0xfeed_beef, 9);
        let record = parse_record_bytes(bytes, 0).unwrap();

        match record.record {
            TraceRecord::Complete(complete) => {
                assert_eq!(complete.header.timestamp_ns, 100);
                assert_eq!(complete.header.seq, 7);
                assert_eq!(complete.result, 0xfeed_beef);
                assert_eq!(complete.retries, 9);
            }
            TraceRecord::Submit(_) => panic!("expected completion"),
        }
    }

    #[test]
    fn parses_submit_without_tail() {
        let bytes = submit_record(100, 1, 4, 5, 6, false, false, &[]);
        let record = parse_record_bytes(bytes, 0).unwrap();

        match record.record {
            TraceRecord::Submit(submit) => {
                assert_eq!(submit.header.timestamp_ns, 100);
                assert_eq!(submit.data_len, 4096);
                assert_eq!(submit.meta_len, 0);
                assert!(!submit.use_sgl);
                assert_eq!(submit.tail, SubmitTail::Prp(Vec::new()));
            }
            TraceRecord::Complete(_) => panic!("expected submit"),
        }
    }

    #[test]
    fn parses_submit_with_prp_tail() {
        let bytes = submit_record(
            100,
            1,
            4,
            5,
            6,
            false,
            false,
            &[0x1000_u64.to_le_bytes(), 0x2000_u64.to_le_bytes()].concat(),
        );
        let record = parse_record_bytes(bytes, 0).unwrap();

        match record.record {
            TraceRecord::Submit(submit) => {
                assert_eq!(submit.nr_entries, 2);
                assert_eq!(submit.tail, SubmitTail::Prp(vec![0x1000, 0x2000]));
            }
            TraceRecord::Complete(_) => panic!("expected submit"),
        }
    }

    #[test]
    fn parses_submit_with_sgl_tail() {
        let mut tail = Vec::new();
        tail.extend_from_slice(&0x1000_u64.to_le_bytes());
        tail.extend_from_slice(&512_u32.to_le_bytes());
        tail.extend_from_slice(&[0, 0, 0, 0x11]);
        tail.extend_from_slice(&0x2000_u64.to_le_bytes());
        tail.extend_from_slice(&1024_u32.to_le_bytes());
        tail.extend_from_slice(&[0, 0, 0, 0x12]);

        let bytes = submit_record(100, 1, 4, 5, 6, true, false, &tail);
        let record = parse_record_bytes(bytes, 0).unwrap();

        match record.record {
            TraceRecord::Submit(submit) => {
                assert_eq!(submit.nr_entries, 2);
                assert_eq!(
                    submit.tail,
                    SubmitTail::Sgl(vec![
                        SglDesc {
                            addr: 0x1000,
                            length: 512,
                            reserved: [0, 0, 0],
                            desc_type: 0x11,
                        },
                        SglDesc {
                            addr: 0x2000,
                            length: 1024,
                            reserved: [0, 0, 0],
                            desc_type: 0x12,
                        },
                    ])
                );
            }
            TraceRecord::Complete(_) => panic!("expected submit"),
        }
    }

    #[test]
    fn rejects_bad_magic_version_and_type() {
        let mut bad_magic = complete_record(1, 1, 0, 1, 1, 0, 0);
        bad_magic[0..4].copy_from_slice(&0_u32.to_le_bytes());
        assert!(matches!(
            parse_record_bytes(bad_magic, 0),
            Err(TraceError::InvalidMagic { .. })
        ));

        let mut bad_version = complete_record(1, 1, 0, 1, 1, 0, 0);
        bad_version[4] = 2;
        assert!(matches!(
            parse_record_bytes(bad_version, 0),
            Err(TraceError::UnsupportedVersion { .. })
        ));

        let mut bad_type = complete_record(1, 1, 0, 1, 1, 0, 0);
        bad_type[5] = 9;
        assert!(matches!(
            parse_record_bytes(bad_type, 0),
            Err(TraceError::UnknownType { .. })
        ));
    }

    #[test]
    fn rejects_truncated_and_invalid_lengths() {
        assert!(matches!(
            parse_record_bytes(vec![0; HEADER_LEN - 1], 0),
            Err(TraceError::TruncatedHeader { .. })
        ));

        let mut too_small = complete_record(1, 1, 0, 1, 1, 0, 0);
        too_small[6..8].copy_from_slice(&(HEADER_LEN as u16 - 1).to_le_bytes());
        too_small.truncate(HEADER_LEN - 1);
        assert!(matches!(
            parse_record_bytes(too_small, 0),
            Err(TraceError::TruncatedHeader { .. }) | Err(TraceError::InvalidLength { .. })
        ));

        let truncated = complete_record(1, 1, 0, 1, 1, 0, 0);
        let mut reader = RecordReader::new(Cursor::new(&truncated[..30]));
        assert!(matches!(
            reader.next_record(),
            Err(TraceError::TruncatedPayload { .. })
        ));
    }

    #[test]
    fn splices_streams_in_timestamp_sequence_order_and_preserves_bytes() {
        let a1 = complete_record(10, 1, 0, 1, 10, 0, 0);
        let a2 = complete_record(30, 3, 0, 1, 30, 0, 0);
        let b1 = complete_record(20, 2, 0, 2, 20, 0, 0);
        let b2 = complete_record(30, 4, 0, 2, 40, 0, 0);

        let mut input_a = Vec::new();
        input_a.extend_from_slice(&a1);
        input_a.extend_from_slice(&a2);
        let mut input_b = Vec::new();
        input_b.extend_from_slice(&b1);
        input_b.extend_from_slice(&b2);

        let mut output = Vec::new();
        let mut warnings = Vec::new();
        splice_named_streams(
            vec![
                ("cpu0.bin".to_string(), Cursor::new(input_a)),
                ("cpu1.bin".to_string(), Cursor::new(input_b)),
            ],
            &mut output,
            |warning| warnings.push(warning),
        )
        .unwrap();

        let mut expected = Vec::new();
        expected.extend_from_slice(&a1);
        expected.extend_from_slice(&b1);
        expected.extend_from_slice(&a2);
        expected.extend_from_slice(&b2);
        assert_eq!(output, expected);
        assert!(warnings.is_empty());
    }

    #[test]
    fn splicing_uses_deterministic_ties() {
        let a = complete_record(10, 1, 0, 1, 2, 0, 0);
        let b = complete_record(10, 1, 0, 1, 1, 0, 0);

        let mut output = Vec::new();
        splice_named_streams(
            vec![
                ("cpu0.bin".to_string(), Cursor::new(a.clone())),
                ("cpu1.bin".to_string(), Cursor::new(b.clone())),
            ],
            &mut output,
            |_| {},
        )
        .unwrap();

        let mut expected = Vec::new();
        expected.extend_from_slice(&b);
        expected.extend_from_slice(&a);
        assert_eq!(output, expected);
    }

    fn complete_record(
        timestamp_ns: u64,
        seq: u32,
        ctrl_id: u8,
        qid: u8,
        cid: u16,
        result: u64,
        retries: u8,
    ) -> Vec<u8> {
        let mut bytes = header(
            NVME_TRACE_COMPLETE,
            COMPLETE_LEN as u16,
            timestamp_ns,
            seq,
            ctrl_id,
            qid,
            cid,
        );
        bytes.extend_from_slice(&result.to_le_bytes());
        bytes.extend_from_slice(&7_u16.to_le_bytes());
        bytes.extend_from_slice(&(qid as u16).to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes.push(retries);
        bytes.push(0);
        assert_eq!(bytes.len(), COMPLETE_LEN);
        bytes
    }

    #[allow(clippy::too_many_arguments)]
    fn submit_record(
        timestamp_ns: u64,
        seq: u32,
        ctrl_id: u8,
        qid: u8,
        cid: u16,
        use_sgl: bool,
        single_segment: bool,
        tail: &[u8],
    ) -> Vec<u8> {
        let entry_len = if use_sgl { SGL_DESC_LEN } else { PRP_ENTRY_LEN };
        assert_eq!(tail.len() % entry_len, 0);
        let nr_entries = tail.len() / entry_len;
        let len = SUBMIT_FIXED_LEN + tail.len();
        let mut bytes = header(
            NVME_TRACE_SUBMIT,
            len as u16,
            timestamp_ns,
            seq,
            ctrl_id,
            qid,
            cid,
        );
        let mut sqe = [0_u8; SQE_LEN];
        sqe[0] = 0x02;
        bytes.extend_from_slice(&sqe);
        bytes.extend_from_slice(&4096_u32.to_le_bytes());
        bytes.extend_from_slice(&0_u32.to_le_bytes());
        bytes.push(u8::from(use_sgl));
        bytes.push(u8::from(single_segment));
        bytes.extend_from_slice(&(nr_entries as u16).to_le_bytes());
        bytes.extend_from_slice(tail);
        bytes
    }

    fn header(
        record_type: u8,
        len: u16,
        timestamp_ns: u64,
        seq: u32,
        ctrl_id: u8,
        qid: u8,
        cid: u16,
    ) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&NVME_TRACE_MAGIC.to_le_bytes());
        bytes.push(NVME_TRACE_VERSION);
        bytes.push(record_type);
        bytes.extend_from_slice(&len.to_le_bytes());
        bytes.extend_from_slice(&timestamp_ns.to_le_bytes());
        bytes.extend_from_slice(&seq.to_le_bytes());
        bytes.push(ctrl_id);
        bytes.push(qid);
        bytes.extend_from_slice(&cid.to_le_bytes());
        assert_eq!(bytes.len(), HEADER_LEN);
        bytes
    }
}
