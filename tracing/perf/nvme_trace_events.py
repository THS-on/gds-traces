"""
Parsing helpers for nvme_pci_submit and nvme_pci_complete perf trace events.

Provides dataclasses and factory functions that turn raw perf-script field
values into structured, typed objects.  Import this module from perf scripts
or any other analysis tool — it has no dependency on the perf Python runtime.
"""

from __future__ import annotations

import struct
from dataclasses import dataclass, field
from typing import List, Optional, Tuple


# ---------------------------------------------------------------------------
# Opcode table (NVMe I/O command set, used by both SQE decoding and printing)
# ---------------------------------------------------------------------------

OPCODES: dict = {
    0x00: "flush",
    0x01: "write",
    0x02: "read",
    0x04: "write_uncorrectable",
    0x05: "compare",
    0x08: "write_zeroes",
    0x09: "dataset_management",
    0x0C: "verify",
}

_READ_WRITE_OPCODES = {0x01, 0x02}


def opcode_name(op: int) -> str:
    return OPCODES.get(op, f"unknown(0x{op:02x})")


# ---------------------------------------------------------------------------
# Dataclasses
# ---------------------------------------------------------------------------


@dataclass
class SglDesc:
    """One 16-byte NVMe SGL data-block descriptor (struct nvme_sgl_desc)."""

    addr: int  # u64 LE — physical address
    length: int  # u32 LE — byte length of the segment
    reserved: Tuple[int, int, int]  # 3 reserved bytes
    desc_type: int  # upper nibble = class, lower nibble = subtype


@dataclass
class SqeInfo:
    """Decoded fields from a 64-byte NVMe Submission Queue Entry."""

    raw: bytes  # verbatim 64-byte SQE
    opcode: int

    # Present for read (0x02) and write (0x01) only; None otherwise.
    nsid: Optional[int] = None
    prp1: Optional[int] = None  # SQE bytes 24–31
    prp2: Optional[int] = None  # SQE bytes 32–39
    slba: Optional[int] = None  # starting LBA (cdw10/11)
    nlb: Optional[int] = None  # number of logical blocks (cdw12 lower 16-bit + 1)

    @property
    def opcode_str(self) -> str:
        return f"{opcode_name(self.opcode)}(0x{self.opcode:02x})"


@dataclass
class NvmePciSubmit:
    """Parsed nvme_pci_submit trace event."""

    ctrl_id: int
    qid: int
    cid: int
    opcode: int
    nsid: int
    data_len: int
    meta_len: int
    use_sgl: bool
    single_segment: bool
    sqe: SqeInfo

    # Exactly one list is populated, the other is empty.
    prp_entries: List[int] = field(default_factory=list)  # use_sgl == False
    sgl_entries: List[SglDesc] = field(default_factory=list)  # use_sgl == True


@dataclass
class NvmePciComplete:
    """Parsed nvme_pci_complete trace event."""

    ctrl_id: int
    qid: int
    cid: int
    result: int
    sq_head: int
    sq_id: int
    status: int
    retries: int


# ---------------------------------------------------------------------------
# Low-level binary parsers
# ---------------------------------------------------------------------------


def _parse_prp_entries(data: bytes) -> List[int]:
    """Decode a flat byte buffer as a list of __le64 PRP addresses."""
    return [struct.unpack_from("<Q", data, i)[0] for i in range(0, len(data) - 7, 8)]


def _parse_sgl_entries(data: bytes) -> List[SglDesc]:
    """Decode a flat byte buffer as a list of struct nvme_sgl_desc (16 B each)."""
    entries = []
    for i in range(0, len(data) - 15, 16):
        addr, length, r0, r1, r2, dtype = struct.unpack_from("<QIBBBB", data, i)
        entries.append(
            SglDesc(addr=addr, length=length, reserved=(r0, r1, r2), desc_type=dtype)
        )
    return entries


def _parse_sqe(raw_sqe) -> SqeInfo:
    """Build an SqeInfo from a bytes-like 64-byte SQE."""
    data = bytes(raw_sqe)
    op = data[0]
    info = SqeInfo(raw=data, opcode=op)
    if op in _READ_WRITE_OPCODES:
        info.nsid = struct.unpack_from("<I", data, 4)[0]
        info.prp1 = struct.unpack_from("<Q", data, 24)[0]
        info.prp2 = struct.unpack_from("<Q", data, 32)[0]
        info.slba = struct.unpack_from("<Q", data, 40)[0]
        info.nlb = struct.unpack_from("<H", data, 48)[0] + 1
    return info


# ---------------------------------------------------------------------------
# Public factory functions
# ---------------------------------------------------------------------------


def parse_submit(
    ctrl_id: int,
    qid: int,
    cid: int,
    opcode: int,
    nsid: int,
    data_len: int,
    meta_len: int,
    use_sgl: int,
    single_segment: int,
    sqe,
    descriptors,
) -> NvmePciSubmit:
    """Parse raw perf-script field values into an NvmePciSubmit instance."""
    desc_bytes = bytes(descriptors)
    sgl_flag = bool(use_sgl)

    prp_entries: List[int] = []
    sgl_entries: List[SglDesc] = []
    if sgl_flag:
        sgl_entries = _parse_sgl_entries(desc_bytes)
    else:
        prp_entries = _parse_prp_entries(desc_bytes)

    return NvmePciSubmit(
        ctrl_id=ctrl_id,
        qid=qid,
        cid=cid,
        opcode=opcode,
        nsid=nsid,
        data_len=data_len,
        meta_len=meta_len,
        use_sgl=sgl_flag,
        single_segment=bool(single_segment),
        sqe=_parse_sqe(sqe),
        prp_entries=prp_entries,
        sgl_entries=sgl_entries,
    )


def parse_complete(
    ctrl_id: int,
    qid: int,
    cid: int,
    result: int,
    sq_head: int,
    sq_id: int,
    status: int,
    retries: int,
) -> NvmePciComplete:
    """Parse raw perf-script field values into an NvmePciComplete instance."""
    return NvmePciComplete(
        ctrl_id=ctrl_id,
        qid=qid,
        cid=cid,
        result=result,
        sq_head=sq_head,
        sq_id=sq_id,
        status=status,
        retries=retries,
    )
