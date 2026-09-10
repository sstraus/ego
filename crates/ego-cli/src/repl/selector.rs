//! Pure state engine shared by model pickers and questions.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectorKind {
    Single,
    Multiple,
    FreeText,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectorKey {
    Up,
    Down,
    Number(usize),
    Enter,
    Escape,
    Space,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectorOutcome {
    consumed: bool,
    selected: Option<String>,
}

impl SelectorOutcome {
    pub fn consumed(&self) -> bool {
        self.consumed
    }
    pub fn selected(&self) -> Option<&str> {
        self.selected.as_deref()
    }
}

/// The canonical typed answer produced on Enter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectorValue {
    Single(String),
    Multiple(Vec<String>),
    FreeText(String),
}

/// A stable answer identity and its independently editable display label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectorOption {
    id: String,
    label: String,
}

impl SelectorOption {
    pub fn new(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
        }
    }
    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn label(&self) -> &str {
        &self.label
    }
}

#[derive(Debug, Clone)]
pub struct Selector {
    kind: SelectorKind,
    choices: Vec<SelectorOption>,
    cursor: usize,
    cursor_active: bool,
    selected: Vec<usize>,
    value: String,
    composer: (String, usize),
    protected_composer: bool,
    result: Option<SelectorValue>,
}

impl Selector {
    pub fn new<I, S>(kind: SelectorKind, choices: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        assert!(
            kind != SelectorKind::FreeText,
            "use Selector::free_text for free text"
        );
        Self::with_options(
            kind,
            choices.into_iter().map(|choice| {
                let choice: String = choice.into();
                SelectorOption::new(choice.clone(), choice)
            }),
        )
    }
    pub fn with_options<I>(kind: SelectorKind, choices: I) -> Self
    where
        I: IntoIterator<Item = SelectorOption>,
    {
        assert!(
            kind != SelectorKind::FreeText,
            "use Selector::free_text for free text"
        );
        Self {
            kind,
            choices: choices.into_iter().collect(),
            cursor: 0,
            cursor_active: true,
            selected: Vec::new(),
            value: String::new(),
            composer: (String::new(), 0),
            protected_composer: true,
            result: None,
        }
    }
    /// Build a nonempty selector with no authoritative highlighted option.
    /// Navigation or a number activates a cursor; bare Enter selects nothing.
    pub fn with_options_unselected<I>(kind: SelectorKind, choices: I) -> Self
    where
        I: IntoIterator<Item = SelectorOption>,
    {
        let mut selector = Self::with_options(kind, choices);
        selector.cursor_active = false;
        selector
    }
    pub fn free_text() -> Self {
        Self {
            kind: SelectorKind::FreeText,
            choices: Vec::new(),
            cursor: 0,
            cursor_active: true,
            selected: Vec::new(),
            value: String::new(),
            composer: (String::new(), 0),
            protected_composer: true,
            result: None,
        }
    }
    pub fn insert(&mut self, text: &str) {
        if self.kind == SelectorKind::FreeText {
            self.value.push_str(text);
        }
    }
    pub fn value(&self) -> &str {
        &self.value
    }
    pub fn preserve_composer(&mut self, buffer: impl Into<String>, cursor: usize) {
        self.composer = (buffer.into(), cursor);
    }
    pub fn composer(&self) -> (&str, usize) {
        (&self.composer.0, self.composer.1)
    }
    pub fn did_not_submit_composer(&self) -> bool {
        self.protected_composer
    }
    /// Stable option identities, always projected in request order.
    pub fn selected_ids(&self) -> Vec<&str> {
        self.choices
            .iter()
            .enumerate()
            .filter(|(index, _)| self.selected.contains(index))
            .map(|(_, choice)| choice.id())
            .collect()
    }
    pub fn result(&self) -> Option<&SelectorValue> {
        self.result.as_ref()
    }
    /// Stable identity of the authoritative highlighted option.
    pub fn current_id(&self) -> Option<&str> {
        if !self.cursor_active {
            return None;
        }
        self.choices.get(self.cursor).map(SelectorOption::id)
    }
    pub fn key(&mut self, key: SelectorKey) -> SelectorOutcome {
        let mut selected = None;
        match key {
            SelectorKey::Up if !self.choices.is_empty() => {
                self.cursor = if self.cursor_active {
                    self.cursor.checked_sub(1).unwrap_or(self.choices.len() - 1)
                } else {
                    self.choices.len() - 1
                };
                self.cursor_active = true;
            }
            SelectorKey::Down if !self.choices.is_empty() => {
                self.cursor = if self.cursor_active {
                    (self.cursor + 1) % self.choices.len()
                } else {
                    0
                };
                self.cursor_active = true;
            }
            SelectorKey::Number(number)
                if self.kind != SelectorKind::FreeText
                    && number > 0
                    && number <= self.choices.len() =>
            {
                self.cursor = number - 1;
                self.cursor_active = true;
                self.choose_cursor();
                selected = self
                    .choices
                    .get(self.cursor)
                    .map(|choice| choice.label.clone());
            }
            SelectorKey::Space
                if self.kind == SelectorKind::Multiple && !self.choices.is_empty() =>
            {
                self.choose_cursor();
                if self.selected.contains(&self.cursor) {
                    selected = self
                        .choices
                        .get(self.cursor)
                        .map(|choice| choice.label.clone());
                }
            }
            SelectorKey::Enter => {
                if self.kind == SelectorKind::Single
                    && self.cursor_active
                    && !self.choices.is_empty()
                {
                    self.choose_cursor();
                }
                selected = self.current_selection();
                self.result = match self.kind {
                    SelectorKind::Single => self
                        .selected_ids()
                        .into_iter()
                        .next()
                        .map(|id| SelectorValue::Single(id.to_owned())),
                    SelectorKind::Multiple => Some(SelectorValue::Multiple(
                        self.selected_ids().into_iter().map(str::to_owned).collect(),
                    )),
                    SelectorKind::FreeText => Some(SelectorValue::FreeText(self.value.clone())),
                };
            }
            SelectorKey::Escape => {
                self.result = None;
            }
            _ => {
                return SelectorOutcome {
                    consumed: false,
                    selected: None,
                };
            }
        }
        SelectorOutcome {
            consumed: true,
            selected,
        }
    }
    fn choose_cursor(&mut self) {
        match self.kind {
            SelectorKind::Single => {
                self.selected.clear();
                self.selected.push(self.cursor);
            }
            SelectorKind::Multiple => {
                if let Some(index) = self
                    .selected
                    .iter()
                    .position(|selected| *selected == self.cursor)
                {
                    self.selected.remove(index);
                } else {
                    self.selected.push(self.cursor);
                }
            }
            SelectorKind::FreeText => {}
        }
    }
    fn current_selection(&self) -> Option<String> {
        match self.kind {
            SelectorKind::FreeText => Some(self.value.clone()),
            SelectorKind::Single => self
                .selected
                .first()
                .and_then(|i| self.choices.get(*i))
                .map(|choice| choice.label.clone()),
            SelectorKind::Multiple => self
                .choices
                .iter()
                .enumerate()
                .find(|(index, _)| self.selected.contains(index))
                .map(|(_, choice)| choice.label.clone()),
        }
    }
}
