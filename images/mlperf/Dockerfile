FROM ubuntu:24.04

ARG DEBIAN_FRONTEND=noninteractive
ARG MLPERF_STORAGE_REF=v2.0

ENV UV_INSTALL_DIR=/usr/local/bin \
    UV_LINK_MODE=copy \
    PATH="/opt/mlcommons/storage:${PATH}" \
    OMPI_ALLOW_RUN_AS_ROOT=1 \
    OMPI_ALLOW_RUN_AS_ROOT_CONFIRM=1

RUN apt-get update && \
    apt-get install -y --no-install-recommends \
        build-essential \
        ca-certificates \
        curl \
        git \
        libopenmpi-dev \
        openmpi-bin \
        openmpi-common \
        openssh-client \
        pkg-config \
        python3 \
        python3-dev \
        python3-pip \
        python3-venv && \
    rm -rf /var/lib/apt/lists/*

RUN curl -LsSf https://astral.sh/uv/install.sh | sh

WORKDIR /opt/mlcommons

RUN git clone --depth 1 --branch "${MLPERF_STORAGE_REF}" https://github.com/mlcommons/storage.git storage

WORKDIR /opt/mlcommons/storage

RUN pip3 install --break-system-packages --no-cache-dir -e . && \
    mlpstorage --help >/dev/null

ENTRYPOINT ["mlpstorage"]
