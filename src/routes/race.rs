//! Racing cookie accounts (and guest) for one scrape, and account health.

use super::dispatch::{scope_key, ParserKind};
use super::{run_parser, AppState, DISCORD_RESPONSE_BUDGET};
use crate::cookies::NOTIFY_FAILURE_THRESHOLD;
use crate::error::FacebedError;
use crate::fetch::ACCOUNT_OVERRIDE;
use crate::parsers::ParsedPost;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

/// A scrape attempt still running after this long is almost certainly a
/// Facebook slow-drip (~0.3 MB/s instead of ~2 MB/s, roughly 1 in 5 reads in a
/// 2026-09-29 prod sample; the same URL re-requested is usually fast). A 3 MB
/// reel page in slow mode takes ~10s, past Discord's crawler cutoff. Healthy
/// attempts finish in 1.3-2.7s.
const ACCOUNT_HEDGE_AFTER: Duration = Duration::from_millis(3000);

/// A new attempt needs about this long to finish; don't start one (hedge or
/// failover) that cannot beat the response deadline. Keeps a request to at
/// most one hedge round instead of piling multi-MB fetches onto one permit.
const ATTEMPT_MIN_REMAINING: Duration = Duration::from_millis(3000);

/// Scrape one post, racing cookie accounts (see [`race_identities`]). HTML
/// and Activity hydration share this; callers hold the fetch permit and
/// response deadline.
pub(super) async fn scrape_with_accounts(
    state: &AppState,
    path: &str,
    kind: ParserKind,
) -> Result<ParsedPost, FacebedError> {
    let key = scope_key(path);
    let order = account_order(&state.ctx.cookies.load(), key.as_deref());
    race_identities(state, path, kind, key, order, |identity| {
        let state = state.clone();
        let path = path.to_owned();
        async move {
            ACCOUNT_OVERRIDE
                .scope(identity, run_parser(&state, &path, kind))
                .await
        }
    })
    .await
}

/// Race identities (`Some(account)` or `None` = guest) for one post.
///
/// The first identity in `order` starts at once. The next starts as soon as
/// an attempt fails, or after [`ACCOUNT_HEDGE_AFTER`] without a result, and
/// the first success wins (the rest are aborted). Only healthy accounts are
/// used as a slow-read hedge; with a single account the hedge re-runs it.
///
/// Guest (no cookies) is the last resort, started only after a failure:
/// public page posts still load when the author blocked our account or the
/// cookie went bad, but guests hit a login wall on reels, photos and groups,
/// so it would be a useless hedge for a slow read.
///
/// Accounts are only penalized on account-level errors: checkpoint, rate
/// limit, login wall. Losing a race is not evidence: private groups fail
/// everywhere, slow-drips are transient, and "not a member of this group" is
/// per-scope, which affinity (set for the winner) already handles.
async fn race_identities<F, Fut>(
    state: &AppState,
    path: &str,
    kind: ParserKind,
    key: Option<String>,
    order: Vec<Option<usize>>,
    attempt: F,
) -> Result<ParsedPost, FacebedError>
where
    F: Fn(Option<usize>) -> Fut,
    Fut: std::future::Future<Output = Result<ParsedPost, FacebedError>> + Send + 'static,
{
    use tokio::time::Instant as TokioInstant;
    let deadline = crate::fetch::RESPONSE_DEADLINE
        .try_with(|deadline| *deadline)
        .unwrap_or_else(|_| Instant::now() + DISCORD_RESPONSE_BUDGET);
    let last_start = TokioInstant::from_std(deadline) - ATTEMPT_MIN_REMAINING;
    let race_started = TokioInstant::now();
    let label = |identity: Option<usize>| match identity {
        Some(i) => state
            .ctx
            .cookies
            .load()
            .label_at(i)
            .unwrap_or("")
            .to_owned(),
        None => String::from("guest"),
    };

    let mut queue: std::collections::VecDeque<Option<usize>> = order.into();
    let first = queue.pop_front().flatten();
    let mut solo_hedge = queue.is_empty();
    let mut guest_fallback = first.is_some();
    let mut tasks = tokio::task::JoinSet::new();
    let spawn = |tasks: &mut tokio::task::JoinSet<_>, identity: Option<usize>| {
        let fut = attempt(identity);
        tasks.spawn(crate::fetch::RESPONSE_DEADLINE.scope(deadline, async move {
            let started = TokioInstant::now();
            (identity, started.elapsed(), fut.await)
        }));
    };
    spawn(&mut tasks, first);
    let mut hedge_at = race_started + ACCOUNT_HEDGE_AFTER;
    let mut failed: Vec<(Option<usize>, FacebedError)> = Vec::new();
    let mut penalized: Vec<usize> = Vec::new();

    loop {
        let hedge = match queue.front() {
            Some(Some(i)) if state.ctx.cookies.load().in_cooldown(*i) => None,
            Some(next) => Some(*next),
            None if solo_hedge => Some(first),
            None => None,
        }
        .filter(|_| hedge_at <= last_start);
        tokio::select! {
            joined = tasks.join_next() => {
                let Some(joined) = joined else { break };
                let Ok((identity, elapsed, result)) = joined else {
                    warn!(path = %path, "scrape attempt aborted");
                    continue;
                };
                match result {
                    Ok(post) => {
                        if let Some(winner) = identity {
                            let jar = state.ctx.cookies.load();
                            jar.mark_ok(winner);
                            if let Some(k) = key {
                                jar.set_affinity(k, winner);
                            }
                        }
                        debug!(
                            path = %path,
                            kind = ?kind,
                            account = %label(identity),
                            scrape_ms = elapsed.as_millis(),
                            race_ms = race_started.elapsed().as_millis(),
                            "post scraped"
                        );
                        return Ok(post);
                    }
                    Err(e) => {
                        warn!(
                            path = %path,
                            kind = ?kind,
                            account = %label(identity),
                            error = %e,
                            attempt_ms = elapsed.as_millis(),
                            race_ms = race_started.elapsed().as_millis(),
                            "scrape attempt failed"
                        );
                        if let Some(account) = identity {
                            if is_account_level(&e) && !penalized.contains(&account) {
                                penalized.push(account);
                                penalize_account(state, account, &e);
                            }
                        }
                        solo_hedge = false;
                        if !is_retryable(&e) {
                            queue.clear();
                            guest_fallback = false;
                        }
                        failed.push((identity, e));
                        let next = queue.pop_front().or_else(|| {
                            std::mem::take(&mut guest_fallback).then_some(None)
                        });
                        if let Some(next) = next.filter(|_| TokioInstant::now() <= last_start) {
                            spawn(&mut tasks, next);
                            hedge_at = TokioInstant::now() + ACCOUNT_HEDGE_AFTER;
                        }
                    }
                }
            }
            _ = tokio::time::sleep_until(hedge_at), if hedge.is_some() => {
                let next = hedge.flatten();
                if queue.front() == Some(&next) {
                    queue.pop_front();
                }
                solo_hedge = false;
                info!(
                    path = %path,
                    account = %label(next),
                    race_ms = race_started.elapsed().as_millis(),
                    "slow scrape; hedging with another attempt"
                );
                spawn(&mut tasks, next);
                hedge_at = TokioInstant::now() + ACCOUNT_HEDGE_AFTER;
            }
        }
    }

    let err = final_error(failed);
    warn!(
        path = %path,
        kind = ?kind,
        error = %err,
        race_ms = race_started.elapsed().as_millis(),
        "embed render failed"
    );
    Err(err)
}

/// Pick the error to report after every attempt failed: a cookie account's
/// parser bug first (so the webhook sees it), then the last cookie-account
/// error, then anything. A guest login wall is expected noise.
fn final_error(mut failed: Vec<(Option<usize>, FacebedError)>) -> FacebedError {
    let pos = failed
        .iter()
        .rposition(|(identity, e)| identity.is_some() && matches!(e, FacebedError::Parse { .. }))
        .or_else(|| failed.iter().rposition(|(identity, _)| identity.is_some()))
        .or(failed.len().checked_sub(1));
    pos.map(|pos| failed.swap_remove(pos).1)
        .unwrap_or_else(|| FacebedError::no_data("no accounts available"))
}

/// Healthy accounts in configured priority, the account that last worked for
/// this group/profile hoisted to the front, then cooled-down accounts as a
/// last resort. An empty jar yields `[None]` (one guest attempt).
fn account_order(jar: &crate::cookies::CookieJar, key: Option<&str>) -> Vec<Option<usize>> {
    if jar.is_empty() {
        return vec![None];
    }
    let mut order = jar.priority_order();
    if let Some(pref) = key.and_then(|k| jar.affinity_for(k)) {
        if let Some(pos) = order.iter().position(|&i| i == pref && !jar.in_cooldown(i)) {
            let pref = order.remove(pos);
            order.insert(0, pref);
        }
    }
    order.into_iter().map(Some).collect()
}

fn is_retryable(e: &FacebedError) -> bool {
    match e {
        FacebedError::NoData(_)
        | FacebedError::LoginWall(_)
        | FacebedError::Parse { .. }
        | FacebedError::RateLimited { .. }
        | FacebedError::Checkpointed => true,
        FacebedError::Http(err) => err.is_timeout() || err.is_connect() || err.is_decode(),
        _ => false,
    }
}

/// Checkpoints, rate limits and login walls implicate the cookie itself.
/// Everything else only counts when another account succeeded instead.
fn is_account_level(e: &FacebedError) -> bool {
    matches!(
        e,
        FacebedError::RateLimited { .. } | FacebedError::Checkpointed | FacebedError::LoginWall(_)
    )
}

/// Apply cooldown and alerting appropriate to a failed attempt's cause.
fn penalize_account(state: &AppState, account_index: usize, e: &FacebedError) {
    let jar = state.ctx.cookies.load();
    match e {
        FacebedError::RateLimited { retry_after } => {
            jar.mark_rate_limited(account_index, *retry_after);
        }
        FacebedError::Checkpointed => {
            let count = jar.mark_checkpointed(account_index);
            maybe_notify_bad_account(state, account_index, count, e);
        }
        _ => {
            let count = jar.mark_failed(account_index);
            maybe_notify_bad_account(state, account_index, count, e);
        }
    }
}

/// Fire a Discord webhook when an account has failed [`NOTIFY_FAILURE_THRESHOLD`]
/// times in a row. Resets the counter afterwards so the next bad streak
/// re-alerts instead of spamming on every subsequent failure.
fn maybe_notify_bad_account(state: &AppState, account_index: usize, count: u64, e: &FacebedError) {
    if count != NOTIFY_FAILURE_THRESHOLD {
        return;
    }
    let label = state
        .ctx
        .cookies
        .load()
        .label_at(account_index)
        .unwrap_or("?")
        .to_owned();
    let msg = format!(
        "@everyone account `{label}` failed {count}× in a row — cookie likely expired or checkpointed. \
         Please re-export and update `cookies-{label}.json`. Last error: `{e}`"
    );
    warn!(account = %label, count, "notifying admin about bad account");
    state.notifier.warn(msg, None);
    state.ctx.cookies.load().reset_failure_count(account_index);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::test_support::{activity_post, test_state};

    use std::sync::Arc;

    #[tokio::test(start_paused = true)]
    async fn slow_first_read_loses_to_hedge_after_guest_fails() {
        use crate::error::FacebedError;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;

        let state = test_state();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cookies.json");
        std::fs::write(&path, r#"[{"name":"c_user","value":"1"}]"#).unwrap();
        state
            .ctx
            .cookies
            .store(Arc::new(crate::cookies::CookieJar::load(&path).unwrap()));
        let calls = Arc::new(AtomicUsize::new(0));
        let attempt = |identity: Option<usize>| {
            let call = calls.fetch_add(1, Ordering::SeqCst);
            async move {
                // Account slow-drips and is cut at 4.8s; its 3s hedge twin is
                // fast; guest (started on the failure) hits a login wall.
                let (delay, result) = match (identity, call) {
                    (Some(0), 0) => (4800, Err(FacebedError::no_data("cut"))),
                    (Some(0), _) => (2700, Ok(activity_post())),
                    (None, _) => (700, Err(FacebedError::LoginWall("guest".into()))),
                    _ => unreachable!(),
                };
                tokio::time::sleep(Duration::from_millis(delay)).await;
                result
            }
        };
        let result = super::race_identities(
            &state,
            "groups/1/posts/2/",
            ParserKind::JsonPost,
            None,
            vec![Some(0)],
            attempt,
        )
        .await;
        assert!(result.is_ok());
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert!(!state.ctx.cookies.load().in_cooldown(0));
    }

    #[test]
    fn final_error_prefers_cookie_parse_bug_over_later_failures() {
        use crate::error::FacebedError;
        let err = super::final_error(vec![
            (Some(0), FacebedError::parse("bug")),
            (Some(1), FacebedError::no_data("private")),
            (None, FacebedError::LoginWall("guest".into())),
        ]);
        assert!(matches!(err, FacebedError::Parse { .. }));
        let err = super::final_error(vec![
            (Some(0), FacebedError::no_data("private")),
            (None, FacebedError::LoginWall("guest".into())),
        ]);
        assert!(matches!(err, FacebedError::NoData(_)));
    }

    #[test]
    fn account_order_prefers_affinity_and_defers_cooled_accounts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cookies.json");
        std::fs::write(
            &path,
            r#"{"accounts":[{"label":"a","entries":[]},{"label":"b","entries":[]},{"label":"c","entries":[]}]}"#,
        )
        .unwrap();
        let jar = crate::cookies::CookieJar::load(&path).unwrap();
        assert_eq!(super::account_order(&jar, None), [0, 1, 2].map(Some));
        jar.mark_failed(0);
        jar.set_affinity("groups/1".into(), 2);
        assert_eq!(
            super::account_order(&jar, Some("groups/1")),
            [2, 1, 0].map(Some)
        );
        assert_eq!(super::account_order(&jar, None), [1, 2, 0].map(Some));
        assert_eq!(
            super::account_order(&crate::cookies::CookieJar::empty(), None),
            [None]
        );
    }

    #[test]
    fn rate_limit_and_checkpoint_are_retryable() {
        use crate::error::FacebedError;

        assert!(super::is_retryable(&FacebedError::rate_limited(Some(30))));
        assert!(super::is_retryable(&FacebedError::checkpointed()));
    }
}
