# Multi-target push mirroring

Generalize the server-side mirror from a single GitHub remote to a list
of arbitrary targets, so quire can mirror each push to GitHub, Gitea, or
any token-authenticated remote at once.

## Background

Today `quire-server/src/mirror.rs` force-pushes every updated ref to one
remote. The URL comes from the per-repo `:github :mirror` key in
`.quire/config.fnl`; the token comes from the global `:github
:mirror-token`. Auth is HTTP Basic with a hardcoded
`x-access-token:{token}` pair.

The `:github` namespace exists solely for mirroring. Nothing else reads
it.

## Scope

This design covers mirroring to more than one remote and removing the
provider-specific naming. It does not cover Gitea Actions, pull-mirror
configuration on the Gitea side, or any change to how CI runs.

## Auth is provider-agnostic

GitHub and Gitea both accept the same token-only Basic-auth form for
git-over-HTTPS push:

```
https://{token}:x-oauth-basic@host/owner/repo.git
```

That is `Authorization: Basic base64("{token}:x-oauth-basic")` — the
token as the username, `x-oauth-basic` as a stub password. Because no
provider-specific behavior remains, a parallel `:gitea` config section
would only duplicate the `:github` one. A single list of targets is the
better shape.

One exception: a GitHub App *installation* token wants
`x-access-token:{token}` instead. The current mirror token is a personal
access token, so `x-oauth-basic` covers it. Revisit only if an
installation token is ever used.

## Config schema

Per-repo `.quire/config.fnl` gains a `:mirrors` list. Each target names
a secret from the global `:secrets` map rather than inlining a token:

```fennel
{:mirrors [{:url "https://github.com/kejadlen/quire.git" :secret :github-mirror}
           {:url "https://gitea.example/kejadlen/quire.git" :secret :gitea-mirror}]}
```

The global config holds the tokens in the existing `:secrets` map, the
same map CI jobs read via `(secret :name)`:

```fennel
{:secrets {:github-mirror "ghp_..."
           :gitea-mirror "..."}}
```

In `quire-server/src/quire/mod.rs`:

- Remove `GlobalGithubConfig` and the `github` field from `GlobalConfig`.
- Remove `RepoGithubConfig`; replace `RepoConfig.github` with
  `mirrors: Vec<MirrorTarget>`, defaulting to empty.
- Add `MirrorTarget { url: String, secret: String }`.

## Mirror flow

For each updated ref, iterate the repo's `mirrors`. Per target:

1. Resolve `target.secret` against the global `:secrets` map.
2. Build the `x-oauth-basic` Basic-auth header from the revealed token.
3. Force-push `+{ref}:{ref}` to `target.url`, passing the header through
   `GIT_CONFIG_*` env vars so it never reaches argv.

Errors collect across every (ref, target) pair, as the current code
already collects per-ref failures. One failing target does not block the
others.

## Error handling

Today an absent token silently skips all mirroring. With named secrets,
a target that points at a missing secret is a misconfiguration worth
surfacing, so add a `SecretNotFound { name }` variant to `MirrorError`
rather than swallowing it. A repo with an empty `:mirrors` list still
does nothing — that is the intended "mirroring off" state.

## Migration

Two config files, both under the operator's control:

- `.quire/config.fnl`: replace `{:github {:mirror "..."}}` with the
  `:mirrors` list above.
- Global config: move the `:github :mirror-token` value into `:secrets`
  under `:github-mirror`, and add `:gitea-mirror`.

## Testing

- Parse tests for the `:mirrors` list, including the empty default and a
  target that references a missing secret.
- Factor auth-header construction and secret resolution so they unit-test
  without shelling out to git.
- Verify a real push to both remotes during implementation, since the
  `x-oauth-basic` assumption for GitHub is only confirmed end-to-end by a
  successful push.

## Docs

Update `docs/config.md`: the `:github :mirror` and `:github
:mirror-token` rows become the `:mirrors` list plus `:secrets` entries.
