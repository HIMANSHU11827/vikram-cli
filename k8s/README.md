# VIKRAM on Kubernetes

One namespace, one writable volume, one process. VIKRAM is a terminal runtime, so the pod runs the
daemon (`vikram daemon start --foreground`), never the TUI: the TUI refuses to start without a real
terminal (`crates/vikram-cli/src/terminal.rs:66-69`) and a cluster gives it a pipe.

## What is here

| File | Purpose |
|---|---|
| `namespace.yaml` | `vikram` namespace, Pod Security Admission `restricted` |
| `pvc.yaml` | 5Gi `ReadWriteOnce` volume for `vikram.db`, `config.json`, `.env` |
| `configmap.yaml` | `config.json`, handed to the pod once and copied onto the volume |
| `secret.yaml` | provider API key — **placeholder, read the header before applying** |
| `deployment.yaml` | the daemon: 1 replica, Recreate, non-root, read-only root filesystem |
| `service.yaml` | ClusterIP for the webhook gateway — inert unless you enable the gateway |
| `kustomization.yaml` | ties them together for `kubectl apply -k` |

## 1. Build and push the image

The pod runs `vikram:latest`, so either push to a registry your nodes can pull from, or load the
image into them directly (kind/minikube/containerd hosts).

```bash
# From the repository root (where the Dockerfile lives)
docker build -t vikram:latest .

# Registry route: tag with something immutable. `latest` plus IfNotPresent means nodes that already
# have a stale `latest` keep running it, which makes rollouts look like they did nothing.
REGISTRY=registry.example.com/vikram
docker tag vikram:latest "$REGISTRY:0.1.0"
docker push "$REGISTRY:0.1.0"

# The image runs as uid 1000 and sets VIKRAM_HOME=/data/vikram, which is exactly what the manifests
# assume. No changes to the Dockerfile are needed for any of this.

# Direct-to-node route instead of a registry:
#   kind load docker-image vikram:latest --name <cluster>
#   minikube image load vikram:latest
```

If you pushed to a registry, point the manifests at it — either edit `image:` in
`deployment.yaml`, or uncomment the `images:` block in `kustomization.yaml`. If you loaded the image
straight onto the nodes, keep `imagePullPolicy: IfNotPresent` or the pull will fail.

## 2. Set the API key

Do not apply `secret.yaml` as-is. Pick one of the options in its header, then apply:

```bash
# Option: out-of-band, nothing in git
kubectl create secret generic vikram-secrets \
  --namespace vikram \
  --from-literal=OPENAI_API_KEY='sk-...'

# Option: Sealed Secrets (commit the sealed output, delete secret.yaml from kustomization.yaml)
kubectl apply -f k8s/secret.sealed.yaml
```

`--from-literal` on an existing Secret needs `--dry-run=client -o yaml | kubectl apply -f -`, since
`create` refuses to overwrite.

## 3. Apply

```bash
kubectl apply -k k8s/

# Without kustomize (the files carry explicit namespaces for this reason):
kubectl apply -f k8s/namespace.yaml -f k8s/pvc.yaml -f k8s/configmap.yaml \
                -f k8s/secret.yaml -f k8s/deployment.yaml -f k8s/service.yaml
```

Note there is no `kustomize` binary requirement: `kubectl apply -k` has it built in, and
`kubectl kustomize k8s/` renders the output without applying.

## 4. Verify it is actually running

```bash
kubectl -n vikram rollout status deploy/vikram --timeout=120s
kubectl -n vikram get pods -l app.kubernetes.io/name=vikram

# The pod is Ready when `vikram --version` succeeds. That is what the probes check.
kubectl -n vikram exec deploy/vikram -- vikram --version

# What the daemon thinks of itself: mode, home, jobs, heartbeat.
kubectl -n vikram exec deploy/vikram -- vikram daemon status
kubectl -n vikram exec deploy/vikram -- vikram daemon health

# Confirm config.json landed on the volume and the runtime can see it.
kubectl -n vikram exec deploy/vikram -- cat /data/vikram/config.json

# Logs. First boot prints the seed line, then "daemon running in the foreground".
kubectl -n vikram logs deploy/vikram -f
```

A pod that is Running but whose model calls fail with 401 means the placeholder key is still in
place — the probes pass either way, by design. `vikram doctor` reports which key source it found:

```bash
kubectl -n vikram exec deploy/vikram -- vikram doctor
```

## 5. Enable the webhook gateway

Two separate conditions, and the Service is inert until both hold.

**a. A webhook platform must have credentials.** The gateway reads them from the environment and
only registers a route when a platform is configured. For Feishu, for example:
`FEISHU_APP_ID`, `FEISHU_APP_SECRET`, and `FEISHU_VERIFICATION_TOKEN` as needed. Without these,
`GATEWAY_WEBHOOK_BIND` is set but no route exists, and the gateway logs
`GATEWAY_WEBHOOK_BIND is set but no webhook platform is configured` while binding nothing
(`crates/vikram-cli/src/gateway.rs:1105-1119`).

**b. The workload must run `vikram gateway`, not the daemon.** It is a different subcommand of the
same binary. Two ways:

*Replace the command* (you lose the job scheduler while the gateway runs):

```yaml
# deployment.yaml
command: ["/usr/bin/tini", "--", "vikram", "gateway"]
env:
  - name: GATEWAY_WEBHOOK_BIND
    value: "0.0.0.0:8080"
```

*Or run a second Deployment* for the gateway, both mounting `vikram-data`. This is the shape the
manifests expect, since `service.yaml` selects `app.kubernetes.io/component: daemon`.

Then expose it. Keep it inside the cluster until you have a TLS-terminating Ingress in front of it:

```bash
# Confirm the port is actually open before trusting the Service
kubectl -n vikram exec deploy/vikram -- \
  sh -c 'command -v ss >/dev/null && ss -ltn || cat /proc/net/tcp'

kubectl -n vikram port-forward svc/vikram 8080:8080
curl -i -X POST http://localhost:8080/feishu -H 'content-type: application/json' -d '{}'
```

Do not add an HTTP probe for the gateway either: the endpoint paths (`/feishu`, `/sms`, ...) answer
webhook semantics, and the bind can be up while every platform is unconfigured.

## 6. Change the config

The init container copies `config.json` onto the PVC **only if it is not already there**. That is
deliberate: the runtime rewrites this file when a mode or model changes (`Config::save()` in
`crates/vikram-runtime/src/runtime.rs`), and blindly re-seeding would undo the operator's choice on
every restart.

So editing `configmap.yaml` and re-applying changes nothing on a running pod. To roll a config
change out:

```bash
kubectl -n vikram exec deploy/vikram -- rm -f /data/vikram/config.json
kubectl -n vikram rollout restart deploy/vikram
kubectl -n vikram rollout status deploy/vikram
```

The pod comes back, the init container seeds the new file, and runtime-made edits since the last
restart are gone. If you would rather keep them, edit the live copy instead:

```bash
kubectl -n vikram exec -it deploy/vikram -- vi /data/vikram/config.json
kubectl -n vikram rollout restart deploy/vikram   # restart so it is re-read
```

## 7. Remove

```bash
kubectl delete -k k8s/
kubectl -n vikram delete pvc vikram-data   # only if you also want the state gone
```

`kubectl delete -k` leaves the PVC behind (it is not garbage-collected with the Deployment), which
is the right default and also why the database survives a mistaken `delete`.

## Design notes

**One replica, always.** `vikram.db` is SQLite and the daemon claims `$VIKRAM_HOME/daemon/daemon.lock`
for its entire life (`crates/vikram-daemon/src/daemon.rs:87-110`). A second replica is a second
writer on one file. The lock does self-heal — a lock whose process is gone is taken over, so a
SIGKILL'd pod does not wedge the next one — but that is crash recovery, not concurrency.

**Recreate, not RollingUpdate.** The volume is `ReadWriteOnce`. A rolling update starts the new pod
while the old one still holds the volume, and the new pod sits in `ContainerCreating` until the
rollout times out. `Recreate` stops the writer first, which SQLite also wants.

**No StatefulSet.** A StatefulSet would buy stable pod names, ordered rollouts, and per-replica
PVCs. All three are irrelevant at one replica with a pre-existing PVC and no listening socket: the
pod name is recorded in the lock body only as a diagnostic, nothing dials the pod by name, and there
is no second replica to order. It would not make the single-writer guarantee any stronger than
`replicas: 1` plus `Recreate` already does. Deployment is the simpler correct answer; revisit only
if VIKRAM grows a sharded multi-instance mode.

**Read-only root filesystem.** `readOnlyRootFilesystem: true` with an `emptyDir` at `/tmp` and the
volume at `/data/vikram`. `HOME` is also pointed at the volume, because the image's `/home/vikram`
is unwritable under this setting and anything resolving `~` would otherwise fail with `EROFS`.

**The config is on the volume, not in the ConfigMap mount.** A ConfigMap mounted via `subPath` is
read-only, and `Config::save()` would fail on every mode or model change. Hence the init container.

## Known limitations

* **`targetPort` is numeric.** Named ports only resolve when the container declares a matching
  `ports:` entry, and the daemon declares none (it opens no socket). Switch to `targetPort: http`
  only after uncommenting the port block in `deployment.yaml`.
* **The OS sandbox is inactive.** `security.sandbox` defaults to enabled, but the image ships no
  `bwrap`, so it degrades to pass-through (`crates/vikram-sandbox/src/lib.rs:84-93`). The container
  boundary is the real sandbox here. Installing bubblewrap and granting user namespaces would
  require changing the image, which is out of scope for `k8s/`.
* **Probes prove the process, not the provider.** `vikram --version` cannot tell you the model
  endpoint is reachable. A provider outage shows up as failed turns in the logs, not as a restart —
  which is the correct trade, but it does mean a green pod is not proof of working inference.
* **No `NetworkPolicy` here.** The daemon needs outbound HTTPS to whatever provider you configure,
  so a default-deny egress policy would need an allow-list per provider. Add one if the namespace is
  shared with untrusted workloads.
