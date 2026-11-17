use super::*;
use serde_json::json;

fn source(id: &str, remaining: f64, models: &[&str]) -> QuotaSource {
    serde_json::from_value(json!({
        "id": id, "label": id, "remaining": remaining, "total": 100,
        "used": 100.0 - remaining, "models": models,
    }))
    .unwrap()
}

fn condition(id: &str, models: &[&str]) -> Condition {
    Condition {
        join: Join::Or,
        source_id: id.into(),
        source: None,
        models: models.iter().map(|model| (*model).into()).collect(),
        metric: Metric::Remaining,
        comparison: Comparison::Lte,
        threshold: 10.0,
        window: None,
    }
}

fn facts(sources: Vec<QuotaSource>) -> sources::Facts {
    sources::Facts::from([(
        "account".into(),
        sources::SourceFacts {
            sources,
            ..Default::default()
        },
    )])
}

fn group(conditions: Vec<Condition>) -> Group {
    Group {
        enabled: true,
        action: Action::Skip,
        target: Target::Models,
        conditions,
    }
}

#[test]
fn comparisons_respect_exact_threshold_boundaries() {
    let mut c = condition("daily", &[]);
    for (comparison, expected) in [
        (Comparison::Lt, false),
        (Comparison::Lte, true),
        (Comparison::Gte, true),
    ] {
        c.comparison = comparison;
        assert_eq!(compare(10.0, &c), expected);
    }
}

#[test]
fn missing_nonfinite_expired_and_other_channel_quotas_do_not_block() {
    let account = json!({"id":"account","quotaChannel":"paid"});
    let c = condition("daily", &[]);
    assert!(
        !read(
            &account,
            &c,
            &sources::Facts::new(),
            &counter::Facts::new(),
            1000
        )
        .hit
    );
    for index in 0..4 {
        let mut quota = source("daily", 0.0, &[]);
        match index {
            0 => quota.remaining = Some(f64::NAN),
            1 => quota.ends_at = 1000,
            2 => quota.channel = "free".into(),
            3 => quota.starts_at = 1001,
            _ => unreachable!(),
        }
        let value = read(
            &account,
            &c,
            &facts(vec![quota]),
            &counter::Facts::new(),
            1000,
        );
        assert!(!value.hit, "case={index}");
    }
}

#[test]
fn only_matching_and_branches_contribute_blocked_models() {
    let account = json!({"id":"account"});
    let facts = facts(vec![
        source("a", 0.0, &[]),
        source("b", 90.0, &[]),
        source("c", 0.0, &[]),
    ]);
    let mut second = condition("b", &["model-b"]);
    second.join = Join::And;
    let group = group(vec![
        condition("a", &["model-a"]),
        second,
        condition("c", &["model-c"]),
    ]);
    let result = state(&account, &group, &facts, &counter::Facts::new(), 1000);
    assert!(result.hit);
    assert!(!result.account);
    assert_eq!(result.models, ["model-c"]);
}

#[test]
fn disabled_groups_do_not_block_and_account_targets_block_all_models() {
    let account = json!({"id":"account"});
    let facts = facts(vec![source("daily", 0.0, &[])]);
    let mut group = group(vec![condition("daily", &["model-a"])]);
    group.enabled = false;
    assert!(!state(&account, &group, &facts, &counter::Facts::new(), 1000).hit);
    group.enabled = true;
    group.target = Target::Account;
    assert!(state(&account, &group, &facts, &counter::Facts::new(), 1000).account);
}

#[test]
fn local_counts_from_the_previous_window_do_not_block_the_new_window() {
    let account = json!({"id":"account"});
    let mut c = condition("local", &[]);
    c.metric = Metric::Tokens;
    c.comparison = Comparison::Gte;
    c.window = Some(Window {
        reset: Reset::Fixed,
        seconds: 60,
        ..Default::default()
    });
    let mut count = counter::Count::default();
    count.tokens = 100;
    count.start = 0;
    let counts = counter::Facts::from([(("account".into(), counter::key(&c)), count)]);
    assert!(read(&account, &c, &sources::Facts::new(), &counts, 59_999).hit);
    assert!(!read(&account, &c, &sources::Facts::new(), &counts, 60_000).hit);
}

#[test]
fn invalid_limits_and_out_of_scope_models_are_rejected() {
    let base = json!([{"conditions":[{"sourceId":"local","metric":"tokens","comparison":"gte","threshold":10,"window":{"reset":"fixed","seconds":60}}]}]);
    assert!(groups::normalize_groups(&base).is_ok());
    for threshold in [
        json!(0),
        json!(-1),
        json!(0.5),
        json!(9_007_199_254_740_992u64),
    ] {
        let mut value = base.clone();
        value[0]["conditions"][0]["threshold"] = threshold;
        assert!(groups::normalize_groups(&value).is_err());
    }
    let mut value = base.clone();
    value[0]["conditions"][0]["window"]["seconds"] = json!(59);
    assert!(groups::normalize_groups(&value).is_err());
    value = base;
    value[0]["conditions"][0]["models"] = json!(["outside"]);
    value[0]["conditions"][0]["source"] =
        json!({"id":"local","label":"local","models":["allowed"]});
    assert!(groups::normalize_groups(&value).is_err());
}

/// 组内含本地条件（Token）时，`state()` 只在**成立的分支**里统计本地命中。
fn token_condition(join: Join) -> Condition {
    let mut c = condition("local", &[]);
    c.join = join;
    c.metric = Metric::Tokens;
    c.comparison = Comparison::Gte;
    c.threshold = 100.0;
    c.window = Some(Window { reset: Reset::Fixed, seconds: 60, ..Default::default() });
    c
}

fn counts_with_tokens(c: &Condition, tokens: i64) -> counter::Facts {
    let mut count = counter::Count::default();
    count.tokens = tokens;
    count.start = 0;
    counter::Facts::from([(("account".into(), counter::key(c)), count)])
}

/// 账号的 `limiterGroups` 来自 `groups::effective_groups`；这里直接塞一条 Skip 组。
fn account_with(group: &Group) -> Value {
    json!({"id": "account", "limiterGroups": [serde_json::to_value(group).unwrap()]})
}

#[test]
fn blocked_exempts_free_models_from_balance_limits_only() {
    let mut balance = group(vec![condition("daily", &[])]);
    balance.target = Target::Account;
    let account = account_with(&balance);
    let facts = facts(vec![source("daily", 0.0, &[])]);
    let none = counter::Facts::new();

    // 余额见底：普通模型被拦，限时免费模型放行。
    assert!(blocked_with(&account, "paid-model", &facts, &none, 1000, |_| false));
    assert!(!blocked_with(&account, "free-model", &facts, &none, 1000, |m| m == "free-model"));
    // 没有指定模型（model 为空）时不做免费判定，保持原有拦截。
    assert!(blocked_with(&account, "", &facts, &none, 1000, |_| true));
}

#[test]
fn blocked_keeps_local_token_limits_for_free_models() {
    let token = token_condition(Join::Or);
    let mut limit = group(vec![token.clone()]);
    limit.target = Target::Account;
    let account = account_with(&limit);
    let counts = counts_with_tokens(&token, 500);
    // Token 超额是本地保护：就算是免费模型也照拦。
    assert!(blocked_with(&account, "free-model", &sources::Facts::new(), &counts, 1000, |_| true));
}

#[test]
fn balance_only_hit_has_no_local_hit() {
    let account = json!({"id":"account"});
    let facts = facts(vec![source("daily", 0.0, &[])]);
    let result = state(&account, &group(vec![condition("daily", &[])]), &facts, &counter::Facts::new(), 1000);
    assert!(result.hit && !result.local_hit);
    assert!(only_balance_hit(&result));
}

#[test]
fn a_token_branch_that_fires_is_a_local_hit() {
    let account = json!({"id":"account"});
    let token = token_condition(Join::Or);
    let counts = counts_with_tokens(&token, 500);
    let result = state(&account, &group(vec![token]), &sources::Facts::new(), &counts, 1000);
    assert!(result.hit && result.local_hit);
    assert!(!only_balance_hit(&result));
}

#[test]
fn a_token_branch_that_did_not_fire_does_not_veto_the_exemption() {
    // `余额不足 OR token 超额`：只有余额分支成立时，token 条件没命中，
    // 这次拦截完全由余额造成，限时免费模型应当能豁免。
    let account = json!({"id":"account"});
    let facts = facts(vec![source("daily", 0.0, &[])]);
    let token = token_condition(Join::Or);
    let counts = counts_with_tokens(&token, 1); // 远低于阈值 100
    let result = state(&account, &group(vec![condition("daily", &[]), token]), &facts, &counts, 1000);
    assert!(result.hit, "余额分支应成立");
    assert!(!result.local_hit, "未成立的 token 分支不应计入本地命中");
    assert!(only_balance_hit(&result));
}

#[test]
fn an_and_branch_with_a_firing_token_keeps_blocking() {
    // `余额不足 AND token 超额`：整条合取成立，其中含本地保护，免费模型也不能豁免。
    let account = json!({"id":"account"});
    let facts = facts(vec![source("daily", 0.0, &[])]);
    let token = token_condition(Join::And);
    let counts = counts_with_tokens(&token, 500);
    let result = state(&account, &group(vec![condition("daily", &[]), token]), &facts, &counts, 1000);
    assert!(result.hit && result.local_hit);
    assert!(!only_balance_hit(&result));
}
#[test]
fn parses_numeric_multipliers_before_credit_strings() {
    // 有效倍率优先：raccoon 的免费模型同时有 0.5 的 billingMultiplier
    // 与 0 的 billingEffectiveMultiplier，实际计费以 effective 为准。
    assert_eq!(
        multiplier_of(&json!({
            "billingEffectiveMultiplier": 0,
            "billingMultiplier": 0.5,
            "pointsMultiplier": 0.5,
            "credits": "x0 credits",
        })),
        Some(0.0)
    );
    assert_eq!(
        multiplier_of(&json!({"billingMultiplier": 1, "credits": "x1 credits"})),
        Some(1.0)
    );
}

#[test]
fn parses_credit_display_strings() {
    assert_eq!(multiplier_of(&json!({"credits": "x0.00 credits"})), Some(0.0));
    assert_eq!(multiplier_of(&json!({"credits": "x0.0"})), Some(0.0));
    assert_eq!(multiplier_of(&json!({"credits": "x0.5 credits"})), Some(0.5));
    assert_eq!(multiplier_of(&json!({"credits": "x1.33 credits"})), Some(1.33));
    // 非数值展示（autoclaw 的低/中/高）不是计费读数，不做免费判定。
    assert_eq!(multiplier_of(&json!({"credits": "低"})), None);
    assert_eq!(multiplier_of(&json!({"credits": ""})), None);
    assert_eq!(multiplier_of(&json!({})), None);
}

#[test]
fn non_finite_readings_are_ignored() {
    assert_eq!(multiplier_of(&json!({"billingMultiplier": "x"})), None);
    assert_eq!(multiplier_of(&json!({"credits": "xNaN credits"})), None);
}

#[test]
fn parses_alternate_credit_fields_and_free_labels() {
    // LobsterAI 把 0 倍率写成「免费」。
    assert_eq!(multiplier_of(&json!({"credits": "免费"})), Some(0.0));
    // CodeArts 的展示串是后缀形态。
    assert_eq!(multiplier_of(&json!({"credits": "0.7x"})), Some(0.7));
    // Tabbit 用 creditMultiplier，不是 credits。
    assert_eq!(multiplier_of(&json!({"creditMultiplier": "x0"})), Some(0.0));
    assert_eq!(multiplier_of(&json!({"creditMultiplier": "x1"})), Some(1.0));
}

#[test]
fn ambiguous_or_abnormal_readings_are_never_treated_as_free() {
    // 通用字段名与数值型 credits 语义不明：不读，不能被当成免费。
    assert_eq!(multiplier_of(&json!({"rate": 0})), None);
    assert_eq!(multiplier_of(&json!({"multiplier": 0})), None);
    assert_eq!(multiplier_of(&json!({"credits": 0})), None);
    // 负数是异常读数，不是免费。
    assert_eq!(multiplier_of(&json!({"billingEffectiveMultiplier": -1})), None);
    assert_eq!(multiplier_of(&json!({"credits": "x-1 credits"})), None);
    // 专用字段的 0 仍然是免费。
    assert_eq!(multiplier_of(&json!({"billingEffectiveMultiplier": 0})), Some(0.0));
}
