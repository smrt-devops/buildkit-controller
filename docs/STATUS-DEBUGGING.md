# Status Update Debugging

If the BuildKitPool status (workers, connections, ready, etc.) is not updating, use this guide to debug.

## Quick Checks

### 1. Check Current Status

```bash
kubectl get buildkitpool <pool-name> -n <namespace> -o yaml | grep -A 20 "status:"
```

### 2. Check if Workers Exist

```bash
kubectl get buildkitworker -n <namespace> -l "buildkit.smrt-devops.net/pool=<pool-name>"
```

### 3. Check Controller Logs

```bash
kubectl logs -n buildkit-system -l control-plane=buildkit-controller --tail=100 | grep -i "status\|worker\|error"
```

## Common Issues

### Issue: Status Shows Empty Objects

**Symptom:**

```yaml
status:
  workers: {}
  connections: {}
```

**Cause:** Status update may have failed or workers don't exist yet.

**Fix:**

1. Ensure workers exist: `kubectl get buildkitworker -n <namespace>`
2. Check controller logs for errors
3. Manually trigger reconciliation: `kubectl annotate buildkitpool <pool-name> -n <namespace> force-reconcile=$(date +%s)`

### Issue: Workers Not Being Counted

**Symptom:** Workers exist but status shows `total: 0`

**Cause:** Workers may not have the correct label or namespace.

**Fix:**

1. Verify worker labels:

   ```bash
   kubectl get buildkitworker <worker-name> -n <namespace> -o yaml | grep labels
   ```

   Should have: `buildkit.smrt-devops.net/pool: <pool-name>`

2. Verify worker namespace matches pool namespace (or pool reference is correct)

3. Check RBAC permissions:
   ```bash
   kubectl get clusterrole buildkit-controller-manager-role -o yaml | grep buildkitworkers
   ```

### Issue: Status Updates Slowly

**Symptom:** Status updates but takes a long time

**Cause:** Reconciliation interval may be too long.

**Fix:**

- Status now updates every 30 seconds by default
- Pool reconciles when workers change (via watch)
- If still slow, check controller resource limits

## Manual Status Update Test

Force a reconciliation to test status updates:

```bash
# Add annotation to trigger reconciliation
kubectl annotate buildkitpool minimal-pool -n buildkit-system \
  buildkit.smrt-devops.net/force-reconcile=$(date +%s) \
  --overwrite

# Wait a few seconds
sleep 5

# Check status
kubectl get buildkitpool minimal-pool -n buildkit-system -o yaml | grep -A 10 "status:"
```

## Verify Status Update is Working

1. **Create a worker:**

   ```bash
   bkctl allocate --pool minimal-pool
   ```

2. **Check status immediately:**

   ```bash
   kubectl get buildkitpool minimal-pool -n buildkit-system -o jsonpath='{.status.workers}'
   ```

3. **Wait 30 seconds and check again** (status should update)

4. **Check controller logs:**
   ```bash
   kubectl logs -n buildkit-system -l control-plane=buildkit-controller --tail=20 | grep "Updated worker status"
   ```

## Expected Status Structure

```yaml
status:
  workers:
    total: 5
    ready: 4
    idle: 3
    allocated: 1
    provisioning: 1
    failed: 0
  connections:
    active: 1
    total: 0
    lastConnectionTime: "2025-12-19T00:00:00Z"
  gateway:
    ready: true
    replicas: 1
    readyReplicas: 1
  phase: Running
```

## Debugging Steps

1. **Check if pool controller is watching workers:**

   - Controller should log when workers change
   - Check: `kubectl logs -n buildkit-system -l control-plane=buildkit-controller | grep "worker"`

2. **Verify RBAC:**

   ```bash
   kubectl auth can-i list buildkitworkers --as=system:serviceaccount:buildkit-system:buildkit-controller -n buildkit-system
   ```

3. **Check for status update errors:**

   ```bash
   kubectl logs -n buildkit-system -l control-plane=buildkit-controller | grep "Failed to update status"
   ```

4. **Verify worker phases:**
   ```bash
   kubectl get buildkitworker -n buildkit-system -o jsonpath='{range .items[*]}{.metadata.name}{"\t"}{.status.phase}{"\n"}{end}'
   ```

## Force Status Refresh

If status is stuck, you can:

1. **Restart controller:**

   ```bash
   kubectl rollout restart deployment/buildkit-controller -n buildkit-system
   ```

2. **Delete and recreate pool** (last resort)

3. **Check for resource conflicts:**
   ```bash
   kubectl get buildkitpool <pool-name> -n <namespace> -o yaml | grep resourceVersion
   ```

## Status Update Frequency

- **Default:** Every 30 seconds (`StatusUpdateInterval`)
- **With workers:** Every 30 seconds (more frequent updates)
- **On worker change:** Immediately (via watch)
- **On pool change:** Immediately

The controller now watches `BuildKitWorker` resources and reconciles the pool whenever workers are created, updated, or deleted.
