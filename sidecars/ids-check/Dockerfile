FROM python:3.13-slim-trixie

COPY --from=ghcr.io/astral-sh/uv:0.13.7 /uv /uvx /bin/

ENV UV_COMPILE_BYTECODE=1
ENV UV_LINK_MODE=copy
ENV UV_NO_DEV=1

WORKDIR /app

RUN --mount=type=cache,target=/root/.cache/uv \
    --mount=type=bind,source=uv.lock,target=uv.lock \
    --mount=type=bind,source=pyproject.toml,target=pyproject.toml \
    uv sync --locked --no-install-project

COPY ids_check.py ./

RUN useradd -r -s /bin/false ids-check
USER ids-check

ENV PATH="/app/.venv/bin:$PATH"

EXPOSE 8000

# python:slim has no curl
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD ["python", "-c", "import urllib.request; urllib.request.urlopen('http://127.0.0.1:8000/health')"]

# tiletopia's data directory is mounted read-only at /data
CMD ["python", "ids_check.py"]
