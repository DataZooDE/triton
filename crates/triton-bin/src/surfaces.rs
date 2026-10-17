//! #352: the channel → surface registry the dispatcher hints to upstream
//! agents with. Built from each chat crate's `SURFACE` profile, so the hint
//! is the very profile the channel's mapper enforces.

use triton_core::SurfaceRegistry;

/// Every channel this binary can dispatch from: the HTTP trio (core
/// defaults) plus one profile per chat adapter. WhatsApp Cloud and the
/// WhatsApp Web bridge are one `whatsapp` channel with one mapper.
pub fn registry() -> SurfaceRegistry {
    let mut r = SurfaceRegistry::with_trio_defaults();
    for profile in [
        &triton_chat_whatsapp::surface_mapper::SURFACE,
        &triton_chat_signal::surface_mapper::SURFACE,
        &triton_chat_telegram::surface_mapper::SURFACE,
        &triton_chat_discord::surface_mapper::SURFACE,
        &triton_chat_googlechat::surface_mapper::SURFACE,
        &triton_chat_msteams::surface_mapper::SURFACE,
        &triton_chat_email::surface_mapper::SURFACE,
        &triton_chat_twilio::surface_mapper::SURFACE_WHATSAPP,
        &triton_chat_twilio::surface_mapper::SURFACE_RCS,
    ] {
        r.insert(profile, None);
    }
    r
}

/// `TRITON_STATIC_UPSTREAM_SURFACE_HINT=off` (or `false`/`0`) drops the
/// `_triton.surface` body field for an agent that rejects unknown args. On by
/// default.
pub fn hint_enabled() -> bool {
    !matches!(
        std::env::var("TRITON_STATIC_UPSTREAM_SURFACE_HINT")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "off" | "false" | "0"
    )
}
