# StarBooru server image: the API, media storage and local AI in one container.
# Works with BuildKit and the legacy builder (no cache mounts).
#
# AI_GPU picks what local AI can use besides the processor:
#   none    processor only, x86-64 and ARM64 (the default)
#   vulkan  AMD, Intel and NVIDIA cards through their Vulkan drivers (x86-64)
#   nvidia  NVIDIA cards through CUDA, faster and about 3 GB larger (x86-64)

ARG RUST_VERSION=1.98

FROM rust:${RUST_VERSION}-trixie AS build

WORKDIR /src
COPY . .

# The version the server shows: a release's tag, or Cargo's when empty.
ARG STARBOORU_VERSION=
RUN mkdir -p /out/ai \
    && cargo build --release --locked -p starbooru-server -p starbooru-ai-worker \
    && cp target/release/starbooru-server /out/starbooru-server \
    && cp target/release/starbooru-ai-worker /out/ai/starbooru-ai-worker

# ONNX Runtime, which runs the models, pinned by version and hash. It comes from
# Microsoft's NuGet packages: the GitHub releases are gone since 2026-11-17, and
# the libraries in these packages are the same files, byte for byte.
ARG AI_GPU=none
ARG ORT_VERSION=1.31.1
ARG ORT_SHA256_CPU=6871ccbc97f3d5bdb4095a1dcec33aa78d56f6ed8de5980ef5fa1d9f2e7c679a
ARG ORT_SHA256_CUDA=cfb5315b87f812d8d91194075eb4aab3cbfd97a932f2b56c9bfa41e36087c4a2
RUN set -eux; \
    case "$AI_GPU:$(uname -m)" in \
      none:x86_64|vulkan:x86_64) pkg=microsoft.ml.onnxruntime; rid=linux-x64; sha="$ORT_SHA256_CPU" ;; \
      none:aarch64) pkg=microsoft.ml.onnxruntime; rid=linux-arm64; sha="$ORT_SHA256_CPU" ;; \
      nvidia:x86_64) pkg=microsoft.ml.onnxruntime.gpu.linux; rid=linux-x64; sha="$ORT_SHA256_CUDA" ;; \
      *) echo "AI_GPU=$AI_GPU isn't available on $(uname -m)" >&2; exit 1 ;; \
    esac; \
    curl -fsSL -A starbooru-build -o /tmp/ort.zip \
      "https://api.nuget.org/v3-flatcontainer/$pkg/$ORT_VERSION/$pkg.$ORT_VERSION.nupkg"; \
    echo "$sha  /tmp/ort.zip" | sha256sum -c -; \
    mkdir /tmp/ort; \
    python3 -c 'import os,sys,zipfile; z=zipfile.ZipFile("/tmp/ort.zip"); d="runtimes/"+sys.argv[1]+"/native/"; [open("/tmp/ort/"+os.path.basename(n),"wb").write(z.read(n)) for n in z.namelist() if n.startswith(d) and n.endswith(".so")]' "$rid"; \
    cp /tmp/ort/libonnxruntime.so /out/ai/libonnxruntime.so; \
    if [ "$AI_GPU" = nvidia ]; then \
      cp /tmp/ort/libonnxruntime_providers_cuda.so /tmp/ort/libonnxruntime_providers_shared.so /out/ai/; \
    fi; \
    rm -rf /tmp/ort /tmp/ort.zip

# The worker for graphics cards, tried before the processor's.
RUN set -eux; \
    case "$AI_GPU" in \
      vulkan) \
        cargo build --release --locked -p starbooru-ai-worker --no-default-features --features webgpu; \
        cp target/release/starbooru-ai-worker /out/ai/starbooru-ai-worker-gpu; \
        cp -L target/release/libwebgpu_dawn.so /out/ai/ ;; \
      nvidia) \
        cargo build --release --locked -p starbooru-ai-worker --features cuda; \
        cp target/release/starbooru-ai-worker /out/ai/starbooru-ai-worker-gpu ;; \
    esac

# The CUDA libraries ONNX Runtime needs, from NVIDIA's wheels on PyPI. The
# container gets the driver itself from the NVIDIA Container Toolkit.
RUN set -eux; \
    [ "$AI_GPU" = nvidia ] || exit 0; \
    cd /tmp; \
    for wheel in \
      "21721774d8e7044debec3034ecc1a8cd22e173eb6dae5d7f5f8a014852382894 https://files.pythonhosted.org/packages/04/36/c6f716dbd080fa030e6056261f238fb82c2b78d94e5074460740f61384b3/nvidia_cublas-13.8.2.12-py3-none-manylinux_2_27_x86_64.whl" \
      "9641f797da20ce1dd8e779b6e96d08cf9ba564cec8e8225458811ee26423f3a5 https://files.pythonhosted.org/packages/98/8a/3431271f6344874b8f1ac03f16b3d679c91493f8da63f716160403e6d0a0/nvidia_cuda_runtime-13.4.92-py3-none-manylinux2014_x86_64.manylinux_2_17_x86_64.whl" \
      "25c3457ae7a224fdd484dab90b0fc5dc0e842fab5db3012afa4a5bd2af4eb7e5 https://files.pythonhosted.org/packages/07/73/3ee8e5b4cb891401e603ffd3a59b35c6afe785fd2de123afe7c7029603dc/nvidia_curand-10.4.4.72-py3-none-manylinux_2_27_x86_64.whl" \
      "9677e76f21862eb5da7ee5ed69d544738b2d8b5c3ce7e5ec125c5592e6cdbdc8 https://files.pythonhosted.org/packages/af/75/96ea5c5368eb595c39d629cde08a66227a864e71ccb0e593add2612bb952/nvidia_cudnn_cu13-9.27.0.42-py3-none-manylinux_2_27_x86_64.whl"; \
    do \
      set -- $wheel; \
      curl -fsSL -A starbooru-build -o wheel.zip "$2"; \
      echo "$1  wheel.zip" | sha256sum -c -; \
      python3 -c 'import os,zipfile; z=zipfile.ZipFile("wheel.zip"); [open("/out/ai/"+os.path.basename(n),"wb").write(z.read(n)) for n in z.namelist() if ".so" in os.path.basename(n)]'; \
      rm wheel.zip; \
    done


FROM debian:trixie-slim

ARG AI_GPU=none

# ffmpeg makes thumbnails for video, AVIF and JPEG XL; pg_dump makes database backups;
# tesseract reads text in images (OCR): English, and Japanese across and down for manga.
# For more OCR languages add their packages here, e.g. tesseract-ocr-ita or
# tesseract-ocr-deu, and list them in STARBOORU_OCR_LANGS (eng+ita).
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates ffmpeg postgresql-client \
       tesseract-ocr tesseract-ocr-eng tesseract-ocr-jpn tesseract-ocr-jpn-vert \
    && rm -rf /var/lib/apt/lists/*
# Vulkan drivers for the graphics card worker. Without the software renderer,
# which is slower than the processor worker, a server with no card finds none.
RUN set -eux; \
    [ "$AI_GPU" = vulkan ] || exit 0; \
    apt-get update; \
    apt-get install -y --no-install-recommends libvulkan1 mesa-vulkan-drivers; \
    rm /usr/share/vulkan/icd.d/lvp_icd.json; \
    rm -rf /var/lib/apt/lists/*
# A fresh named volume copies this directory's ownership, so the server can write to it.
RUN useradd --system --uid 10001 --user-group --no-create-home --home-dir /data starbooru \
    && mkdir -p /data \
    && chown starbooru:starbooru /data

COPY --from=build /out/starbooru-server /app/starbooru-server
COPY --from=build /out/ai/ /app/

# A web client's bundle can be mounted and served with STARBOORU_WEB_DIR.
ENV STARBOORU_BIND=0.0.0.0:8080 \
    LD_LIBRARY_PATH=/app \
    STARBOORU_DATA_DIR=/data

USER starbooru
VOLUME /data
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=5s --start-period=90s --retries=3 \
    CMD ["/app/starbooru-server", "healthcheck"]
ENTRYPOINT ["/app/starbooru-server"]
