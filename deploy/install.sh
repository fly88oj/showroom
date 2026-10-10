#!/usr/bin/env bash
# Showroom 主机部署脚本（在仓库根目录跑；目标机 dejoy@your-host.example.com）
# 用法: ./deploy/install.sh [release二进制目录，默认 target/release]
set -euo pipefail
REL=${1:-target/release}
HOST=dejoy@your-host.example.com
SSHPASS_PW='REDACTED'
ASK=/tmp/zc_askpass.sh

ask() { printf '#!/bin/sh\necho %s\n' "$SSHPASS_PW" > "$ASK"; chmod +x "$ASK"; }
run() { ask; DISPLAY=x SSH_ASKPASS=$ASK SSH_ASKPASS_REQUIRE=force \
  ssh -o PreferredAuthentications=password -o PubkeyAuthentication=no -o ConnectTimeout=10 "$HOST" "$1"; }
put() { ask; DISPLAY=x SSH_ASKPASS=$ASK SSH_ASKPASS_REQUIRE=force \
  scp -o PreferredAuthentications=password -o PubkeyAuthentication=no "$@"; }

test -x "$REL/showroomctl" || { echo "missing $REL/showroomctl"; exit 1; }
echo "==> 上传二进制与单元"
put "$REL/showroomctl" "$REL/showroom" "$HOST:/tmp/"
for f in deploy/*.service deploy/*.timer; do
  put "$f" "$HOST:/tmp/$(basename "$f")"
done
echo "==> 安装"
run "echo $SSHPASS_PW | sudo -S bash -c '
install -m755 /tmp/showroomctl /usr/local/bin/showroomctl
install -m755 /tmp/showroom /usr/local/bin/showroom
install -m644 /tmp/showroom-*.service /tmp/showroom-*.timer /etc/systemd/system/
systemctl daemon-reload
systemctl enable --now showroom-api
systemctl enable --now showroom-reconcile.timer showroom-autoheal.timer showroom-gc-images.timer
'"
echo "==> 验证"
run 'systemctl is-active showroom-api; curl -s http://127.0.0.1:8090/_api/health; echo'
