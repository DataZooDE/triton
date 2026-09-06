//! Precedence-table tests for `triton_chat_routing::resolve`.

use triton_chat_routing::{
    AgentDescriptor, BindingStore, ChooserReason, ConvKey, InMemoryBindingStore, InMemoryCatalog,
    ReservedCmd, Resolution, SelectionSource, USE_AGENT_TOOL, confirm_pick, resolve,
};

fn cat() -> InMemoryCatalog {
    InMemoryCatalog::new(vec![
        AgentDescriptor::new("sales", "Sales", "Pipeline & quotes"),
        AgentDescriptor::new("supplier-risk", "Supplier Risk", "Vendor exposure"),
        AgentDescriptor::new("assistant", "Assistant", "General help").as_default(),
    ])
}

/// Single-agent, no default flag — the only entitled agent.
fn one() -> InMemoryCatalog {
    InMemoryCatalog::new(vec![AgentDescriptor::new("solo", "Solo", "the one")])
}

fn sel(r: &Resolution) -> (&str, &str, SelectionSource, Option<&str>) {
    match r {
        Resolution::Selection {
            agent_id,
            text,
            source,
            bind,
        } => (agent_id, text, *source, bind.as_deref()),
        other => panic!("expected Selection, got {other:?}"),
    }
}

// --- reserved commands (highest precedence) --------------------------------

#[test]
fn reserved_agents_and_whoami() {
    assert_eq!(
        resolve("/agents", Some("sales"), "default", &cat()),
        Resolution::Reserved(ReservedCmd::ListAgents)
    );
    assert_eq!(
        resolve("/whoami", Some("sales"), "default", &cat()),
        Resolution::Reserved(ReservedCmd::WhoAmI)
    );
    // Case-insensitive, and beats an active binding.
    assert_eq!(
        resolve("/Agents please", Some("sales"), "default", &cat()),
        Resolution::Reserved(ReservedCmd::ListAgents)
    );
}

// --- explicit switch: /use (sticky) ----------------------------------------

#[test]
fn use_switch_is_sticky_and_strips_prefix() {
    let r = resolve("/use sales what is my pipeline", None, "default", &cat());
    assert_eq!(
        sel(&r),
        (
            "sales",
            "what is my pipeline",
            SelectionSource::Switch,
            Some("sales")
        )
    );
}

#[test]
fn use_switch_fuzzy_prefix() {
    // "supp" uniquely prefixes "supplier-risk".
    let r = resolve("/use supp check acme", None, "default", &cat());
    assert_eq!(sel(&r).0, "supplier-risk");
    assert_eq!(sel(&r).3, Some("supplier-risk"));
}

#[test]
fn use_switch_no_subject_binds_only() {
    let r = resolve("/use sales", Some("assistant"), "default", &cat());
    assert_eq!(
        sel(&r),
        ("sales", "", SelectionSource::Switch, Some("sales"))
    );
}

#[test]
fn use_switch_unknown_agent_offers_chooser() {
    match resolve("/use nope hello", None, "default", &cat()) {
        Resolution::Chooser {
            reason,
            pending_text,
            candidates,
        } => {
            assert_eq!(reason, ChooserReason::UnknownAgent);
            assert_eq!(pending_text, "hello");
            assert_eq!(candidates.len(), 3);
        }
        other => panic!("expected chooser, got {other:?}"),
    }
}

#[test]
fn use_switch_ambiguous_offers_chooser() {
    // "s" prefixes both "sales" and "supplier-risk".
    match resolve("/use s hi", None, "default", &cat()) {
        Resolution::Chooser {
            reason,
            candidates,
            pending_text,
        } => {
            assert_eq!(reason, ChooserReason::Ambiguous);
            assert_eq!(pending_text, "hi");
            assert_eq!(candidates.len(), 2);
        }
        other => panic!("expected ambiguous chooser, got {other:?}"),
    }
}

// --- explicit switch: @X (one-turn, does NOT bind) -------------------------

#[test]
fn at_switch_is_one_turn() {
    let r = resolve("@sales quick q", Some("assistant"), "default", &cat());
    assert_eq!(
        sel(&r),
        ("sales", "quick q", SelectionSource::Switch, None),
        "@X must switch for this turn only and not overwrite the sticky binding"
    );
}

#[test]
fn at_switch_display_name_match() {
    // Display "Supplier Risk" → match by prefix "supplier".
    let r = resolve("@supplier acme?", None, "default", &cat());
    assert_eq!(sel(&r).0, "supplier-risk");
    assert_eq!(sel(&r).3, None);
}

// --- sticky binding --------------------------------------------------------

#[test]
fn sticky_binding_used_without_rebinding() {
    let r = resolve("normal message", Some("supplier-risk"), "default", &cat());
    assert_eq!(
        sel(&r),
        (
            "supplier-risk",
            "normal message",
            SelectionSource::Sticky,
            None
        )
    );
}

#[test]
fn sticky_binding_gone_reoffers_chooser() {
    match resolve("hello", Some("deleted-agent"), "default", &cat()) {
        Resolution::Chooser { reason, .. } => assert_eq!(reason, ChooserReason::BindingGone),
        other => panic!("expected BindingGone chooser, got {other:?}"),
    }
}

// --- tenant default --------------------------------------------------------

#[test]
fn tenant_default_before_chooser() {
    let c = cat().with_tenant_default("acme", "sales");
    let r = resolve("hi", None, "acme", &c);
    assert_eq!(
        sel(&r),
        ("sales", "hi", SelectionSource::TenantDefault, Some("sales"))
    );
}

// --- first-contact chooser vs global default -------------------------------

#[test]
fn first_contact_multi_agent_no_default_shows_chooser() {
    // Catalog with >1 agent and NO default flag → chooser.
    let c = InMemoryCatalog::new(vec![
        AgentDescriptor::new("sales", "Sales", "x"),
        AgentDescriptor::new("ops", "Ops", "y"),
    ]);
    match resolve("hey", None, "default", &c) {
        Resolution::Chooser {
            reason, candidates, ..
        } => {
            assert_eq!(reason, ChooserReason::FirstContact);
            assert_eq!(candidates.len(), 2);
        }
        other => panic!("expected first-contact chooser, got {other:?}"),
    }
}

#[test]
fn global_default_used_when_configured() {
    // cat() has "assistant" flagged default → used as last resort, and binds.
    let r = resolve("hey", None, "default", &cat());
    assert_eq!(
        sel(&r),
        (
            "assistant",
            "hey",
            SelectionSource::GlobalDefault,
            Some("assistant")
        )
    );
}

#[test]
fn single_entitled_agent_used_directly() {
    let r = resolve("hey", None, "default", &one());
    assert_eq!(
        sel(&r),
        ("solo", "hey", SelectionSource::GlobalDefault, Some("solo"))
    );
}

// --- chooser pick (stateless round-trip) -----------------------------------

#[test]
fn confirm_pick_binds_and_replays_message() {
    let r = confirm_pick("sales", "my buffered question", &cat()).expect("entitled");
    assert_eq!(
        sel(&r),
        (
            "sales",
            "my buffered question",
            SelectionSource::ChooserPick,
            Some("sales")
        )
    );
}

#[test]
fn confirm_pick_fails_closed_for_unentitled() {
    assert!(
        confirm_pick("intruder", "hi", &cat()).is_none(),
        "a pick for an agent not in the caller-scoped catalog must fail closed"
    );
}

// --- ConvKey + in-memory store ---------------------------------------------

#[test]
fn convkey_storage_id_is_stable_and_safe() {
    let k = ConvKey::googlechat("spaces/AAA", "", "users/123");
    assert_eq!(k.storage_id(), "googlechat__spaces-AAA____users-123");
    // Deterministic.
    assert_eq!(
        k.storage_id(),
        ConvKey::googlechat("spaces/AAA", "", "users/123").storage_id()
    );
}

#[test]
fn in_memory_binding_store_roundtrip() {
    let store = InMemoryBindingStore::new();
    let k = ConvKey::msteams("19:abc", "", "29:user");
    assert_eq!(store.get(&k), None);
    store.set(&k, "sales", "29:user");
    assert_eq!(store.get(&k).as_deref(), Some("sales"));
    store.clear(&k);
    assert_eq!(store.get(&k), None);
}

#[test]
fn use_agent_sentinel_constant() {
    assert_eq!(USE_AGENT_TOOL, "__use_agent");
}

// --- axis invariant: tenant is never emitted by resolve --------------------

#[test]
fn resolve_never_returns_tenant() {
    // Structural guarantee: Resolution carries agent + text + bind, never a
    // tenant. This test documents that the type cannot move the tenant pin.
    let r = resolve("@sales hi", None, "tenant-a", &cat());
    // Switching agents in tenant-a yields the same agent id regardless of tenant.
    let r2 = resolve("@sales hi", None, "tenant-b", &cat());
    assert_eq!(sel(&r).0, sel(&r2).0);
}
