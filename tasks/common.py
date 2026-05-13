import socket
import tomllib
from contextlib import contextmanager
from pathlib import Path

CONFIG_TOML = Path(__file__).parent.parent / "config.toml"
MOUNT_POINT = "/mnt/nvme"
FILE_SIZE = "100G"

DOCKER_BASE_FLAGS = [
    "--privileged",
    "--device=nvidia.com/gpu=all",
    "--ipc",
    "host",
    "--volume",
    "/run/udev:/run/udev:ro",
]


def load_nvme_device():
    hostname = socket.gethostname()
    with open(CONFIG_TOML, "rb") as f:
        cfg = tomllib.load(f)
    hosts = cfg.get("hosts", {})
    if hostname not in hosts:
        raise RuntimeError(f"No config entry for host '{hostname}' in {CONFIG_TOML}")
    nvme = hosts[hostname].get("nvme")
    if not nvme:
        raise RuntimeError(f"Missing 'nvme' key for host '{hostname}' in {CONFIG_TOML}")
    return nvme


def prepare_nvme_mount(c):
    """Format the NVMe as ext4 and mount it."""
    nvme = load_nvme_device()
    print(f"Creating ext4 filesystem on {nvme} ...")
    c.run(f"sudo mkfs.ext4 -F {nvme}")
    print(f"Mounting {nvme} at {MOUNT_POINT} ...")
    c.run(f"sudo mkdir -p {MOUNT_POINT}")
    c.run(f"sudo mount -o data=ordered {nvme} {MOUNT_POINT}")
    c.run(f"sudo chmod 777 {MOUNT_POINT}")


def prepare_testfile(c):
    """Format the NVMe as ext4, mount it, and pre-allocate a test file."""
    prepare_nvme_mount(c)
    print(f"Allocating {FILE_SIZE} test file at {MOUNT_POINT}/testfile ...")
    c.run(f"fallocate -l {FILE_SIZE} {MOUNT_POINT}/testfile")


@contextmanager
def nvme_mount(c):
    """Context manager that prepares a clean NVMe filesystem and unmounts on exit."""
    prepare_nvme_mount(c)
    try:
        yield MOUNT_POINT
    finally:
        c.run(f"sudo umount {MOUNT_POINT}")
        c.run(f"sudo rm -r '{MOUNT_POINT}'")


@contextmanager
def testfile_mount(c):
    """Context manager that prepares the NVMe test file and unmounts on exit."""
    prepare_testfile(c)
    try:
        yield MOUNT_POINT
    finally:
        c.run(f"sudo umount {MOUNT_POINT}")
        c.run(f"sudo rm -r '{MOUNT_POINT}'")
