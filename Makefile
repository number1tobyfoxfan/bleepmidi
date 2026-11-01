PREFIX ?= $(HOME)/.local
BINDIR ?= $(PREFIX)/bin
CARGO ?= cargo
TARGET := target/release
BINS := bleepmidi bleepmidi-video

.PHONY: all build install uninstall test fmt lint demo clean

all: build

build:
	$(CARGO) build --release

install: build
	install -d "$(BINDIR)"
	for bin in $(BINS); do install -m 755 "$(TARGET)/$$bin" "$(BINDIR)/$$bin"; done
	@echo "installed $(BINS) in $(BINDIR)"

uninstall:
	for bin in $(BINS); do rm -f "$(BINDIR)/$$bin"; done

test:
	$(CARGO) test

fmt:
	$(CARGO) fmt

lint:
	$(CARGO) clippy --all-targets -- -D warnings

demo: build
	"$(TARGET)/bleepmidi" --demo

clean:
	$(CARGO) clean
	rm -f *.wav *.mp4
