# syntax=docker/dockerfile:1
FROM golang:alpine AS build
WORKDIR /src
ENV CGO_ENABLED=0 GOFLAGS=-trimpath
COPY go.mod go.sum ./
RUN go mod download
COPY . .
RUN go build -ldflags="-s -w" -o /out/marlin .

FROM alpine:3.20
RUN apk add --no-cache ca-certificates tzdata && adduser -D -u 65532 marlin
COPY --from=build /out/marlin /usr/local/bin/marlin
USER 65532:65532
ENV MARLIN_ADDR=:8090 MARLIN_DATA=/data
VOLUME ["/data"]
EXPOSE 8090
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s \
  CMD wget -qO- http://127.0.0.1:8090/healthz >/dev/null 2>&1 || exit 1
ENTRYPOINT ["/usr/local/bin/marlin"]
