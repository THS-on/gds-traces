use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::File;
use std::io::{self, Write};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use nvme_trace::{Header, RecordReader, TraceRecord};

use crate::expand_input_paths;

#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
struct CommandKey {
    ctrl_id: u8,
    qid: u8,
    cid: u16,
}

impl CommandKey {
    fn from_header(header: Header) -> Self {
        Self {
            ctrl_id: header.ctrl_id,
            qid: header.qid,
            cid: header.cid,
        }
    }

    fn queue(self) -> QueueKey {
        QueueKey {
            ctrl_id: self.ctrl_id,
            qid: self.qid,
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash)]
struct QueueKey {
    ctrl_id: u8,
    qid: u8,
}

pub(crate) fn throughput(inputs: &[PathBuf], scale: &str) -> Result<()> {
    let scale_ns = parse_scale_ns(scale)?;
    let mut output = io::BufWriter::new(io::stdout().lock());
    analyze_throughput_inputs(inputs, scale_ns, &mut output, |warning| {
        eprintln!("warning: {warning}");
    })
}

pub(crate) fn queue_depth(inputs: &[PathBuf], scale: &str) -> Result<()> {
    let scale_ns = parse_scale_ns(scale)?;
    let mut output = io::BufWriter::new(io::stdout().lock());
    analyze_queue_depth_inputs(inputs, scale_ns, &mut output, |warning| {
        eprintln!("warning: {warning}");
    })
}

pub(crate) fn queue_depth_percent(inputs: &[PathBuf]) -> Result<()> {
    let mut output = io::BufWriter::new(io::stdout().lock());
    analyze_queue_depth_percent_inputs(inputs, &mut output, |warning| {
        eprintln!("warning: {warning}");
    })
}

fn parse_scale_ns(scale: &str) -> Result<u64> {
    let duration =
        humantime::parse_duration(scale).with_context(|| format!("parsing --scale {scale:?}"))?;
    let nanos = duration.as_nanos();
    if nanos == 0 {
        bail!("--scale must be greater than zero");
    }
    u64::try_from(nanos).context("--scale is too large to represent as nanoseconds")
}

fn analyze_throughput_inputs<W, F>(
    inputs: &[PathBuf],
    scale_ns: u64,
    output: &mut W,
    warn: F,
) -> Result<()>
where
    W: Write,
    F: FnMut(String),
{
    analyze_records(inputs, warn, |records, warn| {
        analyze_throughput_records(records, scale_ns, output, warn)
    })
}

fn analyze_queue_depth_inputs<W, F>(
    inputs: &[PathBuf],
    scale_ns: u64,
    output: &mut W,
    warn: F,
) -> Result<()>
where
    W: Write,
    F: FnMut(String),
{
    analyze_records(inputs, warn, |records, warn| {
        analyze_queue_depth_records(records, scale_ns, output, warn)
    })
}

fn analyze_queue_depth_percent_inputs<W, F>(
    inputs: &[PathBuf],
    output: &mut W,
    warn: F,
) -> Result<()>
where
    W: Write,
    F: FnMut(String),
{
    analyze_records(inputs, warn, |records, warn| {
        analyze_queue_depth_percent_records(records, output, warn)
    })
}

fn analyze_records<W, F, A>(inputs: &[PathBuf], mut warn: F, analyze: A) -> Result<()>
where
    F: FnMut(String),
    A: FnOnce(Vec<TraceRecord>, &mut F) -> Result<W>,
{
    let input_files = expand_input_paths(inputs)?;
    if input_files.is_empty() {
        bail!("no input trace files found");
    }

    let mut records = Vec::new();
    for path in input_files {
        let file = File::open(&path).with_context(|| format!("opening {}", path.display()))?;
        let mut reader = RecordReader::new(file);
        while let Some(raw) = reader
            .next_record()
            .with_context(|| format!("parsing {}", path.display()))?
        {
            records.push(raw.record);
        }
    }

    analyze(records, &mut warn)?;
    Ok(())
}

fn analyze_throughput_records<W, F>(
    records: Vec<TraceRecord>,
    scale_ns: u64,
    output: &mut W,
    warn: &mut F,
) -> Result<()>
where
    W: Write,
    F: FnMut(String),
{
    let mut first_ts = None;
    let mut in_flight = HashMap::<CommandKey, u32>::new();
    let mut bytes_by_bucket = BTreeMap::<u64, u128>::new();

    for record in records {
        let header = record.header();
        let base_ts = *first_ts.get_or_insert(header.timestamp_ns);
        match record {
            TraceRecord::Submit(submit) => {
                let key = CommandKey::from_header(submit.header);
                if in_flight.insert(key, submit.data_len).is_some() {
                    warn(duplicate_submit_warning(key));
                }
            }
            TraceRecord::Complete(complete) => {
                let key = CommandKey::from_header(complete.header);
                let Some(data_len) = in_flight.remove(&key) else {
                    warn(unmatched_completion_warning(key));
                    continue;
                };
                let offset = complete.header.timestamp_ns.saturating_sub(base_ts);
                let bucket = (offset / scale_ns) * scale_ns;
                *bytes_by_bucket.entry(bucket).or_default() += u128::from(data_len);
            }
        }
    }

    writeln!(output, "time_ns\tbytes_per_second")?;
    for (bucket, bytes) in bytes_by_bucket {
        let rate = bytes as f64 * 1_000_000_000_f64 / scale_ns as f64;
        writeln!(output, "{bucket}\t{rate:.6}")?;
    }
    Ok(())
}

fn analyze_queue_depth_records<W, F>(
    records: Vec<TraceRecord>,
    scale_ns: u64,
    output: &mut W,
    warn: &mut F,
) -> Result<()>
where
    W: Write,
    F: FnMut(String),
{
    let mut state = QueueDepthState::default();
    let mut first_ts = None;
    let mut current_offset = None;
    let mut next_sample = 0_u64;

    writeln!(output, "time_ns\tscope\tctrl_id\tqid\tqueue_depth")?;

    for record in records {
        let header = record.header();
        let base_ts = *first_ts.get_or_insert(header.timestamp_ns);
        let offset = header.timestamp_ns.saturating_sub(base_ts);

        if let Some(previous_offset) = current_offset {
            if offset != previous_offset {
                emit_due_samples(
                    output,
                    &state,
                    scale_ns,
                    &mut next_sample,
                    previous_offset,
                    true,
                )?;
                emit_due_samples(output, &state, scale_ns, &mut next_sample, offset, false)?;
                current_offset = Some(offset);
            }
        } else {
            current_offset = Some(offset);
        }

        state.apply_record(record, warn);
    }

    if let Some(offset) = current_offset {
        emit_due_samples(output, &state, scale_ns, &mut next_sample, offset, true)?;
    }

    Ok(())
}

fn emit_due_samples<W>(
    output: &mut W,
    state: &QueueDepthState,
    scale_ns: u64,
    next_sample: &mut u64,
    limit: u64,
    include_limit: bool,
) -> Result<()>
where
    W: Write,
{
    while *next_sample < limit || (include_limit && *next_sample == limit) {
        write_depth_sample(output, *next_sample, state)?;
        let Some(next) = next_sample.checked_add(scale_ns) else {
            break;
        };
        *next_sample = next;
    }
    Ok(())
}

fn write_depth_sample<W>(output: &mut W, time_ns: u64, state: &QueueDepthState) -> Result<()>
where
    W: Write,
{
    writeln!(output, "{time_ns}\tglobal\t\t\t{}", state.global_depth)?;
    for queue in &state.known_queues {
        let depth = state.queue_depths.get(queue).copied().unwrap_or_default();
        writeln!(
            output,
            "{time_ns}\tqueue\t{}\t{}\t{}",
            queue.ctrl_id, queue.qid, depth
        )?;
    }
    Ok(())
}

#[derive(Debug, Default)]
struct QueueDepthState {
    in_flight: HashSet<CommandKey>,
    known_queues: BTreeSet<QueueKey>,
    queue_depths: BTreeMap<QueueKey, u64>,
    global_depth: u64,
}

impl QueueDepthState {
    fn apply_record<F>(&mut self, record: TraceRecord, warn: &mut F)
    where
        F: FnMut(String),
    {
        match record {
            TraceRecord::Submit(submit) => {
                let key = CommandKey::from_header(submit.header);
                let queue = key.queue();
                self.known_queues.insert(queue);
                if !self.in_flight.insert(key) {
                    warn(duplicate_submit_warning(key));
                    return;
                }
                self.global_depth += 1;
                *self.queue_depths.entry(queue).or_default() += 1;
            }
            TraceRecord::Complete(complete) => {
                let key = CommandKey::from_header(complete.header);
                if !self.in_flight.remove(&key) {
                    warn(unmatched_completion_warning(key));
                    return;
                }
                self.global_depth = self.global_depth.saturating_sub(1);
                let queue = key.queue();
                if let Some(depth) = self.queue_depths.get_mut(&queue) {
                    *depth = depth.saturating_sub(1);
                }
            }
        }
    }
}

fn analyze_queue_depth_percent_records<W, F>(
    records: Vec<TraceRecord>,
    output: &mut W,
    warn: &mut F,
) -> Result<()>
where
    W: Write,
    F: FnMut(String),
{
    let mut in_flight = HashSet::<CommandKey>::new();
    let mut global_depth = 0_u64;
    let mut queue_depths = BTreeMap::<QueueKey, u64>::new();
    let mut global_distribution = BTreeMap::<u64, u64>::new();
    let mut queue_distributions = BTreeMap::<QueueKey, BTreeMap<u64, u64>>::new();
    let mut global_samples = 0_u64;
    let mut queue_samples = BTreeMap::<QueueKey, u64>::new();

    for record in records {
        match record {
            TraceRecord::Submit(submit) => {
                let key = CommandKey::from_header(submit.header);
                let queue = key.queue();
                let queue_depth = queue_depths.get(&queue).copied().unwrap_or_default();

                *global_distribution.entry(global_depth).or_default() += 1;
                *queue_distributions
                    .entry(queue)
                    .or_default()
                    .entry(queue_depth)
                    .or_default() += 1;
                global_samples += 1;
                *queue_samples.entry(queue).or_default() += 1;

                if !in_flight.insert(key) {
                    warn(duplicate_submit_warning(key));
                    continue;
                }
                global_depth += 1;
                *queue_depths.entry(queue).or_default() += 1;
            }
            TraceRecord::Complete(complete) => {
                let key = CommandKey::from_header(complete.header);
                if !in_flight.remove(&key) {
                    warn(unmatched_completion_warning(key));
                    continue;
                }
                global_depth = global_depth.saturating_sub(1);
                let queue = key.queue();
                if let Some(depth) = queue_depths.get_mut(&queue) {
                    *depth = depth.saturating_sub(1);
                }
            }
        }
    }

    writeln!(output, "scope\tctrl_id\tqid\tqueue_depth\tcount\tpercent")?;
    write_distribution(output, "global", None, &global_distribution, global_samples)?;
    for (queue, distribution) in queue_distributions {
        let samples = queue_samples.get(&queue).copied().unwrap_or_default();
        write_distribution(output, "queue", Some(queue), &distribution, samples)?;
    }
    Ok(())
}

fn write_distribution<W>(
    output: &mut W,
    scope: &str,
    queue: Option<QueueKey>,
    distribution: &BTreeMap<u64, u64>,
    total: u64,
) -> Result<()>
where
    W: Write,
{
    for (depth, count) in distribution {
        let percent = if total == 0 {
            0.0
        } else {
            *count as f64 * 100.0 / total as f64
        };
        match queue {
            Some(queue) => writeln!(
                output,
                "{scope}\t{}\t{}\t{depth}\t{count}\t{percent:.6}",
                queue.ctrl_id, queue.qid
            )?,
            None => writeln!(output, "{scope}\t\t\t{depth}\t{count}\t{percent:.6}")?,
        }
    }
    Ok(())
}

fn duplicate_submit_warning(key: CommandKey) -> String {
    format!(
        "duplicate outstanding submit for ctrl=nvme{} qid={} cid={}",
        key.ctrl_id, key.qid, key.cid
    )
}

fn unmatched_completion_warning(key: CommandKey) -> String {
    format!(
        "completion without matching submit for ctrl=nvme{} qid={} cid={}",
        key.ctrl_id, key.qid, key.cid
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use nvme_trace::{
        CompleteRecord, Header, NVME_TRACE_COMPLETE, NVME_TRACE_MAGIC, NVME_TRACE_SUBMIT,
        NVME_TRACE_VERSION, SQE_LEN, SubmitRecord, SubmitTail,
    };

    #[test]
    fn parses_scale_with_humantime_units() {
        assert_eq!(parse_scale_ns("1ns").unwrap(), 1);
        assert_eq!(parse_scale_ns("1us").unwrap(), 1_000);
        assert_eq!(parse_scale_ns("2ms").unwrap(), 2_000_000);
        assert_eq!(parse_scale_ns("3s").unwrap(), 3_000_000_000);
        assert!(parse_scale_ns("0ns").is_err());
        assert!(parse_scale_ns("abc").is_err());
        assert!(parse_scale_ns("18446744074s").is_err());
    }

    #[test]
    fn throughput_buckets_completed_bytes_by_completion_time() {
        let records = vec![
            submit(1_000, 1, 0, 1, 10, 100),
            submit(1_050, 2, 0, 1, 11, 200),
            complete(1_100, 3, 0, 1, 10),
            complete(1_250, 4, 0, 1, 11),
        ];
        let (output, warnings) = run_throughput(records, 100);

        assert_eq!(
            output,
            "time_ns\tbytes_per_second\n100\t1000000000.000000\n200\t2000000000.000000\n"
        );
        assert!(warnings.is_empty());
    }

    #[test]
    fn queue_depth_samples_global_and_per_queue() {
        let records = vec![
            submit(1_000, 1, 0, 1, 10, 100),
            submit(1_050, 2, 0, 2, 20, 100),
            complete(1_120, 3, 0, 1, 10),
            complete(1_250, 4, 0, 2, 20),
        ];
        let (output, warnings) = run_queue_depth(records, 100);

        assert_eq!(
            output,
            concat!(
                "time_ns\tscope\tctrl_id\tqid\tqueue_depth\n",
                "0\tglobal\t\t\t1\n",
                "0\tqueue\t0\t1\t1\n",
                "100\tglobal\t\t\t2\n",
                "100\tqueue\t0\t1\t1\n",
                "100\tqueue\t0\t2\t1\n",
                "200\tglobal\t\t\t1\n",
                "200\tqueue\t0\t1\t0\n",
                "200\tqueue\t0\t2\t1\n"
            )
        );
        assert!(warnings.is_empty());
    }

    #[test]
    fn queue_depth_percent_samples_before_submit() {
        let records = vec![
            submit(1_000, 1, 0, 1, 10, 100),
            submit(1_010, 2, 0, 1, 11, 100),
            complete(1_020, 3, 0, 1, 10),
            submit(1_030, 4, 0, 1, 12, 100),
        ];
        let (output, warnings) = run_queue_depth_percent(records);

        assert_eq!(
            output,
            concat!(
                "scope\tctrl_id\tqid\tqueue_depth\tcount\tpercent\n",
                "global\t\t\t0\t1\t33.333333\n",
                "global\t\t\t1\t2\t66.666667\n",
                "queue\t0\t1\t0\t1\t33.333333\n",
                "queue\t0\t1\t1\t2\t66.666667\n"
            )
        );
        assert!(warnings.is_empty());
    }

    #[test]
    fn warns_for_unmatched_completions_and_duplicate_submits() {
        let records = vec![
            complete(1_000, 1, 0, 1, 10),
            submit(1_010, 2, 0, 1, 10, 100),
            submit(1_020, 3, 0, 1, 10, 200),
            complete(1_030, 4, 0, 1, 10),
        ];
        let (_, warnings) = run_throughput(records, 100);

        assert_eq!(warnings.len(), 2);
        assert!(warnings[0].contains("completion without matching submit"));
        assert!(warnings[1].contains("duplicate outstanding submit"));
    }

    fn run_throughput(records: Vec<TraceRecord>, scale_ns: u64) -> (String, Vec<String>) {
        let mut output = Vec::new();
        let mut warnings = Vec::new();
        analyze_throughput_records(records, scale_ns, &mut output, &mut |warning| {
            warnings.push(warning)
        })
        .unwrap();
        (String::from_utf8(output).unwrap(), warnings)
    }

    fn run_queue_depth(records: Vec<TraceRecord>, scale_ns: u64) -> (String, Vec<String>) {
        let mut output = Vec::new();
        let mut warnings = Vec::new();
        analyze_queue_depth_records(records, scale_ns, &mut output, &mut |warning| {
            warnings.push(warning)
        })
        .unwrap();
        (String::from_utf8(output).unwrap(), warnings)
    }

    fn run_queue_depth_percent(records: Vec<TraceRecord>) -> (String, Vec<String>) {
        let mut output = Vec::new();
        let mut warnings = Vec::new();
        analyze_queue_depth_percent_records(records, &mut output, &mut |warning| {
            warnings.push(warning)
        })
        .unwrap();
        (String::from_utf8(output).unwrap(), warnings)
    }

    fn submit(
        timestamp_ns: u64,
        seq: u32,
        ctrl_id: u8,
        qid: u8,
        cid: u16,
        data_len: u32,
    ) -> TraceRecord {
        TraceRecord::Submit(SubmitRecord {
            header: header(NVME_TRACE_SUBMIT, timestamp_ns, seq, ctrl_id, qid, cid),
            sqe: [0; SQE_LEN],
            data_len,
            meta_len: 0,
            use_sgl: false,
            single_segment: false,
            nr_entries: 0,
            tail: SubmitTail::Prp(Vec::new()),
        })
    }

    fn complete(timestamp_ns: u64, seq: u32, ctrl_id: u8, qid: u8, cid: u16) -> TraceRecord {
        TraceRecord::Complete(CompleteRecord {
            header: header(NVME_TRACE_COMPLETE, timestamp_ns, seq, ctrl_id, qid, cid),
            result: 0,
            sq_head: 0,
            sq_id: qid.into(),
            status: 0,
            retries: 0,
        })
    }

    fn header(
        record_type: u8,
        timestamp_ns: u64,
        seq: u32,
        ctrl_id: u8,
        qid: u8,
        cid: u16,
    ) -> Header {
        Header {
            magic: NVME_TRACE_MAGIC,
            version: NVME_TRACE_VERSION,
            record_type,
            len: 0,
            timestamp_ns,
            seq,
            ctrl_id,
            qid,
            cid,
        }
    }
}
