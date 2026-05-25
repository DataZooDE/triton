//! v0.2 PR 32 — per-chat numbered-prompts state machine for the
//! Telegram adapter.
//!
//! Telegram has no native form. Per architecture.md §8.7 the L6′
//! degrade rule `forms: numbered_prompts` flattens a
//! `Component::Form` into a stateful per-chat dialog: the adapter
//! sends one prompt per field, accumulates the user's plain-text
//! replies, coerces them per the field's `FormFieldKind`, and once
//! every field has a value dispatches `(submit_tool, args)`.
//!
//! ### State key
//!
//! `(chat_id, sender_sub)` — the chat_id is the Telegram numeric
//! chat the user is in; the sender_sub is the verified Triton sub
//! from the manifest's `sender_table` (NOT the platform user id —
//! the same platform id can resolve to different subs across
//! senders_table swaps, and we want the key to match whoever
//! installed the form). The pair is needed because one chat can
//! host different sub→tenant resolutions across consecutive
//! manifest reloads.
//!
//! ### Per-tenant cap
//!
//! In-memory only (G-8: no on-disk state). The store enforces a
//! per-tenant cap so a noisy tenant can't OOM the binary by
//! installing forms it never completes. When the cap is hit, the
//! OLDEST in-flight form for that tenant is evicted (LRU) and an
//! `Evicted` event is returned so the adapter can audit the
//! eviction.
//!
//! ### Pure logic, no I/O
//!
//! This module is unit-testable without spinning the binary: every
//! transition is a pure function of (state, input). The adapter
//! wires the courier separately.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use serde_json::{Map, Value};
use triton_core::a2ui::{FormField, FormFieldKind};

/// Default per-tenant cap on the number of in-flight forms.
/// Sized to keep the worst-case map small (10s of KB per tenant)
/// while leaving plenty of headroom for healthy traffic.
pub const DEFAULT_PER_TENANT_CAP: usize = 100;

/// Maximum number of fields we'll accept in a single form. The
/// numbered-prompts flow stays usable only when the field count
/// is small (each field is a round-trip); anything larger is
/// almost certainly a misuse of the surface (and would let a
/// hostile tool keep a chat occupied indefinitely).
pub const MAX_FIELDS_PER_FORM: usize = 16;

/// One in-flight form, scoped to a (chat_id, sender_sub) pair.
#[derive(Debug, Clone)]
pub struct ActiveForm {
    /// Submit tool the form will dispatch when all fields are filled.
    pub submit_tool: String,
    /// Field definitions, in the order they were declared by the
    /// tool. The state machine walks this list left-to-right.
    pub fields: Vec<FormField>,
    /// Accumulated args map. Every key from `fields` is present
    /// from install time; values start as `null` and get filled in
    /// as the user replies (or stay `null` if the field is
    /// optional and the user submitted empty text).
    pub args: Map<String, Value>,
    /// Index into `fields` for the NEXT prompt to send. When this
    /// equals `fields.len()`, the form is complete.
    pub step: usize,
    /// Verified tenant id at install time. Used for the per-tenant
    /// cap accounting on eviction.
    pub tenant: String,
}

impl ActiveForm {
    fn new(submit_tool: String, fields: Vec<FormField>, tenant: String) -> Self {
        let mut args = Map::with_capacity(fields.len());
        for f in &fields {
            args.insert(f.name.clone(), Value::Null);
        }
        Self {
            submit_tool,
            fields,
            args,
            step: 0,
            tenant,
        }
    }

    /// Is every field filled? Once true, the adapter dispatches
    /// `(submit_tool, args)` and clears the slot.
    pub fn is_complete(&self) -> bool {
        self.step >= self.fields.len()
    }

    /// Field the next user message will fill.
    pub fn current_field(&self) -> Option<&FormField> {
        self.fields.get(self.step)
    }

    /// Human-readable prompt for the next field, e.g.
    /// `"1/3 — Your name (required)"`. Returns None when the form
    /// is already complete.
    pub fn next_prompt(&self) -> Option<String> {
        let f = self.current_field()?;
        let total = self.fields.len();
        let label = &f.label;
        let suffix = if f.required {
            " (required)"
        } else {
            " (optional, send a blank message to skip)"
        };
        Some(format!("{}/{} — {}{}", self.step + 1, total, label, suffix))
    }
}

/// Identity for one in-flight form. Pair (chat_id, sender_sub).
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct FormKey {
    pub chat_id: i64,
    pub sender_sub: String,
}

/// What `install` returns. Useful for auditing the eviction case
/// in the adapter without re-entering the lock.
#[derive(Debug)]
pub enum InstallOutcome {
    /// Form installed cleanly; the adapter should send the first
    /// prompt (`active.next_prompt()` is also available pre-lock
    /// drop if you peek).
    Installed,
    /// Form installed, but the per-tenant cap was already at
    /// capacity. The oldest form for this tenant was dropped to
    /// make room; the adapter should emit an audit line marking
    /// the eviction so the operator sees the noisy tenant.
    InstalledEvicted {
        /// Key of the evicted form, so the adapter can name it in
        /// the audit line.
        evicted: FormKey,
    },
}

/// Outcome of feeding the user's next plain-text message into an
/// active form. The adapter inspects the variant to decide whether
/// to send another prompt, dispatch the submit tool, or drop the
/// slot.
#[derive(Debug)]
pub enum AdvanceOutcome {
    /// Stored the value, advanced one step. Adapter sends the next
    /// prompt.
    NeedMore,
    /// All fields collected. Adapter dispatches
    /// `(submit_tool, args)` and clears the slot. The store has
    /// already cleared the slot for this key.
    Complete { submit_tool: String, args: Value },
    /// Couldn't coerce the user's input to the field's kind, or
    /// the field was required and the input was empty. Adapter
    /// re-sends the SAME prompt with `reason` appended; the field
    /// index is NOT advanced.
    Reprompt { reason: String },
}

/// In-memory store of every in-flight form. Cheap to clone the
/// outer struct; the lock + maps live behind an `Arc` so the
/// adapter can pass it freely between handler tasks.
pub struct FormStateStore {
    inner: Mutex<Inner>,
    /// Per-tenant cap. Configurable per-adapter so tests can drive
    /// the eviction path with a tiny cap; production uses
    /// [`DEFAULT_PER_TENANT_CAP`].
    cap_per_tenant: usize,
}

struct Inner {
    /// Active forms keyed by `(chat_id, sender_sub)`.
    forms: HashMap<FormKey, ActiveForm>,
    /// LRU queue per tenant. Oldest in front, newest at back.
    /// We push to the back on `install` and pop from the front
    /// when the cap is hit. On `cancel`/`complete` we remove the
    /// key from this queue too (linear scan; queues are bounded
    /// at `cap_per_tenant`, so this stays cheap).
    per_tenant_order: HashMap<String, VecDeque<FormKey>>,
}

impl FormStateStore {
    /// Production constructor — uses [`DEFAULT_PER_TENANT_CAP`].
    pub fn new() -> Self {
        Self::with_cap(DEFAULT_PER_TENANT_CAP)
    }

    /// Constructor allowing the per-tenant cap to be set. Used by
    /// the integration test that drives the eviction path with a
    /// tiny cap so we don't have to install 100 forms to prove
    /// the LRU code runs.
    pub fn with_cap(cap_per_tenant: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                forms: HashMap::new(),
                per_tenant_order: HashMap::new(),
            }),
            cap_per_tenant: cap_per_tenant.max(1),
        }
    }

    /// Is there an active form for this (chat, sender)? Cheap
    /// peek the inbound handler uses to decide between "feed the
    /// reply into the form" and "fall through to `route_command`".
    pub fn has_active(&self, key: &FormKey) -> bool {
        self.lock().forms.contains_key(key)
    }

    /// Return the next prompt text for this (chat, sender), if a
    /// form is active. Doesn't mutate state; callers that have
    /// already advanced (via [`Self::advance`]) get the next
    /// prompt from the result of advance directly.
    pub fn peek_prompt(&self, key: &FormKey) -> Option<String> {
        self.lock().forms.get(key).and_then(|f| f.next_prompt())
    }

    /// Install a new form for this (chat, sender). Returns the
    /// outcome so the adapter can audit per-tenant evictions. The
    /// FIRST prompt should be fetched via [`Self::peek_prompt`]
    /// (or by inspecting the field directly) AFTER this returns —
    /// we don't pre-format it here because the format is the
    /// adapter's prose, not the state machine's.
    pub fn install(
        &self,
        key: FormKey,
        submit_tool: String,
        fields: Vec<FormField>,
        tenant: String,
    ) -> Result<InstallOutcome, InstallError> {
        if fields.is_empty() {
            return Err(InstallError::NoFields);
        }
        if fields.len() > MAX_FIELDS_PER_FORM {
            return Err(InstallError::TooManyFields(fields.len()));
        }
        // Defend against duplicate / empty field names. The state
        // machine relies on names being unique non-empty strings
        // because `args` is keyed by name; a duplicate would
        // silently collapse to one slot.
        let mut seen = std::collections::HashSet::with_capacity(fields.len());
        for f in &fields {
            if f.name.is_empty() {
                return Err(InstallError::EmptyFieldName);
            }
            if !seen.insert(f.name.as_str()) {
                return Err(InstallError::DuplicateFieldName(f.name.clone()));
            }
        }

        let form = ActiveForm::new(submit_tool, fields, tenant.clone());
        let mut inner = self.lock();
        // If the same key already has a form, replace it. Per
        // architecture: a fresh form-only surface invalidates any
        // previous in-flight form for that (chat, sender).
        // Replacing doesn't change the per-tenant count.
        let replacing = inner.forms.contains_key(&key);

        let evicted = if !replacing {
            self.maybe_evict(&mut inner, &tenant)
        } else {
            None
        };

        inner.forms.insert(key.clone(), form);
        if !replacing {
            inner
                .per_tenant_order
                .entry(tenant)
                .or_default()
                .push_back(key);
        }

        Ok(match evicted {
            Some(k) => InstallOutcome::InstalledEvicted { evicted: k },
            None => InstallOutcome::Installed,
        })
    }

    /// Maybe evict the oldest form for `tenant` to make room. Returns
    /// the evicted key if one was removed. Caller must hold the lock.
    fn maybe_evict(&self, inner: &mut Inner, tenant: &str) -> Option<FormKey> {
        let queue = inner.per_tenant_order.get(tenant)?;
        if queue.len() < self.cap_per_tenant {
            return None;
        }
        let evicted_key = inner
            .per_tenant_order
            .get_mut(tenant)
            .and_then(|q| q.pop_front())?;
        inner.forms.remove(&evicted_key);
        Some(evicted_key)
    }

    /// Feed the user's next plain-text message into the active
    /// form. Returns the next action the adapter should take.
    /// If there is no active form for this key, returns `None`
    /// so the caller falls through to `route_command`.
    pub fn advance(&self, key: &FormKey, message: &str) -> Option<AdvanceOutcome> {
        let mut inner = self.lock();
        let form = inner.forms.get_mut(key)?;
        let Some(field) = form.current_field().cloned() else {
            // Already complete — shouldn't happen because Complete
            // clears the slot. Defensive: clear and treat as no
            // active form.
            let key = key.clone();
            drop_form(&mut inner, &key);
            return None;
        };

        let trimmed = message.trim();
        let value = match coerce(&field, trimmed) {
            Ok(v) => v,
            Err(reason) => return Some(AdvanceOutcome::Reprompt { reason }),
        };
        form.args.insert(field.name.clone(), value);
        form.step += 1;

        if form.is_complete() {
            // Tear down the slot and return the dispatch payload.
            let submit_tool = form.submit_tool.clone();
            let args = Value::Object(form.args.clone());
            let key = key.clone();
            drop_form(&mut inner, &key);
            Some(AdvanceOutcome::Complete { submit_tool, args })
        } else {
            Some(AdvanceOutcome::NeedMore)
        }
    }

    /// Cancel the active form for (chat, sender) if one exists.
    /// Returns true when something was actually cancelled — the
    /// adapter uses that to decide between "form cancelled" and
    /// "no form was active".
    pub fn cancel(&self, key: &FormKey) -> bool {
        let mut inner = self.lock();
        if !inner.forms.contains_key(key) {
            return false;
        }
        drop_form(&mut inner, key);
        true
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // Mutex poisoning here would mean a panic somewhere inside
        // the lock — recover the inner state and keep going. The
        // alternative (propagate the panic) takes down the binary
        // for a transient bug; we'd rather audit + survive.
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Test-only: peek the per-tenant size. Used by unit tests
    /// that drive the LRU eviction.
    #[cfg(test)]
    fn tenant_count(&self, tenant: &str) -> usize {
        self.lock()
            .per_tenant_order
            .get(tenant)
            .map(|q| q.len())
            .unwrap_or(0)
    }
}

impl Default for FormStateStore {
    fn default() -> Self {
        Self::new()
    }
}

fn drop_form(inner: &mut Inner, key: &FormKey) {
    if let Some(form) = inner.forms.remove(key)
        && let Some(q) = inner.per_tenant_order.get_mut(&form.tenant)
    {
        // Linear scan: q.len() ≤ cap_per_tenant.
        if let Some(idx) = q.iter().position(|k| k == key) {
            q.remove(idx);
        }
    }
}

/// Coerce the user's raw text into a JSON value matching the
/// field's declared kind. Returns the value on success or a
/// human-readable reason for the re-prompt on parse / required
/// failure.
fn coerce(field: &FormField, raw: &str) -> Result<Value, String> {
    let empty = raw.is_empty();
    if empty {
        if field.required {
            return Err(format!(
                "the `{}` field is required — please send a non-empty value",
                field.label
            ));
        }
        // Optional + empty: store explicit null and advance.
        return Ok(Value::Null);
    }
    match field.kind {
        FormFieldKind::String => Ok(Value::String(raw.to_string())),
        FormFieldKind::Integer => match raw.parse::<i64>() {
            Ok(n) => Ok(Value::from(n)),
            Err(_) => Err(format!(
                "expected an integer for `{}`, got `{}`. Please send a number.",
                field.label,
                clip(raw, 32)
            )),
        },
        FormFieldKind::Boolean => parse_bool(raw).map(Value::Bool).ok_or_else(|| {
            format!(
                "expected yes/no for `{}`, got `{}`. Reply with yes/no, true/false, or 1/0.",
                field.label,
                clip(raw, 32)
            )
        }),
    }
}

fn parse_bool(raw: &str) -> Option<bool> {
    match raw.to_ascii_lowercase().as_str() {
        "yes" | "y" | "true" | "1" => Some(true),
        "no" | "n" | "false" | "0" => Some(false),
        _ => None,
    }
}

/// Clip a raw value down to N bytes for re-prompt messages. We
/// never log full field values at info level (NFR-S: user PII).
/// Re-prompts that get echoed back into the chat carry only the
/// first ~32 chars of the offending input — enough to show the
/// user what they typed, not enough to spam logs if the user
/// pastes a novel.
fn clip(raw: &str, n: usize) -> String {
    let mut end = raw.len().min(n);
    while end > 0 && !raw.is_char_boundary(end) {
        end -= 1;
    }
    raw[..end].to_string()
}

/// Reasons [`FormStateStore::install`] can refuse the form
/// outright. These are tool-shape bugs, not user errors — the
/// adapter audits them and falls back to deferred text rendering
/// so the user at least sees the form's title.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum InstallError {
    #[error("form has no fields")]
    NoFields,
    #[error("form has {0} fields; max is {MAX_FIELDS_PER_FORM}")]
    TooManyFields(usize),
    #[error("form has a field with an empty `name`")]
    EmptyFieldName,
    #[error("form has duplicate field name `{0}`")]
    DuplicateFieldName(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(chat_id: i64, sender_sub: &str) -> FormKey {
        FormKey {
            chat_id,
            sender_sub: sender_sub.to_string(),
        }
    }

    fn field(name: &str, label: &str, kind: FormFieldKind, required: bool) -> FormField {
        FormField {
            name: name.into(),
            label: label.into(),
            kind,
            required,
        }
    }

    #[test]
    fn install_prompts_first_field() {
        let store = FormStateStore::new();
        let k = key(1, "alice");
        let outcome = store
            .install(
                k.clone(),
                "echo".into(),
                vec![field("name", "Your name", FormFieldKind::String, true)],
                "acme".into(),
            )
            .expect("installs");
        assert!(matches!(outcome, InstallOutcome::Installed));
        let prompt = store.peek_prompt(&k).expect("prompt present");
        assert!(prompt.starts_with("1/1 — Your name"));
        assert!(prompt.contains("required"));
    }

    #[test]
    fn advance_collects_string_then_completes() {
        let store = FormStateStore::new();
        let k = key(1, "alice");
        store
            .install(
                k.clone(),
                "echo".into(),
                vec![field("name", "Your name", FormFieldKind::String, true)],
                "acme".into(),
            )
            .unwrap();
        let r = store.advance(&k, "Bob").expect("advance");
        match r {
            AdvanceOutcome::Complete { submit_tool, args } => {
                assert_eq!(submit_tool, "echo");
                assert_eq!(args["name"], "Bob");
            }
            other => panic!("expected Complete, got {other:?}"),
        }
        // Slot is cleared.
        assert!(!store.has_active(&k));
    }

    #[test]
    fn advance_collects_multi_field_in_order() {
        let store = FormStateStore::new();
        let k = key(1, "alice");
        store
            .install(
                k.clone(),
                "echo".into(),
                vec![
                    field("name", "Name", FormFieldKind::String, true),
                    field("age", "Age", FormFieldKind::Integer, true),
                    field("ok", "OK?", FormFieldKind::Boolean, false),
                ],
                "acme".into(),
            )
            .unwrap();
        assert!(matches!(
            store.advance(&k, "Bob").unwrap(),
            AdvanceOutcome::NeedMore
        ));
        assert!(matches!(
            store.advance(&k, "42").unwrap(),
            AdvanceOutcome::NeedMore
        ));
        let r = store.advance(&k, "yes").unwrap();
        match r {
            AdvanceOutcome::Complete { submit_tool, args } => {
                assert_eq!(submit_tool, "echo");
                assert_eq!(args["name"], "Bob");
                assert_eq!(args["age"], 42);
                assert_eq!(args["ok"], true);
            }
            other => panic!("expected Complete, got {other:?}"),
        }
    }

    #[test]
    fn integer_parse_failure_reprompts_without_advancing() {
        let store = FormStateStore::new();
        let k = key(1, "alice");
        store
            .install(
                k.clone(),
                "echo".into(),
                vec![
                    field("age", "Age", FormFieldKind::Integer, true),
                    field("name", "Name", FormFieldKind::String, true),
                ],
                "acme".into(),
            )
            .unwrap();
        let r = store.advance(&k, "not a number").unwrap();
        assert!(matches!(r, AdvanceOutcome::Reprompt { .. }));
        // Step didn't move — same prompt comes back.
        let prompt = store.peek_prompt(&k).unwrap();
        assert!(prompt.starts_with("1/2 — Age"));
        // Now a valid number advances.
        assert!(matches!(
            store.advance(&k, "33").unwrap(),
            AdvanceOutcome::NeedMore
        ));
        let prompt2 = store.peek_prompt(&k).unwrap();
        assert!(prompt2.starts_with("2/2 — Name"));
    }

    #[test]
    fn boolean_accepts_yes_no_variants() {
        for (input, expected) in [
            ("yes", true),
            ("Y", true),
            ("true", true),
            ("1", true),
            ("no", false),
            ("N", false),
            ("FALSE", false),
            ("0", false),
        ] {
            let store = FormStateStore::new();
            let k = key(1, "alice");
            store
                .install(
                    k.clone(),
                    "echo".into(),
                    vec![field("ok", "OK?", FormFieldKind::Boolean, true)],
                    "acme".into(),
                )
                .unwrap();
            let r = store.advance(&k, input).unwrap();
            match r {
                AdvanceOutcome::Complete { args, .. } => {
                    assert_eq!(args["ok"], expected, "input={input}");
                }
                other => panic!("input {input}: expected Complete, got {other:?}"),
            }
        }
    }

    #[test]
    fn boolean_rejects_garbage_with_reprompt() {
        let store = FormStateStore::new();
        let k = key(1, "alice");
        store
            .install(
                k.clone(),
                "echo".into(),
                vec![field("ok", "OK?", FormFieldKind::Boolean, true)],
                "acme".into(),
            )
            .unwrap();
        let r = store.advance(&k, "maybe").unwrap();
        assert!(matches!(r, AdvanceOutcome::Reprompt { .. }));
        assert!(store.has_active(&k));
    }

    #[test]
    fn required_empty_reprompts_optional_empty_advances_as_null() {
        let store = FormStateStore::new();
        let k = key(1, "alice");
        store
            .install(
                k.clone(),
                "echo".into(),
                vec![
                    field("required_field", "Req", FormFieldKind::String, true),
                    field("optional_field", "Opt", FormFieldKind::String, false),
                ],
                "acme".into(),
            )
            .unwrap();
        // Required field, empty input → reprompt.
        let r = store.advance(&k, "").unwrap();
        assert!(matches!(r, AdvanceOutcome::Reprompt { .. }));
        // Same step.
        assert!(store.peek_prompt(&k).unwrap().starts_with("1/2"));
        // Fill required, then optional with empty input.
        store.advance(&k, "filled").unwrap();
        let r = store.advance(&k, "").unwrap();
        match r {
            AdvanceOutcome::Complete { args, .. } => {
                assert_eq!(args["required_field"], "filled");
                assert!(args["optional_field"].is_null());
            }
            other => panic!("expected Complete with optional=null, got {other:?}"),
        }
    }

    #[test]
    fn cancel_clears_state() {
        let store = FormStateStore::new();
        let k = key(1, "alice");
        store
            .install(
                k.clone(),
                "echo".into(),
                vec![field("n", "N", FormFieldKind::String, true)],
                "acme".into(),
            )
            .unwrap();
        assert!(store.has_active(&k));
        assert!(store.cancel(&k));
        assert!(!store.has_active(&k));
        // Cancelling again returns false (no state to clear).
        assert!(!store.cancel(&k));
    }

    #[test]
    fn state_is_per_chat_and_per_sender() {
        let store = FormStateStore::new();
        let k1 = key(1, "alice");
        let k2 = key(2, "alice");
        let k3 = key(1, "bob");
        store
            .install(
                k1.clone(),
                "echo".into(),
                vec![field("n", "N", FormFieldKind::String, true)],
                "acme".into(),
            )
            .unwrap();
        store
            .install(
                k2.clone(),
                "narrate".into(),
                vec![field("subject", "Subj", FormFieldKind::String, true)],
                "acme".into(),
            )
            .unwrap();
        store.advance(&k1, "from-chat-1").unwrap();
        // chat-2 untouched.
        assert!(store.has_active(&k2));
        let prompt = store.peek_prompt(&k2).unwrap();
        assert!(prompt.contains("Subj"));
        // bob in chat-1 has no state.
        assert!(!store.has_active(&k3));
    }

    #[test]
    fn per_tenant_cap_evicts_oldest_on_overflow() {
        let store = FormStateStore::with_cap(2);
        let f = || vec![field("n", "N", FormFieldKind::String, true)];

        let k1 = key(1, "alice");
        let k2 = key(2, "alice");
        let k3 = key(3, "alice");

        assert!(matches!(
            store
                .install(k1.clone(), "echo".into(), f(), "acme".into())
                .unwrap(),
            InstallOutcome::Installed
        ));
        assert!(matches!(
            store
                .install(k2.clone(), "echo".into(), f(), "acme".into())
                .unwrap(),
            InstallOutcome::Installed
        ));
        // Third install hits the cap → k1 (oldest) is evicted.
        let outcome = store
            .install(k3.clone(), "echo".into(), f(), "acme".into())
            .unwrap();
        match outcome {
            InstallOutcome::InstalledEvicted { evicted } => {
                assert_eq!(evicted, k1);
            }
            other => panic!("expected InstalledEvicted, got {other:?}"),
        }
        assert!(!store.has_active(&k1));
        assert!(store.has_active(&k2));
        assert!(store.has_active(&k3));
        assert_eq!(store.tenant_count("acme"), 2);
    }

    #[test]
    fn install_rejects_no_fields_and_duplicates() {
        let store = FormStateStore::new();
        let k = key(1, "alice");
        assert_eq!(
            store
                .install(k.clone(), "echo".into(), vec![], "acme".into())
                .unwrap_err(),
            InstallError::NoFields
        );
        assert_eq!(
            store
                .install(
                    k.clone(),
                    "echo".into(),
                    vec![
                        field("n", "N", FormFieldKind::String, true),
                        field("n", "N2", FormFieldKind::String, true),
                    ],
                    "acme".into()
                )
                .unwrap_err(),
            InstallError::DuplicateFieldName("n".into())
        );
        assert_eq!(
            store
                .install(
                    k.clone(),
                    "echo".into(),
                    vec![field("", "Empty", FormFieldKind::String, true)],
                    "acme".into()
                )
                .unwrap_err(),
            InstallError::EmptyFieldName
        );
    }

    #[test]
    fn install_replaces_existing_without_evicting() {
        // Re-installing a form for the same (chat, sender) replaces
        // the old one; no LRU eviction is triggered because the
        // tenant count didn't grow.
        let store = FormStateStore::with_cap(1);
        let k = key(1, "alice");
        store
            .install(
                k.clone(),
                "echo".into(),
                vec![field("a", "A", FormFieldKind::String, true)],
                "acme".into(),
            )
            .unwrap();
        let outcome = store
            .install(
                k.clone(),
                "narrate".into(),
                vec![field("subject", "S", FormFieldKind::String, true)],
                "acme".into(),
            )
            .unwrap();
        assert!(
            matches!(outcome, InstallOutcome::Installed),
            "replacing same key MUST NOT report an eviction"
        );
        // Tenant count stays at 1 (no double-counting).
        assert_eq!(store.tenant_count("acme"), 1);
        // And the new form's prompt is the replacement's.
        let prompt = store.peek_prompt(&k).unwrap();
        assert!(prompt.contains("S"));
    }
}
