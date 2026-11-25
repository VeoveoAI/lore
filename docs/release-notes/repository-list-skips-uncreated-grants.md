---
type: fix
---

- `lore-server`: Fixed filtered repository listings failing when the caller's
  catalog includes a repository that hasn't been created. Unreadable metadata
  belonging to an existing repository still fails the filtered listing.
