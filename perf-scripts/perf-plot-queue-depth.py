# perf script event handlers for NVMe queue depth histogram
# Licensed under the terms of the GNU GPL License version 2
#
# Usage:
#   perf script -s perf-plot-queue-depth.py
#
# Output file defaults to queue_depth_histogram.html; override with:
#   PERF_QUEUE_DEPTH_OUTPUT=out.html perf script -s perf-plot-queue-depth.py

from __future__ import print_function

import os
import sys

import plotly.graph_objects as go
from plotly.subplots import make_subplots

sys.path.append(
    os.environ["PERF_EXEC_PATH"] + "/scripts/python/Perf-Trace-Util/lib/Perf/Trace"
)

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from perf_trace_context import *  # noqa: F403
from Core import *  # noqa: F403
from nvme_trace_events import (
    NvmePciComplete,
    NvmePciSubmit,
    parse_complete,
    parse_submit,
)

# current queue depth per (ctrl_id, qid)
_depth: dict = {}

# histogram: (ctrl_id, qid) -> {depth: count}
_buckets: dict = {}

# in-flight commands for sanity check: (ctrl_id, qid, cid) -> True
_in_flight: dict = {}

_output_file = os.environ.get("PERF_QUEUE_DEPTH_OUTPUT", "queue_depth_histogram.html")


def trace_begin():
    pass


def trace_end():
    for inf_key in sorted(_in_flight):
        ctrl_id, qid, cid = inf_key
        print(
            f"WARNING: unmatched submit ctrl_id={ctrl_id} qid={qid} cid={cid} (no completion seen)"
        )

    if not _buckets:
        print("No queue depth data collected.")
        return

    _write_plotly_html(_output_file)
    print(f"Queue depth histogram written to: {_output_file}")


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

    inf_key = (ev.ctrl_id, ev.qid, ev.cid)
    if inf_key in _in_flight:
        print(
            f"WARNING: duplicate submit ctrl_id={ev.ctrl_id} qid={ev.qid} cid={ev.cid} (no completion between submits)"
        )

    queue_key = (ev.ctrl_id, ev.qid)
    _depth[queue_key] = _depth.get(queue_key, 0) + 1
    depth = _depth[queue_key]

    buckets = _buckets.setdefault(queue_key, {})
    buckets[depth] = buckets.get(depth, 0) + 1

    _in_flight[inf_key] = True


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

    inf_key = (ev.ctrl_id, ev.qid, ev.cid)
    if inf_key not in _in_flight:
        print(
            f"WARNING: completion without submit ctrl_id={ev.ctrl_id} qid={ev.qid} cid={ev.cid}"
        )
    else:
        del _in_flight[inf_key]

    queue_key = (ev.ctrl_id, ev.qid)
    current = _depth.get(queue_key, 0)
    if current <= 0:
        print(f"WARNING: queue depth underflow ctrl_id={ev.ctrl_id} qid={ev.qid}")
    else:
        _depth[queue_key] = current - 1


def trace_unhandled(event_name, context, event_fields_dict, perf_sample_dict):
    pass


# ---------------------------------------------------------------------------
# Output helpers
# ---------------------------------------------------------------------------


def _weighted_avg(counts: dict) -> float:
    total = sum(counts.values())
    if not total:
        return 0.0
    return sum(d * c for d, c in counts.items()) / total


def _weighted_percentile(counts: dict, pct: float) -> float:
    total = sum(counts.values())
    if not total:
        return 0.0
    target = total * pct / 100.0
    cumulative = 0
    for depth in sorted(counts):
        cumulative += counts[depth]
        if cumulative >= target:
            return float(depth)
    return float(max(counts))


def _aggregate_buckets() -> dict:
    agg: dict = {}
    for per_queue in _buckets.values():
        for depth, count in per_queue.items():
            agg[depth] = agg.get(depth, 0) + count
    return agg


def _write_plotly_html(path: str) -> None:
    queue_keys = sorted(_buckets)
    n_queues = len(queue_keys)

    # Row 1: aggregated; Row 2..N+1: one per (ctrl, qid)
    subplot_titles = ["Aggregated (all queues)"] + [
        f"ctrl={ctrl_id} q={qid}" for ctrl_id, qid in queue_keys
    ]
    fig = make_subplots(
        rows=1 + n_queues,
        cols=1,
        shared_xaxes=True,
        subplot_titles=subplot_titles,
        vertical_spacing=0.05,
    )

    # --- Aggregated trace ---
    agg = _aggregate_buckets()
    depths = sorted(agg)
    counts = [agg[d] for d in depths]
    total = sum(counts)
    avg = _weighted_avg(agg)
    p50 = _weighted_percentile(agg, 50)
    p99 = _weighted_percentile(agg, 99)

    fig.add_trace(
        go.Bar(
            x=depths,
            y=counts,
            name="All queues",
            marker_color="steelblue",
            opacity=0.8,
            hovertemplate="Depth: %{x}<br>Count: %{y}<extra>All queues</extra>",
        ),
        row=1,
        col=1,
    )
    fig.add_vline(
        x=p50,
        line_dash="dash",
        line_color="green",
        annotation_text=f"p50={p50:.0f}",
        annotation_position="top right",
        row=1,
        col=1,
    )
    fig.add_vline(
        x=p99,
        line_dash="dash",
        line_color="red",
        annotation_text=f"p99={p99:.0f}",
        annotation_position="top right",
        row=1,
        col=1,
    )

    # --- Per-queue traces ---
    for row_idx, queue_key in enumerate(queue_keys, start=2):
        ctrl_id, qid = queue_key
        per_q = _buckets[queue_key]
        q_depths = sorted(per_q)
        q_counts = [per_q[d] for d in q_depths]
        q_p50 = _weighted_percentile(per_q, 50)
        q_p99 = _weighted_percentile(per_q, 99)
        label = f"ctrl={ctrl_id} q={qid}"

        fig.add_trace(
            go.Bar(
                x=q_depths,
                y=q_counts,
                name=label,
                opacity=0.8,
                hovertemplate=f"Depth: %{{x}}<br>Count: %{{y}}<extra>{label}</extra>",
            ),
            row=row_idx,
            col=1,
        )
        fig.add_vline(
            x=q_p50,
            line_dash="dash",
            line_color="green",
            annotation_text=f"p50={q_p50:.0f}",
            annotation_position="top right",
            row=row_idx,
            col=1,
        )
        fig.add_vline(
            x=q_p99,
            line_dash="dash",
            line_color="red",
            annotation_text=f"p99={q_p99:.0f}",
            annotation_position="top right",
            row=row_idx,
            col=1,
        )

    total_height = 400 + 300 * n_queues
    fig.update_layout(
        title=dict(
            text=(
                f"NVMe Queue Depth at Submit Time<br>"
                f"<sup>n={total:,}  avg={avg:.1f}  p50={p50:.0f}  p99={p99:.0f}</sup>"
            ),
            x=0.5,
        ),
        yaxis_title="Count",
        bargap=0.05,
        showlegend=False,
        height=total_height,
        hovermode="x unified",
    )
    fig.update_xaxes(title_text="Queue Depth", row=1 + n_queues, col=1)

    fig.write_html(path, include_plotlyjs="cdn")
