#!/bin/sh
set -eu

if [ "$(id -u)" -ne 0 ]; then
  echo "run this installer as root" >&2
  exit 1
fi

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_root=$(CDPATH= cd -- "$script_dir/../.." && pwd)
unit_dir="$script_dir/systemd"
libexec_dir=/usr/local/libexec/ramjet-route-journal

install -d -m 0755 "$libexec_dir"
install -m 0755 "$repo_root/bench/route_journal_archive.py" "$libexec_dir/route_journal_archive.py"
install -m 0644 "$repo_root/bench/route_replay.py" "$libexec_dir/route_replay.py"
install -m 0644 "$repo_root/bench/serving_cost_audit.py" "$libexec_dir/serving_cost_audit.py"
install -m 0644 "$unit_dir/ramjet-route-journal-collect.service" /etc/systemd/system/
install -m 0644 "$unit_dir/ramjet-route-journal-collect.timer" /etc/systemd/system/
install -m 0644 "$unit_dir/ramjet-route-journal-maintain.service" /etc/systemd/system/
install -m 0644 "$unit_dir/ramjet-route-journal-maintain.timer" /etc/systemd/system/

systemd-analyze verify \
  /etc/systemd/system/ramjet-route-journal-collect.service \
  /etc/systemd/system/ramjet-route-journal-collect.timer \
  /etc/systemd/system/ramjet-route-journal-maintain.service \
  /etc/systemd/system/ramjet-route-journal-maintain.timer
systemctl daemon-reload
systemctl enable --now ramjet-route-journal-collect.timer ramjet-route-journal-maintain.timer
systemctl start ramjet-route-journal-collect.service
