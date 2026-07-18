//! Aggregated integration-test binary for `triton-tests` (`it`).
//!
//! Each file under `tests/` is included here as a module so the whole
//! suite links ONCE instead of as ~58 separate test binaries (CI
//! link-memory; see doc/realizations.md §7 and Cargo.toml `autotests`).
//! To add a test file, drop it in `tests/` and add a `mod` line below.
//!
//! `dead_code` is allowed crate-wide here: a helper that was a crate-root
//! `pub fn` in its own binary (always "reachable", never dead) becomes a
//! plain module item once aggregated, so an otherwise-fine per-file helper
//! used by only some `cfg`s would now warn. That's an artifact of the
//! consolidation, not real dead code.
#![allow(dead_code, clippy::duplicate_mod)]

#[path = "../a2a.rs"]
mod a2a;
#[path = "../a2ui_parity.rs"]
mod a2ui_parity;
#[path = "../acc_hardening.rs"]
mod acc_hardening;
#[path = "../audit_retrieval.rs"]
mod audit_retrieval;
#[path = "../consumer_smoke.rs"]
mod consumer_smoke;
#[path = "../cors.rs"]
mod cors;
#[path = "../demo_panel.rs"]
mod demo_panel;
#[path = "../deploy_manifest.rs"]
mod deploy_manifest;
#[path = "../deploy_triton_manifest.rs"]
mod deploy_triton_manifest;
#[path = "../dev_token.rs"]
mod dev_token;
#[path = "../discord.rs"]
mod discord;
#[path = "../discord_dashboard.rs"]
mod discord_dashboard;
#[path = "../discord_gateway.rs"]
mod discord_gateway;
#[path = "../echo.rs"]
mod echo;
#[path = "../email_outbound.rs"]
mod email_outbound;
#[path = "../explorer_contract.rs"]
mod explorer_contract;
#[path = "../forward_principal.rs"]
mod forward_principal;
#[path = "../forwarded_auth.rs"]
mod forwarded_auth;
#[path = "../google_chat.rs"]
mod google_chat;
#[path = "../healthz.rs"]
mod healthz;
#[path = "../manifest.rs"]
mod manifest;
#[path = "../manifest_endpoint.rs"]
mod manifest_endpoint;
#[path = "../mcp.rs"]
mod mcp;
#[path = "../mcp_apps_acceptance.rs"]
mod mcp_apps_acceptance;
#[path = "../metrics.rs"]
mod metrics;
#[path = "../metrics_endpoint.rs"]
mod metrics_endpoint;
#[path = "../msteams.rs"]
mod msteams;
#[path = "../oidc.rs"]
mod oidc;
#[path = "../optional_adapters.rs"]
mod optional_adapters;
#[path = "../outbound.rs"]
mod outbound;
#[path = "../outbound_issuer.rs"]
mod outbound_issuer;
#[path = "../process_liveness.rs"]
mod process_liveness;
#[path = "../rasterizer.rs"]
mod rasterizer;
#[path = "../rate_limit.rs"]
mod rate_limit;
#[path = "../rest_tools.rs"]
mod rest_tools;
#[path = "../runtime_discovery.rs"]
mod runtime_discovery;
#[path = "../signal.rs"]
mod signal;
#[path = "../sigterm.rs"]
mod sigterm;
#[path = "../static_upstream.rs"]
mod static_upstream;
#[path = "../streaming.rs"]
mod streaming;
#[path = "../surface_render.rs"]
mod surface_render;
#[path = "../telegram.rs"]
mod telegram;
#[path = "../telegram_callback.rs"]
mod telegram_callback;
#[path = "../telegram_courier.rs"]
mod telegram_courier;
#[path = "../telegram_dashboard.rs"]
mod telegram_dashboard;
#[path = "../telegram_dashboard_upstream.rs"]
mod telegram_dashboard_upstream;
#[path = "../telegram_form.rs"]
mod telegram_form;
#[path = "../telegram_longpoll.rs"]
mod telegram_longpoll;
#[path = "../telegram_longpoll_dashboard.rs"]
mod telegram_longpoll_dashboard;
#[path = "../telegram_surface.rs"]
mod telegram_surface;
#[path = "../telegram_upstream_identity.rs"]
mod telegram_upstream_identity;
#[path = "../tool_trace.rs"]
mod tool_trace;
#[path = "../twilio_rcs.rs"]
mod twilio_rcs;
#[path = "../twilio_whatsapp.rs"]
mod twilio_whatsapp;
#[path = "../twilio_whatsapp_button_reply.rs"]
mod twilio_whatsapp_button_reply;
#[path = "../twilio_whatsapp_status_callback.rs"]
mod twilio_whatsapp_status_callback;
#[path = "../twilio_whatsapp_template.rs"]
mod twilio_whatsapp_template;
#[path = "../upstream_listing.rs"]
mod upstream_listing;
#[path = "../version.rs"]
mod version;
#[path = "../whatsapp.rs"]
mod whatsapp;
#[path = "../whatsapp_bridge.rs"]
mod whatsapp_bridge;
#[path = "../whatsapp_dashboard.rs"]
mod whatsapp_dashboard;
#[path = "../whatsapp_env_secrets.rs"]
mod whatsapp_env_secrets;
#[path = "../whatsapp_inbound_tool.rs"]
mod whatsapp_inbound_tool;
#[path = "../whatsapp_interactive.rs"]
mod whatsapp_interactive;
#[path = "../whatsapp_single_port.rs"]
mod whatsapp_single_port;
#[path = "../whatsapp_template.rs"]
mod whatsapp_template;
#[path = "../whatsapp_upstream_identity.rs"]
mod whatsapp_upstream_identity;
