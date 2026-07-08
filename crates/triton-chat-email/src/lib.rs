//! `triton-chat-email` — the L6′ surface mapper for **email**, the one
//! channel that renders the agent's canonical A2UI surface **complete**.
//!
//! Every other chat mapper degrades the surface to fit a limited primitive
//! set: bubbles/cards drop the report chart and *defer* the action buttons
//! (`deferred_buttons: N`). HTML email has none of those limits — buttons
//! become styled `<a>` links, a dashboard becomes an HTML `<table>`, the
//! report renders inline, and the reply carries a real **subject** line (the
//! one field no other mapper produces). The only interaction email can't host
//! is a form *submit* (there's no tool callback from an inbox), so a `Form`
//! renders its fields read-only and counts `deferred_forms`.
//!
//! PR 1 ships the mapper + the `/v1/surface/render` preview arm. The outbound
//! [`OutboundCourier`](triton_core::OutboundCourier) that delivers it over a
//! transactional-email API lands in a follow-up PR; it renders through the
//! SAME [`surface_mapper::try_render_surface`], so the preview can't drift
//! from what a recipient receives.

pub mod surface_mapper;
