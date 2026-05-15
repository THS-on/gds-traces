import re
import shlex
import signal
import socket
import subprocess
import tomllib
from contextlib import contextmanager
from pathlib import Path

PROJECT_ROOT = Path(__file__).parent.parent
CONFIG_TOML = PROJECT_ROOT / "config.toml"
MOUNT_POINT = "/mnt/nvme"
FILE_SIZE = "100G"
PERF_DATA_DIR = PROJECT_ROOT / "perf-data"
PERF_SCRIPTS_DIR = PROJECT_ROOT / "perf-scripts"

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


def load_nvme_controller():
    hostname = socket.gethostname()
    with open(CONFIG_TOML, "rb") as f:
        cfg = tomllib.load(f)
    hosts = cfg.get("hosts", {})
    if hostname not in hosts:
        raise RuntimeError(f"No config entry for host '{hostname}' in {CONFIG_TOML}")

    controller = hosts[hostname].get("nvme_controller")
    if controller:
        return controller

    nvme = Path(load_nvme_device())
    try:
        nvme_name = nvme.resolve(strict=True).name
    except FileNotFoundError:
        nvme_name = nvme.name

    match = re.match(r"(nvme\d+)", nvme_name)
    if not match:
        raise RuntimeError(
            f"Cannot infer NVMe controller from {nvme}; add "
            f"'nvme_controller = \"nvmeX\"' to {CONFIG_TOML}"
        )
    return match.group(1)


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


def _shell_join(parts):
    return " ".join(shlex.quote(str(part)) for part in parts)


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


def _ctrl_id_from_controller(controller: str) -> int:
    match = re.match(r"nvme(\d+)", controller)
    if not match:
        raise RuntimeError(f"Cannot extract ctrl_id from '{controller}'")
    return int(match.group(1))


@contextmanager
def perf_capture(c, output_dir=None, ctrl_id=None, ring_buffer="1G"):
    """Records NVMe submit/complete perf events while the body executes, then injects build-IDs."""
    output_dir = Path(output_dir) if output_dir else PERF_DATA_DIR
    output_dir.mkdir(parents=True, exist_ok=True)

    if ctrl_id is None:
        ctrl_id = _ctrl_id_from_controller(load_nvme_controller())

    raw_path = output_dir / "nvme_raw.data"
    sorted_path = output_dir / "nvme_sorted.data"

    cmd = [
        "sudo",
        "perf",
        "record",
        "-e",
        "nvme:nvme_pci_submit",
        "--filter",
        f"ctrl_id=={ctrl_id}",
        "-e",
        "nvme:nvme_pci_complete",
        "--filter",
        f"ctrl_id=={ctrl_id}",
        "-a",
        "-T",
        "--sample-identifier",
        "-k",
        "CLOCK_MONOTONIC",
        "-m",
        ring_buffer,
        "-r",
        "10",
        "-o",
        str(raw_path),
    ]
    proc = subprocess.Popen(cmd)
    try:
        yield output_dir
    finally:
        proc.send_signal(signal.SIGINT)
        proc.wait(timeout=30)
        c.run(f"sudo perf inject --build-ids -i {raw_path} -o {sorted_path}")
