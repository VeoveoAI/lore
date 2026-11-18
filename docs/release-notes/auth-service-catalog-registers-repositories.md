---
type: feature
---

- `lore-server`: with `[server.auth] repository_catalog = "auth_service"` and neither `[environment.endpoint]
  user_url` nor `auth_url`, creating or deleting a repository registers it at the catalog's `repository_catalog_url`
  through `RebacApi` `CreateResource` and `DeleteResource`, as it is registered at `auth_url` when that is set. A
  token-claim deployment's issuer then learns of each repository its people create with `lore repository create`, so
  it can grant and list it; the creation fails if the issuer refuses it, and the issuer decides a deletion.
