#!/usr/bin/env bash
set -euo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
source_dir="$root/target/bash"
mkdir -p "$source_dir" "$root/tools/bash/out"
archive="$source_dir/bash-5.3.tar.gz"
if [ ! -f "$archive" ]; then
    curl -fL --retry 3 https://mirrors.kernel.org/gnu/bash/bash-5.3.tar.gz -o "$archive"
fi
echo "0d5cd86965f869a26cf64f4b71be7b96f90a3ba8b3d74e27e8e9d9d5550f31ba  $archive" | sha256sum --check

# Build on the host's filesystem; WSL-mounted Windows trees are slow for configure.
work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT
tar xf "$archive" -C "$work"
cd "$work/bash-5.3"
export CC=aarch64-linux-gnu-gcc AR=aarch64-linux-gnu-ar RANLIB=aarch64-linux-gnu-ranlib
export CFLAGS='-Os -ffunction-sections -fdata-sections'
export LDFLAGS='-static -Wl,--gc-sections'
./configure --host=aarch64-linux-gnu --prefix=/system \
    --disable-nls --enable-multibyte --without-bash-malloc \
    --disable-readline --disable-history --disable-bang-history \
    bash_cv_dev_fd=whacky bash_cv_job_control_missing=present \
    bash_cv_sys_siglist=yes bash_cv_func_sigsetjmp=present bash_cv_unusable_rtsigs=no
make -j"$(getconf _NPROCESSORS_ONLN)"
aarch64-linux-gnu-strip bash
aarch64-linux-gnu-readelf -l bash > "$source_dir/program-headers.txt"
if grep -Eq 'INTERP|DYNAMIC' "$source_dir/program-headers.txt"; then
    echo 'Bash must be static' >&2
    exit 1
fi
cp bash "$root/tools/bash/out/bash"
cp COPYING "$root/tools/bash/out/COPYING"
sha256sum "$root/tools/bash/out/bash"
