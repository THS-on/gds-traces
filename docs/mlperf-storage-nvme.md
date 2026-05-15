# MLPerf Storage on NVMe

This repo uses MLPerf Storage as a reproducible ML-shaped I/O generator for the
configured NVMe device in `config.toml`.

## Version

Use the latest official MLCommons Storage release tag, not `main`, for repeatable
work:

```bash
inv mlperf.build --ref v2.0
```

The image builds MLCommons Storage `v2.0` and installs the matching
`dlio-benchmark` dependency declared by that release. This is intentionally
different from older slide material that references MLPerf Storage `v0.5`
workloads such as BERT and U-Net3D.

## What It Exercises

MLPerf Storage `v2.0` training emulates accelerators with DLIO. The benchmark
keeps the data loading path realistic and replaces accelerator compute with a
measured sleep interval. This lets us drive NVMe I/O without requiring GPUs.

The v2.0 training workloads are:

| Model | Domain | Loader | Typical use here |
| --- | --- | --- | --- |
| `unet3d` | medical image segmentation | PyTorch | large sample, storage-heavy smoke and full runs |
| `resnet50` | image classification | TensorFlow | many smaller samples |
| `cosmoflow` | scientific/cosmology | TensorFlow | medium samples, lower AU target |

Checkpointing is also available for `llama3-8b`, `llama3-70b`,
`llama3-405b`, and `llama3-1t`. Use it when the question is checkpoint
write/read behavior rather than training input reads.

## Smoke Runs

Smoke runs are for validating the container, NVMe mount, tracing, and result
collection. They intentionally use a small dataset and are not valid MLPerf
submissions.

```bash
inv mlperf.training-smoke --model unet3d
inv mlperf.training-smoke --model resnet50
inv mlperf.training-smoke --model cosmoflow
```

Or run all three:

```bash
inv mlperf.training-smoke-all
```

Each smoke run:

1. Formats the configured NVMe as ext4.
2. Mounts it at `/mnt/nvme` on the host.
3. Bind-mounts it into the MLPerf container at `/mnt/nvme`.
4. Generates synthetic DLIO data under `/mnt/nvme/mlperf_data`.
5. Runs the selected training workload.
6. Writes host-visible results under `results/mlperf`.
7. Unmounts the NVMe.

For `unet3d`, the task passes `reader.odirect=true` by default so the smoke run
is closer to a storage-device test and less dominated by page cache. MLPerf
Storage v2.0 only supports that parameter for `unet3d`.

## Full Training Flow

A submission-like run should first size the dataset:

```bash
inv mlperf.datasize \
  --model unet3d \
  --client-memory-gb 256 \
  --max-accelerators 8 \
  --accelerator h100 \
  --hosts 127.0.0.1
```

Use the reported `dataset.num_files_train` value for the run:

```bash
inv mlperf.training-smoke \
  --model unet3d \
  --num-files <reported-file-count> \
  --num-processes 8 \
  --num-accelerators 8 \
  --client-memory-gb 256 \
  --accelerator h100
```

For strict MLPerf submission work, keep the release tag fixed, keep benchmark
code unchanged, use the dataset size from `datasize`, run the required
consecutive repetitions, and preserve all logs from `results/mlperf`.

## Checkpoint Smoke

Checkpointing smoke runs use one write and one read so they are quick sanity
checks, not valid submissions:

```bash
inv mlperf.checkpoint-smoke --model llama3-8b
```

For full checkpoint measurements, use the v2.0 rule shape: 10 writes and
10 reads, clear filesystem caches between write and read phases when required,
and use the process counts defined by the selected Llama3 model.

## Tracing

To collect NVMe traces around a workload, start `nvme-trace capture` for the
target controller before invoking the MLPerf task, then stop it after the
container exits. The benchmark I/O is issued by the host kernel to the mounted
NVMe filesystem, so the existing NVMe tracing path should see it.
