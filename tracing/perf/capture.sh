#!/usr/bin/env bash

sudo perf record \
    -e nvme:nvme_pci_submit --filter 'ctrl_id==1' \
    -e nvme:nvme_pci_complete --filter 'ctrl_id==1' \
    -a \
    -T \
    --sample-identifier \
    -k CLOCK_MONOTONIC \
    -o nvme_raw.data \
    sudo fio --name=nvme_write --filename=/dev/disk/by-id/nvme-KIOXIA_KCMYXRUG3T84_4FB0A0CT0LM3_1 --rw=write --bs=128k --iodepth=4 --ioengine=libaio --direct=1 --size=10G --numjobs=1

sudo perf inject --build-ids -i nvme_raw.data -o nvme_sorted.data

sudo perf script -i nvme_sorted.data -s perf-parse-sgl-prp.py