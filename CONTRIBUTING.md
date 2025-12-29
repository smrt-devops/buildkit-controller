# Contributing to BuildKit Controller

Thank you for your interest in contributing to the BuildKit Controller! This document provides guidelines and instructions for contributing.

## Code of Conduct

This project adheres to a Code of Conduct that all contributors are expected to follow. Please read [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md) before participating.

## How to Contribute

### Reporting Bugs

Before creating a bug report, please check the [existing issues](https://github.com/smrt-devops/buildkit-controller/issues) to see if the problem has already been reported.

When creating a bug report, please include:

- A clear, descriptive title
- Steps to reproduce the issue
- Expected behavior
- Actual behavior
- Environment details (Kubernetes version, controller version, etc.)
- Relevant logs or error messages
- Any additional context

Use the [bug report template](.github/ISSUE_TEMPLATE/bug_report.md) when creating an issue.

### Suggesting Features

We welcome feature suggestions! Please use the [feature request template](.github/ISSUE_TEMPLATE/feature_request.md) and include:

- A clear description of the feature
- Use case and motivation
- Proposed implementation (if you have ideas)
- Any alternatives considered

### Pull Requests

1. **Fork the repository** and create a branch from `main`

   ```bash
   git checkout -b feature/your-feature-name
   ```

2. **Make your changes**

   - Follow the coding standards (see below)
   - Add tests for new functionality
   - Update documentation as needed
   - Ensure all tests pass

3. **Commit your changes**

   - Write clear, descriptive commit messages
   - Follow the [Conventional Commits](https://www.conventionalcommits.org/) format when possible
   - Example: `feat: add support for custom cache backends`

4. **Push to your fork** and open a Pull Request

   - Fill out the PR template
   - Link any related issues
   - Request review from maintainers

5. **Address feedback**
   - Respond to review comments
   - Make requested changes
   - Keep the PR up to date with `main`

## Development Setup

### Prerequisites

- Go 1.25 or later
- kubectl configured to access a Kubernetes cluster
- Docker (for building images)
- Helm 3.0+ (for testing Helm charts)
- Make

### Getting Started

1. Clone the repository:

   ```bash
   git clone https://github.com/smrt-devops/buildkit-controller.git
   cd buildkit-controller
   ```

2. Install dependencies:

   ```bash
   go mod download
   ```

3. Install development tools:

   ```bash
   make controller-gen
   ```

4. Generate code:

   ```bash
   make generate
   make manifests
   ```

5. Run tests:

   ```bash
   make test
   ```

6. Build binaries:
   ```bash
   make build
   ```

## Coding Standards

### Go Code

- Follow [Effective Go](https://go.dev/doc/effective_go) guidelines
- Use `gofmt` for formatting (run `make fmt`)
- Run `go vet` before committing (run `make vet`)
- Keep functions focused and small
- Add comments for exported functions and types
- Write unit tests for new code

### Kubernetes Resources

- Follow Kubernetes naming conventions
- Use meaningful resource names
- Include appropriate labels and annotations
- Document any custom resources

### Commit Messages

We follow [Conventional Commits](https://www.conventionalcommits.org/):

- `feat:` - New feature
- `fix:` - Bug fix
- `docs:` - Documentation changes
- `style:` - Code style changes (formatting, etc.)
- `refactor:` - Code refactoring
- `test:` - Adding or updating tests
- `chore:` - Maintenance tasks

Example:

```
feat: add support for S3 cache backend

Adds configuration options for S3-based cache backends in BuildKitPool
spec. Includes validation and documentation updates.

Fixes #123
```

## Testing

### Unit Tests

Run unit tests:

```bash
make test
```

### Integration Tests

For integration tests, you'll need a Kubernetes cluster:

```bash
# Using kind
kind create cluster
make deploy IMG=ghcr.io/smrt-devops/buildkit-controller/controller:test
# Run integration tests
```

### Helm Chart Testing

Test the Helm chart:

```bash
helm lint ./helm/buildkit-controller
helm template ./helm/buildkit-controller --debug
```

## Documentation

- Update README.md for user-facing changes
- Update relevant docs in `docs/` directory
- Add code comments for complex logic
- Update examples if behavior changes

## Review Process

1. All PRs require at least one maintainer approval
2. CI checks must pass
3. Code coverage should not decrease significantly
4. Documentation must be updated
5. Breaking changes require discussion and approval

## Release Process

Releases are managed by maintainers:

- Version numbers follow [Semantic Versioning](https://semver.org/)
- Releases are tagged with `v<version>` (e.g., `v1.0.0`)
- Release notes are automatically generated from PRs

## Getting Help

- Check the [documentation](docs/README.md)
- Search [existing issues](https://github.com/smrt-devops/buildkit-controller/issues)
- Ask questions in [Discussions](https://github.com/smrt-devops/buildkit-controller/discussions)
- Open an issue for bugs or feature requests

## License

By contributing, you agree that your contributions will be licensed under the Apache License 2.0.

Thank you for contributing to BuildKit Controller! 🎉
