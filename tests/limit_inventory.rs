//! Synthetic native-home and quota fixtures for the public inventory contract.
use std::{fs, io::Write, path::Path};

use chrono::{DateTime, Duration, Utc};
use rtr::{
    limits::{
        evaluate, Blocker, CodexSlot, Evaluation, Hold, HoldScope, Meter, MeterKey, MeterScope,
        MeterSource, MeterWindow, ModelFamily, ProfileLimits,
    },
    native_usage::{read_owned, NativeRead},
    paths::Paths,
    quota::{read_holds, read_origins, OriginIndex},
};
use serde_json::{json, Value};

fn now() -> DateTime<Utc> {
    "2026-11-21T18:00:00Z".parse().unwrap()
}

fn write(home: &Path, name: &str, content: &str) {
    let file = home.join(name);
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(file, content).unwrap();
}

fn claude_cache(home: &Path, at: DateTime<Utc>, utilization: Value) -> NativeRead {
    write(
        home,
        ".claude.json",
        &json!({"cachedUsageUtilization": {"fetchedAtMs": at.timestamp_millis(), "utilization": utilization}}).to_string(),
    );
    read_owned(home, "claude", &OriginIndex::default(), now())
}

fn meter(read: &NativeRead, window: MeterWindow, scope: MeterScope) -> &Meter {
    let key = MeterKey { window, scope };
    read.meters.iter().find(|meter| meter.key == key).unwrap()
}

#[test]
fn claude_merges_only_matching_legacy_fields_and_retains_scopes_and_expiry() {
    let t = tempfile::tempdir().unwrap();
    let at = now() - Duration::hours(1);
    let reset = now() + Duration::days(1);
    let precise = reset + Duration::microseconds(116);
    let read = claude_cache(
        t.path(),
        at,
        json!({
            "five_hour": {"utilization": 20, "resets_at": null},
            "seven_day": {"utilization": 67, "resets_at": reset},
            "limits": [
                {"kind":"session", "percent":0, "resets_at":null},
                {"kind":"weekly", "group":"weekly_all", "percent":68, "resets_at":precise},
                {"kind":"weekly_all", "percent":"broken", "resets_at":{}},
                {"kind":"weekly_scoped", "percent":100, "resets_at":reset, "is_active":false,
                 "scope":{"model":{"id":null,"display_name":"Fable"}}},
                {"kind":"weekly_scoped", "percent":100, "resets_at":reset,
                 "scope":{"model":{"id":"not-fable-5-2","display_name":"Fable"}}},
                {"kind":"weekly_scoped", "percent":100, "resets_at":reset,
                 "scope":{"model":{"id":"claude-fable-5-2"}, "surface":"api"}},
                {"kind":"weekly_scoped", "percent":8, "resets_at":now(),
                 "scope":{"surface":{"id":"web"}}},
                {"kind":"weekly_scoped", "percent":101, "resets_at":reset},
                "malformed"
            ]
        }),
    );
    assert_eq!(read.meters.len(), 6);
    assert!(read.meters.iter().all(|m| m.observed_at == at));
    let short = meter(&read, MeterWindow::FiveHour, MeterScope::AllModels);
    assert_eq!(short.used_percent.get(), 0.0);
    assert_eq!(short.reset_at, None);
    let week = meter(&read, MeterWindow::Week, MeterScope::AllModels);
    assert_eq!(week.used_percent.get(), 68.0);
    assert_eq!(week.reset_at, Some(precise));
    let fable = meter(
        &read,
        MeterWindow::Week,
        MeterScope::Model(ModelFamily::Fable),
    );
    assert_eq!(fable.used_percent.get(), 100.0);
    assert_eq!(fable.reset_at, Some(reset));
    for (label, reset) in [
        ("not-fable-5-2", reset),
        ("claude-fable-5-2 / api", reset),
        ("web", now()),
    ] {
        let row = meter(
            &read,
            MeterWindow::Week,
            MeterScope::Unclassified(label.into()),
        );
        assert_eq!(row.reset_at, Some(reset));
    }
    let limits = ProfileLimits {
        meters: read.meters,
        ..Default::default()
    };
    assert_eq!(
        evaluate(&limits, ModelFamily::Opus, now()),
        Evaluation::Unknown
    );
    assert!(
        matches!(evaluate(&limits, ModelFamily::Fable, now()), Evaluation::Out(blockers)
        if matches!(blockers.as_slice(), [Blocker::Meter(m)] if m.key.scope == MeterScope::Model(ModelFamily::Fable)))
    );
    assert_eq!(
        evaluate(&limits, ModelFamily::Fable, reset),
        Evaluation::Unknown
    );
}

#[test]
fn claude_fields_fall_back_independently_within_one_fetch() {
    let t = tempfile::tempdir().unwrap();
    let reset = now() + Duration::hours(2);
    let utilization = json!({
        "five_hour": {"utilization": 28,"resets_at": "broken"},
        "seven_day": {"utilization": 42,"resets_at": reset},
        "limits": [
            {"group":"session", "percent":-1, "resets_at":reset},
            {"kind":"weekly_all", "percent":43, "resets_at":null},
            {"kind":"weekly_scoped", "percent":100, "resets_at":now() - Duration::days(1),
             "scope":{"model":{"id":"claude-fable-5-2[1m]"}}}
        ]
    });
    let future = claude_cache(t.path(), now() + Duration::seconds(1), utilization.clone());
    assert!(future.meters.is_empty());
    let read = claude_cache(t.path(), now(), utilization);
    let short = meter(&read, MeterWindow::FiveHour, MeterScope::AllModels);
    assert_eq!(short.used_percent.get(), 28.0);
    assert_eq!(short.reset_at, Some(reset));
    let week = meter(&read, MeterWindow::Week, MeterScope::AllModels);
    assert_eq!(week.used_percent.get(), 43.0);
    assert_eq!(week.reset_at, Some(reset));
    let fable = meter(
        &read,
        MeterWindow::Week,
        MeterScope::Model(ModelFamily::Fable),
    );
    assert_eq!(fable.reset_at, None);
}

fn window(minutes: u64, used: f64, reset: DateTime<Utc>) -> Value {
    json!({"window_minutes": minutes, "used_percent": used, "resets_at": reset.timestamp()})
}

fn count(at: DateTime<Utc>, limits: Value) -> Value {
    json!({"timestamp":at,"type":"event_msg","payload":{"type":"token_count","rate_limits":limits}})
}

fn rollout(home: &Path, name: &str, id: &str, events: &[Value]) {
    let header = json!({"type":"session_meta", "payload":{"id":id}});
    let mut payload = format!("{header}\n");
    for event in events {
        payload.push_str(&format!("{event}\n"));
    }
    write(home, name, &payload);
}

#[test]
fn codex_weekly_in_primary_does_not_invent_a_short_window() {
    let t = tempfile::tempdir().unwrap();
    let at = now() - Duration::minutes(10);
    let reset = now() + Duration::days(2);
    rollout(
        t.path(),
        "archived_sessions/rollout-weekly.jsonl",
        "weekly",
        &[count(
            at,
            json!({
                "limit_id":"codex", "primary":window(10080, 42.0, reset), "secondary":null,
                "credits":{"has_credits":false, "unlimited":false, "balance":null}
            }),
        )],
    );
    let read = read_owned(t.path(), "codex", &OriginIndex::default(), now());
    assert_eq!(read.meters.len(), 1);
    let m = meter(&read, MeterWindow::Week, MeterScope::AllModels);
    assert_eq!(m.used_percent.get(), 42.0);
    assert_eq!(m.observed_at, at);
    assert_eq!(m.reset_at, Some(reset));
    assert_eq!(
        m.source,
        MeterSource::CodexRollout {
            slot: CodexSlot::Primary
        }
    );
    assert!(!read.credit_info.unwrap().has_credits);
    let limits = ProfileLimits {
        meters: read.meters,
        ..Default::default()
    };
    assert_eq!(
        evaluate(&limits, ModelFamily::Codex, now()),
        Evaluation::Room
    );
}

#[test]
fn codex_isolates_subscription_id_and_chooses_latest_valid_event() {
    let t = tempfile::tempdir().unwrap();
    let at = now() - Duration::minutes(2);
    let reset = now() + Duration::days(2);
    let limits = json!([
        {"limit_id":"premium", "primary":window(10080, 100.0, reset)},
        {"limit_id":"codex", "primary":window(300, 31.0, reset), "secondary":window(10080, 100.0, reset),
         "credits":{"has_credits":true,"unlimited":false,"balance":"42.25"}}
    ]);
    rollout(
        t.path(),
        "sessions/rollout-current.jsonl",
        "current",
        &[
            count(at, limits),
            count(
                at - Duration::minutes(1),
                json!({"limit_id":"codex", "primary":window(300, 54.0, reset)}),
            ),
            count(
                now(),
                json!({"limit_id":"premium", "primary":window(62, 99.0, reset)}),
            ),
            count(
                now(),
                json!({"limit_id":"codex", "primary":null,"secondary":null}),
            ),
            count(
                now() + Duration::seconds(1),
                json!({"limit_id":"codex", "primary":window(10080, 1.0, reset)}),
            ),
        ],
    );
    let read = read_owned(t.path(), "codex", &OriginIndex::default(), now());
    assert_eq!(read.meters.len(), 3);
    let week = meter(&read, MeterWindow::Week, MeterScope::AllModels);
    assert_eq!(week.observed_at, at);
    assert_eq!(week.used_percent.get(), 100.0);
    assert_eq!(
        week.source,
        MeterSource::CodexRollout {
            slot: CodexSlot::Secondary
        }
    );
    assert_eq!(
        meter(&read, MeterWindow::FiveHour, MeterScope::AllModels)
            .used_percent
            .get(),
        31.0
    );
    let extra = meter(
        &read,
        MeterWindow::Minutes(62),
        MeterScope::Unclassified("premium".into()),
    );
    assert_eq!(extra.used_percent.get(), 99.0);
    let credit = read.credit_info.unwrap();
    assert!(credit.has_credits);
    assert_eq!(credit.balance, Some(42.25));
    let limits = ProfileLimits {
        meters: read.meters,
        credit_info: Some(credit),
        ..Default::default()
    };
    assert!(
        matches!(evaluate(&limits, ModelFamily::Codex, now()), Evaluation::Out(blockers)
        if matches!(blockers.as_slice(), [Blocker::Meter(m)] if m.key.window == MeterWindow::Week))
    );
}

#[test]
fn codex_wrong_or_missing_limit_id_never_supplies_subscription_headroom() {
    let t = tempfile::tempdir().unwrap();
    let reset = now() + Duration::days(1);
    rollout(
        t.path(),
        "sessions/rollout-extra.jsonl",
        "extra",
        &[count(
            now(),
            json!([
                {"limit_id":"premium","primary":window(300,100.0,reset)},
                {"primary":{"used_percent":100,"resets_at":reset.timestamp()}}
            ]),
        )],
    );
    let read = read_owned(t.path(), "codex", &OriginIndex::default(), now());
    assert_eq!(read.meters.len(), 2);
    assert!(read
        .meters
        .iter()
        .all(|m| matches!(m.key.scope, MeterScope::Unclassified(_))));
    assert!(read
        .meters
        .iter()
        .any(|m| m.key.window == MeterWindow::Native("primary".into())));
    let limits = ProfileLimits {
        meters: read.meters,
        ..Default::default()
    };
    assert_eq!(
        evaluate(&limits, ModelFamily::Codex, now()),
        Evaluation::Unknown
    );
}

fn paths(root: &Path) -> Paths {
    Paths {
        config_dir: root.join("config"),
        state_dir: root.join("state"),
    }
}

#[test]
fn codex_exact_copy_boundary_excludes_newer_inherited_observations() {
    let t = tempfile::tempdir().unwrap();
    let paths = paths(t.path());
    let home = paths.profile_home_dir("codex", "dest");
    let name = "sessions/rollout-copy.jsonl";
    let path = home.join(name);
    let at = now() - Duration::minutes(2);
    let reset = now() + Duration::days(1);
    let event = |at, used| {
        count(
            at,
            json!({"limit_id":"codex","primary":window(10080,used,reset)}),
        )
    };
    // The inherited token count has clock skew and a newer timestamp than the
    // owned record. Only the physical byte boundary distinguishes the two.
    rollout(&home, name, "copy", &[event(now(), 100.0)]);
    let copied_bytes = fs::metadata(&path).unwrap().len();
    let provenance = json!({"codex":{"dest":{"copy":{"path":path,"bytes":copied_bytes,"at":now() - Duration::minutes(1)}}}}).to_string();
    write(&paths.state_dir, "fork-origins.json", &provenance);
    let index = read_origins(&paths).unwrap();
    assert!(read_owned(&home, "codex", &index, now()).meters.is_empty());
    let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
    writeln!(file, "{}", event(at, 25.0)).unwrap();
    let read = read_owned(&home, "codex", &index, now());
    let m = meter(&read, MeterWindow::Week, MeterScope::AllModels);
    assert_eq!(m.observed_at, at);
    assert_eq!(m.used_percent.get(), 25.0);
    assert_eq!(
        fs::read_to_string(paths.state_dir.join("fork-origins.json")).unwrap(),
        provenance
    );
}

#[test]
fn holds_preserve_legacy_watermarks_and_keep_ambiguous_scope_profile_wide() {
    let t = tempfile::tempdir().unwrap();
    let paths = paths(t.path());
    let at = now() - Duration::minutes(3);
    let reset = now() + Duration::hours(2);
    let limit =
        json!({"kind":"weekly","at":at,"resets_at":reset,"active":true,"message":"fixture"});
    let store = json!({"entries":{
        "claude":{
            "weekly":{"limit":limit,"native_kind":"seven_day","success_at":at,"cleared_at":at - Duration::seconds(1)},
            "short":{"limit":limit,"native_kind":"five_hour"},
            "ambiguous":{"limit":limit,"native_kind":"seven_day_overage_included"},
            "legacy":{"limit":limit}
        },
        "codex":{"generic":{"limit":{"kind":"usage","at":at,"resets_at":null,"active":true,"message":"fixture"}}}
    }}).to_string();
    write(&paths.state_dir, "quota.json", &store);
    let index = read_holds(&paths).unwrap();
    for (profile, window) in [
        ("weekly", MeterWindow::Week),
        ("short", MeterWindow::FiveHour),
    ] {
        let entry = &index[&("claude".into(), profile.into())];
        assert_eq!(
            entry.holds[0].applies_to,
            HoldScope::Meter(MeterKey {
                window,
                scope: MeterScope::AllModels
            })
        );
    }
    let weekly = &index[&("claude".into(), "weekly".into())];
    assert_eq!(weekly.success_watermarks.common, Some(at));
    assert!(weekly.success_watermarks.models.is_empty());
    assert_eq!(weekly.cleared_at, Some(at - Duration::seconds(1)));
    for profile in ["ambiguous", "legacy"] {
        let hold = &index[&("claude".into(), profile.into())].holds[0];
        assert_eq!(hold.applies_to, HoldScope::Profile);
    }
    let generic = &index[&("codex".into(), "generic".into())].holds[0];
    assert_eq!(generic.applies_to, HoldScope::Profile);
    assert_eq!(generic.reset_at, None);
    assert_eq!(generic.until(), at + Duration::minutes(15));
    assert_eq!(
        fs::read_to_string(paths.state_dir.join("quota.json")).unwrap(),
        store
    );
}

#[test]
fn exact_fullness_survives_age_and_watermarks_respect_scope_and_precedence() {
    let t = tempfile::tempdir().unwrap();
    let at = now() - Duration::minutes(15);
    let reset = now() + Duration::hours(1);
    let read = claude_cache(
        t.path(),
        at,
        json!({
            "five_hour": {"utilization": 30, "resets_at":reset},
            "seven_day": {"utilization": 99.9, "resets_at":reset},
            "limits": [{"kind":"weekly_scoped","percent":100,"resets_at":reset,"scope":{"model":{"id":"fable"}}}]
        }),
    );
    let fable_key = meter(
        &read,
        MeterWindow::Week,
        MeterScope::Model(ModelFamily::Fable),
    )
    .key
    .clone();
    let mut limits = ProfileLimits {
        meters: read.meters,
        ..Default::default()
    };
    assert_eq!(
        evaluate(&limits, ModelFamily::Opus, now()),
        Evaluation::Room
    );
    assert_eq!(
        evaluate(&limits, ModelFamily::Unknown, now()),
        Evaluation::Unknown
    );
    let stale = now() + Duration::seconds(1);
    assert_eq!(
        evaluate(&limits, ModelFamily::Opus, stale),
        Evaluation::Unknown
    );
    assert!(matches!(
        evaluate(&limits, ModelFamily::Fable, stale),
        Evaluation::Out(_)
    ));
    limits.holds.push(Hold {
        applies_to: HoldScope::Meter(fable_key),
        native_kind: None,
        at,
        reset_at: Some(reset),
    });
    limits.success_watermarks.common = Some(now());
    limits
        .success_watermarks
        .models
        .insert(ModelFamily::Opus, now());
    assert!(
        matches!(evaluate(&limits, ModelFamily::Fable, now()), Evaluation::Out(blockers) if blockers.len() == 2)
    );
    limits
        .success_watermarks
        .models
        .insert(ModelFamily::Fable, at);
    assert!(
        matches!(evaluate(&limits, ModelFamily::Fable, now()), Evaluation::Out(blockers) if blockers.len() == 2)
    );
    limits
        .success_watermarks
        .models
        .insert(ModelFamily::Fable, now());
    assert_eq!(
        evaluate(&limits, ModelFamily::Fable, now()),
        Evaluation::Unknown
    );
    limits.success_watermarks.models.clear();
    limits.cleared_at = Some(at);
    assert_eq!(
        evaluate(&limits, ModelFamily::Fable, now()),
        Evaluation::Unknown
    );
}
