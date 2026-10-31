# Security review

Scope: the five library crates and the HTTP server, reviewed at the alpha
milestone on 2026-10-31.

## Threat model

The alpha runs as one process per deployment, on a private network, with no
authentication. The operator trusts the network. The attacker controls the
things a client can send: span payloads, table schemas, record bodies, query
filters, and eval logs. If the attacker can write to the object store, they
control segment files too, so every parser must treat stored bytes as hostile.

Out of scope by design, per `AGENTS.md`: user authentication, RBAC, and
multi-region replication.

## Findings

| Severity | Finding | Status |
|----------|---------|--------|
| High | Tenant, namespace, and table names were placed raw into object store paths. A name such as `..` could escape a file-system store root. | Fixed |
| High | The text index container names files. A crafted `.text` object could make the reader write outside its temporary directory. | Fixed |
| Medium | Request bodies had no size limit, so one request could exhaust memory. | Fixed |
| Medium | 500 responses returned internal error text, including object store paths. | Fixed |
| Medium | No authentication. | Accepted for alpha, loopback bind |
| Medium | Read requests had no timeout or per-tenant cap, so one client could occupy a worker indefinitely. | Fixed: 30 s timeout, 16 concurrent reads per tenant, 429 and 504 responses |
| Low | Dependencies were not audited. | Fixed: `audit` workflow runs RustSec on lockfile changes and weekly |
| Medium | No delete, tombstone, or retention path. GDPR deletion is not supported. | Open, alpha blocker for regulated data |
| Low | Field names reached Parquet column names and aggregation SQL. | Fixed by restricting the character set |


## What changed

Identifiers that become key segments or path components now pass
`validate_path_segment`. It rejects an empty value, `/`, `.` and `..`, a
backslash, any control character, and anything longer than 1024 bytes. The key
encoder applies it to tenant, namespace, table, trace, span, and record ids. The
schema registry applies it to tenant, namespace, and table before building a
schema or manifest path. Every table endpoint validates its path parameters
before touching storage, and `/v1/evals` validates the tenant and namespace in
its body.

The text index reader resolves every container name as a relative path built
from normal components only, so `../escape` and absolute names are refused.

Request bodies are capped at 32 MiB by `DefaultBodyLimit`. Server errors return
`{"error":"internal error"}` and write the detail to stderr.

Field names are restricted to letters, digits, `_`, `.`, and `-`, at most 128
bytes. That keeps them safe as Parquet column names and as quoted identifiers in
aggregation SQL.

## Evidence

- `tracelake-core` unit test `path_segments_reject_traversal_and_control_characters`.
- `tracelake-compact` unit test `container_names_cannot_escape_the_extraction_directory`.
- `tracelake-server` test `unsafe_identifiers_are_rejected_before_touching_storage`.
- `tracelake-server` test `tenants_and_namespaces_are_isolated` covers span fetch,
  trace search, schema lookup, and record query across tenants.
- `cargo test --workspace` and `cargo clippy --workspace --all-targets -- -D warnings`.
- `.github/workflows/audit.yml` runs RustSec on lockfile changes and weekly.
- `grep -rn unsafe --include='*.rs' tracelake-*` returns no code use of unsafe.

## Isolation model

Every key starts with `v1/t/{tenant}/ns/{namespace}/` for spans and
`v1/r/t/{tenant}/ns/{namespace}/tb/{table}/` for records. Every query builds its
prefix from caller-supplied tenant and namespace. Segment paths and schema paths
include tenant and namespace. A query cannot read another tenant's data unless it
knows that tenant's name, which is the same trust model as the rest of the alpha.

## Alpha exposure requirements

- Bind to loopback or a private network. The server defaults to `127.0.0.1:8080`.
- Do not store regulated data until deletes and retention exist.
- Keep the data directory writable only by the service user.
- Treat the object store bucket as a trusted boundary. A writer to the bucket can
  change segment bytes, though checksums, pack bounds, and the text container
  bounds make silent corruption unlikely.

## Post-alpha work

- Authentication and per-tenant authorization.
- Request-rate limits. Timeouts and a per-tenant concurrency cap exist; a
  request-rate limit does not.
- Delete and retention with tombstones that survive compaction.
- License checking. The audit job covers advisories, not licenses.
- Encryption at rest is delegated to the object store, and should be stated in
  deployment docs. S3 and GCS use the provider credential chain; the server
  does not read access keys from its own configuration.
