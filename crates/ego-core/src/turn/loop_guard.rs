//! Bound tool work independently of a provider's willingness to stop generating.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::event::id::{EventId, ItemId, TurnId};
use crate::state::SessionState;

const RESPONSE_CALLS: usize = 128;
const RESPONSE_REPEATS: usize = 8;
const TURN_CALLS: usize = 4096;
// Initial polling thresholds; tune only with workload evidence.
const TURN_REPEAT_WARNING: usize = 3;
const TURN_REPEAT_STOP: usize = RESPONSE_REPEATS;

type Fingerprint = [u8; 32];

#[derive(Debug)]
pub(super) enum Violation {
    ControlToken,
    Response,
    ResponseRepeat,
    Turn,
    TurnRepeat,
}

impl std::fmt::Display for Violation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ControlToken => f.write_str("tool arguments contain a model control token"),
            Self::Response => write!(f, "response tool call limit exceeded ({RESPONSE_CALLS})"),
            Self::ResponseRepeat => write!(
                f,
                "identical response tool call limit exceeded ({RESPONSE_REPEATS})"
            ),
            Self::Turn => write!(f, "turn tool call limit exceeded ({TURN_CALLS})"),
            Self::TurnRepeat => {
                f.write_str("repeated tool call limit exceeded after a loop warning")
            }
        }?;
        f.write_str("; no calls from this response were run")
    }
}

#[derive(Default)]
pub(super) struct ResponseCalls {
    total: usize,
    repeats: BTreeMap<Fingerprint, usize>,
}

#[derive(Default)]
pub(super) struct LoopGuard {
    turn: Option<TurnId>,
    total: usize,
    fingerprints: BTreeMap<Fingerprint, Repetition>,
    completed: BTreeSet<ItemId>,
}

struct Repetition {
    generation: EventId,
    responses: usize,
    result: Option<[u8; 32]>,
}

impl LoopGuard {
    fn begin_turn(&mut self, turn: TurnId) {
        if self.turn != Some(turn) {
            *self = Self {
                turn: Some(turn),
                ..Self::default()
            };
        }
    }

    /// Hash model-visible results, excluding rotating evidence ids and timing.
    pub(super) fn completed(&mut self, turn: TurnId, state: &SessionState) {
        self.begin_turn(turn);
        let mut items: Vec<_> = state
            .items
            .iter()
            .filter(|(id, item)| {
                item.turn == turn && item.tool_result.is_some() && !self.completed.contains(*id)
            })
            .collect();
        items.sort_by_key(|(_, item)| item.seq);
        for (id, item) in items {
            self.completed.insert(*id);
            let Some(name) = &item.tool_name else {
                continue;
            };
            let key = fingerprint(name, &item.args);
            let Some(repetition) = self.fingerprints.get_mut(&key) else {
                continue;
            };
            let mut hash = Sha256::new();
            hash.update([u8::from(item.ok == Some(true))]);
            hash.update(
                item.tool_result
                    .as_ref()
                    .expect("filtered above")
                    .model_text()
                    .as_bytes(),
            );
            let result: [u8; 32] = hash.finalize().into();
            if repetition.result.is_some_and(|previous| previous != result) {
                repetition.responses = 1;
            }
            repetition.result = Some(result);
        }
    }

    pub(super) fn observe(
        &mut self,
        turn: TurnId,
        generation: EventId,
        response: &mut ResponseCalls,
        name: &str,
        args: &str,
    ) -> Result<Option<String>, Violation> {
        self.begin_turn(turn);
        if contains_control_token(args) {
            return Err(Violation::ControlToken);
        }
        response.total += 1;
        if response.total > RESPONSE_CALLS {
            return Err(Violation::Response);
        }
        self.total += 1;
        if self.total > TURN_CALLS {
            return Err(Violation::Turn);
        }
        let fingerprint = fingerprint(name, args);
        let repeats = response.repeats.entry(fingerprint).or_default();
        *repeats += 1;
        if *repeats > RESPONSE_REPEATS {
            return Err(Violation::ResponseRepeat);
        }
        let repetition = self.fingerprints.entry(fingerprint).or_insert(Repetition {
            generation,
            responses: 1,
            result: None,
        });
        if repetition.generation == generation {
            return Ok(None);
        }
        repetition.generation = generation;
        repetition.responses += 1;
        if repetition.responses >= TURN_REPEAT_STOP {
            return Err(Violation::TurnRepeat);
        }
        if repetition.responses != TURN_REPEAT_WARNING {
            return Ok(None);
        }
        Ok(Some(format!(
            "Tool loop warning: {name} repeated the same normalized arguments across responses in this turn. Use a blocking wait or back off while waiting for progress. Repeating this call without a changed result will stop the turn at {TURN_REPEAT_STOP} responses before execution."
        )))
    }
}

fn fingerprint(name: &str, args: &str) -> Fingerprint {
    let mut hash = Sha256::new();
    hash.update(name.len().to_le_bytes());
    hash.update(name.as_bytes());
    hash.update(normalized_args(args).as_bytes());
    hash.finalize().into()
}

fn normalized_args(args: &str) -> String {
    fn normalize(value: Value) -> Value {
        match value {
            Value::Object(fields) => Value::Object(
                fields
                    .into_iter()
                    .map(|(key, value)| (key, normalize(value)))
                    .collect::<BTreeMap<_, _>>()
                    .into_iter()
                    .collect(),
            ),
            Value::Array(values) => Value::Array(values.into_iter().map(normalize).collect()),
            value => value,
        }
    }
    serde_json::from_str(args)
        .map(|value| normalize(value).to_string())
        .unwrap_or_else(|_| args.to_owned())
}

/// Refuse the evidenced controlNN token grammar after JSON unescaping.
/// Similar punctuation and model narration are not tool argument control tokens.
fn contains_control_token(args: &str) -> bool {
    fn text_contains_token(text: &str) -> bool {
        text.split("<|control").skip(1).any(|suffix| {
            let digits = suffix.bytes().take_while(u8::is_ascii_digit).count();
            digits > 0 && suffix[digits..].starts_with("|>")
        })
    }
    fn value_contains_token(value: &Value) -> bool {
        match value {
            Value::String(text) => text_contains_token(text),
            Value::Array(values) => values.iter().any(value_contains_token),
            Value::Object(fields) => fields
                .iter()
                .any(|(key, value)| text_contains_token(key) || value_contains_token(value)),
            _ => false,
        }
    }
    serde_json::from_str(args).map_or_else(
        |_| text_contains_token(args),
        |value| value_contains_token(&value),
    )
}
