#!/bin/bash
# Installs the Linux packages on Debian 12, Ubuntu 24.04 and Ubuntu 26.04 (containers,
# without systemd running: the Node isn't started there) and starts each program far
# enough to show its version or help: every library found and every symbol bound,
# FFmpeg's included.
#   installer/linux/deb/test.sh INSTALLERS VERSION   (ROOT/Installers: Media/ and Industrial/)
set -euo pipefail
DIR=$1 VERSION=$2
DOCKER=$(command -v docker || command -v podman)
PRODUCT="${IDN_PRODUCT:-both}"   # media, industrial or both
PKGS="" BINS="" RUN=""
if [ "$PRODUCT" != industrial ]; then
  PKGS="$PKGS /debs/Media/idn-media_${VERSION}_amd64.deb"; BINS="$BINS /usr/lib/idn-media/bin/*"
  RUN="$RUN idn --version | head -1; idn-node --version | head -1;"
  RUN="$RUN idn-switcher --version 2>&1 | head -1; idn-codec --version 2>&1 | head -1;"
  # Screen and webcam capture: the bundled libavdevice loads (and its libavfilter).
  RUN="$RUN /usr/lib/idn-media/bin/idn-codec capture devices >/dev/null;"
  PKGS="$PKGS /debs/Media/idn-site-manager_${VERSION}_amd64.deb"; BINS="$BINS /usr/bin/idn-site-manager"
  RUN="$RUN idn-site-manager --version;"
  # The network audio device, in idn-media since 2026-11-17 (no package of its own).
  BINS="$BINS /usr/lib/idn-media/bin/idn-aoip"
  RUN="$RUN idn-aoip --help | head -1; idn aoip --help | head -1;"
  # IDN's ASIO driver: in cwASIO's folder (its libraries found, below) and listed by postinst.
  BINS="$BINS /usr/lib/cwASIO/libidn_asio.so"
  RUN="$RUN test \"\$(cat /etc/cwASIO/IDN/driver)\" = /usr/lib/cwASIO/libidn_asio.so; idn asio list | grep -q '(IDN)';"
fi
if [ "$PRODUCT" != media ]; then
  PKGS="$PKGS /debs/Industrial/idn-industrial_${VERSION}_amd64.deb"; BINS="$BINS /usr/bin/idn-industrial"
  PKGS="$PKGS /debs/Industrial/idn-site-manager-industrial_${VERSION}_amd64.deb"; BINS="$BINS /usr/bin/idn-site-manager-industrial"
  RUN="$RUN idn-industrial status 2>&1 | head -1; idn-site-manager-industrial version;"
  RUN="$RUN test -f /etc/idn-site-manager-industrial.env; test -f /lib/systemd/system/idn-site-manager-industrial.service;"
  # Its own user and the polkit rule (E7-11); polkit-check says no without root's file (an
  # `if`: `! cmd` never stops `sh -e`; F3-5).
  RUN="$RUN getent passwd idn-industrial; test -f /usr/share/polkit-1/rules.d/50-idn-industrial.rules; if idn-industrial polkit-check reboot; then echo 'polkit-check allowed a reboot'; exit 1; fi;"
fi
for img in debian:12 ubuntu:24.04 ubuntu:26.04; do
  echo "== $img"
  "$DOCKER" run --rm -v "$DIR:/debs:ro" "$img" sh -ec "
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq >/dev/null
    apt-get install -y -qq $PKGS >/dev/null 2>&1 || { apt-get install -y $PKGS; exit 1; }
    missing=\$(for f in $BINS; do ldd \$f | grep 'not found' | sed \"s|^|\$f: |\"; done)
    [ -z \"\$missing\" ] || { echo \"\$missing\"; exit 1; }
    # Every symbol bound at the start (FFmpeg's private glibc ones too), not when first used.
    export LD_BIND_NOW=1
    $RUN
    echo ok
  "
done
