# quadcastctl dev workflow.
#
#   make versions   # which binary is running where?
#   make install    # build release + point launchd at THIS checkout
#   make dev        # stop launchd, run THIS checkout in the foreground
#   make uninstall  # remove the launchd daemon entirely
#   make logs       # tail the launchd daemon's logs
#
# Typical loop while iterating: edit -> `make dev` -> Ctrl-C -> repeat.
# When you're done iterating: `make install` rebinds launchd to the latest
# release build so the daemon keeps running across reboots.

CARGO    ?= cargo
BIN      := ./target/release/quadcastctl
LABEL    := com.miketineo.quadcastctl
PLIST    := $(HOME)/Library/LaunchAgents/$(LABEL).plist
TARGET   := gui/$(shell id -u)/$(LABEL)
# Where to drop the binary so `quadcastctl` on your PATH points at this
# checkout (shadows the Homebrew copy without uninstalling it).
PATH_BIN := $(HOME)/.cargo/bin/quadcastctl

.PHONY: help build versions install link dev uninstall status logs

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?##' $(MAKEFILE_LIST) | awk 'BEGIN{FS=":.*?##"}{printf "  \033[36m%-10s\033[0m %s\n", $$1, $$2}'

build: ## Build the release binary
	@$(CARGO) build --release

versions: ## Compare source / local-build / PATH / launchd-loaded versions
	@printf "source       : %s\n" "$$(awk -F'\"' '/^version/{print $$2; exit}' Cargo.toml)"
	@if [ -x "$(BIN)" ]; then \
	  printf "local build  : %s (%s)\n" "$$($(BIN) --version | awk '{print $$2}')" "$(BIN)"; \
	else \
	  printf "local build  : (not built — run 'make build')\n"; \
	fi
	@if command -v quadcastctl >/dev/null 2>&1; then \
	  printf "PATH binary  : %s (%s)\n" "$$(quadcastctl --version | awk '{print $$2}')" "$$(command -v quadcastctl)"; \
	else \
	  printf "PATH binary  : (not on PATH)\n"; \
	fi
	@if [ -f "$(PLIST)" ]; then \
	  exe=$$(/usr/libexec/PlistBuddy -c "Print :ProgramArguments:0" "$(PLIST)" 2>/dev/null); \
	  if [ -x "$$exe" ]; then \
	    printf "launchd      : %s (%s)\n" "$$($$exe --version | awk '{print $$2}')" "$$exe"; \
	  else \
	    printf "launchd      : (binary missing at %s)\n" "$$exe"; \
	  fi; \
	else \
	  printf "launchd      : (not installed)\n"; \
	fi

install: build link ## Build, rebind launchd, and shadow PATH with the local build
	@$(BIN) uninstall 2>/dev/null || true
	@$(BIN) install

link: build ## Drop the local build into $(PATH_BIN) so `quadcastctl` resolves to it
	@install -m755 "$(BIN)" "$(PATH_BIN)"
	@if [ -L /opt/homebrew/bin/quadcastctl ] && readlink /opt/homebrew/bin/quadcastctl | grep -q Cellar; then \
	  echo "→ /opt/homebrew/bin/quadcastctl is a brew symlink that shadows $(PATH_BIN); running 'brew unlink quadcastctl' to defer to it"; \
	  brew unlink quadcastctl >/dev/null; \
	fi
	@hash -r 2>/dev/null || true
	@printf "linked %s -> %s\n" "$(PATH_BIN)" "$$($(PATH_BIN) --version)"
	@resolved=$$(command -v quadcastctl); \
	if [ "$$resolved" != "$(PATH_BIN)" ]; then \
	  echo "warning: \`quadcastctl\` still resolves to $$resolved — check PATH ordering"; \
	fi

dev: build ## Stop launchd, run the local build in the foreground (Ctrl-C to stop)
	@launchctl bootout $(TARGET) 2>/dev/null || true
	@echo "→ launchd unloaded; running $(BIN) daemon"
	@echo "  when done iterating, run 'make install' to put a launchd daemon back."
	@$(BIN) daemon

uninstall: ## Remove the launchd daemon entirely
	@launchctl bootout $(TARGET) 2>/dev/null || true
	@if [ -f "$(PLIST)" ]; then rm -f "$(PLIST)" && echo "removed $(PLIST)"; else echo "no plist at $(PLIST)"; fi

status: ## Show launchd daemon status
	@launchctl print $(TARGET) 2>/dev/null || echo "not loaded"

logs: ## Tail the launchd daemon's stdout/stderr logs
	@tail -F $(HOME)/Library/Logs/quadcastctl/quadcastctl.out.log $(HOME)/Library/Logs/quadcastctl/quadcastctl.err.log
