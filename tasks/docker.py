from invoke import Collection, task

from .common import DOCKER_BASE_FLAGS, load_nvme_device, testfile_mount

BASE_FLAGS = ["--rm", "-it"] + DOCKER_BASE_FLAGS


def _docker_flags():
    nvme = load_nvme_device()
    return BASE_FLAGS + ["--device", nvme]


@task
def build(c):
    """Build the gds-base container image."""
    c.run("docker build -t gds-base images/gds-base/")


@task
def run(c):
    """Run the gds-base container with GPU access and GPUDirect Storage enabled."""
    flags = " ".join(_docker_flags())
    c.run(f"docker run {flags} gds-base", pty=True)


@task
def gds_check(c):
    """Prepare NVMe test file and run gdscheck inside the gds-base container."""
    with testfile_mount(c) as mount:
        flags = " ".join(_docker_flags() + ["--volume", f"{mount}:/data"])
        c.run(f"docker run {flags} gds-base gdscheck -f /data/testfile", pty=True)


ns = Collection("docker")
ns.add_task(build)
ns.add_task(run)
ns.add_task(gds_check)
