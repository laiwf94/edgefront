# syntax=docker/dockerfile:1
FROM golang:1.27-alpine AS build
WORKDIR /src
COPY go.mod ./
COPY . .
RUN CGO_ENABLED=0 go build -trimpath -ldflags="-s -w" -o /out/steer ./cmd/steer

FROM gcr.io/distroless/static-debian12:nonroot
COPY --from=build /out/steer /steer
COPY examples/docker.json /etc/steer/steer.json
ENV STEER_CONFIG=/etc/steer/steer.json
USER nonroot:nonroot
EXPOSE 8080 9901
ENTRYPOINT ["/steer"]
