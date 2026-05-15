from pathlib import Path

from invoke import Collection, task

from .common import PERF_DATA_DIR, PERF_SCRIPTS_DIR


@task(auto_shortflags=False)
def queue_depth(c, data_dir=None):
    """Analyse a captured NVMe perf trace for queue depth distribution."""
    data_dir = Path(data_dir) if data_dir else PERF_DATA_DIR
    script = PERF_SCRIPTS_DIR / "perf-parse-queue-depth.py"
    c.run(f"sudo perf script -i {data_dir}/nvme_sorted.data -s {script}")


@task(auto_shortflags=False)
def print_(c, data_dir=None):
    """Print all NVMe submit/complete events with SGL/PRP details."""
    data_dir = Path(data_dir) if data_dir else PERF_DATA_DIR
    script = PERF_SCRIPTS_DIR / "perf-parse-sgl-prp.py"
    c.run(f"sudo perf script -i {data_dir}/nvme_sorted.data -s {script}")


ns = Collection("perf")
ns.add_task(queue_depth)
ns.add_task(print_, name="print")
