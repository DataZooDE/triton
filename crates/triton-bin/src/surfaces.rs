//! #352: the channel → surface registry the dispatcher hints to upstream
//! agents with. Built from each chat crate's `SURFACE` profile, so the hint
//! is the very profile the channel's mapper enforces, plus the operator's
//! guidance overrides.

use triton_core::{SurfaceProfile, SurfaceRegistry};
use triton_manifest::{AdapterKind, MAX_SURFACE_GUIDANCE_CHARS, Manifest};

/// The profile a manifest adapter kind renders with. WhatsApp Cloud and the
/// WhatsApp Web bridge are one `whatsapp` channel with one mapper.
fn profile_for(kind: AdapterKind) -> Option<&'static SurfaceProfile> {
    Some(match kind {
        AdapterKind::WhatsappCloud | AdapterKind::WhatsappWeb => {
            &triton_chat_whatsapp::surface_mapper::SURFACE
        }
        AdapterKind::Signal => &triton_chat_signal::surface_mapper::SURFACE,
        AdapterKind::Telegram => &triton_chat_telegram::surface_mapper::SURFACE,
        AdapterKind::Discord => &triton_chat_discord::surface_mapper::SURFACE,
        AdapterKind::GoogleChat => &triton_chat_googlechat::surface_mapper::SURFACE,
        AdapterKind::MsTeams => &triton_chat_msteams::surface_mapper::SURFACE,
        AdapterKind::Email => &triton_chat_email::surface_mapper::SURFACE,
        AdapterKind::TwilioWhatsapp => &triton_chat_twilio::surface_mapper::SURFACE_WHATSAPP,
        AdapterKind::TwilioRcs => &triton_chat_twilio::surface_mapper::SURFACE_RCS,
        _ => return None,
    })
}

/// Every channel this binary can dispatch from: the HTTP trio plus one
/// profile per chat adapter, with guidance overridden by
/// `adapters.<name>.surface_guidance` (the manifest already validated its
/// length) and by `TRITON_SURFACE_GUIDANCE_REST|MCP|A2A` for the trio.
///
/// `Err` names an env override over the cap; the caller refuses boot.
pub fn registry(manifest: Option<&Manifest>) -> Result<SurfaceRegistry, String> {
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

    if let Some(m) = manifest {
        for adapter in m.adapters.values() {
            if let (Some(guidance), Some(profile)) =
                (&adapter.surface_guidance, profile_for(adapter.kind))
            {
                r.insert(profile, Some(guidance));
            }
        }
    }

    for (var, profile) in [
        ("TRITON_SURFACE_GUIDANCE_REST", &triton_core::surface::REST),
        ("TRITON_SURFACE_GUIDANCE_MCP", &triton_core::surface::MCP),
        ("TRITON_SURFACE_GUIDANCE_A2A", &triton_core::surface::A2A),
    ] {
        let Some(guidance) = std::env::var(var).ok().filter(|g| !g.trim().is_empty()) else {
            continue;
        };
        let chars = guidance.chars().count();
        if chars > MAX_SURFACE_GUIDANCE_CHARS {
            return Err(format!(
                "{var} is {chars} characters; the cap is {MAX_SURFACE_GUIDANCE_CHARS} \
                 (it rides every upstream call and lands in agents' prompts)"
            ));
        }
        r.insert(profile, Some(&guidance));
    }
    Ok(r)
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
