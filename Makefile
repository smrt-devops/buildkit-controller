# Image URLs
IMG ?= ghcr.io/smrt-devops/buildkit-controller/controller:latest
GATEWAY_IMG ?= ghcr.io/smrt-devops/buildkit-controller/gateway:latest

# Build configuration
PLATFORMS ?= linux/amd64,linux/arm64
CRD_OPTIONS ?= "crd:generateEmbeddedObjectMeta=true"

# Go configuration
ifeq (,$(shell go env GOBIN))
GOBIN=$(shell go env GOPATH)/bin
else
GOBIN=$(shell go env GOBIN)
endif

# Shell configuration
SHELL = /usr/bin/env bash -o pipefail
.SHELLFLAGS = -ec

# Build dependencies
LOCALBIN ?= $(shell pwd)/bin
CONTROLLER_GEN ?= $(LOCALBIN)/controller-gen
CONTROLLER_TOOLS_VERSION ?= v0.19.0

.PHONY: all
all: build

##@ General

.PHONY: help
help: ## Display this help.
	@awk 'BEGIN {FS = ":.*##"; printf "\nUsage:\n  make \033[36m<target>\033[0m\n"} /^[a-zA-Z_0-9-]+:.*?##/ { printf "  \033[36m%-15s\033[0m %s\n", $$1, $$2 } /^##@/ { printf "\n\033[1m%s\033[0m\n", substr($$0, 5) } ' $(MAKEFILE_LIST)

##@ Development

.PHONY: manifests
manifests: controller-gen ## Generate CustomResourceDefinition objects.
	$(CONTROLLER_GEN) crd webhook paths="./..." \
		output:crd:artifacts:config=helm/buildkit-controller/crds
	@echo "CRDs generated to helm/buildkit-controller/crds/"

.PHONY: generate
generate: controller-gen ## Generate DeepCopy methods.
	$(CONTROLLER_GEN) object:headerFile="hack/boilerplate.go.txt" paths="./..."

.PHONY: fmt
fmt: ## Run go fmt.
	go fmt ./...

.PHONY: vet
vet: ## Run go vet.
	go vet ./...

.PHONY: test
test: manifests generate fmt vet ## Run tests.
	go test ./... -coverprofile cover.out

##@ Build

.PHONY: build
build: generate fmt vet ## Build all binaries.
	go build -o bin/manager ./cmd/controller
	go build -o bin/gateway ./cmd/gateway
	go build -o bin/bkctl ./cmd/bkctl

.PHONY: run
run: manifests generate fmt vet ## Run controller from host.
	go run ./cmd/controller

##@ Docker

.PHONY: docker-build
docker-build: test ## Build controller image (multi-arch).
	docker buildx build --platform $(PLATFORMS) -t $(IMG) -f docker/Dockerfile . --load

.PHONY: docker-build-gateway
docker-build-gateway: test ## Build gateway image (multi-arch).
	docker buildx build --platform $(PLATFORMS) -t $(GATEWAY_IMG) -f docker/Dockerfile.gateway . --load

.PHONY: docker-build-all
docker-build-all: docker-build docker-build-gateway ## Build both images (multi-arch).

.PHONY: docker-push
docker-push: test ## Build and push controller image (multi-arch).
	docker buildx build --platform $(PLATFORMS) -t $(IMG) -f docker/Dockerfile . --push

.PHONY: docker-push-gateway
docker-push-gateway: test ## Build and push gateway image (multi-arch).
	docker buildx build --platform $(PLATFORMS) -t $(GATEWAY_IMG) -f docker/Dockerfile.gateway . --push

.PHONY: docker-push-all
docker-push-all: docker-push docker-push-gateway ## Build and push both images (multi-arch).

##@ Deployment

ifndef ignore-not-found
  ignore-not-found = false
endif

.PHONY: install
install: manifests ## Install CRDs.
	kubectl apply -f helm/buildkit-controller/crds

.PHONY: uninstall
uninstall: manifests ## Uninstall CRDs.
	kubectl delete --ignore-not-found=$(ignore-not-found) -f helm/buildkit-controller/crds

.PHONY: deploy
deploy: manifests ## Deploy controller using Helm.
	@if [ -z "$(IMG)" ]; then \
		echo "Error: IMG is not set. Example: make deploy IMG=ghcr.io/smrt-devops/buildkit-controller/controller:v1.0.0"; \
		exit 1; \
	fi
	helm install buildkit-controller ./helm/buildkit-controller \
		--namespace buildkit-system \
		--create-namespace \
		--set image.repository=$(shell echo $(IMG) | cut -d: -f1) \
		--set image.tag=$(shell echo $(IMG) | cut -d: -f2)

.PHONY: undeploy
undeploy: ## Undeploy controller.
	helm uninstall buildkit-controller --namespace buildkit-system || true

##@ Local Development

.PHONY: dev
dev: manifests ## Set up local development environment (kind cluster, GatewayAPI, images, Helm).
	@./scripts/dev-setup.sh

.PHONY: dev-pool
dev-pool: manifests ## Create a default BuildKitPool.
	@./scripts/create-pool.sh minimal-pool buildkit-system

.PHONY: dev-down
dev-down: ## Tear down local development environment.
	@./scripts/dev-teardown.sh

.PHONY: dev-reload-images
dev-reload-images: ## Rebuild and reload images into kind cluster.
	@./scripts/dev-reload-images.sh

.PHONY: dev-status
dev-status: ## Check development environment status.
	@./scripts/test.sh setup

.PHONY: dev-test
dev-test: bin/bkctl ## Run all tests.
	@./scripts/test.sh all

.PHONY: dev-test-buildx
dev-test-buildx: bin/bkctl ## Run Docker Buildx integration test.
	@./scripts/test.sh buildx

.PHONY: dev-test-oidc
dev-test-oidc: bin/bkctl bin/mock-oidc ## Test OIDC authentication flow.
	@./scripts/test.sh oidc

.PHONY: dev-test-cache
dev-test-cache: bin/bkctl ## Test BuildKit cache functionality.
	@./scripts/test.sh cache

.PHONY: dev-mock-oidc
dev-mock-oidc: ## Deploy mock OIDC server for testing.
	@./scripts/utils/deploy-mock-oidc.sh

##@ Binaries

bin/bkctl: ## Build bkctl CLI.
	go build -o bin/bkctl ./cmd/bkctl

bin/mock-oidc: ## Build mock-oidc CLI.
	go build -o bin/mock-oidc ./cmd/mock-oidc

##@ Build Dependencies

$(LOCALBIN):
	mkdir -p $(LOCALBIN)

.PHONY: controller-gen
controller-gen: $(CONTROLLER_GEN) ## Download controller-gen locally.
$(CONTROLLER_GEN): $(LOCALBIN)
	test -s $(LOCALBIN)/controller-gen || GOBIN=$(LOCALBIN) go install sigs.k8s.io/controller-tools/cmd/controller-gen@$(CONTROLLER_TOOLS_VERSION)
