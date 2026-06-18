# Plan 009 (SPIKE): Reload cookies on SIGHUP without redeploying

> **Executor instructions**: This is a spike — its job is to land a *minimal*
> working cookie-reload mechanism and surface anything that resists. Follow the
> steps, run every verification command, and honor the STOP conditions. The
> mechanism decision in Step 1 is already made and explained; you implement it.
> When done, update the status row in `plans/README.md` and fill the "Spike
> outcome" note at the bottom.
>
> **Drift check (run first)**: `git diff --stat cbed1e3..HEAD -- src/fetch.rs src/routes.rs src/parsers/mod.rs src/main.rs src/cookies.rs Cargo.toml`
> If any in-scope file changed since this plan was written, compare the "Current
> state" excerpts against the live code before proceeding; on a mismatch, treat
> it as a STOP condition. Note: plans 001/003/004 also edit `src/fetch.rs` and
> `src/routes.rs` — if they landed, match on code shape, not line numbers.

## Status

- **Priority**: P3
- **Effort**: M (spike)
- **Risk**: MED
- **Depends on**: none
- **Category**: direction
- **Planned at**: commit `cbed1e3`, 2026-06-17

## Why this matters

The single most common operational failure for this service is a cookie account
going stale (expired / checkpointed). The code already detects it and alerts the
admin (`src/routes.rs:615-632`), and the README tells the operator to re-export
the cookie — but the only way to load the new cookie is a full container
redeploy, because cookies are read exactly once at startup (`src/main.rs:54`).
For an always-on self-hosted service that is real friction.

This spike adds a **SIGHUP-triggered reload**: drop a fresh `cookies-*.json` in
place, send the process `SIGHUP` (`docker compose kill -s HUP <svc>` or
`kill -HUP <pid>`), and the new cookies take effect with no downtime.

### Mechanism decision (the spike's investigation, already resolved)

Three mechanisms were considered:

1. **SIGHUP + whole-jar swap via `arc-swap`** ← chosen. Lowest blast radius:
   `CookieJar` and its 30 tests stay **completely unchanged**; we only wrap the
   shared jar in `arc_swap::ArcSwap` and call the existing `CookieJar::load`
   again on signal. No new HTTP surface, no auth to design, uses tokio's
   already-enabled `signal` feature (`Cargo.toml:21`).
2. **Internal `RwLock<JarState>` refactor of `CookieJar`** — rejected for this
   spike: it forces `account_at`/`label_at` to stop returning borrows, which
   ripples through every accessor and every test. Strictly more churn for the
   same outcome.
3. **HTTP reload endpoint** — rejected for now: needs authentication design
   (an unauthenticated reload endpoint is a DoS/abuse vector). Revisit only if
   signals are unavailable in the deployment.

Trade-off of the whole-jar swap: on reload, per-account cooldown and
consecutive-failure counters and the affinity map reset to empty (a brand-new
jar). That is acceptable and arguably desirable — you just supplied fresh
cookies, so starting the health bookkeeping clean is fine.

## Current state

`CookieJar` is shared as `Arc<CookieJar>` by two holders, both reached from
`AppState`:

- `src/fetch.rs:22-26` — `Fetcher { client, cookies: Arc<CookieJar>, media_size_cache }`.
- `src/parsers/mod.rs:32-36` — `ParserCtx { fetcher, cookies: Arc<CookieJar>, banned_users }`.

`CookieJar::load(&Path) -> anyhow::Result<CookieJar>` (`src/cookies.rs:126`)
already does the full multi-file load and returns an owned jar — **reuse it
verbatim for reload**.

### The jar is built once in `src/main.rs:54-56`

```rust
    let cookies = Arc::new(CookieJar::load(&args.cookies)?);
    let fetcher = Arc::new(Fetcher::new(cookies.clone())?);
    let notifier = Notifier::new(config.notifier_webhook.clone(), fetcher.client().clone());
```

and `args.cookies: PathBuf` (`src/main.rs:36`, the `--cookies` arg) is the path
to reload from.

### Every call site that reaches into the jar (these all gain `.load()`)

`arc_swap::ArcSwap<T>::load()` returns a `Guard` that derefs to `&T`, so
`x.cookies.load().len()` calls `CookieJar::len` transparently. The full list:

- `src/fetch.rs`: `:149`, `:150` (`len`), `:157` (`account_at` — needs a bound
  guard), `:386-407` (`request_for`, `account_at` — needs a bound guard),
  `:465` (`is_empty`), `:573` (`len`), `:577` (`in_cooldown`), `:704-708`
  (`share_account_label`, `label_at` — needs a bound guard), `:720`
  (`attach_share_identity`, `account_at` — needs a bound guard).
- `src/routes.rs`: `:370` (`len`), `:386` (`in_cooldown`), `:393` (`affinity_for`),
  `:419` (`mark_ok`), `:421` (`set_affinity`), `:429`/`:454` (`label_at`, inline
  in a tracing macro — single `.load()` is fine), `:442` (`label_at` bound to a
  `let label` used on later lines — needs a bound guard), `:597`
  (`mark_checkpointed`), `:601` (`mark_failed`), `:606` (`affinity_for`), `:607`
  (`forget_affinity`), `:619-623` (`label_at`, multi-line chain ending in
  `.to_owned()` — single `.load()` is fine), `:631` (`reset_failure_count`).
- `src/main.rs`: `:58` (`is_empty`).

The four "needs a bound guard" sites are the only non-uniform edits — exact
rewrites are in Step 3.

### The two `account_at` borrow sites (exact current code)

`request_for` (`src/fetch.rs:393-405`):

```rust
        if use_cookies {
            let acc = ACCOUNT_OVERRIDE
                .try_with(|i| self.cookies.account_at(*i))
                .ok()
                .flatten()
                .or_else(|| self.cookies.account_at(0));
            if let Some(acc) = acc {
                account_label = acc.label.clone();
                req = req.header("cookie", acc.header_value());
                if let Some(ua) = acc.user_agent.as_deref() {
                    user_agent = ua;
                }
            }
        }
```

`attach_share_identity` (`src/fetch.rs:717-726`):

```rust
    let Some(account_index) = account_index else {
        return req.header("user-agent", fallback_ua);
    };
    let Some(acc) = fetcher.cookies.account_at(account_index) else {
        return req.header("user-agent", fallback_ua);
    };
    let ua = acc.user_agent.as_deref().unwrap_or(DEFAULT_USER_AGENT);
    req.header("cookie", acc.header_value())
        .header("user-agent", ua)
```

`share_account_label` (`src/fetch.rs:704-709`):

```rust
fn share_account_label(fetcher: &Fetcher, account_index: Option<usize>) -> String {
    account_index
        .and_then(|i| fetcher.cookies.label_at(i))
        .unwrap_or("")
        .to_owned()
}
```

(The `label_at` here is inside a closure that returns `&str`; with `.load()` the
Guard must be bound outside the closure — see Step 3.)

## Commands you will need

| Purpose       | Command                          | Expected on success                |
|---------------|----------------------------------|------------------------------------|
| Build (updates Cargo.lock) | `cargo build`       | exit 0                             |
| Tests         | `cargo test`                     | all pass (the existing suite, unchanged) |
| Format check  | `cargo fmt --check`              | exit 0, no diff                    |
| Format apply  | `cargo fmt`                      | rewrites in place                  |
| Manual reload | run server, then `kill -HUP <pid>` | log line `reloaded N cookie account(s) on SIGHUP` |

CI parity: `cargo test --locked` and `cargo fmt --check`. Because you add a
dependency, **commit the updated `Cargo.lock`** or `--locked` will fail.

## Scope

**In scope**:
- `Cargo.toml` — add `arc-swap = "1"`.
- `src/main.rs` — change the jar to `Arc<ArcSwap<CookieJar>>`, add the SIGHUP task.
- `src/fetch.rs` — field type on `Fetcher`, `Fetcher::new` signature, `.load()` edits.
- `src/parsers/mod.rs` — field type on `ParserCtx`.
- `src/routes.rs` — `.load()` edits at the enumerated sites.
- `Cargo.lock` — commit the dependency update.

**Out of scope** (do NOT touch):
- `src/cookies.rs` — `CookieJar` and all its methods/tests stay byte-for-byte
  unchanged. If you find yourself editing it, STOP (see STOP conditions).
- Any HTTP route or endpoint (no reload endpoint in this spike).
- Re-running the live cookie health-check after reload — deferred (see
  Maintenance notes). Do not add it unless it is a trivial reuse of the existing
  startup block and you have spare confidence.
- Windows signal handling — gate the SIGHUP task with `#[cfg(unix)]`; the
  service targets Linux/Docker.

## Git workflow

- Branch: `advisor/009-live-cookie-reload`.
- Commit style: short imperative subject (e.g. `Reload cookies on SIGHUP`).
- Commit `Cargo.lock` alongside `Cargo.toml`.
- Do NOT push or open a PR unless instructed.

## Steps

### Step 1: Add the dependency

In `Cargo.toml` `[dependencies]`, add:

```toml
arc-swap = "1"
```

**Verify**: `cargo build` → exit 0 (pulls the crate; updates `Cargo.lock`).

### Step 2: Change the jar type at the field declarations

- `src/fetch.rs:24`: `cookies: Arc<CookieJar>,` → `cookies: Arc<arc_swap::ArcSwap<CookieJar>>,`
- `src/fetch.rs:130`: `pub fn new(cookies: Arc<CookieJar>)` → `pub fn new(cookies: Arc<arc_swap::ArcSwap<CookieJar>>)`
- `src/parsers/mod.rs:34`: `pub cookies: Arc<CookieJar>,` → `pub cookies: Arc<arc_swap::ArcSwap<CookieJar>>,`

(The `Fetcher` body stores `cookies` unchanged; only the type differs.)

**Verify**: `cargo build` will now produce many errors at the call sites — that
is expected; Step 3 fixes them.

### Step 3: Insert `.load()` at every call site

For all the **uniform** sites listed in "Current state", insert `.load()` right
after `.cookies` (or `.ctx.cookies`). Example: `self.cookies.len()` →
`self.cookies.load().len()`; `state.ctx.cookies.mark_ok(account_index)` →
`state.ctx.cookies.load().mark_ok(account_index)`.

For the **four bound-guard sites**, use these exact rewrites:

`request_for` (`src/fetch.rs:393-405`):

```rust
        if use_cookies {
            let guard = self.cookies.load();
            let acc = ACCOUNT_OVERRIDE
                .try_with(|i| guard.account_at(*i))
                .ok()
                .flatten()
                .or_else(|| guard.account_at(0));
            if let Some(acc) = acc {
                account_label = acc.label.clone();
                req = req.header("cookie", acc.header_value());
                if let Some(ua) = acc.user_agent.as_deref() {
                    user_agent = ua;
                }
            }
        }
```

`Fetcher::check_cookie_account` (`src/fetch.rs:157`):

```rust
        let guard = self.cookies.load();
        let Some(acc) = guard.account_at(account_index) else {
```

(leave the rest of that `else { ... }` block and the body below unchanged; `acc`
now borrows `guard`, which lives to the end of the function.)

`attach_share_identity` (`src/fetch.rs:720`):

```rust
    let guard = fetcher.cookies.load();
    let Some(acc) = guard.account_at(account_index) else {
        return req.header("user-agent", fallback_ua);
    };
```

`share_account_label` (`src/fetch.rs:704-709`):

```rust
fn share_account_label(fetcher: &Fetcher, account_index: Option<usize>) -> String {
    let guard = fetcher.cookies.load();
    account_index
        .and_then(|i| guard.label_at(i))
        .unwrap_or("")
        .to_owned()
}
```

`src/routes.rs:442` (the `let label = ...` bound across later lines):

```rust
                let guard = state.ctx.cookies.load();
                let label = guard.label_at(account_index).unwrap_or("?");
```

(Insert the `let guard = ...` immediately before, and keep `guard` in scope for
the `warn!` that uses `label` on the next line — both are in the same block.)

**Verify**: `cargo build` → exit 0. `cargo test` → all pass (no test changed —
this proves the refactor preserved behavior).

### Step 4: Build the jar as an `ArcSwap` and add the SIGHUP reload task in `src/main.rs`

Add the import near the top:

```rust
use arc_swap::ArcSwap;
```

Change `src/main.rs:54`:

```rust
    let cookies = Arc::new(ArcSwap::from_pointee(CookieJar::load(&args.cookies)?));
```

Change `src/main.rs:58` (`if !cookies.is_empty()`):

```rust
    if !cookies.load().is_empty() {
```

After the startup cookie-check spawn block (after `src/main.rs:92`), add:

```rust
    #[cfg(unix)]
    {
        let jar = cookies.clone();
        let cookies_path = args.cookies.clone();
        tokio::spawn(async move {
            use tokio::signal::unix::{signal, SignalKind};
            let mut hup = match signal(SignalKind::hangup()) {
                Ok(s) => s,
                Err(e) => {
                    warn!("cannot install SIGHUP handler: {e}");
                    return;
                }
            };
            while hup.recv().await.is_some() {
                match CookieJar::load(&cookies_path) {
                    Ok(new_jar) => {
                        let n = new_jar.len();
                        jar.store(Arc::new(new_jar));
                        info!("reloaded {n} cookie account(s) on SIGHUP");
                    }
                    Err(e) => warn!("cookie reload failed: {e}"),
                }
            }
        });
    }
```

(`cookies.clone()` and `ctx`'s `cookies: cookies.clone()` at `src/main.rs:96`
now clone an `Arc<ArcSwap<CookieJar>>` — no other change needed there.)

**Verify**: `cargo build` → exit 0; `cargo test` → all pass; `cargo fmt --check`
→ exit 0 (run `cargo fmt` first if needed).

### Step 5: Manual SIGHUP smoke test

1. `cargo run -- --cookies cookies.json` (use a real or example cookies file).
2. Note the PID (`echo $!` if backgrounded, or `pgrep facebed`).
3. Edit/replace `cookies.json`, then `kill -HUP <pid>`.
4. Confirm the log shows `reloaded N cookie account(s) on SIGHUP`.
5. Send `SIGHUP` again with a deliberately broken JSON file → confirm
   `cookie reload failed: ...` is logged **and the server keeps running** with
   the previous jar (the swap only happens on `Ok`).

## Test plan

- No new unit tests are required: the value is that the **existing** `cargo test`
  suite still passes after the `ArcSwap` refactor, proving behavior was
  preserved. `cookies.rs`'s 30 tests are untouched and must stay green.
- Reload itself is verified by the Step 5 manual smoke test (signal handling and
  process lifecycle are not unit-testable here without a harness — do not add one).

## Done criteria

ALL must hold:

- [ ] `cargo build` exits 0.
- [ ] `cargo test` exits 0; **no test file was modified** (`git diff --stat -- src/cookies.rs` is empty).
- [ ] `cargo fmt --check` exits 0.
- [ ] `grep -rn "Arc<CookieJar>" src/` returns **no** matches (all became `ArcSwap`).
- [ ] `grep -n "SignalKind::hangup" src/main.rs` returns 1 match.
- [ ] `Cargo.lock` is updated and staged (contains `arc-swap`).
- [ ] Step 5 manual test logged both a successful reload and a survived bad-file reload.
- [ ] No files outside the in-scope list are modified.
- [ ] `plans/README.md` status row for 009 updated and the "Spike outcome" note filled.

## STOP conditions

Stop and report (do not improvise) if:

- The "Current state" excerpts do not match the live code (drift since `cbed1e3`).
- The refactor requires editing `src/cookies.rs` to compile. It should not — if
  it does, the API has changed since this plan was written; report the new shape
  rather than rewriting `CookieJar`.
- The borrow checker rejects a site **not** in the four enumerated bound-guard
  sites, and the fix is not a mechanical "bind the guard to a `let` first".
  Report the site.
- `cargo build` fails to resolve `arc-swap` (offline/registry issue) — report;
  do not vendor or hand-roll a swap primitive.

## Maintenance notes

- A reviewer should confirm `cookies.rs` is unchanged and that every `.load()`
  insertion is on the hot path's read side only (no behavioral change to cooldown
  or affinity logic).
- Deferred follow-ups (out of scope here, each a small future task):
  - Re-run `check_cookie_accounts` after a reload so the logs/alerts reflect the
    new cookies immediately (today they reflect startup only).
  - An authenticated HTTP `/reload` endpoint for environments where sending Unix
    signals is awkward (would pair naturally with the `/healthz` direction option).
  - Optional `notify`-crate file-watch to auto-reload on `cookies*.json` change.
- Interaction: if plan 008 (embed cache) lands, a cookie reload does **not**
  invalidate cached embeds — they age out via the cache TTL. That is fine (the
  cached bodies are still valid posts), but note it in review.

## Spike outcome (fill this in when done)

- Mechanism shipped: SIGHUP + `arc-swap` whole-jar swap.
- Call sites touched: <count>.
- Anything that resisted / was deferred: <notes>.
- Recommendation on the deferred follow-ups: <keep / drop / promote to a plan>.
