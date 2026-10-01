---
name: config-coder
description: Rules for changing mitten's config file format. Use whenever you add, rename, remove, or change a key in config.toml (crates/mitten/src/config.rs), so the `mitten configure` TUI (crates/mitten/src/onboarding.rs) and the docs change in the same commit.
---

# Config Coder

Every config key a user may set must be settable from `mitten configure`.
A config change is not done until the TUI, docs, and tests match it.

## Checklist

Change all of these in the same commit:

1. **`crates/mitten/src/config.rs`**
   - Raw `File*` struct field (serde, `deny_unknown_fields`).
   - Resolved `Config` field, with default and validation in `Config::parse`.
   - A test in `config.rs` tests for parsing and for rejecting bad values.
2. **`crates/mitten/src/onboarding.rs` (the TUI)**
   - Field index const. Keep `MCP_FIRST` last and bump it; MCP Keep/Remove rows start there.
   - `fields.push(field(step, label, kind, value))` in `Form::new`, prefilled from `current`
     (the loaded `Config`), else the same default `config.rs` uses.
     - Pick the step from `STEPS` that fits; general settings go in Advanced (step 5).
     - `Kind::Select` for a small fixed set (use `options_with` so hand-edited values survive),
       `Kind::Secret` for tokens and passwords, `Kind::Text` otherwise.
   - Conditional visibility in `Form::shown` if it depends on another field.
   - Validation in a `Form` method, called from `check_step` for its step and from `answers`.
     Return the same error rules as `config.rs`, so the user sees them before saving.
   - Field in `Answers`, written by `render_toml`. Top-level keys go before the first `[table]`.
   - Update `rendered_config_parses_back` and `steps_validate_and_hide_conditional_fields`.
3. **`config.example.toml`**: commented entry with what it does and its default.
4. **`README.md`**: update where the key's behavior is described, if anywhere.
5. **`crates/mitten/src/settings.rs`** (agent's `settings` tool): add the key to `KEYS`
   only if the model should be able to change it. Never add secrets.

## Keys the form doesn't show

Avoid them. If a key truly doesn't belong on the form (rarely touched, like
`tools.searxng.results`), keep its value across saves: store it on `Form` (see
`search_results`, `claude_extra`) and write it back in `render_toml`. Otherwise
`mitten configure` silently resets a hand-edited value.

## Verify

```sh
cargo clippy -p mitten --all-targets
cargo test -p mitten
```

Then run `mitten configure` against a copy of a real config, save, and diff the file.
