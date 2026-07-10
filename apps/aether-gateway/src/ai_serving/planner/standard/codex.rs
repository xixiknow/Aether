#[cfg(test)]
#[path = "codex/tests.rs"]
mod tests;

pub(crate) use crate::ai_serving::{
    apply_codex_openai_responses_special_body_edits, apply_codex_openai_special_headers,
};

pub(crate) fn codex_model_capabilities_for_transport(
    transport: &crate::ai_serving::GatewayProviderTransportSnapshot,
    provider_api_format: &str,
    provider_model: &str,
    source_model: &str,
) -> Option<crate::ai_serving::CodexResponsesModelCapabilities> {
    if !transport
        .provider
        .provider_type
        .trim()
        .eq_ignore_ascii_case("codex")
        || !crate::ai_serving::is_openai_responses_family_format(provider_api_format)
    {
        return None;
    }
    Some(
        crate::ai_serving::resolve_codex_responses_model_capabilities(
            provider_model,
            source_model,
            transport.key.upstream_metadata.as_ref(),
        ),
    )
}
