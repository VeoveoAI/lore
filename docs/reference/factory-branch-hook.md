# Factory pre-push guard

The optional `factory_branch` hook keeps normal pushes by designated factory
identities on their trusted assigned branch. It uses the existing `Hook` and
`HookFactory` interfaces, automatic build registration and `[hooks]` configuration.
Both supported push APIs already call `BranchPush` pre-hooks. No push handler,
authorization middleware, token claim or transport behavior is changed.

```toml
[hooks.factory_branch]
enabled = true

[hooks.factory_branch.assignments."factory-session-identity"]
repository = "0194b726b34e72b0b45550b88a967076"
branch = "0194b726b34e72b0b45550b88a967077"
```

The trusted installation/integration owns this server configuration. Reload it
through Lore's normal configuration/startup workflow; never take assignments from
agent-owned repository files, CLI flags or request headers. Use an authenticated
server with the configured verified identity claim (`sub` by default). Factory
credentials need a distinct verified identity and ordinary repository access;
never set `is_service_account` just because the caller is automated.

The check compares stable repository and branch IDs, so a branch rename does not
change its assignment. A rejected push names the assigned and attempted branch
IDs. Identities absent from `assignments`, including normal human users, keep their
existing permissions. An admitted push still passes through native branch
protection and the ordinary push/merge implementation.

This is a guard on normal push operations, not a per-branch security boundary.
It does not isolate every storage RPC, restrict all branch administration, or
change credential/session expiry. OpenShell owns sandbox isolation, permitted
destinations, credential custody and lifecycle. Qualify provider renewal and access
removal separately, including the limits of already-forwarded requests.
