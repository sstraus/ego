//! Render the shared observational report, without inspecting or changing history.

use ego_core::usage::UsageReport;

pub(super) fn lines(report: &UsageReport) -> Vec<String> {
    let mut lines = Vec::new();
    for session in &report.sessions {
        let attribution = &session.context_attribution;
        if attribution.tools.is_empty() && attribution.unmatched_results == 0 {
            continue;
        }
        lines.push(format!(
            "tool context estimates · session {}",
            session.session
        ));
        lines.push(format!(
            "source={} provenance={} scope={}",
            attribution.source, attribution.provenance, attribution.scope
        ));
        lines.push("Argument and model-visible result text only; excludes framing and non-tool context. Not billing or provider occupancy. Duplicate estimates are a subset, not extra tokens.".into());
        for tool in &attribution.tools {
            lines.push(format!(
                "{}: request ~{} result ~{} duplicate-read ~{} tokens",
                tool.tool,
                tool.request_estimated_tokens,
                tool.result_estimated_tokens,
                tool.duplicate_read_estimated_tokens
            ));
            for call in &tool.calls {
                let result = call.result.as_ref().map_or_else(
                    || "result unknown".into(),
                    |result| {
                        let duplicate = result
                            .duplicate_of_request_seq
                            .map_or_else(String::new, |seq| {
                                format!(" duplicate-of=request-seq:{seq}")
                            });
                        format!(
                            "result-seq:{} {} bytes ~{} tokens provenance={} ok={}{}",
                            result.seq,
                            result.bytes,
                            result.estimated_tokens,
                            result.provenance,
                            result.ok,
                            duplicate
                        )
                    },
                );
                lines.push(format!(
                    "  item={} request-seq:{} {} bytes ~{} tokens; {result}",
                    call.item, call.request_seq, call.request_bytes, call.request_estimated_tokens
                ));
            }
        }
        if attribution.unmatched_results > 0 {
            lines.push(format!(
                "unmatched results: {} (not attributed)",
                attribution.unmatched_results
            ));
        }
    }
    lines
}
