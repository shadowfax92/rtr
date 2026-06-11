BIN := rtr
CARGO ?= cargo
LOCAL_BINDIR ?= bin
PREFIX ?= $(HOME)/.cargo
INSTALL_BINDIR ?= $(PREFIX)/bin
SRC := $(shell find src -type f)

.PHONY: all install clean

all: $(LOCAL_BINDIR)/$(BIN)

$(LOCAL_BINDIR)/$(BIN): Cargo.toml Cargo.lock $(SRC)
	$(CARGO) build --release
	mkdir -p "$(LOCAL_BINDIR)"
	cp "target/release/$(BIN)" "$@"

install: $(LOCAL_BINDIR)/$(BIN)
	install -d "$(INSTALL_BINDIR)"
	install -m 755 "$(LOCAL_BINDIR)/$(BIN)" "$(INSTALL_BINDIR)/$(BIN)"

clean:
	rm -rf "$(LOCAL_BINDIR)"
	$(CARGO) clean
