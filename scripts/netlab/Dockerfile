FROM ubuntu:24.04
COPY install.sh /tmp/netlab-install.sh
RUN bash /tmp/netlab-install.sh && rm /tmp/netlab-install.sh
ENV PATH="/root/.cargo/bin:${PATH}"
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs -o /tmp/rustup.sh \
    && sh /tmp/rustup.sh -y --profile minimal --component rustfmt,clippy \
    && rm /tmp/rustup.sh
WORKDIR /work
