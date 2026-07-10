# agentctl end-to-end + scale/resource benchmark harness.
#
# Hybrid harness: bash/Make does cluster bringup + image build/load + helm
# install; the heavy lifting (functional scenarios + benchmark sweeps) lives in
# the `agentctl-e2e` Rust crate, which these targets shell out to.
#
# Every target honours two env knobs so the IDENTICAL suite runs against a real
# (multi-node) cluster, not just the bundled kind:
#   KUBECONFIG     standard kube client config (exported to the cargo bins too)
#   SKIP_BRINGUP=1 skip `kind create/delete` + cluster add-ons (up/down become
#                  no-ops); point KUBECONFIG at the real cluster and only
#                  images.sh differs (registry push vs `kind load`).
#
# Typical local run:
#   make -C e2e images up install e2e
#   make -C e2e bench report
# Against a real cluster:
#   make -C e2e e2e bench report KUBECONFIG=/path/to/kubeconfig SKIP_BRINGUP=1

SHELL := /bin/bash

# This Makefile lives in <repo>/e2e; the cargo workspace root is its parent.
E2E_DIR   := $(patsubst %/,%,$(dir $(abspath $(lastword $(MAKEFILE_LIST)))))
REPO_ROOT := $(abspath $(E2E_DIR)/..)

# kind cluster name (shared by up.sh / down / images.sh).
CLUSTER ?= agentctl-e2e

# Helm release + namespace (kept in sync with install.sh defaults).
RELEASE   ?= agentctl
NAMESPACE ?= agentctl-system

# Extra values overlays appended to install (space-separated paths or bare
# overlay names resolved against values/). E.g.:
#   make -C e2e install OVERLAYS="values/sec-netpol.yaml"
OVERLAYS ?=

# Export the cluster knobs so up.sh / images.sh / install.sh and the cargo bins
# all observe the same configuration.
export CLUSTER
export RELEASE
export NAMESPACE
export SKIP_BRINGUP
export REGISTRY
ifdef KUBECONFIG
export KUBECONFIG
endif

.PHONY: all up down images install e2e bench report ci clean-disk help

help: ## Show this help.
	@grep -hE '^[a-zA-Z0-9_-]+:.*?## ' $(MAKEFILE_LIST) \
	  | awk 'BEGIN{FS=":.*?## "}{printf "  \033[36m%-12s\033[0m %s\n",$$1,$$2}'

all: images up install e2e ## Build images, bring the cluster up, install, run scenarios.

up: ## Create the kind cluster + add-ons (no-op when SKIP_BRINGUP=1).
	bash $(E2E_DIR)/up.sh

down: ## Delete the kind cluster (no-op when SKIP_BRINGUP=1).
	@if [ -n "$$SKIP_BRINGUP" ]; then \
	  echo "SKIP_BRINGUP set — leaving the cluster alone"; \
	else \
	  kind delete cluster --name "$(CLUSTER)"; \
	fi

images: ## Build/load agentd, mock-agent, and the 6 control-plane images.
	bash $(E2E_DIR)/images.sh

install: ## helm install/upgrade agentctl with values/e2e-base.yaml (+ $(OVERLAYS)).
	bash $(E2E_DIR)/install.sh $(OVERLAYS)

e2e: ## Run the functional scenario suite (nonzero exit on any failure).
	cd $(REPO_ROOT) && cargo run -p agentctl-e2e --bin e2e

bench: ## Run the scale/resource benchmark sweeps into e2e/results/.
	cd $(REPO_ROOT) && cargo run -p agentctl-e2e --bin bench

report: ## Render docs/benchmarks.md from the latest e2e/results/.
	cd $(REPO_ROOT) && cargo run -p agentctl-e2e --bin bench -- --report

ci: images up install e2e ## CI entrypoint: build + bring up + install + scenarios.

clean-disk: ## Reclaim disk after rebuilds (kind disk-full crashes bundled PG).
	-cd $(REPO_ROOT) && cargo clean
	-docker builder prune -f
	-docker image prune -f
	-docker system df
