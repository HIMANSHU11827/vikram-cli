# Kubernetes

## Apply

```sh
kubectl apply -f k8s/namespace.yaml
kubectl create secret generic vikram-secrets --from-literal=OPENAI_API_KEY=sk-... -n vikram
kubectl apply -f k8s/configmap.yaml -f k8s/pvc.yaml -f k8s/deployment.yaml
```

## Interact

The pod runs the interactive TUI. Attach with:

```sh
kubectl exec -it deploy/vikram -n vikram -- vikram
```

Or run a one-shot command:

```sh
kubectl exec -n vikram deploy/vikram -- vikram chat "hello"
```

## Notes

- State persists on the PVC (`vikram-data`).
- Config comes from the ConfigMap; secrets from the Secret.
- For non-interactive workloads, change the container `command` to
  `["vikram", "chat", "..."]` or a script.
- TUI requires a tty; use `kubectl exec -it`.