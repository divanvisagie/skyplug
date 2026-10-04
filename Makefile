VERSION := $(shell awk -F\" '/^version = / { print $$2; exit }' Cargo.toml)
# The CLI's pinned requirement on skyplug-core; must match VERSION or the
# published skyplug would pull an older core from crates.io.
CORE_REQ := $(shell sed -n 's/^skyplug-core = .*version = "\([^"]*\)".*/\1/p' crates/skyplug/Cargo.toml)

RELEASE_BRANCH := master

.DEFAULT_GOAL := help

.PHONY: help build install test lint docs publish-check publish

help: ## Show this help
	@echo "Usage: make <target>"
	@echo
	@awk 'BEGIN { FS = ":.*## " } /^[a-z-]+:.*## / { printf "  \033[1m%-14s\033[0m %s\n", $$1, $$2 }' $(MAKEFILE_LIST)

build: ## Build the release binary (target/release/skyplug)
	cargo build --release

install: ## Install the skyplug binary into ~/.cargo/bin from this checkout
	cargo install --locked --path crates/skyplug

test: ## Run all workspace tests
	cargo test --workspace

lint: ## Run clippy across the workspace
	cargo clippy --workspace --all-targets

docs: docs/index.html ## Render the man page to docs/index.html (needs mandoc)

docs/index.html: crates/skyplug/man/skyplug.1
	@command -v mandoc >/dev/null || { echo "mandoc not found — install it (e.g. apt install mandoc)"; exit 1; }
	mandoc -T lint -W warning $<
	mandoc -T html -O style=style.css,man=https://man7.org/linux/man-pages/man%S/%N.%S.html $< > $@

publish-check: ## Verify both crates can be published (on master, clean, pushed, versions in sync, dry run passes)
	@test "$(CORE_REQ)" = "$(VERSION)" || { echo "crates/skyplug/Cargo.toml requires skyplug-core $(CORE_REQ), but the workspace version is $(VERSION)"; exit 1; }
	@branch="$$(git rev-parse --abbrev-ref HEAD)"; \
		test "$$branch" = "$(RELEASE_BRANCH)" || { echo "publishing is done from $(RELEASE_BRANCH), but you are on $$branch"; exit 1; }
	@test -z "$$(git status --porcelain)" || { echo "working tree has uncommitted changes"; exit 1; }
	@git fetch --quiet origin "$(RELEASE_BRANCH)"
	@test "$$(git rev-parse HEAD)" = "$$(git rev-parse "origin/$(RELEASE_BRANCH)")" || { echo "HEAD differs from origin/$(RELEASE_BRANCH) — push or pull first"; exit 1; }
	cargo test --workspace
	cargo publish --workspace --dry-run
	@echo "Ready to publish skyplug-core and skyplug $(VERSION)"

publish: publish-check ## Publish skyplug-core then skyplug to crates.io
	cargo publish --workspace
