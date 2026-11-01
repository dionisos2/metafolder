# metafolder — build & install
#
# A task runner over the project's cargo/npm/script commands. `make help` lists
# every target. Release binaries land in $(BINDIR); the git-backed user config
# at ~/.config/metafolder/ is installed by metafolder-sync-config.
#
# Common flows:
#   make check-deps          # verify build/runtime deps, list what is missing
#   make dev                 # the complete development build (debug) + user config
#   make build               # build everything (release)
#   make install             # build + install binaries + install user config
#   make install-headless    # daemon + CLI only (no GUI toolkit; node renders the docs)
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

# metafolder-watchd, the privileged fanotify broker (doc "The fanotify broker"),
# is a system service, installed apart by `install-watchd`: root-owned, where
# its unit runs it from — never under $(HOME), where a binary your user can
# rewrite would run with CAP_SYS_ADMIN.
WATCHD_BIN  := /usr/local/bin/metafolder-watchd
WATCHD_UNIT := /etc/systemd/system/metafolder-watchd.service
WATCHD_USER := /etc/sysusers.d/metafolder-watchd.conf

.DEFAULT_GOAL := help

# ── help ─────────────────────────────────────────────────────────────────────
.PHONY: help
help:
	@echo 'metafolder — make targets:'
	@echo ''
	@echo '  check-deps         verify build & runtime dependencies'
	@echo '  dev                complete debug build: frontend, workspace, test binaries,'
	@echo '                     prune of target/, docs, then the user config applied'
	@echo '  build              build everything, release (daemon, cli, gui, config)'
	@echo '  build-headless     build daemon + cli only (no GUI toolkit / frontend)'
	@echo '  frontend           (re)build the GUI frontend bundle'
	@echo '  docs               render the documentation wiki (docs/wiki/dist/)'
	@echo ''
	@echo '  install            build + install binaries into $(BINDIR) + user config'
	@echo '  install-headless   daemon + cli + user config'
	@echo '  install-config     install/update ~/.config/metafolder/ (sync-config)'
	@echo '  uninstall          remove installed binaries from $(BINDIR)'
	@echo ''
	@echo '  install-watchd     the fanotify broker as a system service (sudo):'
	@echo '                     binary, system user, unit, (re)started; rerun to update'
	@echo '  uninstall-watchd   stop and remove it (the daemon falls back to inotify)'
	@echo ''
	@echo '  run-daemon         run the daemon from the build tree'
	@echo '  run-gui            build frontend + run the GUI from the build tree'
	@echo '  test / check       run the suite (+ a global total)  /  all static checks'
	@echo '  clean / prune      cargo clean  /  scripts/dev/prune-target.sh'
	@echo ''
	@echo '  PREFIX=$(PREFIX)  BINDIR=$(BINDIR)'

# ── dependency check ─────────────────────────────────────────────────────────
.PHONY: check-deps
check-deps:
	@scripts/dev/check-deps.sh

.PHONY: check-deps-headless
check-deps-headless:
	@scripts/dev/check-deps.sh --headless

# ── npm dependencies ─────────────────────────────────────────────────────────
# node_modules lives at the repo root: the frontend and the documentation wiki
# are members of the npm workspace declared there, and the root package-lock.json
# is what pins them. `npm --prefix crates/gui/frontend install` ignores all that.
# The hidden lockfile npm writes is the stamp, so a pull that moves the lock
# reinstalls.
NPM_STAMP := node_modules/.package-lock.json

$(NPM_STAMP): package-lock.json
	$(NPM) ci

# ── build ────────────────────────────────────────────────────────────────────
# The GUI frontend (Tauri embeds crates/gui/frontend/dist at compile time) must
# be built BEFORE cargo touches the gui crate — hence `build` depends on it.
.PHONY: frontend
frontend: $(NPM_STAMP)
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

.PHONY: build-watchd
build-watchd:
	$(CARGO) build --release -p metafolder-watchd

.PHONY: build-gui
build-gui: frontend
	$(CARGO) build --release -p metafolder-gui

.PHONY: build
build: build-headless build-gui

# The documentation wiki rendered into docs/wiki/dist/, which install-config
# installs as ~/.config/metafolder/docs/ (the help panel reads it there).
# Rendered on every install: sync-config only checks that dist/ exists, so a
# stale one went in silently — a new note missing from the help panel.
.PHONY: docs
docs: $(NPM_STAMP)
	scripts/dev/doc build

# The complete development build, doc "Building". The frontend comes before cargo:
# Tauri embeds it at compile time, and bundled after, the binary keeps the
# previous bundle until the next run. The prune comes after all three builds, and
# only here: between two feature sets each would evict the other, and the script
# reads every profile, so a release-only target would evict a debug set it did not
# rebuild (doc "Keeping the target directory small").
.PHONY: dev
dev: frontend docs
	$(CARGO) build
	$(CARGO) test --workspace --no-run
	$(CARGO) build -p metafolder-core --features sync-config --bin metafolder-sync-config
	scripts/dev/prune-target.sh
	target/debug/metafolder-sync-config

# ── install ──────────────────────────────────────────────────────────────────
# install-config runs the freshly built binary from the repo root: sync-config
# gathers crates/*/default-config/ relative to the working directory.
.PHONY: install-config
install-config: build-config docs
	$(TARGET)/metafolder-sync-config

$(BINDIR):
	@mkdir -p $(BINDIR)

.PHONY: install-headless
install-headless: check-deps-headless build-headless install-config | $(BINDIR)
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

# The broker, in one command: built, installed root-owned, its system user
# created, its unit installed, enabled and (re)started — rerunning it is how
# the broker is updated. Daemons pick it up when they (re)load a repository.
.PHONY: install-watchd
install-watchd: build-watchd
	sudo install -D -m 0755 $(TARGET)/metafolder-watchd $(WATCHD_BIN)
	sudo install -D -m 0644 scripts/dev/metafolder-watchd.sysusers $(WATCHD_USER)
	sudo systemd-sysusers $(WATCHD_USER)
	sudo install -D -m 0644 scripts/dev/metafolder-watchd.service $(WATCHD_UNIT)
	sudo systemctl daemon-reload
	sudo systemctl enable metafolder-watchd
	sudo systemctl restart metafolder-watchd
	@sleep 1; if systemctl is-active --quiet metafolder-watchd; then \
	    echo 'metafolder-watchd is running. Restart the daemon to watch with it;'; \
	    echo '`mf -n <repo> watch status` then reports backend fanotify.'; \
	else \
	    echo 'metafolder-watchd failed to start:'; \
	    sudo journalctl -u metafolder-watchd -n 30 --no-pager; exit 1; \
	fi

.PHONY: uninstall-watchd
uninstall-watchd:
	-sudo systemctl disable --now metafolder-watchd
	sudo rm -f $(WATCHD_UNIT) $(WATCHD_BIN)
	sudo systemctl daemon-reload
	@echo 'Removed. Restart the daemon: it watches with inotify again.'
	@echo '(The metafolder-watchd system user stays; remove $(WATCHD_USER) and the user to drop it.)'

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
# scripts/dev/run-tests.sh is `cargo test --workspace` plus the report cargo never
# prints: the totals across every test binary, and which tests failed where.
test:
	@CARGO='$(CARGO)' scripts/dev/run-tests.sh

.PHONY: check
check:
	@scripts/dev/check.sh

.PHONY: prune
prune:
	@scripts/dev/prune-target.sh

.PHONY: clean
clean:
	$(CARGO) clean
