//! Gemini Enterprise surface mapper — a markdown-forward answer surface.
//!
//! **Preview-only** (no live courier yet): the crate exists so the
//! Explorer's per-turn channel preview can show how a canonical A2UI surface
//! degrades onto **Gemini Enterprise / Agentspace**. Unlike the chat channels
//! (plain text, or native cards), Gemini's answer surface renders rich
//! **markdown** — so this mapper is the one that renders a `Dashboard` inline
//! as a markdown **table** and a `Sources` component as a numbered
//! **citation** list, rather than deferring or rasterising them. When a live
//! Gemini courier lands it reuses this exact mapper, the same way the chat
//! couriers share theirs (so the preview can't drift from production).
pub mod surface_mapper;
