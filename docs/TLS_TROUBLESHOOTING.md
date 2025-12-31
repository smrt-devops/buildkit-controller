# TLS Certificate Troubleshooting

## "bad certificate" Error

If you see repeated `TLS handshake failed` errors with `"remote error: tls: bad certificate"`, this means client certificates are not being validated by the gateway.

### Root Cause

The gateway validates client certificates against the CA certificate stored in `/etc/gateway/tls/ca.crt`. If client certificates are not signed by this CA, the handshake fails.

### Diagnosis

1. **Check the gateway's CA certificate (from the mounted secret):**

   ```bash
   # The gateway mounts the TLS secret, so check the secret directly
   kubectl get secret buildkit-pool-tls -n buildkit-system -o jsonpath='{.data.ca\.crt}' | base64 -d
   ```

   **Note:** If the container is minimal/distroless (no `cat` or `openssl`), check the Kubernetes secret instead of exec'ing into the pod.

2. **Check the controller's CA certificate:**

   ```bash
   kubectl get secret buildkit-ca -n buildkit-system -o jsonpath='{.data.ca\.crt}' | base64 -d
   ```

3. **Compare them:**

   ```bash
   # Get gateway CA (from the TLS secret mounted to gateway)
   kubectl get secret buildkit-pool-tls -n buildkit-system -o jsonpath='{.data.ca\.crt}' | base64 -d > /tmp/gateway-ca.crt

   # Get controller CA
   kubectl get secret buildkit-ca -n buildkit-system -o jsonpath='{.data.ca\.crt}' | base64 -d > /tmp/controller-ca.crt

   # Compare
   diff /tmp/gateway-ca.crt /tmp/controller-ca.crt
   ```

   **Alternative:** Compare the certificate subjects directly (faster):

   ```bash
   # Gateway CA subject
   kubectl get secret buildkit-pool-tls -n buildkit-system -o jsonpath='{.data.ca\.crt}' | base64 -d | openssl x509 -noout -subject

   # Controller CA subject
   kubectl get secret buildkit-ca -n buildkit-system -o jsonpath='{.data.ca\.crt}' | base64 -d | openssl x509 -noout -subject
   ```

   The subjects should match (both should show "CN=BuildKit CA").

   **They should be identical!** If they differ, that's the problem.

4. **Check client certificate CA:**

   ```bash
   # If you have a client certificate, check its issuer
   openssl x509 -in client.crt -noout -issuer

   # Compare with controller CA subject
   kubectl get secret buildkit-ca -n buildkit-system -o jsonpath='{.data.ca\.crt}' | base64 -d | openssl x509 -noout -subject
   ```

### Common Issues

#### Issue 1: CA Mismatch

**Symptom:** Gateway CA doesn't match controller CA

**Cause:**

- CA was regenerated but gateway secret wasn't updated
- Gateway secret was manually edited
- Multiple CAs exist

**Solution:**

1. Delete the gateway TLS secret to force regeneration:
   ```bash
   kubectl delete secret buildkit-pool-tls -n buildkit-system
   ```
2. The controller will automatically regenerate it with the correct CA

#### Issue 2: Client Certificate from Wrong CA

**Symptom:** Client certificates were issued by a different CA

**Cause:**

- Client certificates were generated manually
- Client certificates were issued before CA was created/updated
- Multiple controllers using different CAs

**Solution:**

1. Get new client certificates from the controller API:

   ```bash
   # Using bkctl
   bkctl cert request --pools buildkit-pool

   # Or via API
   curl -X POST https://controller-api/api/v1/cert \
     -H "Authorization: Bearer $TOKEN" \
     -d '{"pools": ["buildkit-pool"]}'
   ```

#### Issue 3: Expired Certificates

**Symptom:** Certificates expired

**Check:**

```bash
# Check gateway server cert (from secret, since container may not have openssl)
kubectl get secret buildkit-pool-tls -n buildkit-system -o jsonpath='{.data.tls\.crt}' | base64 -d | openssl x509 -noout -dates

# Check controller CA
kubectl get secret buildkit-ca -n buildkit-system -o jsonpath='{.data.ca\.crt}' | base64 -d | openssl x509 -noout -dates
```

**Solution:**

- Server certificates are auto-rotated by the controller
- If CA expired, you need to regenerate it (this will invalidate all existing certificates)

### Verification

1. **Verify gateway can validate client certs:**

   ```bash
   # Get a valid client certificate
   bkctl cert request --pools buildkit-pool

   # Test connection
   buildctl --addr tcp://buildkit-pool.buildkit-system.svc:1235 \
     --tlscacert ca.crt \
     --tlscert client.crt \
     --tlskey client.key \
     debug workers
   ```

2. **Check gateway logs for successful connections:**
   ```bash
   kubectl logs -n buildkit-system deployment/buildkit-pool-gateway | grep -i "handshake\|connection"
   ```

### Prevention

1. **Never manually edit TLS secrets** - let the controller manage them
2. **Use the controller API to get client certificates** - don't generate them manually
3. **Monitor certificate expiration** - the controller auto-rotates, but monitor for issues
4. **Use a single CA** - ensure all components use the same CA from `buildkit-ca` secret

### Quick Fix

If certificates are mismatched, the quickest fix is to force regeneration:

```bash
# Delete gateway TLS secret (will be regenerated with correct CA)
kubectl delete secret buildkit-pool-tls -n buildkit-system

# Wait for controller to regenerate (check logs)
kubectl logs -n buildkit-system deployment/buildkit-controller -f

# Verify new secret has correct CA
kubectl get secret buildkit-pool-tls -n buildkit-system -o jsonpath='{.data.ca\.crt}' | base64 -d | openssl x509 -noout -subject
```

The gateway pod will automatically pick up the new secret when it's regenerated.

## Gateway Requires Allocation Token in Certificate

**Important:** When connecting directly to a gateway endpoint (using `BKCTL_GATEWAY_ENDPOINT`), you **must** use a client certificate that includes an allocation token in the Common Name (CN).

### The Problem

The gateway validates client certificates and extracts the allocation token from the certificate's CN. The CN must be in the format `alloc:<token>`.

**Regular certificates from `/api/v1/cert` have CN=your-identity** - these will NOT work with the gateway!

### Solution: Allocate a Worker First

You must allocate a worker to get a certificate with an allocation token:

```bash
# Allocate a worker (this issues a cert with allocation token)
bkctl allocate --pool buildkit-pool --namespace buildkit-system

# This will:
# 1. Call /api/v1/worker/allocate
# 2. Get a certificate with CN="alloc:<token>"
# 3. Save it to ~/.config/bkctl/certs/
# 4. Use that certificate automatically
```

### Verify Your Certificate Has Allocation Token

```bash
# Check if your client cert has allocation token in CN
openssl x509 -in ~/.config/bkctl/certs/client.crt -noout -subject

# Should show: CN = alloc:<some-token>
# If it shows something else (like your identity), it won't work!
```

### Manual Connection

If you want to connect manually:

1. **Allocate a worker via API:**

   ```bash
   curl -X POST http://buildkit-controller-api.buildkit-system.svc:8082/api/v1/worker/allocate \
     -H "Authorization: Bearer $TOKEN" \
     -d '{"poolName": "buildkit-pool", "namespace": "buildkit-system"}'
   ```

2. **Save the certificates from the response:**

   ```bash
   # Response includes:
   # - caCert (base64)
   # - clientCert (base64) - has CN="alloc:<token>"
   # - clientKey (base64)

   echo $CLIENT_CERT | base64 -d > client.crt
   echo $CLIENT_KEY | base64 -d > client.key
   echo $CA_CERT | base64 -d > ca.crt
   ```

3. **Use those certificates with buildctl:**
   ```bash
   buildctl --addr tcp://10.43.112.50:1235 \
     --tlscacert ca.crt \
     --tlscert client.crt \
     --tlskey client.key \
     debug workers
   ```

### Why This Happens

- **Gateway connection:** Requires allocation token in certificate CN (`alloc:<token>`)
- **Regular cert endpoint (`/api/v1/cert`):** Issues certificates with CN=your-identity (no allocation token)
- **Worker allocation endpoint (`/api/v1/worker/allocate`):** Issues certificates with CN=`alloc:<token>` (has allocation token)

**You cannot use certificates from `/api/v1/cert` to connect directly to the gateway!**
