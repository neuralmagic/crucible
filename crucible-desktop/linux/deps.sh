#!/bin/sh
# What building, testing and packaging the app on Fedora needs, for the builder image and CI.
set -eu
dnf -y install \
    gcc gcc-c++ make cmake git perl pkgconf-pkg-config \
    libxkbcommon-devel libxkbcommon-x11-devel wayland-devel libxcb-devel \
    dbus-devel dbus-daemon fontconfig-devel openssl-devel \
    vulkan-loader mesa-vulkan-drivers \
    python3 rpm-build desktop-file-utils
dnf clean all
