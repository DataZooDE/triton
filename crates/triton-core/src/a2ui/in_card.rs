//! In-card actions on the MCP Apps answer card (#369).
//!
//! A control on the card that asks for the same answer with other arguments
//! (a page, a filter chip, a refresh) need not be a new user turn. The widget
//! calls the app-only tool [`TOOL_NAME`] over the host's server connection
//! (`tools/call`, MCP Apps `callServerTool`), Triton calls the producing tool
//! again, and the result replaces the card in place.
//!
//! The call is stateless, like every Triton card action: what to call rides
//! in a token signed with the A2UI action key ([`ActionSigner`]) over
//! `(tool, kind, args)` and bound to the tenant, the principal's `sub` and an
//! expiry. A client cannot change the target tool or its arguments, and a
//! leaked card does not work for another user. There is no nonce store: a
//! replay within the expiry by the same principal re-reads data that
//! principal may read anyway. That is acceptable only because the dispatcher
//! admits in-card calls to read-only tools alone
//! (`Dispatcher::invoke_in_card`); a write must stay a visible new turn.
//!
//! Two scopes:
//!
//! * [`Scope::Rerun`]: a control on Triton's own card ([`super::Action::Rerun`],
//!   a chip's `rerun`). The token carries the complete arguments of the
//!   repeat call (the original call's, with the control's laid over them).
//! * [`Scope::Event`]: an event on a producer's own surface whose name starts
//!   with [`EVENT_PREFIX`]. The token is the result's, carried in result
//!   `_meta[`[`META_KEY`]`]`, and names the original call and each
//!   `(surface, event)` pair the answer carried. The event itself reaches
//!   the upstream under `_triton.client.a2ui_events`, as on A2A.

use serde_json::{Map, Value, json};

use super::ge::ActionSigner;

/// The app-only tool the widget calls (hidden from the model).
pub const TOOL_NAME: &str = "triton_ui_event";
/// The event a Triton card control fires for an in-card call.
pub const EVENT: &str = "triton.in_card";
/// A producer event with a name starting with this is an in-card call.
pub const EVENT_PREFIX: &str = "triton.in_card:";
/// Result `_meta` key: the token for the producer surfaces' in-card events.
pub const META_KEY: &str = "triton/in_card";
/// Upper bound on one token (it carries the call's arguments).
pub const TOKEN_CAP: usize = 8 * 1024;
/// How long a card's in-card controls stay valid. Expiry is stored in whole
/// hours, rounded up, so the effective lifetime is at most 24 h.
pub const TOKEN_TTL_SECS: u64 = 23 * 3600;
/// Binding platform: an in-card token is never a GE or chat token.
const PLATFORM: &str = "mcp-app";

/// What a token allows beyond `(tool, args)`: the tool is called with the
/// signed arguments either way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// A Triton card control: no event.
    Rerun,
    /// A producer surface's event, handed to the tool: exactly these
    /// `(surface id, event name)` pairs (each name starting with
    /// [`EVENT_PREFIX`]), as the vetted answer carried them. A client cannot invent another event the
    /// upstream might act on, nor fire one surface's event on another.
    Event { pairs: Vec<(String, String)> },
}

impl Scope {
    /// Whether this scope admits event `name` fired on `surface_id`.
    pub fn allows_event(&self, surface_id: &str, name: &str) -> bool {
        match self {
            Scope::Rerun => false,
            Scope::Event { pairs } => pairs.iter().any(|(s, n)| s == surface_id && n == name),
        }
    }

    /// The scope's name in the audit line: `rerun` or `event`.
    pub fn label(&self) -> &'static str {
        match self {
            Scope::Rerun => "rerun",
            Scope::Event { .. } => "event",
        }
    }
}

/// A verified token.
#[derive(Debug, Clone, PartialEq)]
pub struct Signed {
    pub tool: String,
    pub scope: Scope,
    /// Always a JSON object.
    pub args: Value,
}

/// Who a token is for: the tenant, the subject and the issuer that vouched
/// for it (two issuers on one route may hand out equal subjects).
#[derive(Debug, Clone, Copy)]
pub struct Holder<'a> {
    pub tenant: &'a str,
    pub sub: &'a str,
    /// The verified issuer, `""` when the principal names none.
    pub issuer: &'a str,
}

impl<'a> Holder<'a> {
    /// The holder of a verified principal.
    pub fn of(p: &'a crate::principal::Principal) -> Self {
        Holder {
            tenant: &p.tenant,
            sub: &p.sub,
            issuer: p
                .idp
                .as_ref()
                .and_then(|i| i.issuer.as_deref())
                .unwrap_or(""),
        }
    }

    fn sender(&self) -> String {
        format!("{}\u{1f}{}", self.issuer, self.sub)
    }
}

fn binding<'a>(tenant: &'a str, sender: &'a str) -> triton_correlation::Binding<'a> {
    triton_correlation::Binding {
        platform: PLATFORM,
        tenant,
        sender,
    }
}

fn body(scope: &Scope, args: &Value) -> Option<Value> {
    if !args.is_object() {
        return None;
    }
    Some(match scope {
        Scope::Rerun => json!({ "k": "rerun", "a": args }),
        Scope::Event { pairs } => {
            if pairs.is_empty() || pairs.iter().any(|(_, n)| !n.starts_with(EVENT_PREFIX)) {
                return None;
            }
            json!({ "k": "event", "a": args, "p": pairs })
        }
    })
}

/// Mint a token for `tool` with `args` (an object), valid for
/// [`TOKEN_TTL_SECS`]. `None` when the args are no object, an event scope
/// names no in-card event, or the token would exceed [`TOKEN_CAP`] (the
/// control then degrades to a turn).
pub fn sign(
    signer: &ActionSigner,
    tool: &str,
    scope: &Scope,
    args: &Value,
    holder: Holder<'_>,
) -> Option<String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    sign_at(
        signer,
        tool,
        scope,
        args,
        holder,
        now.saturating_add(TOKEN_TTL_SECS),
    )
}

/// [`sign`] with an absolute expiry (unix seconds). Production mints with
/// [`sign`]; this exists so a test can present a stale token to a running
/// binary.
pub fn sign_at(
    signer: &ActionSigner,
    tool: &str,
    scope: &Scope,
    args: &Value,
    holder: Holder<'_>,
    exp_unix_secs: u64,
) -> Option<String> {
    let sender = holder.sender();
    triton_correlation::encode_bound_at(
        tool,
        &body(scope, args)?,
        signer.ring().signing(),
        TOKEN_CAP,
        binding(holder.tenant, &sender),
        exp_unix_secs,
    )
    .ok()
}

/// Verify a token for `holder`. `Err` names the reason class only (for the
/// audit line): `malformed`, `signature` (forged, another key, another
/// tenant, principal or issuer) or `expired`.
pub fn open(
    signer: &ActionSigner,
    token: &str,
    holder: Holder<'_>,
) -> Result<Signed, &'static str> {
    use triton_correlation::DecodeError;
    let sender = holder.sender();
    let (tool, body) = triton_correlation::decode_bound_any(
        token,
        signer.ring(),
        TOKEN_CAP,
        binding(holder.tenant, &sender),
    )
    .map_err(|e| match e {
        DecodeError::Malformed => "malformed",
        DecodeError::BadSignature => "signature",
        DecodeError::Body(m) if m.contains("expired") => "expired",
        DecodeError::Body(_) => "malformed",
    })?;
    let pairs = || -> Option<Vec<(String, String)>> {
        body.get("p")?
            .as_array()?
            .iter()
            .map(|p| match p.as_array()?.as_slice() {
                [s, n] => Some((s.as_str()?.to_string(), n.as_str()?.to_string())),
                _ => None,
            })
            .collect()
    };
    let scope = match body.get("k").and_then(Value::as_str) {
        Some("rerun") => Scope::Rerun,
        Some("event") => Scope::Event {
            pairs: pairs().ok_or("malformed")?,
        },
        _ => return Err("malformed"),
    };
    let args = body.get("a").cloned().unwrap_or(Value::Null);
    if !args.is_object() {
        return Err("malformed");
    }
    Ok(Signed { tool, scope, args })
}

/// The arguments of a repeat call: the original call's (without Triton's
/// reserved `_triton`), with `overlay`'s top-level keys laid over them.
/// `None` when the original arguments are no object: a repeat call could
/// not carry Triton's `_triton` context, so it is no in-card call.
pub fn merge_args(original: &Value, overlay: &Map<String, Value>) -> Option<Value> {
    let mut out = original.as_object()?.clone();
    out.remove("_triton");
    for (k, v) in overlay {
        if k != "_triton" {
            out.insert(k.clone(), v.clone());
        }
    }
    Some(Value::Object(out))
}

/// The in-card event names a producer surface's components fire.
pub fn event_names(components: &Value) -> Vec<String> {
    fn walk(v: &Value, out: &mut Vec<String>) {
        match v {
            Value::Object(m) => {
                if let Some(n) = m
                    .get("event")
                    .and_then(|e| e.get("name"))
                    .and_then(Value::as_str)
                    .filter(|n| n.starts_with(EVENT_PREFIX))
                    && !out.iter().any(|o| o == n)
                {
                    out.push(n.to_string());
                }
                m.values().for_each(|c| walk(c, out));
            }
            Value::Array(a) => a.iter().for_each(|c| walk(c, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(components, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signer() -> ActionSigner {
        ActionSigner::new(
            triton_correlation::KeyRing::single(b"in-card-test-key-32-bytes-long!!".to_vec())
                .unwrap(),
        )
    }

    const ALICE: Holder<'static> = Holder {
        tenant: "acme",
        sub: "alice",
        issuer: "https://idp.example",
    };

    fn pair(surface: &str, name: &str) -> (String, String) {
        (surface.to_string(), name.to_string())
    }

    fn refresh() -> Scope {
        Scope::Event {
            pairs: vec![pair("triton-a2ui-1", "triton.in_card:refresh")],
        }
    }

    #[test]
    fn a_token_names_the_tool_its_scope_and_its_args() {
        let s = signer();
        let args = json!({ "question": "planned agents", "page": 2 });
        let t = sign(&s, "list_agents", &Scope::Rerun, &args, ALICE).unwrap();
        assert_eq!(
            open(&s, &t, ALICE),
            Ok(Signed {
                tool: "list_agents".into(),
                scope: Scope::Rerun,
                args
            })
        );
        let e = sign(&s, "list_agents", &refresh(), &json!({}), ALICE).unwrap();
        assert_eq!(open(&s, &e, ALICE).unwrap().scope, refresh());
    }

    #[test]
    fn only_object_args_and_in_card_events_are_signed() {
        let s = signer();
        assert!(sign(&s, "t", &Scope::Rerun, &json!("text"), ALICE).is_none());
        assert!(sign(&s, "t", &Scope::Rerun, &json!([1]), ALICE).is_none());
        let none = Scope::Event { pairs: vec![] };
        assert!(sign(&s, "t", &none, &json!({}), ALICE).is_none());
        let foreign = Scope::Event {
            pairs: vec![pair("x", "approve")],
        };
        assert!(sign(&s, "t", &foreign, &json!({}), ALICE).is_none());
    }

    /// Each event is allowed on the surface that carried it only: with
    /// `refresh` on A and `next` on B, `next` on A and `refresh` on B are
    /// not allowed, before or after a round trip through the token.
    #[test]
    fn an_event_scope_binds_each_event_to_its_surface() {
        let s = signer();
        let scope = Scope::Event {
            pairs: vec![
                pair("A", "triton.in_card:refresh"),
                pair("B", "triton.in_card:next"),
            ],
        };
        let t = sign(&s, "list_agents", &scope, &json!({}), ALICE).unwrap();
        let opened = open(&s, &t, ALICE).unwrap().scope;
        assert_eq!(opened, scope);
        for sc in [&scope, &opened] {
            assert!(sc.allows_event("A", "triton.in_card:refresh"));
            assert!(sc.allows_event("B", "triton.in_card:next"));
            assert!(!sc.allows_event("A", "triton.in_card:next"));
            assert!(!sc.allows_event("B", "triton.in_card:refresh"));
            assert!(!sc.allows_event("C", "triton.in_card:refresh"));
        }
        assert!(!Scope::Rerun.allows_event("A", "triton.in_card:refresh"));
    }

    #[test]
    fn a_scope_names_itself_for_the_audit_line() {
        assert_eq!(Scope::Rerun.label(), "rerun");
        assert_eq!(refresh().label(), "event");
    }

    #[test]
    fn a_tampered_token_is_refused() {
        let s = signer();
        let t = sign(
            &s,
            "list_agents",
            &Scope::Rerun,
            &json!({ "page": 2 }),
            ALICE,
        )
        .unwrap();
        let mut b = t.into_bytes();
        let i = b.len() / 3;
        b[i] = if b[i] == b'A' { b'B' } else { b'A' };
        let t = String::from_utf8(b).unwrap();
        assert!(open(&s, &t, ALICE).is_err());
        assert_eq!(open(&s, "not-a-token", ALICE), Err("malformed"));
        let other = ActionSigner::new(
            triton_correlation::KeyRing::single(b"another-key-for-in-card-tests!!!".to_vec())
                .unwrap(),
        );
        let t = sign(&other, "list_agents", &Scope::Rerun, &json!({}), ALICE).unwrap();
        assert_eq!(open(&s, &t, ALICE), Err("signature"));
    }

    #[test]
    fn a_token_is_for_its_principal_tenant_and_issuer_only() {
        let s = signer();
        let t = sign(&s, "list_agents", &Scope::Rerun, &json!({}), ALICE).unwrap();
        for other in [
            Holder {
                sub: "bob",
                ..ALICE
            },
            Holder {
                tenant: "globex",
                ..ALICE
            },
            Holder {
                issuer: "https://other-idp.example",
                ..ALICE
            },
        ] {
            assert_eq!(open(&s, &t, other), Err("signature"), "{other:?}");
        }
        assert!(open(&s, &t, ALICE).is_ok());
    }

    #[test]
    fn an_expired_token_is_refused() {
        let s = signer();
        let past = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            - 3 * 3600;
        let t = sign_at(&s, "list_agents", &Scope::Rerun, &json!({}), ALICE, past).unwrap();
        assert_eq!(open(&s, &t, ALICE), Err("expired"));
    }

    #[test]
    fn an_oversized_token_is_not_minted() {
        let s = signer();
        let big = json!({ "q": "x".repeat(TOKEN_CAP) });
        assert!(sign(&s, "list_agents", &Scope::Rerun, &big, ALICE).is_none());
    }

    #[test]
    fn a_rerun_lays_its_args_over_the_call_and_never_carries_triton_keys() {
        let original =
            json!({ "question": "planned", "page": 1, "_triton": { "protocol": "mcp" } });
        let mut overlay = Map::new();
        overlay.insert("page".into(), json!(2));
        overlay.insert("_triton".into(), json!({ "forged": true }));
        assert_eq!(
            merge_args(&original, &overlay),
            Some(json!({ "question": "planned", "page": 2 }))
        );
        assert_eq!(
            merge_args(&original, &Map::new()),
            Some(json!({ "question": "planned", "page": 1 }))
        );
        assert_eq!(merge_args(&json!("text"), &overlay), None);
    }

    #[test]
    fn the_in_card_events_of_a_surface_are_found_wherever_they_sit() {
        let comps = json!([
            { "id": "a", "component": "Button", "action": { "event": { "name": "triton.in_card:refresh" } } },
            { "id": "b", "component": "Button", "action": { "event": { "name": "apply" } } },
            { "id": "c", "component": "Card", "child": { "action": { "event": { "name": "triton.in_card:more" } } } },
            { "id": "d", "component": "Button", "action": { "event": { "name": "triton.in_card:refresh" } } }
        ]);
        assert_eq!(
            event_names(&comps),
            vec![
                "triton.in_card:refresh".to_string(),
                "triton.in_card:more".to_string()
            ]
        );
    }
}
