#!/usr/bin/env bash

sudo perf record \
    -e nvme:nvme_pci_submit \
    -e nvme:nvme_pci_complete \
    -a \
    -T \
    --sample-identifier \
    -k CLOCK_MONOTONIC \
    -o nvme_raw.data

sudo perf inject --build-ids -i nvme_raw.data -o nvme_sorted.data

perf script -i nvme_sorted.data -s perf-parse-sgl-prp.py