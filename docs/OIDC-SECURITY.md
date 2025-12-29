# OIDC Security Model

## How OIDC Authentication Works

OIDC (OpenID Connect) provides secure authentication through **cryptographic token signatures**. Here's how it works:

### 1. Token Generation (Client Side)

When you run:

```bash
bkctl build --pool oidc-pool --oidc-actor my-user --oidc-repository my-org/my-repo
```

`bkctl` does **NOT** just pass those flags to the API. Instead:

1. **Contacts the OIDC Issuer** (e.g., GitHub Actions, Google, etc.)
2. **Requests a token** with the specified claims (actor, repository, etc.)
3. **The issuer signs the token** with its **private key**
4. **Returns the signed token** to `bkctl`
5. `bkctl` sends the **signed token** to the API server

### 2. Token Verification (Server Side)

The API server:

1. **Fetches the issuer's public keys** from the issuer's well-known endpoint:

   - GitHub Actions: `https://token.actions.githubusercontent.com/.well-known/openid-configuration`
   - Google: `https://accounts.google.com/.well-known/openid-configuration`
   - Your issuer: `https://your-issuer.com/.well-known/openid-configuration`

2. **Verifies the token signature** using the issuer's public keys

   - If the signature is invalid → **authentication fails**
   - If the signature is valid → **authentication succeeds**

3. **Extracts and validates claims** (actor, repository, pools, etc.)

### 3. Why This Is Secure

**The key security property**: Anyone can pass `--oidc-actor` flags, but they **cannot forge a valid token signature** without the issuer's private key.

#### Example Attack Scenarios

**❌ Attack 1: Forged Token**

```bash
# Attacker tries to use a fake token
export BKCTL_TOKEN="fake-token-12345"
bkctl build --pool oidc-pool
```

**Result**: ❌ **FAILS** - Token signature doesn't match issuer's public keys

**❌ Attack 2: Modified Claims**

```bash
# Attacker tries to modify a valid token's claims
# (e.g., change actor from "alice" to "admin")
```

**Result**: ❌ **FAILS** - Modifying claims invalidates the signature

**✅ Valid Request**

```bash
# User gets a real token from GitHub Actions
bkctl build --pool oidc-pool --oidc-actor alice --oidc-repository my-org/my-repo
```

**Result**: ✅ **SUCCEEDS** - Token is signed by GitHub Actions and verified

## Production Setup

### GitHub Actions Example

For production with GitHub Actions:

1. **Configure the pool** to use GitHub Actions as the issuer:

```yaml
apiVersion: buildkit.smrt-devops.net/v1alpha1
kind: BuildKitPool
metadata:
  name: prod-pool
spec:
  auth:
    methods:
      - type: oidc
        oidc:
          issuer: https://token.actions.githubusercontent.com
          audience: buildkit-controller
          claimsMapping:
            user: "actor" # GitHub Actions actor
            pools: "repository" # GitHub repository name
```

2. **In GitHub Actions workflow**, the token is automatically provided:

```yaml
jobs:
  build:
    runs-on: ubuntu-latest
    permissions:
      id-token: write # Required for OIDC
    steps:
      - uses: actions/checkout@v3
      - name: Build with BuildKit
        run: |
          bkctl build --pool prod-pool \
            --oidc-actor ${{ github.actor }} \
            --oidc-repository ${{ github.repository }}
```

3. **Security**:
   - GitHub Actions **signs** the token with GitHub's private key
   - The API server **verifies** it using GitHub's public keys
   - Only tokens from GitHub Actions are accepted
   - Claims (actor, repository) are cryptographically verified

### Other Production Issuers

**Google Cloud:**

```yaml
oidc:
  issuer: https://accounts.google.com
  audience: your-client-id
```

**Azure AD:**

```yaml
oidc:
  issuer: https://login.microsoftonline.com/{tenant-id}/v2.0
  audience: your-client-id
```

**Custom OIDC Provider:**

```yaml
oidc:
  issuer: https://your-oidc-provider.com
  audience: your-client-id
  clientSecretRef:
    name: oidc-secret
    key: client-secret
```

## Mock OIDC (Testing Only)

The `mock-oidc` server is **only for local testing**. It:

- Uses a **self-signed certificate** (not trusted in production)
- Allows anyone to generate tokens (no real authentication)
- Should **never** be used in production

**For production**, you must use a real OIDC provider (GitHub Actions, Google, etc.).

## Token Flow Diagram

```
┌─────────────┐
│   bkctl     │
│  (Client)   │
└──────┬──────┘
       │ 1. Request token with claims
       │    (actor=alice, repo=my-org/my-repo)
       ▼
┌─────────────────────┐
│  OIDC Issuer        │
│  (GitHub Actions)   │
│                     │
│  2. Sign token      │
│     with private    │
│     key             │
└──────┬──────────────┘
       │ 3. Return signed token
       ▼
┌─────────────┐
│   bkctl     │
└──────┬──────┘
       │ 4. Send signed token
       ▼
┌─────────────────────┐
│  API Server         │
│  (Controller)       │
│                     │
│  5. Fetch issuer's   │
│     public keys      │
│                     │
│  6. Verify signature│
│     ✓ Valid         │
│                     │
│  7. Extract claims  │
│     (actor, repo)   │
└─────────────────────┘
```

## Security Checklist

- [ ] Use a **real OIDC provider** in production (not mock-oidc)
- [ ] Configure **issuer URL** correctly in pool spec
- [ ] Set **audience** to match your client ID
- [ ] Map **claims** correctly (user, pools, etc.)
- [ ] Ensure API server can reach issuer's `.well-known` endpoint
- [ ] Disable **dev mode** in production
- [ ] Use **HTTPS** for issuer endpoints
- [ ] Verify **token expiration** is reasonable
- [ ] Implement **RBAC** based on claims (actor, repository, etc.)

## Common Issues

### Issue: "Token verification failed"

**Causes:**

- Token not signed by configured issuer
- Wrong audience in token
- Token expired
- Issuer's public keys not accessible
- Clock skew between client and server

**Fix:**

- Verify issuer URL matches token issuer
- Check audience matches
- Ensure network connectivity to issuer
- Check system clocks are synchronized

### Issue: "No valid OIDC configuration found"

**Causes:**

- No `BuildKitOIDCConfig` CRD exists
- OIDC config disabled in pool spec
- Issuer URL incorrect

**Fix:**

- Create `BuildKitOIDCConfig` resource
- Enable OIDC in pool spec
- Verify issuer URL is correct

## Best Practices

1. **Use separate OIDC configs per environment** (dev, staging, prod)
2. **Rotate client secrets** regularly
3. **Monitor token verification failures** in logs
4. **Use least-privilege claims** (only grant access to needed pools)
5. **Implement claim-based RBAC** (e.g., only allow specific repositories)
6. **Log all authentication attempts** for audit
