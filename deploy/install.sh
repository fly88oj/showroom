#!/usr/bin/env bash
# Showroom 主机部署脚本（在仓库根目录跑；目标机 dejoy@your-host.example.com）
# 前置：本机对该主机已配公钥免密（ssh dejoy@your-host.example.com 直接可登）。
# 用法: ./deploy/install.sh [release二进制目录，默认 target/release]
set -euo pipefail
REL=${1:-target/release}
HOST=dejoy@your-host.example.com

test -x "$REL/showroomctl" || { echo "missing $REL/showroomctl"; exit 1; }
echo "==> 上传二进制与单元"
scp "$REL/showroomctl" "$REL/showroom" "$HOST:/tmp/"
for f in deploy/*.service deploy/*.timer; do
  scp "$f" "$HOST:/tmp/$(basename "$f")"
done
echo "==> 安装"
ssh -t "$HOST" 'sudo bash -c "
install -m755 /tmp/showroomctl /usr/local/bin/showroomctl
install -m755 /tmp/showroom /usr/local/bin/showroom
install -m644 /tmp/showroom-*.service /tmp/showroom-*.timer /etc/systemd/system/
systemctl daemon-reload
systemctl enable --now showroom-api
systemctl enable --now showroom-reconcile.timer showroom-autoheal.timer showroom-gc-images.timer
"'
echo "==> 验证"
systemd_ok=$(ssh "$HOST" 'systemctl is-active showroom-api')
# 零信息暴露（SPEC §3）：/_api/* 未签名一律 404 E_AUTH——
# curl 得 404 即证明服务在跑且鉴权生效（200/403 反而说明鉴权没接上）
code=$(ssh "$HOST" "curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:8090/_api/health")
echo "showroom-api: $systemd_ok; /_api/health (unsigned): HTTP $code"
[ "$systemd_ok" = active ] || { echo "verify: showroom-api not active"; exit 1; }
[ "$code" = 404 ] || { echo "verify: expected 404 E_AUTH, got $code"; exit 1; }
