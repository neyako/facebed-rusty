# Plan 012: Reload `timezone` and `banned_users` on SIGHUP without redeploy

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md` — unless a reviewer dispatched you and told you they
> maintain the index.
>
> **Drift check (run first)**:
> `git diff --stat 2e6610f..HEAD -- src/main.rs src/routes.rs src/parsers/mod.rs`
> If any of these changed since this plan was written, compare the "Current
> state" excerpts against the live code before proceeding; on a mismatch,
> treat it as a STOP condition.

## Status

- **Priority**: P3
- **Effort**: S-M
- **Risk**: MED
- **Depends on**: none (but reuses the SIGHUP pattern that plan 009 already
  landed in `src/main.rs`)
- **Category**: direction
- **Planned at**: commit `2e6610f`, 2026-06-18

## Why this matters

Plan 009 made **cookies** reload live on `SIGHUP` (see `src/main.rs:96-122`), but
**config does not**. Today, changing `banned_users` (to block an abusive author)
or `timezone` (embed timestamp offset) means editing `config.yaml` and **fully
redeploying** the service — dropping in-flight requests and the warm embed cache.
For a single-maintainer self-hosted service the most operationally useful of
these is `banned_users`: blocking a user should be a one-line edit + `kill -HUP`,
not a redeploy.

This plan extends the **existing** SIGHUP handler to also reload the config file,
and makes `timezone` and `banned_users` read from a hot-swappable `Config`. It
deliberately leaves `host`, `port`, and `notifier_webhook` as restart-only — see
"Scope" and "Maintenance notes" for why.

## Current state

The SIGHUP machinery and the `ArcSwap` pattern already exist — you are extending
them, not inventing them.

- `src/main.rs` builds config as a plain `Arc<Config>` and the cookie jar as an
  `ArcSwap`:
  ```rust
  // src/main.rs:48-56 (condensed)
  let config = match args.config {
      Some(p) => Config::load(&p)?,
      None => { warn!("no config provided; using defaults"); Config::default() }
  };
  let cookies = Arc::new(ArcSwap::from_pointee(CookieJar::load(&args.cookies)?));
  ```
- The existing SIGHUP handler reloads **only** cookies:
  ```rust
  // src/main.rs:96-122 (condensed)
  #[cfg(unix)]
  {
      let jar = cookies.clone();
      let cookies_path = args.cookies.clone();
      tokio::spawn(async move {
          use tokio::signal::unix::{signal, SignalKind};
          let mut hup = signal(SignalKind::hangup())...;
          while hup.recv().await.is_some() {
              match validate_cookie_json_files(&cookies_path)
                  .and_then(|_| CookieJar::load(&cookies_path)) {
                  Ok(new_jar) => { let n = new_jar.len(); jar.store(Arc::new(new_jar));
                      info!("reloaded {n} cookie account(s) on SIGHUP"); }
                  Err(e) => warn!("cookie reload failed: {e}"),
              }
          }
      });
  }
  ```
- `ParserCtx` is built once and **bakes in** `banned_users` as an owned `Vec`:
  ```rust
  // src/main.rs:124-128
  let ctx = Arc::new(ParserCtx {
      fetcher: fetcher.clone(),
      cookies: cookies.clone(),
      banned_users: config.banned_users.clone(),
  });
  ```
- `ParserCtx` already holds an `ArcSwap` (`cookies`) — so adding a swappable
  config follows an established field shape:
  ```rust
  // src/parsers/mod.rs:32-42
  pub struct ParserCtx {
      pub fetcher: Arc<Fetcher>,
      pub cookies: Arc<arc_swap::ArcSwap<CookieJar>>,
      pub banned_users: Vec<String>,
  }
  impl ParserCtx {
      pub fn is_banned(&self, author_id: &str) -> bool {
          self.banned_users.iter().any(|b| b == author_id)
      }
  }
  ```
- `AppState.config` is `Arc<Config>` (`src/routes.rs:31`), read in exactly one
  place for `timezone`:
  ```rust
  // src/routes.rs:617
  let tz = state.config.timezone;
  ```
- `Config` and its loader/validator:
  ```rust
  // src/config.rs
  pub struct Config { pub host: String, pub port: u16, pub timezone: i32,
      pub banned_users: Vec<String>, pub notifier_webhook: String }
  pub fn load(path: &Path) -> anyhow::Result<Self>  // reads + validates
  pub fn validate(&self) -> anyhow::Result<()>      // timezone range -12..14
  ```
- **Convention**: hot-swappable shared state is `Arc<arc_swap::ArcSwap<T>>`; read
  it with `.load()` (returns a guard that derefs to `&T`). The cookie jar is the
  exemplar — `cookies.load()` is used ~20 places across `fetch.rs`/`routes.rs`.
  `arc-swap` is already a dependency (`Cargo.toml`).

## Commands you will need

| Purpose   | Command                       | Expected on success       |
|-----------|-------------------------------|---------------------------|
| Build     | `cargo build`                 | exit 0                    |
| Tests     | `cargo test`                  | all pass (88 today + new) |
| One test  | `cargo test banned_reload`    | the new test passes       |
| Format    | `cargo fmt -- --check`        | exit 0, no diff           |

## Scope

**In scope** (the only files you should modify):
- `src/parsers/mod.rs` — change `ParserCtx` to hold a swappable config; update
  `is_banned`; add a unit test.
- `src/main.rs` — make `config` an `ArcSwap`, build `ParserCtx` with it, extend
  the SIGHUP handler to reload config.
- `src/routes.rs` — change `AppState.config` type and the one `timezone` read.
- `plans/README.md` — status row only.

**Out of scope** (do NOT touch):
- `src/config.rs` — `Config::load`/`validate` are reused as-is. Do not add fields.
- `src/cookies.rs`, `src/fetch.rs`, `src/notifier.rs` — no changes.
- **`host`/`port`/`notifier_webhook` live-reload** — explicitly NOT in this plan.
  `host`/`port` can't rebind a running listener; `notifier_webhook` is baked into
  `Notifier` at construction (`src/main.rs:58`) and rewiring it is a separate,
  larger change. Document them as restart-only (Step 5), do not implement.

## Git workflow

- Branch: `advisor/012-config-reload`
- Commit style: short imperative subject, no Conventional Commits prefix (e.g.
  "Reload config timezone and banned_users on SIGHUP").
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Make `ParserCtx` read `banned_users` from a swappable config

In `src/parsers/mod.rs`, replace the `banned_users: Vec<String>` field with a
shared config handle, and read it live in `is_banned`:

```rust
pub struct ParserCtx {
    pub fetcher: Arc<Fetcher>,
    pub cookies: Arc<arc_swap::ArcSwap<CookieJar>>,
    pub config: Arc<arc_swap::ArcSwap<crate::config::Config>>,
}

impl ParserCtx {
    pub fn is_banned(&self, author_id: &str) -> bool {
        self.config
            .load()
            .banned_users
            .iter()
            .any(|b| b == author_id)
    }
}
```

The three call sites (`json_post.rs:81`, `reels.rs:66`, `stories.rs:89`) all call
`ctx.is_banned(...)` and need **no change** — confirm with
`grep -rn "ctx.is_banned" src/parsers/`.

**Verify**: `cargo build` → fails to compile only at the `ParserCtx { … }` literal
in `src/main.rs` (field mismatch). Expected. Proceed.

### Step 2: Change `AppState.config` to a swappable config

In `src/routes.rs`, change the field type:

```rust
// in AppState (src/routes.rs:31)
pub config: Arc<arc_swap::ArcSwap<Config>>,
```

And the one read site:

```rust
// src/routes.rs:617
let tz = state.config.load().timezone;
```

`Config` is already imported (`use crate::config::Config;`, `src/routes.rs:1`).

**Verify**: `cargo build` → still fails only at the `AppState { … }` literal in
`main.rs`. Proceed.

### Step 3: Build the swappable config in `main.rs` and wire it through

In `src/main.rs`, wrap config in an `ArcSwap` right after it's loaded, and use it
for both `ParserCtx` and `AppState`:

```rust
let config = match args.config {
    Some(p) => Config::load(&p)?,
    None => { warn!("no config provided; using defaults"); Config::default() }
};
let config = Arc::new(ArcSwap::from_pointee(config));
```

Then update the two construction sites:

```rust
let ctx = Arc::new(ParserCtx {
    fetcher: fetcher.clone(),
    cookies: cookies.clone(),
    config: config.clone(),
});
```

```rust
let state = AppState {
    config: config.clone(),
    ctx,
    notifier,
    fetcher,
    embed_cache: /* unchanged */,
};
```

**Note**: `config` is read for `host`/`port` and `notifier_webhook` *before* this
point (the bind address at `src/main.rs:130`, the notifier at `:58`). Those reads
happen at startup on the initial value and are fine — just make sure you only
wrap `config` in `ArcSwap` **after** those startup reads, OR read through
`config.load()` at those sites. Pick whichever keeps the diff smallest; the
constraint is only that the bind address and notifier still get the startup value.
**If reordering is awkward, capture the host/port into locals before wrapping.**

**Verify**: `cargo build` → exit 0.

### Step 4: Extend the SIGHUP handler to reload config

In the existing `#[cfg(unix)]` SIGHUP block (`src/main.rs:96-122`), the loop
currently reloads only cookies. Add a config reload **alongside** it. You need the
config path and the swap handle in the closure:

```rust
#[cfg(unix)]
{
    let jar = cookies.clone();
    let cookies_path = args.cookies.clone();
    let cfg_swap = config.clone();
    let cfg_path = args.config.clone(); // Option<PathBuf>
    tokio::spawn(async move {
        use tokio::signal::unix::{signal, SignalKind};
        let mut hup = match signal(SignalKind::hangup()) {
            Ok(s) => s,
            Err(e) => { warn!("cannot install SIGHUP handler: {e}"); return; }
        };
        while hup.recv().await.is_some() {
            // cookies (existing behavior — keep exactly as-is)
            match validate_cookie_json_files(&cookies_path)
                .and_then(|_| CookieJar::load(&cookies_path)) {
                Ok(new_jar) => { let n = new_jar.len(); jar.store(Arc::new(new_jar));
                    info!("reloaded {n} cookie account(s) on SIGHUP"); }
                Err(e) => warn!("cookie reload failed: {e}"),
            }
            // config (new) — only if a config file was provided at startup
            if let Some(p) = &cfg_path {
                match Config::load(p) {
                    Ok(new_cfg) => { cfg_swap.store(Arc::new(new_cfg));
                        info!("reloaded config on SIGHUP (timezone + banned_users live; \
                               host/port/webhook need restart)"); }
                    Err(e) => warn!("config reload failed: {e}"),
                }
            }
        }
    });
}
```

`Config::load` already validates (`timezone` range), so a malformed or
out-of-range config is rejected and the old config stays live — same safety shape
as the cookie reload. **Do not** swap an unvalidated config.

**Verify**: `cargo build` → exit 0; `cargo fmt -- --check` → exit 0.

### Step 5: Unit test live `banned_users` reload

Add a `#[cfg(test)] mod tests` (or extend one if present) in `src/parsers/mod.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn banned_reload_takes_effect_after_swap() {
        use crate::config::Config;
        let swap = Arc::new(arc_swap::ArcSwap::from_pointee(Config::default()));
        // Build a ParserCtx-like check without a real Fetcher: test is_banned
        // logic against the swapped config directly.
        assert!(!swap.load().banned_users.iter().any(|b| b == "100012345"));
        let mut cfg = Config::default();
        cfg.banned_users = vec!["100012345".to_string()];
        swap.store(Arc::new(cfg));
        assert!(swap.load().banned_users.iter().any(|b| b == "100012345"),
            "swapping config must make the new banned id visible");
    }
}
```

(If constructing a full `ParserCtx` in a unit test is impractical because
`Fetcher` needs real cookies, testing the swap-then-read on the `ArcSwap<Config>`
directly — as above — is the accepted proof; `is_banned` is a one-line `.any`
over exactly that vector.)

**Verify**: `cargo test banned_reload` → 1 passed. `cargo test` → all pass.

## Test plan

- New test `banned_reload_takes_effect_after_swap` in `src/parsers/mod.rs`:
  proves a config swap is visible to a subsequent read (the exact mechanism
  `is_banned` relies on). Models after the existing per-module `#[cfg(test)]`
  blocks added by plan 006 (`single_photo.rs`, `photocom.rs`, `stories.rs`).
- Manual smoke (optional, not required for done): start with a `config.yaml`,
  add an id to `banned_users`, `kill -HUP <pid>`, confirm the log line
  "reloaded config on SIGHUP" and that the banned post now returns the "Banned"
  embed.

## Done criteria

ALL must hold:

- [ ] `cargo build` exits 0
- [ ] `cargo fmt -- --check` exits 0 (no diff)
- [ ] `cargo test` exits 0; `banned_reload_takes_effect_after_swap` passes
- [ ] `grep -n "banned_users: Vec<String>" src/parsers/mod.rs` returns **nothing**
      (field replaced)
- [ ] `grep -n "config.load().timezone" src/routes.rs` shows the live read
- [ ] `grep -n "config reload failed\|reloaded config on SIGHUP" src/main.rs`
      shows both the success and failure log lines
- [ ] `grep -rn "ctx.is_banned" src/parsers/` still shows the 3 unchanged call
      sites
- [ ] No files outside the in-scope list are modified (`git status`)
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report (do not improvise) if:

- The drift check shows `src/main.rs`, `src/routes.rs`, or `src/parsers/mod.rs`
  changed and the excerpts no longer match.
- There is **no** existing `#[cfg(unix)]` SIGHUP block in `main.rs` (plan 009 not
  present) — this plan extends it; without it, report and ask whether to add a
  fresh handler.
- Wrapping `config` in `ArcSwap` would force changes to `Notifier` construction or
  the bind-address logic beyond capturing a local — report the coupling instead of
  restructuring.
- `Config::load`'s signature differs from `load(path: &Path) -> Result<Config>`.

## Maintenance notes

For whoever owns this next:

- **Only `timezone` and `banned_users` are live.** `host`/`port` need a restart
  (can't rebind a live listener); `notifier_webhook` needs a restart (it's baked
  into `Notifier` at construction). This is a documented partial reload — the log
  line on SIGHUP says so. If you later want live webhook reload, that's a separate
  change to `Notifier` to read the URL from the same `ArcSwap<Config>`.
- **Windows**: the SIGHUP handler is `#[cfg(unix)]`. On Windows there's no reload
  path — config is load-once. That's pre-existing (cookies behave the same).
- **Reviewer**: confirm the validated-before-swap invariant holds — a bad
  `config.yaml` (e.g. `timezone: 99`) must log `config reload failed` and keep the
  previous config, never swap in an invalid one. This mirrors plan 009's
  cookie-validation safety.
- This composes with plan 011 (`banned_users` is now editable without a redeploy,
  giving a faster lever against an abusive author than restarting).
