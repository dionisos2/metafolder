# metafolder — build & install
#
# A task runner over the project's cargo/npm/script commands. `make help` lists
# every target. Release binaries land in $(BINDIR); the git-backed user config
# at ~/.config/metafolder/ is installed by metafolder-sync-config.
#
# Common flows:
#   make check-deps          # verify build/runtime deps, list what is missing
#   make                     # build everything (release)
#   make install             # build + install binaries + install user config
#   make install-headless    # daemon + CLI only (no GUI: skips webkit/npm)
#   make run-daemon / run-gui
#   make uninstall
#
# Override the install prefix:  make install PREFIX=/usr/local

PREFIX  ?= $(HOME)/.local
BINDIR  ?= $(PREFIX)/bin

CARGO   ?= cargo
NPM     ?= npm
FRONTEND := crates/gui/frontend
TARGET  := target/release

# Binaries copied by `install` (name in target/release).
BINS_HEADLESS := metafolder-daemon mf
BINS_GUI      := metafolder-gui

.DEFAULT_GOAL := help

# ── help ─────────────────────────────────────────────────────────────────────
.PHONY: help
help:
	@echo 'metafolder — make targets:'
	@echo ''
	@echo '  check-deps         verify build & runtime dependencies'
	@echo '  build              build everything, release (daemon, cli, gui, config)'
	@echo '  build-headless     build daemon + cli only (no GUI toolkit / npm)'
	@echo '  frontend           (re)build the GUI frontend bundle'
	@echo ''
	@echo '  install            build + install binaries into $(BINDIR) + user config'
	@echo '  install-headless   daemon + cli only + user config'
	@echo '  install-config     install/update ~/.config/metafolder/ (sync-config)'
	@echo '  uninstall          remove installed binaries from $(BINDIR)'
	@echo ''
	@echo '  run-daemon         run the daemon from the build tree'
	@echo '  run-gui            build frontend + run the GUI from the build tree'
	@echo '  test / check       run the suite (+ a global total)  /  all static checks'
	@echo '  clean / prune      cargo clean  /  scripts/prune-target.sh'
	@echo ''
	@echo '  PREFIX=$(PREFIX)  BINDIR=$(BINDIR)'

# ── dependency check ─────────────────────────────────────────────────────────
.PHONY: check-deps
check-deps:
	@scripts/check-deps.sh

# ── build ────────────────────────────────────────────────────────────────────
# The GUI frontend (Tauri embeds crates/gui/frontend/dist at compile time) must
# be built BEFORE cargo touches the gui crate — hence `build` depends on it.
.PHONY: frontend
frontend:
	@test -d $(FRONTEND)/node_modules || $(NPM) --prefix $(FRONTEND) install
	$(NPM) --prefix $(FRONTEND) run build

# sync-config is feature-gated on core only: enabling the feature on the whole
# workspace would recompile every crate against a feature-enabled core and
# duplicate all artifacts in target/.
.PHONY: build-config
build-config:
	$(CARGO) build --release -p metafolder-core --features sync-config --bin metafolder-sync-config

.PHONY: build-headless
build-headless: build-config
	$(CARGO) build --release -p metafolder-daemon -p metafolder-cli

.PHONY: build-gui
build-gui: frontend
	$(CARGO) build --release -p metafolder-gui

.PHONY: build
build: build-headless build-gui

# ── install ──────────────────────────────────────────────────────────────────
# install-config runs the freshly built binary from the repo root: sync-config
# gathers crates/*/default-config/ relative to the working directory.
.PHONY: install-config
install-config: build-config
	$(TARGET)/metafolder-sync-config

$(BINDIR):
	@mkdir -p $(BINDIR)

.PHONY: install-headless
install-headless: check-deps build-headless install-config | $(BINDIR)
	@for b in $(BINS_HEADLESS); do \
	    echo "install $(BINDIR)/$$b"; \
	    install -m 0755 $(TARGET)/$$b $(BINDIR)/$$b; \
	done
	@echo 'Installed. Ensure $(BINDIR) is on your PATH.'

.PHONY: install
install: check-deps build install-config | $(BINDIR)
	@for b in $(BINS_HEADLESS) $(BINS_GUI); do \
	    echo "install $(BINDIR)/$$b"; \
	    install -m 0755 $(TARGET)/$$b $(BINDIR)/$$b; \
	done
	@echo 'Installed. Ensure $(BINDIR) is on your PATH.'

.PHONY: uninstall
uninstall:
	@for b in $(BINS_HEADLESS) $(BINS_GUI); do \
	    if [ -e $(BINDIR)/$$b ]; then echo "rm $(BINDIR)/$$b"; rm -f $(BINDIR)/$$b; fi; \
	done

# ── run / test / maintenance ─────────────────────────────────────────────────
.PHONY: run-daemon
run-daemon:
	$(CARGO) run --release -p metafolder-daemon

.PHONY: run-gui
run-gui: frontend
	$(CARGO) run --release -p metafolder-gui

.PHONY: test
# scripts/run-tests.sh is `cargo test --workspace` plus the report cargo never
# prints: the totals across every test binary, and which tests failed where.
test:
	@CARGO='$(CARGO)' scripts/run-tests.sh

.PHONY: check
check:
	@scripts/check.sh

.PHONY: prune
prune:
	@scripts/prune-target.sh

.PHONY: clean
clean:
	$(CARGO) clean
