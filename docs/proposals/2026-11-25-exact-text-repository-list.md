---
lep: 2026-11-25-exact-text-repository-list
title: Exact text metadata in repository listings
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

## Proposed design

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
that ignores new protobuf fields must not broaden discovery. Unreadable candidate
metadata makes a filtered stream unavailable rather than claiming exhaustion.
Unfiltered calls preserve existing skip/error and visibility behavior.

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

## Security

The predicate never discovers repositories outside the caller's native catalog.
Other metadata keys and values are not projected. Missing/nontext metadata is not
coerced to text. Failures are not represented as a negative match.

## Validation

Native in-memory store tests cover exact text, case/type mismatch, missing keys,
creator conjunction, unreadable metadata, authorized subsets, plain listing,
filter bounds and an empty supported stream. Hub tests must reject absent support
and incorrect echoes before adopting the new filtered API.

## Adoption

Publish and qualify the native server before clients depend on the optional
predicate. No data migration is required. Independent review and normal repository
checks remain required before merging this draft implementation.
