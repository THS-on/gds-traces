import sysconfig
from pathlib import Path

from invoke import Collection, task

from .common import PERF_DATA_DIR, PERF_SCRIPTS_DIR


@task(auto_shortflags=False)
def queue_depth(c, data_dir=None, output=None):
    """Generate an interactive HTML queue depth histogram."""
    data_dir = Path(data_dir) if data_dir else PERF_DATA_DIR
    script = PERF_SCRIPTS_DIR / "perf-plot-queue-depth.py"
    out = Path(output) if output else data_dir / "queue_depth_histogram.html"
    site_packages = sysconfig.get_path("purelib")
    c.run(
        f"sudo PYTHONPATH={site_packages} PERF_QUEUE_DEPTH_OUTPUT={out} "
        f"perf script -i {data_dir}/nvme_sorted.data -s {script}"
    )


@task(auto_shortflags=False)
def latency(c, data_dir=None, output=None):
    """Generate an interactive HTML latency histogram (submit → complete)."""
    data_dir = Path(data_dir) if data_dir else PERF_DATA_DIR
    script = PERF_SCRIPTS_DIR / "perf-parse-latency.py"
    out = Path(output) if output else data_dir / "latency_histogram.html"
    site_packages = sysconfig.get_path("purelib")
    c.run(
        f"sudo PYTHONPATH={site_packages} PERF_LATENCY_OUTPUT={out} "
        f"perf script -i {data_dir}/nvme_sorted.data -s {script}"
    )


@task(auto_shortflags=False)
def print_(c, data_dir=None):
    """Print all NVMe submit/complete events with SGL/PRP details."""
    data_dir = Path(data_dir) if data_dir else PERF_DATA_DIR
    script = PERF_SCRIPTS_DIR / "perf-parse-sgl-prp.py"
    c.run(f"sudo perf script -i {data_dir}/nvme_sorted.data -s {script}")


ns = Collection("perf")
ns.add_task(queue_depth)
ns.add_task(latency)
ns.add_task(print_, name="print")
