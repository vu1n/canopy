INSTALL_DIR ?= $(HOME)/.local/bin
CLAUDE_SETTINGS := $(HOME)/.claude/settings.json

.PHONY: build build-release test install install-service setup-claude fmt lint clean

build:
	cargo build

build-release:
	cargo build --release

test:
	cargo test

install: build-release
	@mkdir -p $(INSTALL_DIR)
	cp target/release/canopy $(INSTALL_DIR)/canopy
	cp target/release/canopy-mcp $(INSTALL_DIR)/canopy-mcp
	@echo "Installed canopy + canopy-mcp to $(INSTALL_DIR)"

install-service: build-release
	@mkdir -p $(INSTALL_DIR)
	cp target/release/canopy-service $(INSTALL_DIR)/canopy-service
	@echo "Installed canopy-service to $(INSTALL_DIR)"

setup-claude:
	@mkdir -p $(HOME)/.claude
	@if [ -f "$(CLAUDE_SETTINGS)" ] && grep -q '"canopy"' "$(CLAUDE_SETTINGS)" 2>/dev/null; then \
		echo "canopy MCP server already configured in $(CLAUDE_SETTINGS)"; \
	elif [ -f "$(CLAUDE_SETTINGS)" ] && grep -q '"mcpServers"' "$(CLAUDE_SETTINGS)" 2>/dev/null; then \
		if [ "$$(uname -s)" = "Darwin" ]; then \
			sed -i '' 's/"mcpServers" *: *{/"mcpServers": { "canopy": { "command": "canopy-mcp" },/' "$(CLAUDE_SETTINGS)"; \
		else \
			sed -i 's/"mcpServers" *: *{/"mcpServers": { "canopy": { "command": "canopy-mcp" },/' "$(CLAUDE_SETTINGS)"; \
		fi; \
		echo "Added canopy to existing $(CLAUDE_SETTINGS)"; \
	else \
		echo '{ "mcpServers": { "canopy": { "command": "canopy-mcp" } } }' > "$(CLAUDE_SETTINGS)"; \
		echo "Created $(CLAUDE_SETTINGS) with canopy MCP server"; \
	fi

fmt:
	cargo fmt

lint:
	cargo clippy --all-targets

clean:
	cargo clean
