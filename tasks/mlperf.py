import shlex
from pathlib import Path

from invoke import Collection, task

from .common import nvme_mount

IMAGE = "mlperf-storage"
PROJECT_ROOT = Path(__file__).parent.parent
HOST_RESULTS_DIR = PROJECT_ROOT / "results" / "mlperf"
CONTAINER_DATA_DIR = "/mnt/nvme/mlperf_data"
CONTAINER_RESULTS_DIR = "/results"

BASE_FLAGS = [
    "--rm",
    "-it",
    "--privileged",
    "--ipc",
    "host",
    "--volume",
    "/run/udev:/run/udev:ro",
]

TRAINING_MODELS = ("unet3d", "resnet50", "cosmoflow")


def _shell_join(parts):
    return " ".join(shlex.quote(str(part)) for part in parts)


def _docker_flags(mount=None):
    flags = [*BASE_FLAGS]
    if mount:
        flags += ["--volume", f"{mount}:/mnt/nvme"]
    HOST_RESULTS_DIR.mkdir(parents=True, exist_ok=True)
    flags += ["--volume", f"{HOST_RESULTS_DIR}:{CONTAINER_RESULTS_DIR}"]
    return flags


def _run_mlperf(c, args, mount=None):
    command = ["docker", "run", *_docker_flags(mount), IMAGE, *args]
    c.run(_shell_join(command), pty=True)


def _training_params(model, num_files, read_threads, prefetch_size, odirect):
    params = [f"dataset.num_files_train={num_files}"]
    if read_threads:
        params.append(f"reader.read_threads={read_threads}")
    if prefetch_size:
        params.append(f"reader.prefetch_size={prefetch_size}")
    if odirect and model == "unet3d":
        params.append("reader.odirect=true")
    return params


@task(auto_shortflags=False)
def build(c, ref="v2.0"):
    """Build the MLPerf Storage container image."""
    c.run(
        f"docker build --build-arg MLPERF_STORAGE_REF={ref} -t {IMAGE} images/mlperf/"
    )


@task(auto_shortflags=False)
def run(c):
    """Build the MLPerf Storage container image, then start an interactive shell."""
    build(c)
    flags = _shell_join(_docker_flags())
    c.run(f"docker run {flags} --entrypoint /bin/bash {IMAGE}", pty=True)


@task(auto_shortflags=False)
def datasize(
    c,
    model="unet3d",
    client_memory_gb=64,
    max_accelerators=4,
    accelerator="h100",
    hosts="127.0.0.1",
):
    """Calculate the v2.0 training dataset size for a target client/accelerator shape."""
    host_list = hosts.split(",")
    args = [
        "training",
        "datasize",
        "--hosts",
        *host_list,
        "--model",
        model,
        "--client-host-memory-in-gb",
        client_memory_gb,
        "--max-accelerators",
        max_accelerators,
        "--num-client-hosts",
        len(host_list),
        "--accelerator-type",
        accelerator,
        "--results-dir",
        CONTAINER_RESULTS_DIR,
        "--allow-run-as-root",
    ]
    _run_mlperf(c, args)


@task(auto_shortflags=False)
def training_smoke(
    c,
    model="unet3d",
    num_files=192,
    num_processes=8,
    num_accelerators=4,
    client_memory_gb=64,
    accelerator="h100",
    hosts="127.0.0.1",
    read_threads=4,
    prefetch_size=2,
    odirect=True,
):
    """Format the configured NVMe and run a small MLPerf Storage v2.0 training workflow."""
    host_list = hosts.split(",")
    params = _training_params(model, num_files, read_threads, prefetch_size, odirect)
    with nvme_mount(c) as mount:
        common = [
            "--hosts",
            *host_list,
            "--model",
            model,
            "--data-dir",
            CONTAINER_DATA_DIR,
            "--results-dir",
            CONTAINER_RESULTS_DIR,
            "--allow-run-as-root",
            "--params",
            *params,
        ]
        _run_mlperf(
            c,
            [
                "training",
                "datagen",
                *common,
                "--num-processes",
                num_processes,
            ],
            mount,
        )
        _run_mlperf(
            c,
            [
                "training",
                "run",
                *common,
                "--client-host-memory-in-gb",
                client_memory_gb,
                "--num-accelerators",
                num_accelerators,
                "--num-client-hosts",
                len(host_list),
                "--accelerator-type",
                accelerator,
            ],
            mount,
        )
    _run_mlperf(c, ["reports", "reportgen", "--results-dir", CONTAINER_RESULTS_DIR])


@task(auto_shortflags=False)
def training_smoke_all(c):
    """Run small NVMe-backed training workflows for all MLPerf Storage v2.0 training models."""
    for model in TRAINING_MODELS:
        training_smoke(c, model=model)


@task(auto_shortflags=False)
def checkpoint_smoke(
    c,
    model="llama3-8b",
    num_processes=8,
    client_memory_gb=512,
    hosts="127.0.0.1",
):
    """Format the configured NVMe and run a one-write/one-read checkpointing smoke workflow."""
    host_list = hosts.split(",")
    with nvme_mount(c) as mount:
        _run_mlperf(
            c,
            [
                "checkpointing",
                "run",
                "--hosts",
                *host_list,
                "--model",
                model,
                "--client-host-memory-in-gb",
                client_memory_gb,
                "--num-processes",
                num_processes,
                "--checkpoint-folder",
                f"{CONTAINER_DATA_DIR}/checkpoints",
                "--results-dir",
                CONTAINER_RESULTS_DIR,
                "--num-checkpoints-read",
                1,
                "--num-checkpoints-write",
                1,
                "--allow-run-as-root",
            ],
            mount,
        )


ns = Collection("mlperf")
ns.add_task(build)
ns.add_task(run)
ns.add_task(datasize)
ns.add_task(training_smoke)
ns.add_task(training_smoke_all)
ns.add_task(checkpoint_smoke)
