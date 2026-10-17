# Branch-scoped authoring grants

This extension build accepts an optional signed `lore_authoring` JWT claim:

```json
{"version":1,"repository":"0194b726b34e72b0b45550b88a967076","branch":"0194b726b34e72b0b45550b88a967077"}
```

The normal configured issuer, audience, signature and repository grants still apply.
This claim only narrows access. It requires canonical identifiers, a maximum
five-minute token lifetime, hard expiry and no `is_service_account` bypass.
Clients without the claim keep their existing authorization contract.

Scoped clients may read the authorized repository, upload immutable fragments with
the repository's `write` permission, and push its assigned branch through either
supported push API. Ordinary branch protection still applies. Branch administration,
raw mutable writes, `PutResolved`, unlisted RPCs, HTTP and QUIC are denied. Issuers
must attach this claim to every authoring credential; never issue repository-wide
write credentials to an untrusted factory. Endpoint policy remains independently
required.

The scoped gRPC lifetime layer bounds handler execution and both stream directions
by the grant's exact expiration. An established session does not extend a grant.
Clients reconnect with a renewed token; an in-flight operation may be cancelled at
expiry and must follow normal retry semantics. Removing issuer authority prevents
renewal; existing signed grants remain valid until hard expiry. Select a grant TTL
that meets the installation's revocation bound and qualify native provider-binding
removal separately.

`lore-server/examples/authoring_fixture.rs` seeds disposable repositories using
existing store/repository interfaces. It is an operator-only offline test utility:
stop the owning server before opening its stores and preserve a backup. It creates
fresh IDs, an assigned branch, another author's branch and a protected main branch;
it does not add a remotely callable bootstrap or administrative API.
