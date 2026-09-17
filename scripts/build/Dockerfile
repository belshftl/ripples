FROM debian:12

ARG RUST_VERSION=1.98.1
ARG CARGO_DEB_VERSION=3.8.0
ARG CARGO_GENERATE_RPM_VERSION=0.21.0

RUN apt-get update \
	&& case "$(dpkg --print-architecture)" in \
		amd64) cross=crossbuild-essential-arm64 ;; \
		arm64) cross=crossbuild-essential-amd64 ;; \
		*) echo "unsupported build arch: $(dpkg --print-architecture)" >&2; exit 1 ;; \
	esac \
	&& apt-get install -y --no-install-recommends \
		binutils ca-certificates curl gcc libc6-dev "$cross" \
	&& rm -rf /var/lib/apt/lists/*

ENV RUSTUP_HOME=/usr/local/rustup \
	CARGO_HOME=/usr/local/cargo \
	PATH=/usr/local/cargo/bin:$PATH

RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path --profile minimal \
		--default-toolchain "$RUST_VERSION" \
	&& rustup target add \
		x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu \
		x86_64-unknown-linux-musl aarch64-unknown-linux-musl \
	&& cargo install --locked \
		"cargo-deb@$CARGO_DEB_VERSION" "cargo-generate-rpm@$CARGO_GENERATE_RPM_VERSION" \
	&& rm -rf "$CARGO_HOME/registry"

ENV CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=x86_64-linux-gnu-gcc \
	CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=x86_64-linux-gnu-gcc \
	CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
	CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=aarch64-linux-gnu-gcc \
	CARGO_TARGET_DIR=/target
