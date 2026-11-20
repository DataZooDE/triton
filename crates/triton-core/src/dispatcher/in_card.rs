//! In-card calls (#369): a card's control re-runs the tool that produced it.
//!
//! The adapter hands over what the widget sent ([`InCardEvent`]); everything
//! that decides whether it runs is here, at the audit pivot (ADR-6), in an
//! order that keeps refusals cheap:
//!
//! 1. the principal is not revoked;
//! 2. the principal's in-card rate limit admits it (before any token or
//!    tool-list work, so junk cannot amplify);
//! 3. the token verifies for this tenant, `sub` and issuer and has not
//!    expired ([`crate::a2ui::in_card`]): it names the tool and its
//!    arguments, so a client cannot pick either;
//! 4. the event fits the token's scope (Triton's card fires [`EVENT`]; a
//!    producer event must be one the vetted answer carried, on one of its
//!    surfaces) and its fields fit their limits;
//! 5. the principal may invoke the tool at all ([`Dispatcher::can_invoke`]);
//! 6. the tool is declared in-card capable by the operator AND annotated
//!    `readOnlyHint: true` on the `tools/list` of the endpoint that executes
//!    it (no other endpoint's word counts): replaying a read is harmless, a
//!    write must stay a visible new turn.
//!
//! Exactly one audit line per call, refused or not, with the in-card part
//! ([`InCardAudit`]): key names and sizes of the context, never values.
//!
//! The dispatch is buffered (no `text/event-stream` offered), so an upstream
//! cannot turn an in-card call into an asynchronous task.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::{Dispatch, Dispatcher};
use crate::a2ui::ge::ActionSigner;
use crate::a2ui::in_card::{self as card, EVENT, Holder, Kind, Scope, TOOL_NAME};
use crate::audit::InCardAudit;
use crate::dispatch_context::ContextOverrides;
use crate::error::TritonError;
use crate::principal::Principal;
use crate::ratelimit::PerTenantBuckets;

/// In-card calls per second per principal, sustained.
pub const IN_CARD_PER_SEC: u32 = 2;
/// In-card calls a principal may make in a burst (a few quick page turns).
pub const IN_CARD_BURST: u32 = 10;
/// Bytes of an event's context, serialized (as on A2A).
pub const MAX_CONTEXT_BYTES: usize = 8 * 1024;
/// Characters of the event name, surface id and component id.
pub const MAX_ID_CHARS: usize = 256;
/// How long a tool found read-only is believed.
const READ_ONLY_TTL: Duration = Duration::from_secs(60);
/// How long a tool found not read-only is believed before its endpoint is
/// asked again (an endpoint that did not answer looks the same as one that
/// says no).
const NOT_READ_ONLY_TTL: Duration = Duration::from_secs(5);

/// The last answer for one declared tool.
#[derive(Debug, Clone, Copy)]
struct ReadOnly {
    at: Instant,
    yes: bool,
}

/// Which tools a card may call in place, and how often.
#[derive(Debug)]
pub(crate) struct InCardPolicy {
    tools: HashSet<String>,
    per_sec: u32,
    burst: u32,
    buckets: PerTenantBuckets,
    /// Per declared tool, the last read-only answer of its own endpoint.
    /// Entries outlive their TTL, so a repeated "no" is told apart from a
    /// change. Bounded by the declared tools.
    read_only: Mutex<HashMap<String, ReadOnly>>,
    /// One read per tool at a time (single flight per tool: a slow endpoint
    /// delays only the checks of its own tools).
    fills: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl Default for InCardPolicy {
    fn default() -> Self {
        Self::new(HashSet::new(), IN_CARD_PER_SEC, IN_CARD_BURST)
    }
}

impl InCardPolicy {
    fn new(tools: HashSet<String>, per_sec: u32, burst: u32) -> Self {
        Self {
            tools,
            per_sec,
            burst,
            buckets: PerTenantBuckets::new(per_sec.max(1), burst.max(1)),
            read_only: Mutex::new(HashMap::new()),
            fills: Mutex::new(HashMap::new()),
        }
    }

    /// The cached answer for `tool`, if still believed.
    fn cached(&self, tool: &str) -> Option<bool> {
        let r = *self.read_only.lock().expect("poisoned").get(tool)?;
        let ttl = if r.yes {
            READ_ONLY_TTL
        } else {
            NOT_READ_ONLY_TTL
        };
        (r.at.elapsed() < ttl).then_some(r.yes)
    }

    /// Record `yes` for `tool`; whether it differs from the last answer
    /// (the first answer counts as a change).
    fn store(&self, tool: &str, yes: bool) -> bool {
        let last = self.read_only.lock().expect("poisoned").insert(
            tool.to_string(),
            ReadOnly {
                at: Instant::now(),
                yes,
            },
        );
        last.is_none_or(|l| l.yes != yes)
    }

    /// The single-flight lock of `tool`'s read.
    fn fill(&self, tool: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.fills
            .lock()
            .expect("poisoned")
            .entry(tool.to_string())
            .or_default()
            .clone()
    }
}

/// What the widget sent: the clicked control's event and its token.
#[derive(Debug, Clone, Default)]
pub struct InCardEvent {
    pub surface_id: String,
    pub name: String,
    pub source_component_id: String,
    /// The event's resolved context (a producer's bound values); `{}` when
    /// absent.
    pub context: Value,
    pub token: Option<String>,
}

/// A completed in-card call: which tool ran with which arguments (the
/// adapter signs the new card's controls from them), and its result.
#[derive(Debug)]
pub struct InCardDispatch {
    pub tool: String,
    pub kind: Kind,
    pub args: Value,
    pub dispatch: Dispatch,
}

impl InCardEvent {
    fn audit(&self) -> InCardAudit {
        let keys = self
            .context
            .as_object()
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default();
        InCardAudit {
            ingress: "mcp-app-event",
            kind: "-",
            event: self.name.clone(),
            source_component_id: self.source_component_id.clone(),
            token: "unchecked",
            context_keys: keys,
            context_bytes: serde_json::to_vec(&self.context).map_or(0, |b| b.len()),
        }
    }
}

impl Dispatcher {
    /// Declare the tools a card may call in place (#369; the operator's
    /// manifest `tools.<name>.in_card: true`). Each must also be annotated
    /// `readOnlyHint: true`, checked per call. Replaces any earlier set and
    /// keeps the rate.
    pub fn with_in_card_tools(mut self, tools: impl IntoIterator<Item = String>) -> Self {
        let (per_sec, burst) = (self.in_card.per_sec, self.in_card.burst);
        self.in_card = InCardPolicy::new(
            tools.into_iter().filter(|t| !t.trim().is_empty()).collect(),
            per_sec,
            burst,
        );
        self
    }

    /// The per-principal in-card rate (default [`IN_CARD_PER_SEC`],
    /// [`IN_CARD_BURST`]); keeps the declared tools.
    pub fn with_in_card_rate(mut self, per_sec: u32, burst: u32) -> Self {
        let tools = std::mem::take(&mut self.in_card.tools);
        self.in_card = InCardPolicy::new(tools, per_sec, burst);
        self
    }

    /// Whether the operator declared `tool` in-card capable (cheap).
    pub fn in_card_capable(&self, tool: &str) -> bool {
        self.in_card.tools.contains(tool)
    }

    /// Whether a card may call `tool` in place now: declared in-card
    /// capable and annotated `readOnlyHint: true`. The adapter signs a
    /// card's controls only then; the call is checked again when it comes.
    pub async fn in_card_ready(&self, tool: &str) -> bool {
        self.in_card_capable(tool) && self.read_only(tool).await
    }

    /// Whether any tool is in-card capable (the app-only tool is listed
    /// only then).
    pub fn has_in_card_tools(&self) -> bool {
        !self.in_card.tools.is_empty()
    }

    /// Whether declared `tool` is annotated `readOnlyHint: true` by the
    /// endpoint that executes it ([`super::UpstreamDispatch::tool_info`]; an
    /// in-process tool carries no annotations, so it is not). One read per
    /// tool at a time; a yes is reused for [`READ_ONLY_TTL`], a no for
    /// [`NOT_READ_ONLY_TTL`]. A "no" is logged when the answer changes to
    /// it, not on every read.
    async fn read_only(&self, tool: &str) -> bool {
        if let Some(hit) = self.in_card.cached(tool) {
            return hit;
        }
        let fill = self.in_card.fill(tool);
        let _fill = fill.lock().await;
        // Another call may have read it while this one waited.
        if let Some(hit) = self.in_card.cached(tool) {
            return hit;
        }
        let hit = match &self.upstream {
            Some(up) if !self.is_in_process(tool) => {
                up.tool_info(tool)
                    .await
                    .and_then(|i| i.annotations)
                    .and_then(|a| a.get("readOnlyHint").and_then(Value::as_bool))
                    == Some(true)
            }
            _ => false,
        };
        if self.in_card.store(tool, hit) && !hit {
            tracing::warn!(
                tool,
                "tool declared in-card capable is not annotated readOnlyHint (or its \
                 upstream did not answer tools/list): its card controls stay new turns"
            );
        }
        hit
    }

    /// Run an in-card call (see the module doc for the checks). The
    /// dispatch goes out on the `mcp` channel with `_triton.client.in_card =
    /// true` and, for a producer event, the event under
    /// `_triton.client.a2ui_events`.
    pub async fn invoke_in_card(
        &self,
        ev: InCardEvent,
        principal: Principal,
    ) -> Result<InCardDispatch, TritonError> {
        let mut audit = ev.audit();
        let refuse = |tool: &str, audit: &InCardAudit, e: TritonError| {
            self.refuse_in_card(tool, &principal, audit, e)
        };
        if self.is_denied(&principal) {
            return Err(refuse(
                TOOL_NAME,
                &audit,
                TritonError::Forbidden(format!(
                    "principal `{}/{}` has been revoked by the operator",
                    principal.tenant, principal.sub
                )),
            ));
        }
        // The bucket is the token's holder: tenant, issuer and subject (two
        // issuers on one route may hand out equal subjects), joined by a
        // separator none of them carries in practice; a collision would only
        // share a throttle.
        let h = Holder::of(&principal);
        let key = format!("{}\u{1f}{}\u{1f}{}", h.tenant, h.issuer, h.sub);
        let saturated = self.in_card.buckets.map_full_refusals();
        if let Err(retry) = self.in_card.buckets.try_take(&key) {
            if self.in_card.buckets.map_full_refusals() > saturated {
                self.metrics.record_in_card_throttle_saturated();
            }
            return Err(refuse(
                TOOL_NAME,
                &audit,
                TritonError::RateLimited(format!("in-card calls: retry after {retry:.1}s")),
            ));
        }
        let opened = match ev.token.as_deref() {
            Some(t) => card::open(ActionSigner::global(), t, Holder::of(&principal)),
            None => Err("missing"),
        };
        let signed = match opened {
            Ok(s) => s,
            Err(reason) => {
                audit.token = reason;
                return Err(refuse(
                    TOOL_NAME,
                    &audit,
                    TritonError::Auth(format!("in-card token refused: {reason}")),
                ));
            }
        };
        audit.token = "verified";
        audit.kind = signed.scope.kind().as_str();
        let tool = signed.tool.clone();
        let fits = match &signed.scope {
            Scope::Rerun => ev.name == EVENT,
            scope => scope.allows_event(&ev.surface_id, &ev.name),
        };
        let too_long = [&ev.name, &ev.surface_id, &ev.source_component_id]
            .iter()
            .any(|s| s.chars().count() > MAX_ID_CHARS);
        if !fits || too_long || audit.context_bytes > MAX_CONTEXT_BYTES || !ev.context.is_object() {
            let why = if fits {
                "event fields over their size limits".to_string()
            } else {
                format!(
                    "event `{}` on `{}` is not one this card's {} token allows",
                    ev.name,
                    ev.surface_id,
                    signed.scope.kind().as_str()
                )
            };
            return Err(refuse(&tool, &audit, TritonError::Validation(why)));
        }
        if let Err(e) = self.can_invoke(&principal, &tool) {
            return Err(refuse(&tool, &audit, e));
        }
        if !self.in_card_ready(&tool).await {
            return Err(refuse(
                &tool,
                &audit,
                TritonError::Forbidden(format!(
                    "`{tool}` may not be called from a card: in-card calls need a tool the \
                     operator declared in-card capable and that is annotated readOnlyHint"
                )),
            ));
        }

        let mut client = json!({ "in_card": true });
        if signed.scope.kind() == Kind::Event {
            client["a2ui_events"] = json!([{
                "version": "v0.9",
                "action": {
                    "name": ev.name,
                    "surfaceId": ev.surface_id,
                    "sourceComponentId": ev.source_component_id,
                    "timestamp": crate::audit::now_rfc3339(),
                    "context": ev.context,
                }
            }]);
        }
        let mut overrides = ContextOverrides::default();
        overrides.triton_meta.insert("client".into(), client);
        let dispatch = self
            .dispatch_authorized(
                &tool,
                signed.args.clone(),
                principal,
                "mcp",
                overrides,
                Some(&audit),
            )
            .await?;
        Ok(InCardDispatch {
            tool,
            kind: signed.scope.kind(),
            args: signed.args,
            dispatch,
        })
    }

    /// The one audit line of a refused in-card call; hands `e` back.
    fn refuse_in_card(
        &self,
        tool: &str,
        principal: &Principal,
        audit: &InCardAudit,
        e: TritonError,
    ) -> TritonError {
        let outcome: Result<Value, TritonError> = Err(e);
        self.audit_dispatch_with(tool, "mcp", principal, 0, &outcome, Some(audit));
        match outcome {
            Err(e) => e,
            Ok(_) => unreachable!("constructed as Err"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use async_trait::async_trait;

    use super::*;
    use crate::a2ui::in_card::{sign, sign_at};
    use crate::audit::AuditBuffer;
    use crate::dispatcher::DispatchControls as Controls;
    use crate::dispatcher::{DispatchControls, UpstreamDispatch, UpstreamToolInfo};
    use crate::tool::ToolRegistry;

    /// An upstream with a read-only `pages` and a writing `file_request`;
    /// it answers with the args it got.
    struct Pages;

    #[async_trait]
    impl UpstreamDispatch for Pages {
        async fn invoke(
            &self,
            tool: &str,
            args: Value,
            _p: &Principal,
            ctx: &crate::dispatch_context::DispatchContext,
        ) -> Result<Value, TritonError> {
            Ok(json!({ "tool": tool, "args": args, "channel": ctx.channel }))
        }
        async fn list_agents(&self) -> Vec<String> {
            vec!["pages".into(), "file_request".into()]
        }
        async fn agent_tools(&self) -> HashMap<String, UpstreamToolInfo> {
            let info = |ro: bool| UpstreamToolInfo {
                input_schema: json!({ "type": "object" }),
                description: None,
                annotations: Some(json!({ "readOnlyHint": ro })),
            };
            HashMap::from([
                ("pages".to_string(), info(true)),
                ("file_request".to_string(), info(false)),
            ])
        }
    }

    fn dispatcher() -> Dispatcher {
        Dispatcher::new(
            Arc::new(ToolRegistry::new()),
            "test",
            DispatchControls::unenforced(),
        )
        .with_upstream(Arc::new(Pages))
        .with_in_card_tools(["pages".to_string(), "file_request".to_string()])
    }

    fn principal(sub: &str, trace: &str) -> Principal {
        Principal {
            sub: sub.into(),
            scopes: Vec::new(),
            groups: Vec::new(),
            tenant: "acme".into(),
            raw_token: String::new(),
            trace_id: trace.into(),
            sender_ref: None,
            conversation_ref: None,
            idp: None,
        }
    }

    fn holder(sub: &str) -> Holder<'_> {
        Holder {
            tenant: "acme",
            sub,
            issuer: "",
        }
    }

    fn refresh() -> Scope {
        Scope::Event {
            pairs: vec![("triton-a2ui-0".into(), "triton.in_card:refresh".into())],
        }
    }

    fn token(tool: &str, scope: Scope, args: Value, sub: &str) -> String {
        sign(ActionSigner::global(), tool, &scope, &args, holder(sub)).unwrap()
    }

    fn rerun(token: String) -> InCardEvent {
        InCardEvent {
            surface_id: "triton-card".into(),
            name: EVENT.into(),
            source_component_id: "c7".into(),
            context: json!({ "fallback": "Show page 2" }),
            token: Some(token),
        }
    }

    fn line(trace: &str) -> crate::audit::AuditEntry {
        let mut l = AuditBuffer::recent(10, Some(trace));
        assert_eq!(l.len(), 1, "exactly one audit line for {trace}: {l:?}");
        l.remove(0)
    }

    #[tokio::test]
    async fn a_signed_rerun_calls_its_tool_with_its_args_and_audits_once() {
        let d = dispatcher();
        let t = token(
            "pages",
            Scope::Rerun,
            json!({ "q": "planned", "page": 2 }),
            "alice",
        );
        let out = d
            .invoke_in_card(rerun(t), principal("alice", "ic-ok"))
            .await
            .unwrap();
        assert_eq!(out.tool, "pages");
        let r = out.dispatch.result;
        assert_eq!(r["args"]["page"], 2);
        assert_eq!(r["args"]["q"], "planned");
        assert_eq!(r["args"]["_triton"]["client"]["in_card"], true);
        assert!(r["args"]["_triton"]["client"].get("a2ui_events").is_none());
        assert_eq!(r["channel"], "mcp");
        let l = line("ic-ok");
        assert_eq!(l.tool, "pages");
        assert_eq!(l.result, "ok");
        let ic = l.in_card.expect("in-card part");
        assert_eq!(
            (ic.ingress, ic.kind, ic.token),
            ("mcp-app-event", "rerun", "verified")
        );
        assert_eq!(ic.context_keys, vec!["fallback".to_string()]);
        // Keys and sizes only: the context's values never reach the line.
        assert!(!serde_json::to_string(&ic).unwrap().contains("Show page 2"));
    }

    #[tokio::test]
    async fn a_producer_event_reaches_the_tool_as_an_a2ui_event() {
        let d = dispatcher();
        let t = token("pages", refresh(), json!({ "q": "planned" }), "alice");
        let ev = InCardEvent {
            surface_id: "triton-a2ui-0".into(),
            name: "triton.in_card:refresh".into(),
            source_component_id: "btn".into(),
            context: json!({ "rev": "r7" }),
            token: Some(t),
        };
        let out = d
            .invoke_in_card(ev, principal("alice", "ic-ev"))
            .await
            .unwrap();
        let ev = &out.dispatch.result["args"]["_triton"]["client"]["a2ui_events"][0];
        assert_eq!(ev["action"]["name"], "triton.in_card:refresh");
        assert_eq!(ev["action"]["surfaceId"], "triton-a2ui-0");
        assert_eq!(ev["action"]["context"]["rev"], "r7");
        assert_eq!(line("ic-ev").in_card.unwrap().kind, "event");
    }

    /// A producer token allows exactly the events and surfaces the vetted
    /// answer carried: no invented event, no other surface.
    #[tokio::test]
    async fn a_producer_event_must_be_one_the_answer_carried() {
        let d = dispatcher();
        for (trace, name, surface) in [
            ("ic-ev-name", "triton.in_card:approve", "triton-a2ui-0"),
            ("ic-ev-surf", "triton.in_card:refresh", "triton-a2ui-9"),
        ] {
            let ev = InCardEvent {
                surface_id: surface.into(),
                name: name.into(),
                source_component_id: "b".into(),
                context: json!({}),
                token: Some(token("pages", refresh(), json!({}), "alice")),
            };
            let e = d
                .invoke_in_card(ev, principal("alice", trace))
                .await
                .unwrap_err();
            assert!(matches!(e, TritonError::Validation(_)), "{trace}: {e}");
            assert_eq!(line(trace).result, "error:validation");
        }
    }

    /// A revoked or scope-restricted principal: refused, with the in-card
    /// part on the one line.
    #[tokio::test]
    async fn revoked_and_restricted_principals_are_refused_with_the_in_card_part() {
        let mut controls = Controls::unenforced();
        controls
            .denied_principals
            .insert(("acme".into(), "mallory".into()));
        controls
            .scope_restrictions
            .insert("pairing".into(), HashSet::from(["enrol".to_string()]));
        let d = Dispatcher::new(Arc::new(ToolRegistry::new()), "test", controls)
            .with_upstream(Arc::new(Pages))
            .with_in_card_tools(["pages".to_string()]);
        let t = token("pages", Scope::Rerun, json!({}), "mallory");
        let e = d
            .invoke_in_card(rerun(t), principal("mallory", "ic-rev"))
            .await
            .unwrap_err();
        assert!(matches!(e, TritonError::Forbidden(_)), "{e}");
        let l = line("ic-rev");
        assert_eq!(l.in_card.unwrap().token, "unchecked");
        let mut p = principal("pat", "ic-scope");
        p.scopes = vec!["pairing".into()];
        let t = token("pages", Scope::Rerun, json!({}), "pat");
        let e = d.invoke_in_card(rerun(t), p).await.unwrap_err();
        assert!(matches!(e, TritonError::Forbidden(_)), "{e}");
        assert_eq!(line("ic-scope").in_card.unwrap().token, "verified");
    }

    #[tokio::test]
    async fn a_bad_token_is_refused_with_one_line() {
        let d = dispatcher();
        let past = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            - 3 * 3600;
        let expired = sign_at(
            ActionSigner::global(),
            "pages",
            &Scope::Rerun,
            &json!({}),
            holder("alice"),
            past,
        )
        .unwrap();
        let bobs = token("pages", Scope::Rerun, json!({}), "bob");
        for (trace, tok, reason) in [
            ("ic-exp", Some(expired), "expired"),
            ("ic-bob", Some(bobs), "signature"),
            ("ic-junk", Some("junk".to_string()), "malformed"),
            ("ic-none", None, "missing"),
        ] {
            let ev = InCardEvent {
                token: tok,
                ..rerun(String::new())
            };
            let e = d
                .invoke_in_card(ev, principal("alice", trace))
                .await
                .unwrap_err();
            assert!(matches!(e, TritonError::Auth(_)), "{trace}: {e}");
            let l = line(trace);
            assert_eq!(l.tool, TOOL_NAME);
            assert_eq!(l.result, "error:auth");
            assert_eq!(l.in_card.unwrap().token, reason, "{trace}");
        }
    }

    #[tokio::test]
    async fn only_declared_read_only_tools_run_in_place() {
        // Declared but not read-only.
        let d = dispatcher();
        let t = token("file_request", Scope::Rerun, json!({}), "alice");
        let e = d
            .invoke_in_card(rerun(t), principal("alice", "ic-w"))
            .await
            .unwrap_err();
        assert!(matches!(e, TritonError::Forbidden(_)), "{e}");
        assert_eq!(line("ic-w").result, "error:forbidden");
        // Read-only but not declared.
        let d = Dispatcher::new(
            Arc::new(ToolRegistry::new()),
            "test",
            DispatchControls::unenforced(),
        )
        .with_upstream(Arc::new(Pages));
        assert!(!d.has_in_card_tools());
        let t = token("pages", Scope::Rerun, json!({}), "alice");
        let e = d
            .invoke_in_card(rerun(t), principal("alice", "ic-nd"))
            .await
            .unwrap_err();
        assert!(matches!(e, TritonError::Forbidden(_)), "{e}");
    }

    #[tokio::test]
    async fn an_event_must_fit_its_token() {
        let d = dispatcher();
        // A rerun token fired as a producer event, and the reverse.
        let t = token("pages", Scope::Rerun, json!({}), "alice");
        let ev = InCardEvent {
            name: "triton.in_card:refresh".into(),
            ..rerun(t)
        };
        let e = d
            .invoke_in_card(ev, principal("alice", "ic-f1"))
            .await
            .unwrap_err();
        assert!(matches!(e, TritonError::Validation(_)), "{e}");
        let t = token("pages", refresh(), json!({}), "alice");
        let ev = InCardEvent {
            name: "refresh".into(),
            ..rerun(t)
        };
        assert!(
            d.invoke_in_card(ev, principal("alice", "ic-f2"))
                .await
                .is_err()
        );
        // An oversized context.
        let t = token("pages", Scope::Rerun, json!({}), "alice");
        let ev = InCardEvent {
            context: json!({ "big": "x".repeat(MAX_CONTEXT_BYTES) }),
            ..rerun(t)
        };
        let e = d
            .invoke_in_card(ev, principal("alice", "ic-f3"))
            .await
            .unwrap_err();
        assert!(matches!(e, TritonError::Validation(_)), "{e}");
    }

    #[tokio::test]
    async fn the_rate_limit_is_per_principal() {
        let d = dispatcher().with_in_card_rate(1, 2);
        let go = |sub: &'static str, trace: String| {
            let t = token("pages", Scope::Rerun, json!({}), sub);
            let d = &d;
            async move { d.invoke_in_card(rerun(t), principal(sub, &trace)).await }
        };
        assert!(go("alice", "ic-r1".into()).await.is_ok());
        assert!(go("alice", "ic-r2".into()).await.is_ok());
        let e = go("alice", "ic-r3".into()).await.unwrap_err();
        assert!(matches!(e, TritonError::RateLimited(_)), "{e}");
        assert_eq!(line("ic-r3").result, "error:ratelimit");
        // Another principal has its own bucket.
        assert!(go("bob", "ic-r4".into()).await.is_ok());
    }

    /// The throttle is keyed like the token: the same `sub` vouched for by
    /// another issuer is another principal.
    #[tokio::test]
    async fn the_rate_limit_is_per_issuer_and_subject() {
        let d = dispatcher().with_in_card_rate(1, 1);
        let with_issuer = |iss: &str, trace: &str| {
            let mut p = principal("alice", trace);
            p.idp = Some(crate::principal::IdpIdentity {
                issuer: Some(iss.to_string()),
                ..Default::default()
            });
            p
        };
        for (trace, iss) in [
            ("ic-i1", "https://a.example"),
            ("ic-i2", "https://b.example"),
        ] {
            let p = with_issuer(iss, trace);
            let t = sign(
                ActionSigner::global(),
                "pages",
                &Scope::Rerun,
                &json!({}),
                Holder::of(&p),
            )
            .unwrap();
            assert!(
                d.invoke_in_card(rerun(t), p).await.is_ok(),
                "{iss}: its own bucket"
            );
        }
        let p = with_issuer("https://a.example", "ic-i3");
        let t = sign(
            ActionSigner::global(),
            "pages",
            &Scope::Rerun,
            &json!({}),
            Holder::of(&p),
        )
        .unwrap();
        let e = d.invoke_in_card(rerun(t), p).await.unwrap_err();
        assert!(matches!(e, TritonError::RateLimited(_)), "{e}");
    }
}
