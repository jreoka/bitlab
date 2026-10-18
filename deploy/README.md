# Deploying Marlin on Kubernetes

`marlin.yaml` contains everything: namespace, 40Gi cache volume, Deployment,
Service and an nginx Ingress for `stream.dill.moe`.

The container image is only used as a shell — the Marlin binary itself is
fetched from the pinned GitHub release in an initContainer and verified against
its SHA-256, so the cluster needs no registry credentials.

```bash
# 1. admin password (kept out of the repo)
kubectl -n marlin create secret generic marlin-secrets \
  --from-literal=admin-password="$(openssl rand -base64 18)"

# 2. apply
kubectl apply -f deploy/marlin.yaml

# 3. point DNS at the node, then wait for the certificate
kubectl -n marlin get certificate marlin-tls -w
```

Assumptions to check on a fresh cluster:

- a default `ssd` StorageClass (`kubectl get sc`) — otherwise set
  `spec.storageClassName` on the PVC;
- an nginx IngressClass plus cert-manager with a `letsencrypt-prod`
  ClusterIssuer. Without cert-manager, drop the `tls:` block from the Ingress
  and terminate TLS at your proxy.

The Ingress annotations disable proxy buffering and raise the read/send
timeouts: without them nginx buffers the video and players stall.
