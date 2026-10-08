.PHONY: dev check test e2e build run export release

IMAGE_NAME = lnurlcash/lnurlcash-run
CONTAINER_NAME = lnurlcash-run

# Biome, Playwright and noble are pinned in e2e/package.json
BIOME = e2e/node_modules/.bin/biome

# the page and the API on http://127.0.0.1:8200; edits to web/ need a rebuild
# (the page is compiled into the binary)
dev:
	PUBLIC_URL=http://127.0.0.1:8200 cargo run

check: e2e/node_modules
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings
	$(BIOME) ci

test:
	cargo test

e2e/node_modules: e2e/package.json
	npm ci --prefix e2e

# the page in headless Chromium: sign-up, approval, provisioning into git
e2e: e2e/node_modules
	npx --prefix e2e playwright install chromium
	e2e/run.sh

build:
	docker build --pull -t $(IMAGE_NAME) .

ENV_FILE ?= $(wildcard .env)

# The container runs as whoever runs `make run` (--user), as lnurl-mint-rust's
# does: its state is a host directory owned by that same user (DATA, default
# ./data), not a volume, since the image's own user (uid 1000) is someone else.
# The port is published on loopback only: the proxy in front of it terminates
# https. DATA_DIR is pinned to the mount, whatever .env says.
RUN_UID := $(shell id -u)
RUN_GID := $(shell id -g)
DATA ?= $(CURDIR)/data
PORT ?= 127.0.0.1:8200

run:
	@echo "Restarting container..."
	docker pull $(IMAGE_NAME) 2>/dev/null || true
	mkdir -p $(DATA)
	docker stop $(CONTAINER_NAME) 2>/dev/null || true
	docker rm $(CONTAINER_NAME) 2>/dev/null || true
	docker run --restart always -d --name $(CONTAINER_NAME) \
		-p $(PORT):8200 \
		--user $(RUN_UID):$(RUN_GID) \
		$(if $(ENV_FILE),--env-file $(ENV_FILE),) \
		-v $(DATA):/data \
		-e DATA_DIR=/data \
		$(IMAGE_NAME)
	@echo "Container $(CONTAINER_NAME) is running on $(PORT)"

# the beta list as JSON lines, npubs included
export:
	cargo run --quiet -- export

# tags v$(VERSION) and pushes the tag: CI's release workflow stamps that
# version into the build and pushes $(IMAGE_NAME) to Docker Hub
release:
	@test -n "$(VERSION)" || { echo "usage: make release VERSION=x.y.z"; exit 1; }
	git tag "v$(VERSION)" && git push origin "v$(VERSION)"
