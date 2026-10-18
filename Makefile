BIN := marlin

.PHONY: build run test tidy docker clean

build:
	CGO_ENABLED=0 go build -ldflags="-s -w" -o $(BIN) .

run: build
	MARLIN_DATA=./data ./$(BIN)

test:
	go test ./...

tidy:
	go mod tidy

docker:
	docker build -t ghcr.io/jreoka/marlin:latest .

clean:
	rm -f $(BIN)
