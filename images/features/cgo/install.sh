#!/usr/bin/env bash
set -euo pipefail

arch="$(uname -m)"
case "${arch}" in
    x86_64)
        goarch=amd64
        tok_sha="c5f6e6f63491c297d754bde0783c50a04df2b959907a87cb0c81ac7d3b6b4b63"
        tok_prev_sha="af0f391646c54255b4e522dc81514ee238bcce3a949a6d35b18076175f6c7a4e"
        ;;
    aarch64)
        goarch=arm64
        tok_sha="25c1678dba890f234079838620e1019951a45103526d08a61eacaa58f804e3d4"
        tok_prev_sha="6acaab6c03c34166e97fe551292193bf26a7de0bef673cbec3a0e47c513ff02f"
        ;;
    *) echo "unsupported arch ${arch}" >&2; exit 1 ;;
esac

zmq_sha="6653ef5910f17954861fe72332e68b03ca6e4d9c7160eb3a8de5a5a913bfab43"

curl -fsSL -o /tmp/zeromq.tar.gz \
    "https://github.com/zeromq/libzmq/releases/download/v${PIN_LIBZMQ}/zeromq-${PIN_LIBZMQ}.tar.gz"
echo "${zmq_sha}  /tmp/zeromq.tar.gz" | sha256sum -c -
mkdir -p /tmp/zeromq
tar -xzf /tmp/zeromq.tar.gz -C /tmp/zeromq --strip-components=1
(
    cd /tmp/zeromq
    ./configure --prefix=/usr --libdir=/usr/lib64 --without-docs --disable-Werror
    make -j"$(nproc)"
    make install
    ldconfig
)
rm -rf /tmp/zeromq /tmp/zeromq.tar.gz

install_tokenizers() {
    local version="$1" sha="$2" dir="/opt/libtokenizers/v${1}"
    curl -fsSL -o /tmp/libtokenizers.tar.gz \
        "https://github.com/daulet/tokenizers/releases/download/v${version}/libtokenizers.linux-${goarch}.tar.gz"
    echo "${sha}  /tmp/libtokenizers.tar.gz" | sha256sum -c -
    mkdir -p "${dir}"
    tar -xzf /tmp/libtokenizers.tar.gz -C "${dir}" libtokenizers.a
    rm -f /tmp/libtokenizers.tar.gz
}

install_tokenizers "${PIN_TOKENIZERS}" "${tok_sha}"
install_tokenizers "${PIN_TOKENIZERS_PREV}" "${tok_prev_sha}"
chmod -R a+rX /opt/libtokenizers

gcc --version
pkg-config --modversion libzmq
python3.12-config --cflags
test -f "/opt/libtokenizers/v${PIN_TOKENIZERS}/libtokenizers.a"
test -f "/opt/libtokenizers/v${PIN_TOKENIZERS_PREV}/libtokenizers.a"
