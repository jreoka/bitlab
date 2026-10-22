#!/usr/bin/env bash
# Deploy Bitlab. Use this instead of a bare `kubectl apply`: client-side apply
# merges containerPort lists by containerPort, so the UDP 51413 entry is
# silently dropped from a manifest that also has TCP 51413.
set -euo pipefail
cd "$(dirname "$0")/.."

kubectl apply -f deploy/bitlab.yaml

ports="$(kubectl -n bitlab get deploy bitlab -o jsonpath='{.spec.template.spec.containers[0].ports[*].name}')"
if [[ " $ports " != *" torrent-udp "* ]]; then
  kubectl -n bitlab patch deploy bitlab --type=json \
    -p='[{"op":"add","path":"/spec/template/spec/containers/0/ports/-","value":{"name":"torrent-udp","containerPort":51413,"hostPort":51413,"protocol":"UDP"}}]'
fi

kubectl -n bitlab rollout status deploy/bitlab --timeout=240s
