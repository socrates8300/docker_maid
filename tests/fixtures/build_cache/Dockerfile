# syntax=docker/dockerfile:1
FROM busybox:latest

RUN --mount=type=cache,id=docker-maid-build-cache-proof,target=/cache \
    dd if=/dev/zero of=/cache/blob bs=1M count=1
RUN echo docker-maid-build-cache-proof > /proof
