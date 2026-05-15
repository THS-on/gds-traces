# perf script event handlers for NVMe submission-to-completion latency
# Licensed under the terms of the GNU GPL License version 2
#
# Usage:
#   perf script -s perf-parse-latency.py
#
# Output file defaults to latency_histogram.html; override with:
#   PERF_LATENCY_OUTPUT=out.html perf script -s perf-parse-latency.py

from __future__ import print_function

import os
import sys

import plotly.graph_objects as go

sys.path.append(
    os.environ["PERF_EXEC_PATH"] + "/scripts/python/Perf-Trace-Util/lib/Perf/Trace"
)

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from perf_trace_context import *  # noqa: F403
from Core import *  # noqa: F403
from nvme_trace_events import (
    NvmePciComplete,
    NvmePciSubmit,
    opcode_name,
    parse_complete,
    parse_submit,
)

# (ctrl_id, qid, cid) -> (submit_ts_ns, opcode_str)
_pending: dict = {}

# (ctrl_id, qid, opcode_str) -> [latency_us, ...]
_latencies: dict = {}

_output_file = os.environ.get("PERF_LATENCY_OUTPUT", "latency_histogram.html")


def trace_begin():
    pass


def trace_end():
    for key in sorted(_pending):
        ctrl_id, qid, cid = key
        print(
            f"WARNING: unmatched submit ctrl_id={ctrl_id} qid={qid} cid={cid} (no completion seen)"
        )

    if not _latencies:
        print("No latency data collected.")
        return

    _write_plotly_html(_output_file)
    print(f"Latency histogram written to: {_output_file}")


def nvme__nvme_pci_submit(
    event_name,
    context,
    common_cpu,
    common_secs,
    common_nsecs,
    common_pid,
    common_comm,
    common_callchain,
    ctrl_id,
    qid,
    cid,
    opcode,
    nsid,
    data_len,
    meta_len,
    use_sgl,
    single_segment,
    sqe,
    descriptors,
    perf_sample_dict,
):
    ev: NvmePciSubmit = parse_submit(
        ctrl_id,
        qid,
        cid,
        opcode,
        nsid,
        data_len,
        meta_len,
        use_sgl,
        single_segment,
        sqe,
        descriptors,
    )
    ts_ns = common_secs * 1_000_000_000 + common_nsecs
    key = (ev.ctrl_id, ev.qid, ev.cid)
    if key in _pending:
        print(
            f"WARNING: duplicate submit ctrl_id={ev.ctrl_id} qid={ev.qid} cid={ev.cid}"
        )
    _pending[key] = (ts_ns, opcode_name(ev.opcode))


def nvme__nvme_pci_complete(
    event_name,
    context,
    common_cpu,
    common_secs,
    common_nsecs,
    common_pid,
    common_comm,
    common_callchain,
    ctrl_id,
    qid,
    cid,
    result,
    sq_head,
    sq_id,
    status,
    retries,
    perf_sample_dict,
):
    ev: NvmePciComplete = parse_complete(
        ctrl_id, qid, cid, result, sq_head, sq_id, status, retries
    )
    ts_ns = common_secs * 1_000_000_000 + common_nsecs
    key = (ev.ctrl_id, ev.qid, ev.cid)
    if key not in _pending:
        print(
            f"WARNING: completion without submit ctrl_id={ev.ctrl_id} qid={ev.qid} cid={ev.cid}"
        )
        return

    submit_ts_ns, op = _pending.pop(key)
    latency_us = (ts_ns - submit_ts_ns) / 1_000.0
    bucket_key = (ev.ctrl_id, ev.qid, op)
    _latencies.setdefault(bucket_key, []).append(latency_us)


def trace_unhandled(event_name, context, event_fields_dict, perf_sample_dict):
    pass


# ---------------------------------------------------------------------------
# Output helpers
# ---------------------------------------------------------------------------


def _write_plotly_html(path: str) -> None:
    # Collect summary stats for the hover annotation
    all_samples: list = []
    traces = []
    for bucket_key in sorted(_latencies):
        ctrl_id, qid, op = bucket_key
        samples = _latencies[bucket_key]
        all_samples.extend(samples)
        label = f"ctrl={ctrl_id} q={qid} {op}"
        traces.append((label, samples))

    fig = go.Figure()
    for label, samples in traces:
        fig.add_trace(
            go.Histogram(
                x=samples,
                name=label,
                opacity=0.75,
                nbinsx=200,
                hovertemplate="Latency: %{x:.1f} µs<br>Count: %{y}<extra>%{fullData.name}</extra>",
            )
        )

    n = len(all_samples)
    avg = sum(all_samples) / n if n else 0.0
    p50 = _percentile(all_samples, 50)
    p99 = _percentile(all_samples, 99)

    fig.update_layout(
        title=dict(
            text=(
                f"NVMe Command Latency (submit → complete)<br>"
                f"<sup>n={n:,}  avg={avg:.1f} µs  p50={p50:.1f} µs  p99={p99:.1f} µs</sup>"
            ),
            x=0.5,
        ),
        xaxis_title="Latency (µs)",
        yaxis_title="Count",
        barmode="overlay",
        bargap=0.02,
        legend=dict(x=0.7, y=0.95),
        hovermode="x unified",
    )

    # Percentile annotations
    for pct, val, color in ((50, p50, "green"), (99, p99, "red")):
        fig.add_vline(
            x=val,
            line_dash="dash",
            line_color=color,
            annotation_text=f"p{pct}={val:.1f} µs",
            annotation_position="top right",
        )

    fig.write_html(path, include_plotlyjs="cdn")


def _percentile(data: list, pct: float) -> float:
    if not data:
        return 0.0
    s = sorted(data)
    k = (len(s) - 1) * pct / 100.0
    lo, hi = int(k), min(int(k) + 1, len(s) - 1)
    return s[lo] + (s[hi] - s[lo]) * (k - lo)
