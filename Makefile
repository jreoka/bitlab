BIN := bitlab
DIST := dist
PLATFORMS := linux/amd64 linux/arm64 darwin/amd64 darwin/arm64 windows/amd64 windows/arm64
GO ?= go
LDFLAGS := -s -w
# macOS ships shasum rather than sha256sum.
SHA256 := $(shell command -v sha256sum >/dev/null 2>&1 && echo sha256sum || echo "shasum -a 256")

.PHONY: build run test tidy docker dist clean help

help:
	@echo "build   native binary for this machine"
	@echo "test    go test ./..."
	@echo "dist    linux/darwin/windows for amd64+arm64, plus SHA256SUMS"
	@echo "run     build, then run out of ./data"
	@echo "docker  linux container image"
	@echo "clean   remove the native binary and dist/"

build:
	CGO_ENABLED=0 $(GO) build -trimpath -ldflags="$(LDFLAGS)" -o $(BIN) .

run: build
	BITLAB_DATA=./data ./$(BIN)

test:
	$(GO) test ./...

tidy:
	$(GO) mod tidy

# Everything is pure Go (modernc sqlite, no cgo), so one host can produce every
# target. This is the same set of binaries a release publishes.
dist:
	@rm -rf $(DIST) && mkdir -p $(DIST)
	@set -e; for p in $(PLATFORMS); do \
		os=$${p%/*}; arch=$${p#*/}; out="$(DIST)/bitlab-$$os-$$arch"; \
		if [ "$$os" = windows ]; then out="$$out.exe"; fi; \
		echo "  $$out"; \
		CGO_ENABLED=0 GOOS=$$os GOARCH=$$arch $(GO) build -trimpath -ldflags="$(LDFLAGS)" -o "$$out" . ; \
	done
	@cd $(DIST) && $(SHA256) bitlab-* > SHA256SUMS && cat SHA256SUMS

docker:
	docker build -t ghcr.io/jreoka/bitlab:latest .

clean:
	rm -f $(BIN)
	rm -rf $(DIST)
