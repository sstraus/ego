//! Journal-boundary contracts for passive session-local lifetime evidence.
use ego_core::compact::{CompactionConfig, IdleCompaction};
use ego_core::cost::CostDelta;
use ego_core::event::domain::{
    DomainEvent, Envelope, EventKind, GenerationRole, OccupancyProvenance,
};
use ego_core::event::id::{EventId, GenerationId, Producer, Seq, TurnId};
use ego_core::event::time::Timestamp;
use ego_core::generator::attempt::{AttemptRef, DeliveryCertainty, Outcome, UsageReport};
use ego_core::generator::types::StopReason;
use ego_core::state::{SessionState, Usage};
use ego_core::usage::{
    CacheRequestIdentity, CacheTransport, PromptCachePolicy, PromptCacheRequest,
};

const MODEL: &str = "provider/model";

struct Session {
    state: SessionState,
    log: Vec<DomainEvent>,
    seq: Seq,
    at: u64,
    items: usize,
    mode: Option<CacheTransport>,
    read_override: Option<u64>,
}

impl Default for Session {
    fn default() -> Self {
        Self {
            state: SessionState::default(),
            log: Vec::new(),
            seq: Seq::default(),
            at: 0,
            items: 0,
            mode: Some(CacheTransport::HttpFull),
            read_override: None,
        }
    }
}

impl Session {
    fn apply(&mut self, at: u64, kind: EventKind) {
        let seq = self.seq;
        self.seq.next();
        let event = DomainEvent {
            env: Envelope {
                seq,
                id: EventId::new(),
                turn: TurnId::new(),
                item: None,
                parent: None,
                ts: Timestamp::from_millis(at),
                producer: Producer::Main,
            },
            kind,
        };
        self.state.apply(&event);
        self.log.push(event);
    }

    fn request(&self) -> PromptCacheRequest {
        PromptCacheRequest {
            model: MODEL.into(),
            policy: PromptCachePolicy::default(),
            identity: Some(CacheRequestIdentity {
                scope: "a".repeat(64),
                frame: "b".repeat(64),
                prefix: format!("{:064x}", self.items + 1),
                previous_prefix: (self.items > 0).then(|| format!("{:064x}", self.items)),
                input_items: self.items + 1,
            }),
        }
    }

    fn observe(&mut self, gap: u64, hit: bool) {
        self.finish(
            gap,
            hit,
            100,
            StopReason::Stop,
            GenerationRole::Main,
            self.request(),
            true,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn finish(
        &mut self,
        gap: u64,
        hit: bool,
        latency: u64,
        stop: StopReason,
        role: GenerationRole,
        request: PromptCacheRequest,
        reported: bool,
    ) {
        let start = self.at + gap * 1_000;
        let attempt = AttemptRef::first(GenerationId::new());
        self.apply(
            start,
            EventKind::GenerationStarted {
                profile: None,
                attempt,
                role,
                snapshot: None,
                cache: Some(request),
            },
        );
        // Each appended request adds ten input tokens. A normal hit covers
        // the entire preceding input; the override models a partial shared head.
        let input = 110 + self.items as u64 * 10;
        let read = if hit {
            self.read_override.unwrap_or(input - 10)
        } else {
            0
        };
        let usage = Usage {
            uncached_input_tokens: input - read,
            cache_read_tokens: read,
            ..Usage::IDENTITY
        };
        self.at = start + latency;
        self.apply(
            self.at,
            EventKind::UsageUpdated {
                attempt: Some(attempt),
                usage,
                cost_delta: CostDelta::free(
                    ego_core::cost::CurrencyCode::new("USD").unwrap(),
                    ego_core::cost::CostAttribution::Table {
                        revision: ego_core::cost::PricingRevision("cache-fixture".into()),
                    },
                ),
            },
        );
        let mut outcome = Outcome::new(
            stop,
            DeliveryCertainty::DefinitelySent,
            if reported {
                UsageReport::Reported(usage)
            } else {
                UsageReport::Missing
            },
        );
        if let Some(mode) = self.mode {
            outcome = outcome.with_cache_transport(mode);
        }
        self.apply(self.at, EventKind::AttemptSettled { attempt, outcome });
        self.items += 1;
    }

    fn losses(&mut self, gaps: &[u64]) {
        if self.items == 0 {
            self.observe(0, true);
            self.observe(1, true);
            self.observe(95, true);
        }
        for gap in gaps {
            self.observe(*gap, false);
            assert!(
                self.state.prompt_cache.is_none(),
                "a miss must not schedule compaction"
            );
            self.observe(1, true);
        }
    }
}

// Catches: counting cold retries or publishing before three independent loss/rewarm cycles.
#[test]
fn cache_lifetime_requires_three_cycles_and_a_confirmed_rewarm() {
    let mut s = Session::default();
    s.losses(&[100, 105]);
    assert!(s.state.prompt_cache.is_none());
    s.observe(110, false);
    assert!(s.state.prompt_cache.is_none());
    s.observe(110, false);
    assert_eq!(s.state.cache_lifetime.estimate().unwrap().samples, 3);
    s.observe(1, true);
    let cache = s.state.prompt_cache.as_ref().unwrap();
    let estimate = cache.estimate.as_ref().unwrap();
    assert_eq!(
        (estimate.min_seconds, estimate.max_seconds, estimate.samples),
        (100, 110, 3)
    );
    assert_eq!(cache.expires_at.as_millis(), s.at + 94_900);
    let mut cold = Session::default();
    cold.observe(0, true);
    cold.observe(1, true);
    cold.observe(95, true);
    for _ in 0..4 {
        cold.observe(100, false);
    }
    cold.observe(1, true);
    assert!(cold.state.prompt_cache.is_none());
}

// Catches: keeping incompatible cohorts, accepting >10%, or ignoring a contradictory long hit.
#[test]
fn cache_lifetime_resets_outliers_and_contradictory_hits() {
    let mut s = Session::default();
    s.losses(&[100, 100, 111]);
    assert!(s.state.prompt_cache.is_none());
    s.losses(&[111, 111]);
    assert!(
        s.state.prompt_cache.is_none(),
        "an outlier also discards the old hit bound"
    );
    s.observe(105, true);
    assert_eq!(
        s.state.prompt_cache.as_ref().unwrap().ttl_seconds.get(),
        105
    );
    s.observe(111, true);
    assert!(s.state.cache_lifetime.estimate().is_none());
    assert!(s.state.prompt_cache.is_none());
}

// Catches: network/generation time being attributed to idle expiry, or equal-input retries adding samples.
#[test]
fn cache_lifetime_rejects_long_latency_and_equal_input_losses() {
    for equal_input in [false, true] {
        let mut s = Session::default();
        s.losses(&[100, 100]);
        let mut req = s.request();
        if equal_input {
            let identity = req.identity.as_mut().unwrap();
            identity.input_items -= 1;
            identity.prefix = identity.previous_prefix.clone().unwrap();
        }
        s.finish(
            100,
            false,
            if equal_input { 100 } else { 10_001 },
            StopReason::Stop,
            GenerationRole::Main,
            req,
            true,
        );
        s.observe(1, true);
        assert!(s.state.prompt_cache.is_none());
    }
}

// Catches: cancelled/unknown/missing reports, helper requests, or changed request identities completing a cohort.
#[test]
fn cache_lifetime_excludes_unproven_or_changed_observations() {
    for case in 0..11 {
        let mut s = Session::default();
        s.losses(&[100, 100]);
        let mut req = s.request();
        let mut stop = StopReason::Stop;
        let mut role = GenerationRole::Main;
        match case {
            0 => stop = StopReason::Aborted,
            1 => stop = StopReason::Error,
            2 => req.identity = None,
            3 => role = GenerationRole::Compaction,
            4 => req.model = "other/model".into(),
            5 => req.identity.as_mut().unwrap().scope = "c".repeat(64),
            6 => req.identity.as_mut().unwrap().frame = "c".repeat(64),
            7 => req.identity.as_mut().unwrap().previous_prefix = Some("c".repeat(64)),
            8 => req.identity.as_mut().unwrap().prefix = "not a digest".into(),
            10 => stop = StopReason::Deferred,
            _ => (),
        }
        s.finish(100, false, 100, stop, role, req, case != 9);
        s.observe(1, true);
        assert!(s.state.prompt_cache.is_none(), "case {case}");
    }
}

// Catches: replay losing the cohort, duplicate settlements adding samples, or learned state leaking to a new session.
#[test]
fn cache_lifetime_replays_and_resumes_without_duplicate_samples() {
    let mut s = Session::default();
    s.losses(&[100, 105]);
    let decoded = s
        .log
        .iter()
        .map(|ev| serde_json::from_str(&serde_json::to_string(ev).unwrap()).unwrap())
        .collect::<Vec<DomainEvent>>();
    s.state = SessionState::replay(decoded);
    s.losses(&[110]);
    assert!(s.state.prompt_cache.is_some());
    let before = s.state.cache_lifetime.clone();
    s.state.apply(s.log.last().unwrap());
    assert_eq!(s.state.cache_lifetime, before);
    assert_eq!(
        s.state.prompt_cache,
        SessionState::replay(s.log).prompt_cache
    );
    assert!(SessionState::default().cache_lifetime.is_empty());
}

// Catches: an empirical deadline overriding a known policy, or old journals guessing continuity.
#[test]
fn cache_lifetime_known_policy_wins_and_legacy_requests_do_not_learn() {
    let mut s = Session::default();
    s.losses(&[100, 100, 100]);
    let mut req = s.request();
    req.policy.ttl_seconds = std::num::NonZeroU64::new(300);
    s.finish(
        1,
        true,
        100,
        StopReason::Stop,
        GenerationRole::Main,
        req,
        true,
    );
    let cache = s.state.prompt_cache.as_ref().unwrap();
    assert!(cache.estimate.is_none());
    assert_eq!(cache.ttl_seconds.get(), 300);
    assert!(s.state.cache_lifetime.is_empty());
    let old: PromptCacheRequest = serde_json::from_value(serde_json::json!({ "model": MODEL, "ttl_seconds": null, "refresh_on_read": true, "lock_ttl": false })).unwrap();
    assert!(old.identity.is_none());
    for hit in [false, true, false, true, false, true] {
        s.finish(
            100,
            hit,
            100,
            StopReason::Stop,
            GenerationRole::Main,
            old.clone(),
            true,
        );
    }
    assert!(s.state.prompt_cache.is_none());
}

// Catches: a learned deadline not reaching the existing opt-in idle policy or bypassing its expired/margin checks.
#[test]
fn cache_lifetime_feeds_existing_idle_compaction_policy() {
    let mut s = Session::default();
    s.losses(&[100, 100, 100]);
    s.apply(
        s.at,
        EventKind::ContextWindowUpdated {
            used_tokens: 150_000,
            provenance: OccupancyProvenance::GenerationBoundary,
            window: Some(200_000),
        },
    );
    let config = CompactionConfig {
        idle: true,
        ..CompactionConfig::default()
    };
    let expiry = s.state.prompt_cache.as_ref().unwrap().expires_at;
    assert_eq!(
        config.idle_compaction(&s.state, MODEL, None, Timestamp::from_millis(s.at)),
        IdleCompaction::After(std::time::Duration::from_millis(64_900))
    );
    assert_eq!(
        config.idle_compaction(&s.state, MODEL, None, Timestamp::from_millis(s.at + 64_900)),
        IdleCompaction::Now { expires_at: expiry }
    );
    assert_eq!(
        config.idle_compaction(&s.state, MODEL, None, expiry),
        IdleCompaction::Never
    );
    assert_eq!(
        CompactionConfig::default().idle_compaction(
            &s.state,
            MODEL,
            None,
            Timestamp::from_millis(s.at)
        ),
        IdleCompaction::Never
    );
}

// Catches: failed accounting dropping the last scope anchor but retaining old loss samples.
#[test]
fn cache_lifetime_failure_cannot_bridge_two_request_scopes() {
    let mut s = Session::default();
    s.losses(&[100, 100]);
    s.finish(
        1,
        true,
        100,
        StopReason::Error,
        GenerationRole::Main,
        s.request(),
        true,
    );
    for (gap, hit) in [(1, true), (100, false), (1, true), (100, false), (1, true)] {
        let mut request = s.request();
        request.identity.as_mut().unwrap().scope = "d".repeat(64);
        s.finish(
            gap,
            hit,
            100,
            StopReason::Stop,
            GenerationRole::Main,
            request,
            true,
        );
    }
    assert!(s.state.prompt_cache.is_none());
    assert!(s.state.cache_lifetime.estimate().is_none());
}

// Catches: attributing a long preceding generation to the idle gap before a miss.
#[test]
fn cache_lifetime_rejects_a_long_hit_anchor() {
    let mut s = Session::default();
    s.losses(&[100, 100]);
    s.finish(
        1,
        true,
        20_000,
        StopReason::Stop,
        GenerationRole::Main,
        s.request(),
        true,
    );
    s.observe(100, false);
    s.observe(1, true);
    assert!(s.state.prompt_cache.is_none());
    assert!(s.state.cache_lifetime.estimate().is_none());
}

// Catches: three habitual cold arrivals becoming an unsafe upper-bound idle timer.
#[test]
fn cache_lifetime_requires_a_nearby_confirmed_hit_not_just_similar_misses() {
    let mut s = Session::default();
    s.observe(0, true);
    s.observe(1, true);
    s.losses(&[1_800, 1_820, 1_790]);
    assert!(s.state.cache_lifetime.estimate().is_none());
    assert!(s.state.prompt_cache.is_none());
    s.observe(1_700, true);
    let deadline = s.state.prompt_cache.as_ref().unwrap();
    assert_eq!(deadline.ttl_seconds.get(), 1_700);
    let estimate = deadline.estimate.as_ref().unwrap();
    assert_eq!(
        (
            estimate.confirmed_seconds,
            estimate.min_seconds,
            estimate.max_seconds
        ),
        (1_700, 1_790, 1_820)
    );
}

// Catches: rounding the hit/loss bracket outward or allowing a contradictory hit bound.
#[test]
fn cache_lifetime_confirmed_hit_bracket_has_a_ten_percent_boundary() {
    for (hit_gap, expected) in [(90, None), (91, Some(91)), (100, None)] {
        let mut s = Session::default();
        s.observe(0, true);
        s.observe(1, true);
        s.observe(hit_gap, true);
        s.losses(&[100, 100, 100]);
        assert_eq!(
            s.state
                .prompt_cache
                .as_ref()
                .map(|cache| cache.ttl_seconds.get()),
            expected,
            "hit gap {hit_gap}"
        );
    }
}

// Catches: predicting HTTP for missing proof, accepting incremental continuation, or combining modes.
#[test]
fn cache_lifetime_uses_only_one_observed_full_input_transport() {
    for mode in [
        None,
        Some(CacheTransport::WebSocketContinuation),
        Some(CacheTransport::WebSocketFresh),
        Some(CacheTransport::WebSocketReusedFull),
    ] {
        let mut s = Session::default();
        s.losses(&[100, 100]);
        s.mode = mode;
        s.observe(100, false);
        s.observe(1, true);
        assert!(s.state.prompt_cache.is_none(), "mixed mode {mode:?}");
    }
    for mode in [
        CacheTransport::HttpFull,
        CacheTransport::WebSocketFresh,
        CacheTransport::WebSocketReusedFull,
    ] {
        let mut s = Session {
            mode: Some(mode),
            ..Session::default()
        };
        s.losses(&[100, 100, 100]);
        assert_eq!(
            s.state.prompt_cache.as_ref().unwrap().ttl_seconds.get(),
            95,
            "same mode {mode:?}"
        );
    }
}

// Catches: a few shared-head cache tokens renewing an inferred whole-context lifetime.
#[test]
fn cache_lifetime_partial_read_cannot_renew_or_anchor_a_cohort() {
    for partial in [1, 189] {
        let mut s = Session::default();
        s.losses(&[100, 100, 100]);
        assert_eq!(s.state.prompt_cache.as_ref().unwrap().ttl_seconds.get(), 95);
        // The preceding request's full input is 190 tokens in this fixture.
        s.read_override = Some(partial);
        s.observe(95, true);
        assert!(s.state.prompt_cache.is_none(), "partial read {partial}");
        assert!(s.state.cache_lifetime.estimate().is_none());
        s.read_override = None;
        s.observe(100, false);
        s.observe(1, true);
        assert!(s.state.prompt_cache.is_none());
    }
}

// Catches: mirrored child requests silently refreshing a supposedly independent idle gap.
#[test]
fn cache_lifetime_mirrored_child_start_invalidates_only_empirical_deadline() {
    let mut s = Session::default();
    s.losses(&[100, 100, 100]);
    assert!(s.state.prompt_cache.is_some());
    let child = AttemptRef::first(GenerationId::new());
    s.apply(
        s.at + 1,
        EventKind::GenerationStarted {
            profile: None,
            attempt: child,
            role: GenerationRole::Subagent,
            cache: None,
            snapshot: None,
        },
    );
    assert!(s.state.prompt_cache.is_none());
    assert!(s.state.cache_lifetime.is_empty());
    s.apply(
        s.at + 2,
        EventKind::AttemptSettled {
            attempt: child,
            outcome: Outcome::new(
                StopReason::Stop,
                DeliveryCertainty::DefinitelySent,
                UsageReport::Missing,
            ),
        },
    );
    s.observe(1, true);
    assert!(s.state.prompt_cache.is_none());
}
