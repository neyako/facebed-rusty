# Plan 005: Switch the release build from size-optimized to speed-optimized

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving on. If a
> STOP condition occurs, stop and report. When done, update the status row for
> this plan in `plans/README.md`.
>
> **Drift check (run first)**: `git diff --stat 3b6521c..HEAD -- Cargo.toml`
> If `Cargo.toml` changed since this plan was written, compare the "Current
> state" excerpt below against the live code; on a mismatch, STOP.

## Status

- **Priority**: P2
- **Effort**: S
- **Risk**: LOW
- **Depends on**: none
- **Category**: perf
- **Planned at**: commit `3b6521c`, 2026-06-17

## Why this matters

facebed's per-request work is CPU-bound: it parses large Facebook HTML with
html5ever and walks the resulting JSON recursively (`jq::first/all`) many times
per request. The release profile currently compiles with `opt-level = "z"`,
which optimizes aggressively for **binary size** at a measurable cost to
**runtime speed** — the opposite of what a latency-sensitive crawler wants. The
deploy artifact is a static binary in a `scratch` image, so a modest size
increase is cheap; the per-request CPU saving is the thing that matters. Moving
to `opt-level = 3` (or `2`) trades a larger binary for faster execution. This is
a one-line change; the speed gain is real for CPU-bound code but its magnitude
varies, so the done-criteria verify correctness and the maintenance note explains
how to measure the win.

## Current state

`Cargo.toml` (lines 13-18):

```toml
[profile.release]
lto = true
codegen-units = 1
strip = true
opt-level = "z"
```

`lto = true`, `codegen-units = 1`, and `strip = true` are good for both size and
speed — keep them. Only `opt-level` changes.

### Convention

- The Dockerfile builds `--release` (`Dockerfile`: `cargo build --release`), and
  CI builds the release image. No separate profile override exists. This change
  applies to every release/Docker build automatically.

## Commands you will need

| Purpose         | Command                  | Expected on success     |
|-----------------|--------------------------|-------------------------|
| Release build   | `cargo build --release`  | exit 0                  |
| Tests           | `cargo test --locked`    | all pass                |
| Format check    | `cargo fmt --check`      | exit 0 (no code changed) |
| Binary runs     | `./target/release/facebed --help` | prints usage, exit 0 |

## Scope

**In scope** (the only file you may modify):
- `Cargo.toml` — the `opt-level` value in `[profile.release]`.

**Out of scope** (do NOT touch):
- Any other field in `[profile.release]` (`lto`, `codegen-units`, `strip`) — they
  already help; leave them.
- `[dependencies]`, `[profile.dev]` (there is none — do not add one), the
  Dockerfile, CI.

## Git workflow

- Branch: `advisor/005-opt-level`.
- Commit style: short imperative subject (e.g. "Optimize release build for speed").
- Do NOT push or open a PR unless instructed.

## Steps

### Step 1: Change `opt-level`

In `Cargo.toml`, change:

```toml
opt-level = "z"
```

to:

```toml
opt-level = 3
```

(Use the integer `3`, not a string. If a later validation shows the binary-size
increase is unacceptable for your deploy, `2` is the balanced fallback — but
default to `3` for this speed-focused service.)

**Verify**: `cargo build --release` → exit 0.

### Step 2: Confirm the binary still works and nothing else changed

**Verify**:
- `./target/release/facebed --help` → prints the clap usage for `facebed` and
  exits 0.
- `cargo test --locked` → all pass (no source changed, so this is a sanity gate).
- `git diff --stat` → shows only `Cargo.toml` changed, one line.

## Test plan

There is no automated way to assert "faster" in this repo (no benchmark
harness), so the test plan is correctness-only:

- `cargo test --locked` passes (behavior unchanged).
- The release binary builds and runs (`--help`).
- Do NOT add a benchmark harness as part of this plan — out of scope.

## Done criteria

ALL must hold:

- [ ] `cargo build --release` exits 0.
- [ ] `./target/release/facebed --help` exits 0 and prints usage.
- [ ] `cargo test --locked` exits 0.
- [ ] `grep -n 'opt-level' Cargo.toml` shows `opt-level = 3` and no remaining
      `"z"`.
- [ ] `git status` shows only `Cargo.toml` modified.
- [ ] `plans/README.md` status row for 005 updated.

## STOP conditions

Stop and report back if:

- The drift check shows `Cargo.toml`'s `[profile.release]` no longer matches the
  excerpt.
- `cargo build --release` fails (it should not — this is a profile value change).

## Maintenance notes

- **Validate the win before relying on it.** With the server running, time the
  crawler path the README documents:
  `curl -A 'Discordbot/2.0' 'http://127.0.0.1:9812/<facebook-path>'` against a
  cookie-viewable post, comparing a `z` build to the `3` build (and watch the
  `parse_ms` / `total_ms` fields in the logs). If `3` shows no improvement over
  `2` for your workload, prefer `2` for the smaller binary.
- The binary will grow (size-vs-speed trade). The `scratch` deploy image absorbs
  this cheaply; only revisit if image size becomes a constraint.
- This composes with plan 001 (algorithmic fix to the partial-fetch scan): 001
  removes wasted work; 005 makes the remaining work compile faster. They are
  independent.
