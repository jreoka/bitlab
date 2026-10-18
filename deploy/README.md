# Deploying Bitlab on Kubernetes

`bitlab.yaml` contains everything: namespace, 20Gi cache volume, Deployment,
Service and an nginx Ingress for `stream.dill.moe`.

The container image is only used as a shell — the Bitlab binary itself is
fetched from the pinned GitHub release in an initContainer and verified against
its SHA-256, so the cluster needs no registry credentials.

```bash
# 1. admin password (kept out of the repo)
kubectl -n bitlab create secret generic bitlab-secrets \
  --from-literal=admin-password="$(openssl rand -base64 18)"

# 2. apply
kubectl apply -f deploy/bitlab.yaml

# 3. point DNS at the node, then wait for the certificate
kubectl -n bitlab get certificate bitlab-tls -w
```

Assumptions to check on a fresh cluster:

- a default `ssd` StorageClass (`kubectl get sc`) — otherwise set
  `spec.storageClassName` on the PVC. Rackspace Spot rejects volumes outside
  5–20 GiB, which is why the cache budget defaults to 16 GiB;
- an nginx IngressClass plus cert-manager with a `letsencrypt-prod`
  ClusterIssuer. Without cert-manager, drop the `tls:` block from the Ingress
  and terminate TLS at your proxy.

The Ingress annotations disable proxy buffering and raise the read/send
timeouts: without them nginx buffers the video and players stall.

The pod runs `hostNetwork: true` on a fixed torrent port (51413) so it can accept
inbound peer connections — on small swarms that is most of the difference between
streaming and buffering. Single replica only: two pods cannot share the volume or
the port.
