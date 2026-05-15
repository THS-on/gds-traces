from pathlib import Path

from invoke import Collection, task

from .common import DOCKER_BASE_FLAGS, perf_capture, testfile_mount

IMAGE = "breuner/elbencho:master-ubuntu-cuda-multiarch"
GDS_ARGS = "/data/testfile -w -r -t 256 -s 12g -b 4m --direct --gpuids all --gds"

PROJECT_ROOT = Path(__file__).parent.parent
PERF_DIR = PROJECT_ROOT / "results" / "elbencho-perf"


@task
def run(c):
    """Format NVMe as ext4, pre-allocate a 100G test file, then run elbencho via Docker with GDS."""
    with testfile_mount(c) as mount:
        with perf_capture(c, output_dir=PERF_DIR):
            print("Starting elbencho ...")
            flags = " ".join(
                [
                    "--rm",
                    "-it",
                    *DOCKER_BASE_FLAGS,
                    "--env",
                    "CUFILE_USE_PCIP2PDMA=true",
                    "--env",
                    "CUFILE_ALLOW_COMPAT_MODE=false",
                    "--volume",
                    f"{mount}:/data",
                ]
            )
            c.run(f"docker run {flags} {IMAGE} {GDS_ARGS}", pty=True)


ns = Collection("elbencho")
ns.add_task(run)
