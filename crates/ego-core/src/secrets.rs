//! Exact-value disclosure masking. This policy never changes a child environment.

/// Known nonempty secret values, longest first; no pattern inference.
#[derive(Clone, Default)]
pub struct SecretValues(Vec<Vec<u8>>);

impl SecretValues {
    pub fn new(values: impl IntoIterator<Item = Vec<u8>>) -> Self {
        let mut values: Vec<_> = values
            .into_iter()
            .filter(|value| !value.is_empty())
            .collect();
        values.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        values.dedup();
        Self(values)
    }

    pub fn mask(&self, text: &str) -> String {
        // Replacing complete UTF-8 values preserves UTF-8; other values cannot match.
        String::from_utf8(self.stream().push(text.as_bytes(), true))
            .expect("exact masking preserves UTF-8")
    }

    pub fn stream(&self) -> SecretStream<'_> {
        SecretStream {
            values: self,
            pending: Vec::new(),
        }
    }

    pub fn mask_json(&self, value: &mut serde_json::Value) {
        match value {
            serde_json::Value::String(text) => *text = self.mask(text),
            serde_json::Value::Array(items) => {
                items.iter_mut().for_each(|item| self.mask_json(item))
            }
            serde_json::Value::Object(items) => {
                items.values_mut().for_each(|item| self.mask_json(item))
            }
            _ => {}
        }
    }
}

/// Retains an incomplete potential match across pipe reads, before persistence.
pub struct SecretStream<'a> {
    values: &'a SecretValues,
    pending: Vec<u8>,
}

impl SecretStream<'_> {
    pub fn push(&mut self, bytes: &[u8], eof: bool) -> Vec<u8> {
        self.pending.extend_from_slice(bytes);
        let mut out = Vec::new();
        let mut offset = 0;
        while offset < self.pending.len() {
            let tail = &self.pending[offset..];
            if !eof
                && self
                    .values
                    .0
                    .iter()
                    .any(|value| value.len() > tail.len() && value.starts_with(tail))
            {
                break;
            }
            if let Some(value) = self.values.0.iter().find(|value| tail.starts_with(value)) {
                out.extend_from_slice(b"[REDACTED]");
                offset += value.len();
            } else {
                out.push(tail[0]);
                offset += 1;
            }
        }
        self.pending.drain(..offset);
        out
    }
}
