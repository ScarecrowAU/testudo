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
# and on macOS, $(MACAPPDIR)/Testudo.app (default ~/Applications): Finder,
# Spotlight, Launchpad and the Dock find it there. It runs bin/testudo, so
# self-updates of the binary apply to it too.
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

UNAME_S    := $(shell uname -s)
VERSION    := $(shell sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
MACAPPDIR  ?= $(HOME)/Applications
MACAPP     := $(DESTDIR)$(MACAPPDIR)/Testudo.app
MACAPP_FILES := Contents/Info.plist Contents/MacOS/testudo Contents/Resources/Testudo.icns
# iconset entry:source PNG size, for iconutil.
ICNS_SET   := icon_16x16:16 icon_16x16@2x:32 icon_32x32:32 icon_32x32@2x:64 icon_128x128:128 \
              icon_128x128@2x:256 icon_256x256:256 icon_256x256@2x:512 icon_512x512:512
LSREGISTER := /System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister

BINS     := testudo testudo-cli
ADAPTERS := $(wildcard adapters/js/*.mjs) adapters/js/package.json

.PHONY: help build install uninstall test lint clean dev screenshots macos-app

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
	@[ "$(UNAME_S)" != Darwin ] || $(MAKE) --no-print-directory macos-app
	@echo "installed to $(DESTDIR)$(PREFIX)"
	@case ":$$PATH:" in *":$(PREFIX)/bin:"*) ;; *) echo "note: $(PREFIX)/bin is not on PATH";; esac

uninstall: ## remove an install from $(PREFIX); recorded history is kept
	rm -f $(addprefix $(BINDIR)/,$(BINS)) $(APPDIR)/testudo.desktop
	rm -f $(ICONDIR)/scalable/apps/testudo.svg $(foreach s,$(ICON_SIZES),$(ICONDIR)/$(s)x$(s)/apps/testudo.png)
	rm -rf $(LIBDIR)
	# Testudo.app: only the files macos-app writes, then its (now empty) folders.
	[ "$(UNAME_S)" != Darwin ] || { rm -f $(addprefix "$(MACAPP)"/,$(MACAPP_FILES)); \
	  rmdir "$(MACAPP)/Contents/MacOS" "$(MACAPP)/Contents/Resources" "$(MACAPP)/Contents" "$(MACAPP)" 2>/dev/null || true; }
	@echo "removed from $(DESTDIR)$(PREFIX) (history in ~/.local/share/testudo/{testudo.db,runs,exports} untouched)"

macos-app: ## macOS: install Testudo.app into $(MACAPPDIR), running $(PREFIX)/bin/testudo
	install -d "$(MACAPP)/Contents/MacOS" "$(MACAPP)/Contents/Resources" target/Testudo.iconset
	sed 's/@VERSION@/$(VERSION)/g' packaging/macos/Info.plist > "$(MACAPP)/Contents/Info.plist"
	printf '#!/bin/sh\n# Runs the installed binary, so self-updates apply here too.\nexec "%s/bin/testudo" "$$@"\n' "$(PREFIX)" > "$(MACAPP)/Contents/MacOS/testudo"
	chmod 755 "$(MACAPP)/Contents/MacOS/testudo"
	for e in $(ICNS_SET); do cp packaging/icons/testudo-$${e#*:}.png target/Testudo.iconset/$${e%:*}.png; done
	iconutil -c icns -o "$(MACAPP)/Contents/Resources/Testudo.icns" target/Testudo.iconset
	# Have Finder and the Dock pick up the new icon now, not after a re-login.
	-[ -n "$(DESTDIR)" ] || [ ! -x $(LSREGISTER) ] || $(LSREGISTER) -f "$(MACAPP)"

test: ## run all tests
	$(CARGO) test --workspace

lint: ## clippy with warnings as errors
	$(CARGO) clippy --workspace --all-targets -- -D warnings

clean: ## remove build output
	$(CARGO) clean

screenshots: ## retake the README screenshots from examples/acme-shop (Linux, KDE Plasma 6)
	scripts/screenshots.sh

dev: ## rebuild on change and restart the GUI when the build succeeds (ARGS="<project dir> ...")
	$(CARGO) run -q -p testudo-dev -- $(ARGS)
