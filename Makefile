BIN := bitlab

.PHONY: build run test tidy docker clean

build:
	CGO_ENABLED=0 go build -ldflags="-s -w" -o $(BIN) .

run: build
	BITLAB_DATA=./data ./$(BIN)

test:
	go test ./...

tidy:
	go mod tidy

docker:
	docker build -t ghcr.io/jreoka/bitlab:latest .

clean:
	rm -f $(BIN)
