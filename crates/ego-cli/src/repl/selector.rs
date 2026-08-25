//! Pure state engine shared by model pickers and questions.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectorKind { Single, Multiple, FreeText }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectorKey { Up, Down, Number(usize), Enter, Escape, Space }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectorOutcome { consumed: bool, selected: Option<String> }

impl SelectorOutcome {
    pub fn consumed(&self) -> bool { self.consumed }
    pub fn selected(&self) -> Option<&str> { self.selected.as_deref() }
}

#[derive(Debug, Clone)]
pub struct Selector {
    kind: SelectorKind,
    choices: Vec<String>,
    cursor: usize,
    selected: Vec<usize>,
    value: String,
    composer: (String, usize),
    protected_composer: bool,
}

impl Selector {
    pub fn new<I, S>(kind: SelectorKind, choices: I) -> Self
    where I: IntoIterator<Item = S>, S: Into<String> {
        assert!(kind != SelectorKind::FreeText, "use Selector::free_text for free text");
        Self { kind, choices: choices.into_iter().map(Into::into).collect(), cursor: 0,
            selected: Vec::new(), value: String::new(), composer: (String::new(), 0), protected_composer: true }
    }
    pub fn free_text() -> Self {
        Self { kind: SelectorKind::FreeText, choices: Vec::new(), cursor: 0, selected: Vec::new(),
            value: String::new(), composer: (String::new(), 0), protected_composer: true }
    }
    pub fn insert(&mut self, text: &str) { if self.kind == SelectorKind::FreeText { self.value.push_str(text); } }
    pub fn value(&self) -> &str { &self.value }
    pub fn preserve_composer(&mut self, buffer: impl Into<String>, cursor: usize) { self.composer = (buffer.into(), cursor); }
    pub fn composer(&self) -> (&str, usize) { (&self.composer.0, self.composer.1) }
    pub fn did_not_submit_composer(&self) -> bool { self.protected_composer }
    pub fn key(&mut self, key: SelectorKey) -> SelectorOutcome {
        let mut selected = None;
        match key {
            SelectorKey::Up if !self.choices.is_empty() => self.cursor = self.cursor.checked_sub(1).unwrap_or(self.choices.len() - 1),
            SelectorKey::Down if !self.choices.is_empty() => self.cursor = (self.cursor + 1) % self.choices.len(),
            SelectorKey::Number(number) if self.kind != SelectorKind::FreeText && number > 0 && number <= self.choices.len() => {
                self.cursor = number - 1;
                self.choose_cursor();
                selected = self.choices.get(self.cursor).cloned();
            }
            SelectorKey::Space if self.kind == SelectorKind::Multiple && !self.choices.is_empty() => {
                self.choose_cursor();
                selected = self.choices.get(self.cursor).cloned();
            }
            SelectorKey::Enter => {
                if self.kind == SelectorKind::Single && !self.choices.is_empty() { self.choose_cursor(); }
                selected = self.current_selection();
            }
            SelectorKey::Escape => {}
            _ => return SelectorOutcome { consumed: false, selected: None },
        }
        SelectorOutcome { consumed: true, selected }
    }
    fn choose_cursor(&mut self) {
        match self.kind {
            SelectorKind::Single => { self.selected.clear(); self.selected.push(self.cursor); }
            SelectorKind::Multiple => {
                if let Some(index) = self.selected.iter().position(|selected| *selected == self.cursor) { self.selected.remove(index); }
                else { self.selected.push(self.cursor); }
            }
            SelectorKind::FreeText => {}
        }
    }
    fn current_selection(&self) -> Option<String> {
        match self.kind {
            SelectorKind::FreeText => Some(self.value.clone()),
            SelectorKind::Single => self.selected.first().and_then(|i| self.choices.get(*i)).cloned(),
            SelectorKind::Multiple => {
                let values: Vec<_> = self.selected.iter().filter_map(|i| self.choices.get(*i)).cloned().collect();
                (!values.is_empty()).then(|| values.join(","))
            }
        }
    }
}
