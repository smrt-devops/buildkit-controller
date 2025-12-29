# Security Policy

## Supported Versions

We provide security updates for the following versions:

| Version | Supported          |
| ------- | ------------------ |
| 1.x.x   | :white_check_mark: |
| < 1.0   | :x:                |

## Reporting a Vulnerability

We take security vulnerabilities seriously. If you discover a security vulnerability, please follow these steps:

### 1. **Do NOT** open a public issue

Please do not report security vulnerabilities through public GitHub issues.

### 2. Report privately

Email security details to: **security@smrt-devops.net**

Include the following information:

- Type of vulnerability
- Full paths of source file(s) related to the vulnerability
- The location of the affected code (tag/branch/commit or direct URL)
- Step-by-step instructions to reproduce the issue
- Proof-of-concept or exploit code (if possible)
- Impact of the vulnerability, including how an attacker might exploit it

### 3. Response timeline

- **Initial Response**: Within 48 hours
- **Status Update**: Within 7 days
- **Resolution**: Depends on severity and complexity

### 4. Disclosure policy

- We will acknowledge receipt of your report within 48 hours
- We will provide regular updates on the status of the vulnerability
- Once the vulnerability is fixed, we will:
  - Credit you in the security advisory (unless you prefer to remain anonymous)
  - Publish a security advisory with details of the vulnerability and fix
  - Release a patched version

### 5. What to expect

- We will work with you to understand and resolve the issue quickly
- We will keep you informed of our progress
- We will credit you for the discovery (unless you prefer otherwise)

## Security Best Practices

### For Users

- Always use the latest stable version
- Keep your Kubernetes cluster updated
- Use RBAC to restrict controller permissions
- Regularly rotate certificates and secrets
- Monitor controller logs for suspicious activity
- Review and apply security updates promptly

### For Developers

- Follow secure coding practices
- Keep dependencies updated
- Run security scans on container images
- Use least-privilege principles
- Review code changes for security implications
- Test security configurations

## Security Features

The BuildKit Controller includes several security features:

- **mTLS Authentication**: Mutual TLS for secure client-server communication
- **OIDC Integration**: Support for OpenID Connect authentication
- **RBAC Support**: Kubernetes Role-Based Access Control
- **Non-root Containers**: Runs with non-root user by default
- **Distroless Images**: Minimal attack surface with distroless base images
- **Certificate Management**: Automatic certificate generation and rotation

## Known Security Considerations

### Certificate Management

- CA certificates are stored in Kubernetes secrets
- Ensure secrets are properly secured and access is restricted
- Regularly rotate CA certificates in production environments

### Network Security

- The controller exposes API endpoints on port 8082
- Use NetworkPolicies to restrict network access
- Consider using a service mesh for additional security

### Resource Access

- The controller requires cluster-wide permissions to manage BuildKit pools
- Review and restrict RBAC permissions based on your security requirements
- Use service accounts with minimal required permissions

## Security Updates

Security updates are released as:

- **Patch releases** (e.g., 1.0.0 → 1.0.1) for critical security fixes
- **Minor releases** (e.g., 1.0.0 → 1.1.0) for security enhancements

Subscribe to [GitHub releases](https://github.com/smrt-devops/buildkit-controller/releases) to be notified of security updates.

## Additional Resources

- [Kubernetes Security Best Practices](https://kubernetes.io/docs/concepts/security/)
- [OWASP Container Security](https://owasp.org/www-project-container-security/)
- [CNCF Security Best Practices](https://www.cncf.io/blog/2021/08/13/kubernetes-security-best-practices/)

## Contact

For security-related questions or concerns:

- **Email**: security@smrt-devops.net
- **PGP Key**: [Available upon request]

Thank you for helping keep BuildKit Controller secure!
