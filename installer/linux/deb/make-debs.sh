#!/bin/bash
# The two Linux packages from the builder's programs (installer/linux/deb/build.sh):
#   idn-media_VER_amd64.deb      the Node (a systemd service), the `idn` command and tools,
#                                the switcher, the mixer, idn-session, IDN's ASIO driver
#                                for cwASIO hosts (/usr/lib/cwASIO, listed as "IDN" in
#                                /etc/cwASIO), the Node's control
#                                panel (IDN Media's only UI: the rest is the host application's), the CLAP
#                                and VST3 plugins, the guide, the network audio device
#                                (idn-aoip, IDN's own; replaces the earlier separate
#                                package idn-aoip); FFmpeg 8.1 (LGPL) bundled
#   idn-industrial_VER_amd64.deb the Industrial edition's Node (a systemd service)
# For Debian 12, Ubuntu 24.04 and Ubuntu 26.04 (amd64): built on Debian 12, they need no
# newer C library than it has, and their dependencies by Debian's names (Ubuntu's renamed
# libraries provide those).
#   make-debs.sh VERSION BUILT FFMPEG GUIDE OUT
set -euo pipefail
# Files root's to write, everyone's to read (644, 755), whatever the builder's umask.
umask 022
VERSION=$1 BUILT=$2 FF=$3 GUIDE=$4 OUT=$5
HERE="$(cd "$(dirname "$0")" && pwd)"
IMPL="$(cd "$HERE/../../.." && pwd)"
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
mkdir -p "$OUT"

PRODUCT="${IDN_PRODUCT:-both}"   # media, industrial or both
# ---- idn-media
if [ "$PRODUCT" != industrial ]; then
P=$STAGE/idn-media
L=$P/usr/lib/idn-media
mkdir -p $L/bin $L/lib $P/usr/bin $P/DEBIAN $P/usr/share/doc/idn-media \
  $P/usr/share/applications $P/etc/xdg/autostart $P/usr/share/icons/hicolor/256x256/apps \
  $P/usr/lib/clap $P/usr/lib/vst3/IDN.vst3/Contents/x86_64-linux $P/usr/lib/cwASIO
for b in idn idn-node idn-codec idn-switcher idn-plugin-host idn-session idn-display idn-aoip; do
  install -m 755 "$BUILT/$b" "$L/bin/$b"
  strip --strip-unneeded "$L/bin/$b"
  ln -s ../lib/idn-media/bin/$b "$P/usr/bin/$b"
done
# The tools' old names: `idn` under that name, which runs that command (R.17).
for b in idn-channel idn-discovery-server idn-doctor idn-dsctl idn-find idn-publish idn-recv idn-send idn-audio idn-mixer; do
  ln -s idn "$L/bin/$b"
  ln -s ../lib/idn-media/bin/$b "$P/usr/bin/$b"
done
# FFmpeg's libraries the programs load, under their sonames (the programs' run path is
# $ORIGIN/../lib), and its licence.
# libavdevice (screens and webcams, loaded when wanted) and the libavfilter it needs.
for so in libavcodec libavformat libavutil libswscale libswresample libavdevice libavfilter; do
  f=$(readlink -f "$FF"/lib/$so.so)
  cp "$f" "$L/lib/$(objdump -p "$f" | awk '/SONAME/{print $2}')"
done
chmod 644 $L/lib/*
cp -r "$GUIDE" "$L/guide"
install -m 644 "$BUILT/libidn_audio_plugins.so" $P/usr/lib/clap/IDN.clap
install -m 644 "$BUILT/libidn_audio_plugins.so" $P/usr/lib/vst3/IDN.vst3/Contents/x86_64-linux/IDN.so
# IDN's ASIO driver where cwASIO suggests (asio-interface.md §A.5); listed by postinst.
install -m 644 "$BUILT/libidn_asio.so" $P/usr/lib/cwASIO/libidn_asio.so
strip --strip-unneeded $P/usr/lib/cwASIO/libidn_asio.so
install -m 644 "$IMPL/media/idn-node/assets/idn-256.png" \
  $P/usr/share/icons/hicolor/256x256/apps/idn-media.png
desktop() { # FILE NAME EXEC COMMENT [TERMINAL]
  printf '[Desktop Entry]\nType=Application\nName=%s\nExec=%s\nComment=%s\nIcon=idn-media\nCategories=AudioVideo;\nTerminal=%s\n' \
    "$2" "$3" "$4" "${5:-false}" > "$1"
}
desktop $P/usr/share/applications/idn-node.desktop "IDN Node" "idn-session --open" "Open the IDN Node control panel (in the browser)"
desktop $P/usr/share/applications/idn-guide.desktop "IDN Guide" "xdg-open /usr/lib/idn-media/guide/index.html" "The IDN Media user guide"
# The session agent at every log-in, as on Windows (no window, no icon: IDN Media has no UI
# but the Node's control panel; the UI is the host application's).
desktop $P/etc/xdg/autostart/idn-session.desktop "IDN session" "idn-session" "The IDN Node's side in this session: sound, screens and webcams"
{
  echo "IDN Media (IDN Open Protocol), (c) ADC Industries: MIT OR Apache-2.0."
  echo "FFmpeg 8.1 (BtbN's LGPL build, configured --enable-version3), in /usr/lib/idn-media/lib:"
  echo "LGPL-3.0-or-later."
  echo "The site clock: IDN's modified copy of statime 0.4.0, (c) 2022-2023 Tweede Golf and Contributors: MIT OR Apache-2.0."
  echo
  cat "$IMPL/LICENSE-MIT"; echo; cat "$IMPL/LICENSE-APACHE"; echo
  echo "======================================== Third-party code built in (NOTICE)"; cat "$IMPL/NOTICE"; echo
  echo "======================================== FFmpeg"; cat "$FF/LICENSE.txt"
} > $P/usr/share/doc/idn-media/copyright
cat > $P/DEBIAN/control <<C
Package: idn-media
Version: $VERSION
Architecture: amd64
Maintainer: ADC Industries <code@adcnetwork.org>
Section: video
Priority: optional
Depends: $(cat "$BUILT/DEPENDS"), systemd
Recommends: libvulkan1, xdg-utils
Replaces: idn-aoip
Conflicts: idn-aoip
Provides: idn-aoip
Description: IDN Media: video and sound over the network, NDI-Compatible
 The IDN Node (this machine's site identity and control panel, a service), the
 session agent idn-session (this session's sound, screens and webcams), the idn
 command and its tools, the video switcher and the mixing desk (servers with APIs;
 their UI is a host application), display outputs, and the CLAP/VST3 plugins. FFmpeg 8.1 (LGPL)
 is bundled.
C
cat > $P/DEBIAN/postinst <<'S'
#!/bin/sh
set -e
if [ "$1" = configure ]; then
  # IDN's ASIO driver listed for cwASIO hosts as "IDN" (again on an update: the same
  # library, no change). Instances added with `idn asio register NAME` stay listed.
  /usr/lib/idn-media/bin/idn asio register >/dev/null \
    || echo "idn-media: IDN's ASIO driver isn't listed for cwASIO hosts (see above; then: sudo idn asio register)"
  if [ -d /run/systemd/system ]; then
    # The service, its account and group `idn`, the update path (idn-node.md N.12).
    /usr/lib/idn-media/bin/idn-node install || echo "idn-media: the IDN Node didn't start: sudo idn-node install"
    [ -n "$2" ] && systemctl try-restart idn-node.service || true
    # The desktop's users may use the Node (group idn, from their next log-in): whoever ran
    # sudo or pkexec, else (an app store or a desktop installer) the administrators logged
    # in now (sudo, admin or wheel: they could add themselves anyway). Not every logged-in
    # user: a guest's or a child's account would have had the Node's settings.
    users="${SUDO_USER:-}"
    [ -n "${PKEXEC_UID:-}" ] && users="$users $(getent passwd "$PKEXEC_UID" | cut -d: -f1)"
    if [ -z "$(echo $users)" ]; then
      for u in $(loginctl list-users --no-legend 2>/dev/null | awk '$1 >= 1000 && $1 < 65534 {print $2}'); do
        if id -nG "$u" 2>/dev/null | tr ' ' '\n' | grep -qxE 'sudo|admin|wheel'; then
          users="$users $u"
        else
          echo "idn-media: $u may use the IDN Node after: sudo usermod -aG idn $u"
        fi
      done
    fi
    for u in $users; do
      [ "$u" = root ] && continue
      if ! id -nG "$u" 2>/dev/null | tr ' ' '\n' | grep -qx idn; then
        usermod -aG idn "$u" && echo "idn-media: added $u to group idn: log out and in again for this session to reach the IDN Node"
      fi
    done
  else
    echo "idn-media: systemd isn't running: start the IDN Node later with: sudo idn-node install"
  fi
fi
S
cat > $P/DEBIAN/prerm <<'S'
#!/bin/sh
set -e
if [ "$1" = remove ] && [ -x /usr/lib/idn-media/bin/idn-node ]; then
  /usr/lib/idn-media/bin/idn-node uninstall || true
fi
# Removed: every listing of IDN's ASIO driver (its instances too) off cwASIO's list.
if [ "$1" = remove ] && [ -x /usr/lib/idn-media/bin/idn ]; then
  /usr/lib/idn-media/bin/idn asio unregister --all >/dev/null 2>&1 || true
  rmdir /etc/cwASIO 2>/dev/null || true
fi
S
cat > $P/DEBIAN/postrm <<'S'
#!/bin/sh
set -e
# Purged: the machine's site identity goes too (its media folder, /var/lib/idn-media, stays).
# A site this machine ran (its Site Manager's CA, devices and users) is kept aside, not
# deleted with the Node: the whole site's trust (deep bug run 2026-11-09).
if [ "$1" = purge ]; then
  if [ -d /var/lib/idn-node/site-manager ]; then
    KEPT=/var/lib/idn-site-manager-kept-$(date +%s)
    mv /var/lib/idn-node/site-manager "$KEPT" && chmod 700 "$KEPT" \
      && echo "idn-media: this machine's site (its Site Manager) kept in $KEPT"
  fi
  rm -rf /var/lib/idn-node /var/lib/idn-update
fi
S
chmod 755 $P/DEBIAN/postinst $P/DEBIAN/prerm $P/DEBIAN/postrm
dpkg-deb --root-owner-group -Zxz --build $P "$OUT/idn-media_${VERSION}_amd64.deb"
fi

# ---- idn-industrial
if [ "$PRODUCT" != media ]; then
P=$STAGE/idn-industrial
mkdir -p $P/usr/bin $P/lib/systemd/system $P/DEBIAN $P/usr/share/doc/idn-industrial \
  $P/usr/share/polkit-1/rules.d
install -m 755 "$BUILT/idn-industrial" $P/usr/bin/idn-industrial
strip --strip-unneeded $P/usr/bin/idn-industrial
sed -e 's|/usr/local/bin/idn-industrial|/usr/bin/idn-industrial|g' -e '/^#/d' \
  "$IMPL/installer/linux/idn-industrial.service" > $P/lib/systemd/system/idn-industrial.service
# Its user may reboot and restart the services root's actions file names (E7-11).
install -m 644 "$IMPL/installer/linux/50-idn-industrial.rules" $P/usr/share/polkit-1/rules.d/50-idn-industrial.rules
{ echo "IDN Node | Industrial (IDN Open Protocol), (c) ADC Industries: MIT OR Apache-2.0."
  echo "The site clock: IDN's modified copy of statime 0.4.0, (c) 2022-2023 Tweede Golf and Contributors: MIT OR Apache-2.0."
  echo; cat "$IMPL/LICENSE-MIT"; echo; cat "$IMPL/LICENSE-APACHE"
  echo; echo "======================================== Third-party code built in (NOTICE)"; cat "$IMPL/NOTICE"; } > $P/usr/share/doc/idn-industrial/copyright
cat > $P/DEBIAN/control <<C
Package: idn-industrial
Version: $VERSION
Architecture: amd64
Maintainer: ADC Industries <code@adcnetwork.org>
Section: net
Priority: optional
Depends: $(cat "$BUILT/DEPENDS-industrial"), systemd, polkitd | policykit-1
Description: IDN Node | Industrial: a machine's link to its IDN Site Manager | Industrial
 A service from boot: health, the screens and roles the Site Manager sets, its audit.
 Join a site with: sudo idn-industrial enrol IDNI1-...
C
cat > $P/DEBIAN/postinst <<'S'
#!/bin/sh
set -e
if [ "$1" = configure ]; then
  # Its own user (E7-11): the service runs as idn-industrial, not root.
  # (A group of that name left behind, e.g. by an admin: joined, not made again; F3-11.)
  if ! getent passwd idn-industrial >/dev/null; then
    if getent group idn-industrial >/dev/null; then grp="-g idn-industrial"; else grp=--user-group; fi
    useradd --system $grp --home-dir /var/lib/idn-industrial --no-create-home \
      --shell /usr/sbin/nologin idn-industrial
  fi
  # Root's actions file lives here (the service can read it, not change it).
  install -d -m 755 -o root -g root /etc/idn-industrial
  # Actions kept in the settings files from before (run as root then): no longer used by
  # the service; said once, here and in its log.
  if [ ! -e /etc/idn-industrial/actions.toml ] \
     && grep -qs '^\[\[action\]\]' /var/lib/idn-industrial/node.toml /var/lib/idn-industrial/modules/*.toml; then
    echo "IDN Node | Industrial now runs as its own user. Its actions ([[action]] in node.toml"
    echo "or modules/) are no longer used: move them to /etc/idn-industrial/actions.toml"
    echo "(root's: sudo nano /etc/idn-industrial/actions.toml), then: sudo systemctl restart idn-industrial"
  fi
fi
if [ "$1" = configure ] && [ -d /run/systemd/system ]; then
  systemctl daemon-reload
  systemctl enable idn-industrial.service
  if [ -n "$2" ]; then systemctl restart idn-industrial.service; else systemctl start idn-industrial.service; fi
  [ -n "$2" ] || echo "IDN Node | Industrial runs. Join the site: sudo idn-industrial enrol IDNI1-..."
fi
S
cat > $P/DEBIAN/prerm <<'S'
#!/bin/sh
set -e
if [ "$1" = remove ] && [ -d /run/systemd/system ]; then
  systemctl disable --now idn-industrial.service || true
fi
S
cat > $P/DEBIAN/postrm <<'S'
#!/bin/sh
set -e
[ -d /run/systemd/system ] && systemctl daemon-reload || true
if [ "$1" = purge ]; then
  rm -rf /var/lib/idn-industrial /etc/idn-industrial
  userdel idn-industrial 2>/dev/null || true
fi
S
chmod 755 $P/DEBIAN/postinst $P/DEBIAN/prerm $P/DEBIAN/postrm
dpkg-deb --root-owner-group -Zxz --build $P "$OUT/idn-industrial_${VERSION}_amd64.deb"
fi

# ---- idn-site-manager-industrial
if [ "$PRODUCT" != media ]; then
P=$STAGE/idn-site-manager-industrial
D=$P/usr/share/doc/idn-site-manager-industrial
mkdir -p $P/usr/bin $P/lib/systemd/system $P/etc $P/DEBIAN $D
install -m 755 "$BUILT/idn-site-manager-industrial" $P/usr/bin/idn-site-manager-industrial
strip --strip-unneeded $P/usr/bin/idn-site-manager-industrial
sed -e '/^#/d' "$IMPL/installer/linux/idn-site-manager-industrial.service" \
  > $P/lib/systemd/system/idn-site-manager-industrial.service
install -m 640 "$IMPL/installer/linux/idn-site-manager-industrial.env" $P/etc/idn-site-manager-industrial.env
echo /etc/idn-site-manager-industrial.env > $P/DEBIAN/conffiles
{ echo "IDN Site Manager | Industrial (IDN Open Protocol), (c) ADC Industries: MIT OR Apache-2.0."
  echo "The site clock: IDN's modified copy of statime 0.4.0, (c) 2022-2023 Tweede Golf and Contributors: MIT OR Apache-2.0."
  echo; cat "$IMPL/LICENSE-MIT"; echo; cat "$IMPL/LICENSE-APACHE"
  echo; echo "======================================== Third-party code built in (NOTICE)"; cat "$IMPL/NOTICE"; } > $D/copyright
cat > $P/DEBIAN/control <<C
Package: idn-site-manager-industrial
Version: $VERSION
Architecture: amd64
Maintainer: ADC Industries <code@adcnetwork.org>
Section: net
Priority: optional
Depends: $(cat "$BUILT/DEPENDS-site-manager-industrial"), systemd
Description: IDN Site Manager | Industrial: a company's machines, their data and commands
 One per company, a service from boot: enrolment, the machines' links, store and
 forward, the MQTT broker for the application server, the web UI on port 8080.
 Settings: /etc/idn-site-manager-industrial.env; data: /var/lib/idn-site-manager-industrial.
C
cat > $P/DEBIAN/postinst <<'S'
#!/bin/sh
set -e
if [ "$1" = configure ] && [ -d /run/systemd/system ]; then
  systemctl daemon-reload
  systemctl enable idn-site-manager-industrial.service
  if [ -n "$2" ]; then systemctl restart idn-site-manager-industrial.service; else systemctl start idn-site-manager-industrial.service; fi
  if [ -z "$2" ]; then
    echo "IDN Site Manager | Industrial runs: its web UI on http://<this machine>:8080/"
    echo "  Set IDN_PUBLIC_URLS in /etc/idn-site-manager-industrial.env (where machines reach it),"
    echo "  then: sudo systemctl restart idn-site-manager-industrial"
    echo "  The first visit to the web UI sets the administrator password with the API key:"
    echo "  sudo cat /var/lib/idn-site-manager-industrial/api.key"
  fi
fi
S
cat > $P/DEBIAN/prerm <<'S'
#!/bin/sh
set -e
if [ "$1" = remove ] && [ -d /run/systemd/system ]; then
  systemctl disable --now idn-site-manager-industrial.service || true
fi
S
cat > $P/DEBIAN/postrm <<'S'
#!/bin/sh
set -e
[ -d /run/systemd/system ] && systemctl daemon-reload || true
# Purged: the site goes too (its CA, machines and data). Back it up first:
#   sudo systemctl stop idn-site-manager-industrial
#   sudo idn-site-manager-industrial backup site.idnsite
if [ "$1" = purge ]; then rm -rf /var/lib/idn-site-manager-industrial /var/lib/private/idn-site-manager-industrial; fi
S
chmod 755 $P/DEBIAN/postinst $P/DEBIAN/prerm $P/DEBIAN/postrm
dpkg-deb --root-owner-group -Zxz --build $P "$OUT/idn-site-manager-industrial_${VERSION}_amd64.deb"
fi

# ---- idn-site-manager (IDN Media's Site Manager on its own: a server, a VPS)
if [ "$PRODUCT" != industrial ]; then
P=$STAGE/idn-site-manager
D=$P/usr/share/doc/idn-site-manager
mkdir -p $P/usr/bin $P/lib/systemd/system $P/etc $P/DEBIAN $D
install -m 755 "$BUILT/idn-site-manager" $P/usr/bin/idn-site-manager
strip --strip-unneeded $P/usr/bin/idn-site-manager
sed -e '/^#/d' "$IMPL/installer/linux/idn-site-manager.service" > $P/lib/systemd/system/idn-site-manager.service
install -m 640 "$IMPL/installer/linux/idn-site-manager.env" $P/etc/idn-site-manager.env
echo /etc/idn-site-manager.env > $P/DEBIAN/conffiles
{ echo "IDN Site Manager (IDN Open Protocol), (c) ADC Industries: MIT OR Apache-2.0."
  echo "The site clock: IDN's modified copy of statime 0.4.0, (c) 2022-2023 Tweede Golf and Contributors: MIT OR Apache-2.0."
  echo; cat "$IMPL/LICENSE-MIT"; echo; cat "$IMPL/LICENSE-APACHE"
  echo; echo "======================================== Third-party code built in (NOTICE)"; cat "$IMPL/NOTICE"; } > $D/copyright
cat > $P/DEBIAN/control <<C
Package: idn-site-manager
Version: $VERSION
Architecture: amd64
Maintainer: ADC Industries <code@adcnetwork.org>
Section: video
Priority: optional
Depends: $(cat "$BUILT/DEPENDS-site-manager"), systemd
Description: IDN Site Manager: an IDN Media site's authority and its API
 One per site, a service from boot, without the rest of IDN Media: the site's
 machines, approvals, routing, settings, backups; its API on https port 29202,
 administered from a host application (no web panel of its own).
 Settings: /etc/idn-site-manager.env; the site: /var/lib/idn-site-manager.
C
cat > $P/DEBIAN/postinst <<'S'
#!/bin/sh
set -e
if [ "$1" = configure ] && [ -d /run/systemd/system ]; then
  systemctl daemon-reload
  systemctl enable idn-site-manager.service
  if [ -n "$2" ]; then systemctl restart idn-site-manager.service; else systemctl start idn-site-manager.service; fi
  if [ -z "$2" ]; then
    echo "IDN Site Manager runs: its API on https://<this machine>:29202/api/v1/ (administer it from a host application)"
    echo "  Set IDN_SITE_PASSWORD in /etc/idn-site-manager.env, then: sudo systemctl restart idn-site-manager"
  fi
fi
S
cat > $P/DEBIAN/prerm <<'S'
#!/bin/sh
set -e
if [ "$1" = remove ] && [ -d /run/systemd/system ]; then
  systemctl disable --now idn-site-manager.service || true
fi
S
cat > $P/DEBIAN/postrm <<'S'
#!/bin/sh
set -e
[ -d /run/systemd/system ] && systemctl daemon-reload || true
# Purged: the site goes too (its CA and devices). Back it up first (through its API).
if [ "$1" = purge ]; then rm -rf /var/lib/idn-site-manager /var/lib/private/idn-site-manager; fi
S
chmod 755 $P/DEBIAN/postinst $P/DEBIAN/prerm $P/DEBIAN/postrm
dpkg-deb --root-owner-group -Zxz --build $P "$OUT/idn-site-manager_${VERSION}_amd64.deb"
fi
