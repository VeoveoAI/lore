---
type: feature
---

- `lore-client`: `LORE_IDENTITY` supplies `--identity` when the flag is absent, as `LORE_AUTH_MODE` does for
  `--auth-mode`. The flag outranks the variable and an empty value counts as absent. `LORE_IDENTITY` does not apply
  beside an identity token, from `--identity-token` or `LORE_IDENTITY_TOKEN`, which names its own identity. An access
  token, from `--access-token` or `LORE_ACCESS_TOKEN`, may go with an explicit identity, which an opaque one requires.
  A process whose environment holds a placeholder that a trusted transport resolves, such as a sandbox's provider
  credential, then runs plain `lore` commands with it.
