import os
import re
import shlex
import signal
import socket
import subprocess
import sys
import threading
import tomllib
from contextlib import contextmanager
from datetime import datetime
from pathlib import Path

PROJECT_ROOT = Path(__file__).parent.parent
CONFIG_TOML = PROJECT_ROOT / "config.toml"
MOUNT_POINT = "/mnt/nvme"
FILE_SIZE = "100G"
NVME_TRACE_FLAKE_OUTPUT = f"{PROJECT_ROOT}#nvme-trace"
NVME_TRACE_RESULTS_DIR = PROJECT_ROOT / "results" / "nvme-traces"

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


def _trace_output_dir(label, controller):
    stamp = datetime.now().strftime("%Y%m%d-%H%M%S")
    safe_label = re.sub(r"[^A-Za-z0-9_.-]+", "-", label).strip("-")
    name = (
        f"{stamp}-{safe_label}-{controller}" if safe_label else f"{stamp}-{controller}"
    )
    return NVME_TRACE_RESULTS_DIR / name


def _nvme_trace_bin(c):
    result = c.run(
        _shell_join(
            ["nix", "build", "--no-link", "--print-out-paths", NVME_TRACE_FLAKE_OUTPUT]
        ),
        hide=True,
    )
    out_path = result.stdout.strip().splitlines()[-1]
    return Path(out_path) / "bin" / "nvme-trace"


def _forward_stderr(process, ready):
    assert process.stderr is not None
    for line in process.stderr:
        sys.stderr.write(line)
        sys.stderr.flush()
        if "capturing;" in line:
            ready.set()


@contextmanager
def nvme_trace(
    c,
    label,
    *,
    enabled=True,
    controller=None,
    out_dir=None,
    trace_dir=None,
    drain_ms=250,
    ready_timeout_s=15,
):
    """Capture an NVMe relay trace while the wrapped task body runs."""
    if not enabled:
        yield None
        return

    controller = controller or load_nvme_controller()
    out_dir = Path(out_dir) if out_dir else _trace_output_dir(label, controller)
    out_dir.mkdir(parents=True, exist_ok=True)
    nvme_trace_bin = _nvme_trace_bin(c)

    command = [
        "sudo",
        "-E",
        nvme_trace_bin,
        "capture",
        "--controller",
        controller,
        "--out",
        out_dir,
        "--drain-ms",
        drain_ms,
    ]
    if trace_dir:
        command += ["--trace-dir", trace_dir]

    print(f"Starting NVMe trace for {controller} in {out_dir} ...")
    process = subprocess.Popen(
        [str(part) for part in command],
        stderr=subprocess.PIPE,
        text=True,
        start_new_session=True,
    )
    ready = threading.Event()
    stderr_thread = threading.Thread(
        target=_forward_stderr,
        args=(process, ready),
        daemon=True,
    )
    stderr_thread.start()

    try:
        if not ready.wait(ready_timeout_s):
            exit_code = process.poll()
            if exit_code is not None:
                raise RuntimeError(
                    f"nvme-trace exited before capture started: {exit_code}"
                )
            raise RuntimeError(
                f"nvme-trace did not report readiness within {ready_timeout_s}s"
            )
        yield out_dir
    finally:
        if process.poll() is None:
            print("Stopping NVMe trace ...")
            os.killpg(process.pid, signal.SIGINT)

        exit_code = process.wait()
        stderr_thread.join(timeout=1)
        if exit_code not in (0, 130, -signal.SIGINT) and sys.exc_info()[0] is None:
            raise RuntimeError(f"nvme-trace exited with status {exit_code}")
        print(f"NVMe trace written to {out_dir}")


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
