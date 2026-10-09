# The image the release workflow publishes to ghcr.io/warcon-app/ciws: each platform's static
# Linux binary from the same release, on a base that has CA certificates and nothing else. Made
# from binaries already built, so building it for several platforms needs no emulation. The
# Dockerfile at the top of the repository builds CIWS from source instead.
FROM gcr.io/distroless/static-debian12:nonroot
ARG TARGETARCH
COPY ${TARGETARCH}/ciws /usr/local/bin/ciws
# Mount the folder that holds ciws.toml here. Nothing else is kept: no volume needed.
WORKDIR /config
ENV CIWS_CONFIG=/config/ciws.toml \
    CIWS_ADMIN_LISTEN=0.0.0.0:7781
EXPOSE 7780 7781
ENTRYPOINT ["/usr/local/bin/ciws"]
CMD ["run"]
