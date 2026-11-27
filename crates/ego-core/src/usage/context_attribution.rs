//! Observational tool payload sizes. Never accounting or provider occupancy.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::event::domain::{DomainEvent, EventKind};
use crate::event::id::ItemId;
use crate::permit::ToolResultProvenance;

/// Local payload estimates exclude protocol framing, non-tool context and replay costs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextAttribution {
    pub source: String,
    pub provenance: String,
    pub scope: String,
    /// Largest estimated request + result contribution first; name breaks ties.
    pub tools: Vec<ToolContextEstimate>,
    pub unmatched_results: u64,
}

impl Default for ContextAttribution {
    fn default() -> Self {
        Self {
            source: "durable_tool_events".into(),
            provenance: "utf8_bytes_div_4_ceil".into(),
            scope: "local_session_requests_in_report_period".into(),
            tools: Vec::new(),
            unmatched_results: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolContextEstimate {
    pub tool: String,
    pub request_estimated_tokens: u64,
    pub result_estimated_tokens: u64,
    /// Subset of request + result estimates, not an additional contribution.
    pub duplicate_read_estimated_tokens: u64,
    pub calls: Vec<ToolContextCall>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolContextCall {
    pub item: ItemId,
    pub request_seq: u64,
    pub request_bytes: u64,
    pub request_estimated_tokens: u64,
    pub result: Option<ToolContextResult>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolContextResult {
    pub seq: u64,
    pub bytes: u64,
    pub estimated_tokens: u64,
    pub provenance: String,
    pub ok: bool,
    /// Earlier successful builtin read with equal parsed arguments and result text.
    pub duplicate_of_request_seq: Option<u64>,
}

type ReadKey = (String, [u8; 32], [u8; 32]);

struct Call {
    tool: String,
    args: Option<[u8; 32]>,
    included: bool,
    observation: ToolContextCall,
}

#[derive(Default)]
pub(super) struct AttributionBuilder {
    calls: Vec<Call>,
    pending: BTreeMap<ItemId, usize>,
    reads: BTreeMap<ReadKey, u64>,
    unmatched_results: u64,
}

impl AttributionBuilder {
    pub fn observe(&mut self, event: &DomainEvent, included: bool) {
        match &event.kind {
            EventKind::ToolCallStarted { name, args, .. } => {
                let Some(item) = event.env.item else { return };
                let bytes = args.len() as u64;
                let canonical =
                    serde_json::from_str::<serde_json::Value>(args)
                        .ok()
                        .map(|mut value| {
                            value.sort_all_objects();
                            digest(value.to_string().as_bytes())
                        });
                self.pending.insert(item, self.calls.len());
                self.calls.push(Call {
                    tool: name.clone(),
                    args: canonical,
                    included,
                    observation: ToolContextCall {
                        item,
                        request_seq: event.env.seq.get(),
                        request_bytes: bytes,
                        request_estimated_tokens: estimate(bytes),
                        result: None,
                    },
                });
            }
            EventKind::ToolCallCompleted { ok, result } => {
                let Some(index) = event.env.item.and_then(|item| self.pending.remove(&item)) else {
                    self.unmatched_results += u64::from(included);
                    return;
                };
                let call = &mut self.calls[index];
                let text = result.model_text();
                let bytes = text.len() as u64;
                let builtin = matches!(result.provenance(), ToolResultProvenance::Builtin);
                let duplicate = if *ok && builtin && is_read(&call.tool) {
                    call.args.and_then(|args| {
                        let key = (call.tool.clone(), args, digest(text.as_bytes()));
                        let previous = self.reads.get(&key).copied();
                        self.reads
                            .entry(key)
                            .or_insert(call.observation.request_seq);
                        previous.filter(|seq| *seq < call.observation.request_seq)
                    })
                } else {
                    None
                };
                call.observation.result = Some(ToolContextResult {
                    seq: event.env.seq.get(),
                    bytes,
                    estimated_tokens: estimate(bytes),
                    provenance: if builtin { "builtin" } else { "foreign" }.into(),
                    ok: *ok,
                    duplicate_of_request_seq: duplicate,
                });
            }
            _ => {}
        }
    }

    pub fn finish(self) -> ContextAttribution {
        let mut tools = BTreeMap::<String, ToolContextEstimate>::new();
        for call in self.calls.into_iter().filter(|call| call.included) {
            let tool = tools
                .entry(call.tool.clone())
                .or_insert_with(|| ToolContextEstimate {
                    tool: call.tool,
                    request_estimated_tokens: 0,
                    result_estimated_tokens: 0,
                    duplicate_read_estimated_tokens: 0,
                    calls: Vec::new(),
                });
            let observation = call.observation;
            tool.request_estimated_tokens += observation.request_estimated_tokens;
            if let Some(result) = &observation.result {
                tool.result_estimated_tokens += result.estimated_tokens;
                if result.duplicate_of_request_seq.is_some() {
                    tool.duplicate_read_estimated_tokens +=
                        observation.request_estimated_tokens + result.estimated_tokens;
                }
            }
            tool.calls.push(observation);
        }
        let mut tools: Vec<_> = tools.into_values().collect();
        tools.sort_by(|left, right| {
            let total = |tool: &ToolContextEstimate| {
                u128::from(tool.request_estimated_tokens) + u128::from(tool.result_estimated_tokens)
            };
            total(right)
                .cmp(&total(left))
                .then_with(|| left.tool.cmp(&right.tool))
        });
        ContextAttribution {
            tools,
            unmatched_results: self.unmatched_results,
            ..ContextAttribution::default()
        }
    }
}

fn digest(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn estimate(bytes: u64) -> u64 {
    bytes.div_ceil(4)
}

fn is_read(name: &str) -> bool {
    matches!(
        name,
        "read" | "glob" | "grep" | "read_tool_result" | "web_fetch"
    )
}
