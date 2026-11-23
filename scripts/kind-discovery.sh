#!/usr/bin/env bash
# Prepares the current kubectl cluster (CI's kind cluster) for
# crates/edgefront-proxy/tests/discovery_kind.rs: a namespace, a service
# account allowed only to list and watch EndpointSlices in it (all the
# Kubernetes source needs), and its token and the cluster's CA in files
# under DIR. Prints the tests' environment as NAME=value lines, for
# $GITHUB_ENV or `export`.
#
#   scripts/kind-discovery.sh DIR [NAMESPACE]
set -euo pipefail

dir=${1:?usage: scripts/kind-discovery.sh DIR [NAMESPACE]}
ns=${2:-edgefront-discovery}
mkdir -p "$dir"
dir=$(cd "$dir" && pwd)

kubectl create namespace "$ns" --dry-run=client -o yaml | kubectl apply -f - >&2
kubectl -n "$ns" create serviceaccount edgefront-discovery --dry-run=client -o yaml | kubectl apply -f - >&2
kubectl -n "$ns" create role endpointslice-reader \
  --verb=list,watch --resource=endpointslices.discovery.k8s.io \
  --dry-run=client -o yaml | kubectl apply -f - >&2
kubectl -n "$ns" create rolebinding edgefront-discovery \
  --role=endpointslice-reader --serviceaccount="$ns:edgefront-discovery" \
  --dry-run=client -o yaml | kubectl apply -f - >&2

# The account can do what the source needs and nothing more.
as="system:serviceaccount:$ns:edgefront-discovery"
for verb in list watch; do
  test "$(kubectl auth can-i "$verb" endpointslices.discovery.k8s.io -n "$ns" --as "$as")" = yes
done
for verb in get create update delete; do
  test "$(kubectl auth can-i "$verb" endpointslices.discovery.k8s.io -n "$ns" --as "$as" || true)" = no
done
test "$(kubectl auth can-i list pods -n "$ns" --as "$as" || true)" = no

kubectl -n "$ns" create token edgefront-discovery --duration=1h > "$dir/token"
kubectl config view --raw --minify \
  -o jsonpath='{.clusters[0].cluster.certificate-authority-data}' | base64 -d > "$dir/ca.crt"
server=$(kubectl config view --raw --minify -o jsonpath='{.clusters[0].cluster.server}')

# The source's own list request, made with the token and CA alone.
curl -fsS --cacert "$dir/ca.crt" -H "Authorization: Bearer $(cat "$dir/token")" \
  -H "Accept: application/json" \
  "$server/apis/discovery.k8s.io/v1/namespaces/$ns/endpointslices?labelSelector=kubernetes.io%2Fservice-name%3Dweb&limit=500" \
  | jq -e '.kind == "EndpointSliceList"' >/dev/null

echo "EDGEFRONT_KIND_API=$server"
echo "EDGEFRONT_KIND_CA=$dir/ca.crt"
echo "EDGEFRONT_KIND_TOKEN=$dir/token"
echo "EDGEFRONT_KIND_NAMESPACE=$ns"
