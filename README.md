# open-search

An open-source foundation for provider-neutral AI search.

The project separates web retrieval, answer generation, orchestration, and HTTP delivery so search engines and AI model providers can be added without coupling them to the public API.

## Current status

This repository contains the initial runnable architecture. No external search or AI provider is configured yet, so `POST /v1/search` returns `503 Service Unavailable` until adapters are wired into the server.

## Workspace

- `open-search-core`: provider-neutral requests, responses, errors, and contracts
- `open-search-providers`: external search and answer provider adapters
- `open-search-runtime`: retrieval and answer-generation orchestration
- `open-search-server`: Axum HTTP API and process composition

## Run

```bash
cargo run
```

The server listens on `127.0.0.1:8080` by default. Override it with `OPEN_SEARCH_LISTEN_ADDRESS`.

```bash
curl http://127.0.0.1:8080/healthz

curl --json '{"query":"What is retrieval-augmented generation?"}' \
  http://127.0.0.1:8080/v1/search
```

## Development

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

## License

The open-source license has not been selected yet. A license file must be added before the first public release.
