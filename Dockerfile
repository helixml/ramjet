FROM golang:1.24-alpine AS build
WORKDIR /src
COPY go.mod go.sum ./
RUN go mod download
COPY . .
RUN CGO_ENABLED=0 go build -trimpath -ldflags="-s -w" -o /mini-dynamo ./cmd/mini-dynamo

FROM gcr.io/distroless/static-debian12
COPY --from=build /mini-dynamo /mini-dynamo
EXPOSE 8000 9090
ENTRYPOINT ["/mini-dynamo"]
