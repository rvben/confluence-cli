COMPOSE_FILE := docker/docker-compose.yml
BACKUP_DIR := docker/backup
CONFLUENCE_DATA_VOLUME := docker_confluence-data
POSTGRES_DATA_VOLUME := confluencecli_postgres-data
CONFLUENCE_E2E_PROFILE ?= local-dc
CONFLUENCE_E2E_SPACE ?= TEST

.PHONY: build test test-e2e fmt lint check confluence-start confluence-stop confluence-wait confluence-logs confluence-reset confluence-backup confluence-restore

build:
	cargo build

test:
	cargo test

# Run end-to-end tests against a real Confluence instance.
# Defaults to the local Data Center profile created during local setup.
# Optional overrides:
#   CONFLUENCE_E2E_PROFILE
#   CONFLUENCE_E2E_SPACE
#   CONFLUENCE_E2E_BASE_URL / CONFLUENCE_E2E_TOKEN / CONFLUENCE_E2E_PROVIDER / CONFLUENCE_E2E_API_PATH
# Set CONFLUENCE_E2E_PROFILE= to force env-driven mode instead of profile mode.
test-e2e:
	CONFLUENCE_E2E_PROFILE=$(CONFLUENCE_E2E_PROFILE) CONFLUENCE_E2E_SPACE=$(CONFLUENCE_E2E_SPACE) cargo nextest run --test e2e --run-ignored all

fmt:
	cargo fmt

lint:
	cargo fmt -- --check
	cargo clippy --all-targets --all-features -- -D warnings

check: lint test

# ── Local Confluence (Data Center) for integration testing ───────────────────
confluence-start:
	docker compose -f $(COMPOSE_FILE) up -d

confluence-stop:
	docker compose -f $(COMPOSE_FILE) down

confluence-wait:
	docker/wait-for-confluence.sh

confluence-logs:
	docker compose -f $(COMPOSE_FILE) logs -f confluence

confluence-reset:
	docker compose -f $(COMPOSE_FILE) down -v
	docker volume rm -f $(CONFLUENCE_DATA_VOLUME) $(POSTGRES_DATA_VOLUME) >/dev/null 2>&1 || true

confluence-backup:
	docker compose -f $(COMPOSE_FILE) stop
	mkdir -p $(BACKUP_DIR)
	docker run --rm -v $(CONFLUENCE_DATA_VOLUME):/data -v $(CURDIR)/$(BACKUP_DIR):/backup busybox tar czf /backup/confluence-data.tar.gz -C /data .
	docker run --rm -v $(POSTGRES_DATA_VOLUME):/data -v $(CURDIR)/$(BACKUP_DIR):/backup busybox tar czf /backup/postgres-data.tar.gz -C /data .
	docker compose -f $(COMPOSE_FILE) start
	@echo "Backup written to $(BACKUP_DIR)/ (confluence-data.tar.gz, postgres-data.tar.gz)"

confluence-restore:
	test -f $(BACKUP_DIR)/confluence-data.tar.gz
	test -f $(BACKUP_DIR)/postgres-data.tar.gz
	docker compose -f $(COMPOSE_FILE) down -v
	docker volume rm -f $(CONFLUENCE_DATA_VOLUME) $(POSTGRES_DATA_VOLUME) >/dev/null 2>&1 || true
	docker volume create $(CONFLUENCE_DATA_VOLUME)
	docker volume create $(POSTGRES_DATA_VOLUME)
	docker run --rm -v $(CONFLUENCE_DATA_VOLUME):/data -v $(CURDIR)/$(BACKUP_DIR):/backup busybox tar xzf /backup/confluence-data.tar.gz -C /data
	docker run --rm -v $(POSTGRES_DATA_VOLUME):/data -v $(CURDIR)/$(BACKUP_DIR):/backup busybox tar xzf /backup/postgres-data.tar.gz -C /data
	docker compose -f $(COMPOSE_FILE) up -d
	@echo "Restore complete - run 'make confluence-wait' to confirm readiness (first boot may take several minutes)"
