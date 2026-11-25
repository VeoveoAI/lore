---
lep: 2026-11-25-exact-text-repository-list
title: Exact text metadata in repository listings
authors:
  - rozgo
status: Draft
created: 2026-11-25
updated: 2026-11-25
---

# Exact text metadata in repository listings

## Summary

Add an optional exact text metadata predicate to the native v1 RepositoryList RPC.
A result echoes only that matching key and text, and the response announces support
even when no repositories match. The existing caller-authorized catalog remains
the boundary for every result.

## Motivation

A client discovering repositories by one native metadata key currently lists
repositories, then reads each metadata value separately. RepositoryList already
loads each authorized repository's metadata to build its record. Applying the
predicate to that same decoded value avoids source checkout and per-repository
network requests.

## Goals / Non-Goals

### Goals

- Discover exact native text matches within the caller's authorized catalog in one RPC.
- Let clients distinguish supported empty results from ignored optional predicates.

### Non-Goals

- Cross-repository snapshot isolation, full-text search or a metadata index.
- Project-specific rules or changes to stored metadata and caller grants.

## Proposed Design

`RepositoryListRequest.text_metadata` optionally carries `RepositoryTextMetadata`
with `key` and `text`. The key is nonempty and at most 256 UTF-8 bytes; text is at
most 4096 bytes. Matching is exact and case-sensitive against native String
metadata. Missing keys and other metadata types do not match. Creator and metadata
predicates both apply. The existing response's repository includes its actual
native default branch.

`RepositoryListResponse.matched_text_metadata` echoes the predicate only for an
explicit native match. A filtered response includes initial gRPC metadata
`lore-repository-text-filter: 1`, including an empty result. Clients requiring the
filter must check both the support header and each result's echo; an older server
that ignores new protobuf fields must not broaden discovery. A catalog candidate
whose initial repository metadata-pointer lookup returns typed `AddressNotFound`
is absent and is skipped. Once that pointer exists, unreadable or malformed
metadata makes a filtered stream unavailable rather than claiming exhaustion.
Other pointer lookup errors also remain unavailable. Unfiltered calls preserve
existing skip/error and visibility behavior.

## Compatibility

- Wire: additive optional message fields; existing clients ignore them.
- Client/server: clients making filtered reads must require explicit support and
  matching echoes. Existing unfiltered clients retain their current results.
- On-disk format: unchanged.
- CLI and public C API: unchanged.

## Non-functional considerations

The existing catalog decides candidate visibility before metadata inspection.
Each candidate's metadata is decoded once, using the existing bounded native
metadata decoder; matching inspects one typed key. Only matched key/text is added
to each selected response. The predicate is shared across existing candidate tasks.
No durable or cross-request state is added. Metadata changes are observed at each
repository's existing read boundary, not a new catalog-wide transaction.

## Migration Plan

N/A — additive optional fields require no stored-data migration. Qualify the native
server first; a filtered client fails closed until support and match echoes exist.
Unfiltered clients continue to use the same RPC and results.

## Security Considerations

The predicate never discovers repositories outside the caller's native catalog.
Other metadata keys and values are not projected. Missing/nontext metadata is not
coerced to text. Failures are not represented as a negative match.

## Privacy Considerations

The authorized catalog is consulted before metadata inspection. Each result adds
only the requested matching key and text; unrelated values are not projected.

## Risks and Assumptions

- **Assumption:** the configured catalog returns only repositories readable by the
  caller — *invalidated if:* its authorization contract is violated.
- **Risk:** metadata reads fail during enumeration — *mitigation:* a filtered stream
  returns an error instead of treating unreadable candidates as non-matches.
- **Risk:** metadata changes while listing — *mitigation:* document the existing
  per-repository read boundary; there is no cross-repository snapshot claim.

## Drawbacks

- The server still performs O(caller-readable repositories) metadata reads. This
  avoids per-repository client handshakes but does not provide an indexed query.

## Alternatives Considered

### Client reads every repository

Rejected because separate metadata reads repeat network and decoding work that
RepositoryList already performs.

### Dedicated discovery index

Rejected because it introduces mutable derived state, freshness and authorization
responsibilities beyond the existing native catalog.

## Prior Art

The existing native [RepositoryList creator predicate](../../lore-proto/proto/lore/repository/v1/repository.proto)
already filters authorized results by exact text. This proposal applies the same
query shape to one typed native metadata entry and adds explicit support evidence.

## Unresolved Questions

- The draft requires public protocol review before merge; implementation may be
  qualified on a non-default development branch in the meantime.

## Validation

Native in-memory store tests cover exact text, case/type mismatch, missing keys,
creator conjunction, granted but uncreated repositories, missing referenced
metadata blobs, malformed metadata, pointer read failures, authorized subsets, plain listing,
filter bounds and an empty supported stream. Client tests must reject absent support
and incorrect echoes before adopting the new filtered API. The relevant native
[handler](../../lore-server/src/grpc/repository/v1/repository_list.rs),
[metadata decoder](../../lore-revision/src/metadata.rs),
[built-in fields](../../lore-revision/src/repository.rs) and
[unit tests](../../lore-server/tests/unit/grpc/repository/v1/repository_list.rs)
define and exercise these contracts.

## Adoption

Publish and qualify the native server before clients depend on the optional
predicate. No data migration is required. Independent review and normal repository
checks remain required before merging this draft implementation.
