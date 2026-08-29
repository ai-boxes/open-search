# open-search

An open-source foundation for provider-neutral AI search.

The project exposes one provider-neutral search contract. Each provider adapter owns its complete search flow, whether that is a hosted AI search request or a traditional retrieval pipeline.

## Current status

This repository contains the provider-neutral search contract, Grok OAuth and hosted-search adapter, and a Streamable HTTP MCP server. Mock upstream tests cover the Grok wire path, and direct X/Web searches have been verified with a real Grok account. Fresh OAuth and container-persistence smoke tests are still required before the first release.

## Workspace

- `open-search-core`: provider-neutral X/Web requests, provider result envelopes, errors, and contracts
- `open-search-providers`: complete external search provider adapters
- `open-search-runtime`: validation, capability checks, and provider execution
- `open-search-server`: Streamable HTTP MCP server, health endpoint, compatibility HTTP API, and process composition

## Run

Authorize the single Grok account first:

```bash
cargo run -- grok oauth
```

The command prints the xAI verification URL and user code, waits for the device authorization, and stores the credential in `data/open-search.db`. The database contains OAuth secrets and must be protected as sensitive data.

Start the service:

```bash
cargo run
```

The server listens on `127.0.0.1:8080` by default. Override it with `LISTEN_ADDRESS`.

To require Bearer authentication for `/mcp` and `/v1/search`, set `MCP_BEARER_TOKEN` when starting the server:

```bash
MCP_BEARER_TOKEN=replace-with-a-secret cargo run
```

Configure the MCP client with the same Bearer token. If the variable is unset or empty, authentication is disabled. `/healthz` always remains public.

The default logs report startup configuration, one completion or failure event per search, and credential refresh outcomes. Noisy `rmcp` session and client metadata events are suppressed by the default `info,rmcp=warn` filter. Logs never include search queries, prompts, result bodies, authorization headers, or tokens. Use the standard `RUST_LOG` variable to explicitly override the tracing filter.

Connect an MCP client to:

```text
http://127.0.0.1:8080/mcp
```

The MCP server exposes exactly two tools:

- `grok_x_search`
- `grok_web_search`

The Grok model is an internal upstream setting and is not part of either MCP tool schema.
Web search returns Grok's raw completed `web_search_call`. X search waits for Grok's backend search to finish and returns the raw final assistant `message`, because the preceding `custom_tool_call` objects contain only internal call metadata. The service does not issue a second summary request or reformat the upstream result.
Search requests use Grok 4.6 with low reasoning effort to reduce time-to-result while preserving the final search text and citations.

```bash
curl http://127.0.0.1:8080/healthz

curl --json '{"type":"web","query":"What is retrieval-augmented generation?"}' \
  --header 'Authorization: Bearer replace-with-a-secret' \
  http://127.0.0.1:8080/v1/search
```

`POST /v1/search` is currently retained as a compatibility and diagnostic endpoint. MCP clients should use `/mcp`.

The database location can be overridden with `DATABASE_PATH`.

## Docker

Build the image locally with BuildKit:

```bash
docker buildx build --load -t open-search .
```

Create a persistent volume and complete Grok OAuth:

```bash
docker volume create open-search-data

docker run --rm -it \
  -v open-search-data:/app/data \
  open-search grok oauth
```

Start the MCP service and expose it only on the local machine by default:

```bash
docker run --rm \
  -p 127.0.0.1:8080:8080 \
  -e MCP_BEARER_TOKEN=replace-with-a-secret \
  -v open-search-data:/app/data \
  open-search
```

Connect the MCP client to `http://127.0.0.1:8080/mcp` and configure `replace-with-a-secret` as its Bearer token. Omit `MCP_BEARER_TOKEN` and the client token for an unauthenticated local deployment.

The image declares `/app/data` as a volume, but persistence still requires an explicit named or bind mount. The SQLite database contains OAuth credentials and must be handled as secret data.

Branch pushes and `v*` tags automatically build multi-platform images through GitHub Actions and publish them to `ghcr.io/<owner>/<repository>`. Pull requests build the image without publishing it.

## Development

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

## License

The open-source license has not been selected yet. A license file must be added before the first public release.
