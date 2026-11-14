# Testudo build and install.
#
#   make install                  # release build, install to ~/.local
#   make install PREFIX=/usr/local  (needs write access; use sudo for system dirs)
#   make uninstall
#
# Layout under $(PREFIX):
#   bin/testudo, bin/testudo-cli
#   lib/testudo/adapters/          JS adapters the runner injects
#     (not share/testudo: with PREFIX=~/.local that is the history data dir)
#   share/applications/testudo.desktop
#   share/icons/hicolor/{scalable,NxN}/apps/testudo.{svg,png}
#
# DESTDIR is honoured for staged/package installs.

PREFIX   ?= $(HOME)/.local
DESTDIR  ?=
CARGO    ?= cargo

BINDIR      := $(DESTDIR)$(PREFIX)/bin
LIBDIR      := $(DESTDIR)$(PREFIX)/lib/testudo
ADAPTERDIR  := $(LIBDIR)/adapters
APPDIR      := $(DESTDIR)$(PREFIX)/share/applications
ICONDIR     := $(DESTDIR)$(PREFIX)/share/icons/hicolor
ICON_SIZES  := 16 24 32 48 64 128 256 512

BINS     := testudo testudo-cli
ADAPTERS := $(wildcard adapters/js/*.mjs) adapters/js/package.json

.PHONY: help build install uninstall test lint clean dev

help: ## show targets
	@grep -E '^[a-z-]+:.*## ' $(MAKEFILE_LIST) | awk -F':.*## ' '{printf "  %-10s %s\n", $$1, $$2}'

build: ## release build of testudo and testudo-cli
	$(CARGO) build --release --locked -p testudo-ui -p testudo-cli

install: build ## build, then install to $(PREFIX) (default ~/.local)
	install -d $(BINDIR) $(ADAPTERDIR) $(APPDIR)
	install -m 755 $(addprefix target/release/,$(BINS)) $(BINDIR)/
	install -m 644 $(ADAPTERS) $(ADAPTERDIR)/
	install -m 644 packaging/testudo.desktop $(APPDIR)/
	# install -d then -m, not GNU-only -D, so this also works with BSD install (macOS)
	install -d $(ICONDIR)/scalable/apps
	install -m 644 packaging/testudo.svg $(ICONDIR)/scalable/apps/testudo.svg
	for s in $(ICON_SIZES); do install -d $(ICONDIR)/$${s}x$${s}/apps && install -m 644 packaging/icons/testudo-$$s.png $(ICONDIR)/$${s}x$${s}/apps/testudo.png; done
	-[ -n "$(DESTDIR)" ] || ! command -v gtk-update-icon-cache >/dev/null 2>&1 || gtk-update-icon-cache -qtf $(ICONDIR)
	-[ -n "$(DESTDIR)" ] || ! command -v update-desktop-database >/dev/null 2>&1 || update-desktop-database $(APPDIR)
	@echo "installed to $(DESTDIR)$(PREFIX)"
	@case ":$$PATH:" in *":$(PREFIX)/bin:"*) ;; *) echo "note: $(PREFIX)/bin is not on PATH";; esac

uninstall: ## remove an install from $(PREFIX); recorded history is kept
	rm -f $(addprefix $(BINDIR)/,$(BINS)) $(APPDIR)/testudo.desktop
	rm -f $(ICONDIR)/scalable/apps/testudo.svg $(foreach s,$(ICON_SIZES),$(ICONDIR)/$(s)x$(s)/apps/testudo.png)
	rm -rf $(LIBDIR)
	@echo "removed from $(DESTDIR)$(PREFIX) (history in ~/.local/share/testudo/{testudo.db,runs,exports} untouched)"

test: ## run all tests
	$(CARGO) test --workspace

lint: ## clippy with warnings as errors
	$(CARGO) clippy --workspace --all-targets -- -D warnings

clean: ## remove build output
	$(CARGO) clean

dev: ## rebuild on change and restart the GUI when the build succeeds (ARGS="<project dir> ...")
	$(CARGO) run -q -p testudo-dev -- $(ARGS)
