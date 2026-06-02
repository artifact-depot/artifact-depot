# SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
#
# SPDX-License-Identifier: Apache-2.0

.PHONY: all build test debug release debug_test release_test lint security fmt demo demo-data coverage docker clean notices \
	notices-cargo notices-npm \
	lint-cargo-licenses lint-npm-licenses lint-reuse lint-fmt lint-clippy \
	security-cargo-advisories security-npm-audit \
	test-debug test-ui test-dynamodb test-docker-auth \
	docs docs-serve screenshots observability-screenshots

all: test

# Individual lint checks (parallelizable with make -j)
lint-cargo-licenses:
	@command -v cargo-deny >/dev/null 2>&1 || cargo install cargo-deny
	@echo "Checking Rust dependency licenses..."
	@output=$$(cargo deny check licenses 2>&1) || { echo "$$output"; exit 1; }

lint-npm-licenses:
	@echo "Checking frontend dependency licenses..."
	@cd ui/frontend && npx --yes license-checker --failOn 'GPL-2.0;GPL-3.0;AGPL-3.0;AGPL-1.0;SSPL-1.0;EUPL-1.1;EUPL-1.2' > /dev/null

# Vulnerability checks (parallelizable with make -j). Kept out of `make
# lint` so a freshly disclosed advisory does not block local development;
# CI runs `make security` to gate merges on new vulnerabilities.
security-cargo-advisories:
	@command -v cargo-deny >/dev/null 2>&1 || cargo install cargo-deny
	@echo "Auditing Rust dependencies for vulnerabilities..."
	@output=$$(cargo deny check advisories 2>&1) || { echo "$$output"; exit 1; }

security-npm-audit:
	@echo "Auditing frontend dependencies for vulnerabilities..."
	@output=$$(cd ui/frontend && npm audit 2>&1) || { echo "$$output"; exit 1; }

lint-reuse:
	@echo "Checking REUSE compliance..."
	@output=$$(reuse lint 2>&1) || { echo "$$output"; exit 1; }

lint-fmt:
	@echo "Checking formatting..."
	@output=$$(cargo fmt --check 2>&1) || { echo "$$output"; exit 1; }

lint-clippy:
	@echo "Running clippy..."
	@output=$$(cargo clippy --all-targets -- -D warnings 2>&1) || { echo "$$output"; exit 1; }

# Third-party license notices (parallelizable with make -j)
notices-cargo:
	@echo "Gathering Rust license notices..."
	@output=$$(bash scripts/generate-notices-cargo.sh 2>&1) || { echo "$$output"; exit 1; }

notices-npm:
	@echo "Gathering npm license notices..."
	@output=$$(bash scripts/generate-notices-npm.sh 2>&1) || { echo "$$output"; exit 1; }

notices: notices-cargo notices-npm
	@echo "Generating third-party notices..."
	@output=$$(bash scripts/generate-notices.sh 2>&1) || { echo "$$output"; exit 1; }

# All lint checks
lint: lint-cargo-licenses lint-npm-licenses lint-reuse lint-fmt lint-clippy

# Vulnerability checks. Run manually (or on a schedule) rather than in
# the per-PR pipeline so a newly disclosed advisory does not block
# unrelated development work.
security: security-cargo-advisories security-npm-audit

# Build targets
debug: notices
	@echo "Building depot..."
	@DEPOT_INSTRUMENT_FRONTEND=1 cargo build

release: notices
	cargo build --release

build: debug

# Individual test suites (parallelizable with make -j)
test-debug: debug lint
	@echo "Running debug tests..."
	@start=$$(date +%s); output=$$(DEPOT_INSTRUMENT_FRONTEND=1 cargo test -q 2>&1) || { echo "$$output"; exit 1; }; echo "  debug tests passed in $$(($$(date +%s) - $$start))s"

test-ui: debug lint
	@echo "Running UI tests..."
	@start=$$(date +%s); output=$$(bash scripts/ui-test.sh --skip-build 2>&1) || { echo "$$output"; exit 1; }; echo "  UI tests passed in $$(($$(date +%s) - $$start))s"

test-dynamodb: debug lint
	@echo "Running DynamoDB tests..."
	@start=$$(date +%s); output=$$(bash scripts/ext-test.sh dynamodb 2>&1) || { echo "$$output"; exit 1; }; echo "  DynamoDB tests passed in $$(($$(date +%s) - $$start))s"

test-docker-auth: debug lint
	@echo "Running Docker auth tests..."
	@start=$$(date +%s); output=$$(bash scripts/ext-test.sh docker-auth 2>&1) || { echo "$$output"; exit 1; }; echo "  Docker auth tests passed in $$(($$(date +%s) - $$start))s"

# All test suites
test: test-debug test-ui test-dynamodb test-docker-auth

# Legacy targets
debug_test: debug
	DEPOT_INSTRUMENT_FRONTEND=1 cargo test -q

release_test: release
	cargo test -q --release

# Demo
demo: debug
	@echo "Starting demo server..."
	@bash scripts/demo.sh

demo-data: debug
	@echo "Starting demo server with seeded data..."
	@bash scripts/demo.sh true

# Formatting
fmt:
	cargo fmt

# Coverage (Rust via llvm-cov + TypeScript via Istanbul/NYC)
coverage:
	@command -v cargo-llvm-cov >/dev/null 2>&1 || cargo install cargo-llvm-cov
	bash scripts/coverage.sh

# Docker
docker:
	docker buildx build -t depot .

# Documentation site (Jekyll + just-the-docs).
# Not wired into `all` / `test` -- doc builds shouldn't gate code CI.
docs:
	@echo "Building docs site..."
	@cd docs && bundle install --quiet && bundle exec jekyll build

docs-serve:
	@cd docs && bundle install --quiet && bundle exec jekyll serve --livereload --host 0.0.0.0 --baseurl ""

# Generate the curated UI screenshots embedded in docs/screenshots.md.
# Boots a depot, seeds with `depot-bench demo`, kicks off `depot-bench
# trickle` for live activity, then drives Chromium via the `screenshots`
# Playwright project. Outputs land in docs/screenshots/ ready to commit.
screenshots:
	@bash scripts/screenshots.sh

# Generate the Grafana-driven observability screenshots embedded in
# docs/observability.md.  Brings up docker/standalone with the
# `monitoring` profile (depot + otel-collector + Tempo + Loki +
# Prometheus + Grafana + MinIO), seeds + trickles depot, waits for
# dashboards to populate, then captures via the `observability`
# Playwright project.  Outputs land in docs/screenshots/observability/.
observability-screenshots:
	@bash scripts/observability-screenshots.sh

# Utility
clean:
	cargo clean
	rm -rf build/demo
	rm -rf docs/_site docs/.jekyll-cache
