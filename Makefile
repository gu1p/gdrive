# Convenience targets around cargo. `make install` builds the release binary and copies it
# into $(BINDIR), which defaults to ~/.local/bin, the per-user directory that most Linux and
# macOS shells already have on PATH. Override with `make install PREFIX=/usr/local` (needs
# write access there) or `make install BINDIR=/some/dir`. Packagers can stage with DESTDIR.
#
# On Windows use `cargo build --release --locked` and copy target\release\gdrive.exe yourself.

CARGO  ?= cargo
PREFIX ?= $(HOME)/.local
BINDIR ?= $(PREFIX)/bin
DESTDIR ?=
BIN     = gdrive
TARGET  = target/release/$(BIN)

.PHONY: all build release test fmt clippy check install uninstall clean help

all: build

## build:     debug build (cargo build --locked)
build:
	$(CARGO) build --locked

## release:   optimized build, the binary is target/release/gdrive
release:
	$(CARGO) build --release --locked

## test:      run the test suite
test:
	$(CARGO) test --locked

## fmt:       check formatting (cargo fmt --check)
fmt:
	$(CARGO) fmt --all -- --check

## clippy:    lint with warnings denied, as CI does
clippy:
	$(CARGO) clippy --all-targets --locked -- -D warnings

## check:     fmt + clippy + test, the same gate as CI
check: fmt clippy test

## install:   build the release binary and install it to $(BINDIR) (default ~/.local/bin)
install: release
	install -d "$(DESTDIR)$(BINDIR)"
	install -m 0755 "$(TARGET)" "$(DESTDIR)$(BINDIR)/$(BIN)"
	@echo "installed $(DESTDIR)$(BINDIR)/$(BIN)"
	@case ":$$PATH:" in \
	  *":$(BINDIR):"*) ;; \
	  *) echo ""; \
	     echo "note: $(BINDIR) is not on your PATH. Add this line to your shell profile"; \
	     echo "      (~/.bashrc, ~/.zshrc or ~/.profile) and open a new shell:"; \
	     echo ""; \
	     echo "        export PATH=\"$(BINDIR):\$$PATH\""; \
	     echo "";; \
	esac
	@"$(DESTDIR)$(BINDIR)/$(BIN)" version 2>/dev/null | head -1 || true

## uninstall: remove the installed binary
uninstall:
	rm -f "$(DESTDIR)$(BINDIR)/$(BIN)"
	@echo "removed $(DESTDIR)$(BINDIR)/$(BIN)"

## clean:     remove build artifacts (cargo clean)
clean:
	$(CARGO) clean

## help:      list targets
help:
	@grep -E '^## ' $(MAKEFILE_LIST) | sed 's/^## //'
