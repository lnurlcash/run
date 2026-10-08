.PHONY: dev check test e2e build export

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
	docker build -t lnurlcash-run .

# the beta list as JSON lines, npubs included
export:
	cargo run --quiet -- export
