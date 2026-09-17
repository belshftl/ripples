#!/bin/sh
set -eu

# must be kept in sync with `libc (>= ...)` in [package.metadata.deb] in Cargo.toml
glibc_ver=2.34

fail() {
    echo "artifacts: $*" >&2
    exit 1
}

check_gnu() {
    needed=$(readelf -dW "$1" | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p' | sort | tr '\n' ' ')
    if [ "$needed" != "libc.so.6 libgcc_s.so.1 " ]; then
        fail "$1 needs $needed, expected only libc and libgcc"
    fi
    newest=$(readelf -VW "$1" | grep -o 'GLIBC_[0-9.]*' | cut -d_ -f2 | sort -uV | tail -n 1)
    if [ "$(printf '%s\n%s\n' "$newest" "$glibc_ver" | sort -V | tail -n 1)" != "$glibc_ver" ]
    then
        fail "$1 needs glibc $newest, expected not newer than the declared $glibc_ver"
    fi
}

check_static() {
    if readelf -dW "$1" | grep -q NEEDED || readelf -lW "$1" | grep -q INTERP; then
        fail "$1 is unexpectedly not statically linked"
    fi
}

cd /src
version=$(cargo pkgid | sed 's/.*[#@]//')
out=$(mktemp -d)

for arch in x86_64 aarch64; do
    gnu=$arch-unknown-linux-gnu
    musl=$arch-unknown-linux-musl
    cargo build --locked --release --target "$gnu"
    cargo build --locked --release --target "$musl"
    check_gnu "$CARGO_TARGET_DIR/$gnu/release/ripples"
    check_static "$CARGO_TARGET_DIR/$musl/release/ripples"

    cp "$CARGO_TARGET_DIR/$gnu/release/ripples" "$out/ripples-$version-$arch-linux-gnu"
    cp "$CARGO_TARGET_DIR/$musl/release/ripples" "$out/ripples-$version-$arch-linux-musl"
    cargo deb --no-build --target "$gnu" --output "$out/"
    cargo generate-rpm --target "$gnu" --target-dir "$CARGO_TARGET_DIR" --output "$out/"
done

(cd "$out" && sha256sum -- * > sha256sums)
rm -rf /dist/*
cp "$out"/* /dist/
chown -R "$OWNER" /dist
ls -l /dist
