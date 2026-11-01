//! Panels: a small description of an app's controls that LOKI draws inside its own
//! dashboard. Apps that are services rather than sites (game servers, bots) need no
//! pages of their own, and their panels work wherever the dashboard does.
//!
//! LOKI asks for the panel with `GET /_loki/panel` and sends each action with
//! `POST /_loki/panel`; the action is in [`crate::Context::panel_action`]. Answer both
//! with a [`PanelResponse`]. Everything is plain text: LOKI escapes it all.
//!
//! ```ignore
//! fn panel(request: Request) -> Result<Response> {
//!     let mut notice = None;
//!     if let Some(action) = &request.context().panel_action {
//!         if action.action == "reset" { reset()?; notice = Some("Counter reset."); }
//!     }
//!     Panel::new()
//!         .section(Section::new("Counter").stats([("Count", count()?.to_string())]))
//!         .section(Section::new("").buttons([Action::new("reset", "Reset").confirm("Reset the counter?")]))
//!         .respond_with(notice)
//! }
//! ```
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Most sections, items in a section, and rows in a list LOKI will draw.
pub const MAX_SECTIONS: usize = 16;
pub const MAX_ITEMS: usize = 32;
pub const MAX_ROWS: usize = 500;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Panel {
    #[serde(default)]
    pub sections: Vec<Section>,
}
impl Panel {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn section(mut self, section: Section) -> Self {
        self.sections.push(section);
        self
    }
    /// The panel as a response, with a short confirmation of what just happened.
    #[cfg(feature = "guest")]
    pub fn respond_with(self, notice: Option<&str>) -> crate::Result<crate::Response> {
        PanelResponse {
            panel: self,
            notice: notice.unwrap_or_default().into(),
            error: String::new(),
        }
        .respond()
    }
    #[cfg(feature = "guest")]
    pub fn respond(self) -> crate::Result<crate::Response> {
        self.respond_with(None)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Section {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
    #[serde(default)]
    pub items: Vec<Item>,
}
impl Section {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            items: Vec::new(),
        }
    }
    pub fn item(mut self, item: Item) -> Self {
        self.items.push(item);
        self
    }
    pub fn text(self, text: impl Into<String>) -> Self {
        self.item(Item::Text {
            text: text.into(),
            tone: Tone::Normal,
        })
    }
    pub fn muted(self, text: impl Into<String>) -> Self {
        self.item(Item::Text {
            text: text.into(),
            tone: Tone::Muted,
        })
    }
    pub fn warning(self, text: impl Into<String>) -> Self {
        self.item(Item::Text {
            text: text.into(),
            tone: Tone::Warning,
        })
    }
    pub fn stats<L: Into<String>, V: Into<String>>(
        self,
        stats: impl IntoIterator<Item = (L, V)>,
    ) -> Self {
        self.item(Item::Stats {
            items: stats
                .into_iter()
                .map(|(label, value)| Stat {
                    label: label.into(),
                    value: value.into(),
                })
                .collect(),
        })
    }
    /// Rows, each with its own buttons. `empty` shows when there are none.
    pub fn list(self, rows: impl IntoIterator<Item = Row>, empty: impl Into<String>) -> Self {
        self.item(Item::List {
            items: rows.into_iter().collect(),
            empty: empty.into(),
        })
    }
    /// A file picker. The chosen file arrives with the action, as an upload.
    pub fn upload(
        self,
        action: impl Into<String>,
        label: impl Into<String>,
        accept: impl Into<String>,
        button: impl Into<String>,
    ) -> Self {
        self.item(Item::Upload {
            action: action.into(),
            label: label.into(),
            accept: accept.into(),
            button: button.into(),
        })
    }
    pub fn buttons(self, actions: impl IntoIterator<Item = Action>) -> Self {
        self.item(Item::Buttons {
            items: actions.into_iter().collect(),
        })
    }
    /// Fields sent together with one action.
    pub fn form(
        self,
        action: impl Into<String>,
        fields: impl IntoIterator<Item = Field>,
        button: impl Into<String>,
    ) -> Self {
        self.item(Item::Form {
            action: action.into(),
            fields: fields.into_iter().collect(),
            button: button.into(),
        })
    }
    /// A value people copy, such as an address to connect to.
    pub fn code(self, label: impl Into<String>, text: impl Into<String>) -> Self {
        self.item(Item::Code {
            label: label.into(),
            text: text.into(),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Item {
    Text {
        text: String,
        #[serde(default)]
        tone: Tone,
    },
    Stats {
        items: Vec<Stat>,
    },
    List {
        items: Vec<Row>,
        #[serde(default)]
        empty: String,
    },
    Upload {
        action: String,
        #[serde(default)]
        label: String,
        #[serde(default)]
        accept: String,
        #[serde(default)]
        button: String,
    },
    Buttons {
        items: Vec<Action>,
    },
    Form {
        action: String,
        fields: Vec<Field>,
        #[serde(default)]
        button: String,
    },
    Code {
        #[serde(default)]
        label: String,
        text: String,
    },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tone {
    #[default]
    Normal,
    Muted,
    Warning,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Stat {
    pub label: String,
    pub value: String,
}

/// One row of a list.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Row {
    pub title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub badge: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<Action>,
}
impl Row {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            ..Self::default()
        }
    }
    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = detail.into();
        self
    }
    pub fn badge(mut self, badge: impl Into<String>) -> Self {
        self.badge = badge.into();
        self
    }
    pub fn action(mut self, action: Action) -> Self {
        self.actions.push(action);
        self
    }
}

/// A button. Pressing it sends `action` and `value` to the app.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Action {
    pub action: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub value: String,
    #[serde(default)]
    pub style: Style,
    /// When set, LOKI asks this question before sending the action.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub confirm: String,
}
impl Action {
    pub fn new(action: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            action: action.into(),
            label: label.into(),
            ..Self::default()
        }
    }
    pub fn value(mut self, value: impl Into<String>) -> Self {
        self.value = value.into();
        self
    }
    pub fn primary(mut self) -> Self {
        self.style = Style::Primary;
        self
    }
    pub fn danger(mut self) -> Self {
        self.style = Style::Danger;
        self
    }
    pub fn confirm(mut self, question: impl Into<String>) -> Self {
        self.confirm = question.into();
        self
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Style {
    Primary,
    #[default]
    Subtle,
    Danger,
}

/// A form field. Values arrive as text in [`crate::PanelAction::fields`]; a checked
/// box is `"true"`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Field {
    pub key: String,
    pub label: String,
    #[serde(default)]
    pub kind: FieldKind,
    #[serde(default)]
    pub value: Value,
    /// For `select`: (value, label) pairs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<(String, String)>,
}
impl Field {
    pub fn new(key: impl Into<String>, label: impl Into<String>, kind: FieldKind) -> Self {
        Self {
            key: key.into(),
            label: label.into(),
            kind,
            value: Value::Null,
            options: Vec::new(),
        }
    }
    pub fn value(mut self, value: impl Into<Value>) -> Self {
        self.value = value.into();
        self
    }
    pub fn option(mut self, value: impl Into<String>, label: impl Into<String>) -> Self {
        self.options.push((value.into(), label.into()));
        self
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldKind {
    #[default]
    Text,
    Number,
    Bool,
    Select,
}

/// What an app returns for `/_loki/panel`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PanelResponse {
    pub panel: Panel,
    /// A short confirmation, shown briefly.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub notice: String,
    /// Why an action failed, shown above the panel.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
}
impl PanelResponse {
    pub fn error(panel: Panel, error: impl Into<String>) -> Self {
        Self {
            panel,
            notice: String::new(),
            error: error.into(),
        }
    }
    #[cfg(feature = "guest")]
    pub fn respond(&self) -> crate::Result<crate::Response> {
        crate::Response::json(self)
    }
    /// Whether LOKI will draw it: within the size limits, with names it can send back.
    pub fn valid(&self) -> bool {
        let action = |a: &Action| {
            short(&a.action, 64)
                && short(&a.label, 80)
                && short(&a.value, 256)
                && short(&a.confirm, 300)
        };
        short(&self.notice, 300)
            && short(&self.error, 1000)
            && self.panel.sections.len() <= MAX_SECTIONS
            && self.panel.sections.iter().all(|s| {
                short(&s.title, 120)
                    && s.items.len() <= MAX_ITEMS
                    && s.items.iter().all(|item| match item {
                        Item::Text { text, .. } => short(text, 4000),
                        Item::Stats { items } => {
                            items.len() <= 16
                                && items
                                    .iter()
                                    .all(|s| short(&s.label, 80) && short(&s.value, 120))
                        }
                        Item::List { items, empty } => {
                            short(empty, 300)
                                && items.len() <= MAX_ROWS
                                && items.iter().all(|r| {
                                    short(&r.title, 200)
                                        && short(&r.detail, 500)
                                        && short(&r.badge, 40)
                                        && r.actions.len() <= 4
                                        && r.actions.iter().all(action)
                                })
                        }
                        Item::Upload {
                            action: a,
                            label,
                            accept,
                            button,
                        } => {
                            short(a, 64)
                                && short(label, 120)
                                && short(accept, 200)
                                && short(button, 80)
                        }
                        Item::Buttons { items } => items.len() <= 8 && items.iter().all(action),
                        Item::Form {
                            action: a,
                            fields,
                            button,
                        } => {
                            short(a, 64)
                                && short(button, 80)
                                && fields.len() <= 16
                                && fields.iter().all(|f| {
                                    short(&f.key, 64)
                                        && short(&f.label, 120)
                                        && f.value.to_string().len() <= 4000
                                        && f.options.len() <= 100
                                        && f.options
                                            .iter()
                                            .all(|(v, l)| short(v, 256) && short(l, 120))
                                })
                        }
                        Item::Code { label, text } => short(label, 120) && short(text, 2000),
                    })
            })
    }
}
fn short(text: &str, max: usize) -> bool {
    text.chars().count() <= max
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn panels_serialize_to_the_documented_shape() {
        let panel = Panel::new().section(
            Section::new("Dreams")
                .stats([("Players", "2")])
                .list(
                    [Row::new("Poolrooms")
                        .badge("Playing")
                        .action(Action::new("remove", "Remove").value("a").danger())],
                    "None yet.",
                )
                .upload("upload", "Add a dream", ".dream", "Upload"),
        );
        let value = serde_json::to_value(PanelResponse {
            panel,
            notice: "Saved.".into(),
            error: String::new(),
        })
        .unwrap();
        assert_eq!(
            value,
            json!({"panel":{"sections":[{"title":"Dreams","items":[
                {"type":"stats","items":[{"label":"Players","value":"2"}]},
                {"type":"list","items":[{"title":"Poolrooms","badge":"Playing","actions":[{"action":"remove","label":"Remove","value":"a","style":"danger"}]}],"empty":"None yet."},
                {"type":"upload","action":"upload","label":"Add a dream","accept":".dream","button":"Upload"}
            ]}]},"notice":"Saved."})
        );
        let back: PanelResponse = serde_json::from_value(value).unwrap();
        assert!(back.valid());
    }
    #[test]
    fn oversized_or_unknown_items_are_refused() {
        let long = Panel::new().section(Section::new("x".repeat(121)));
        assert!(
            !PanelResponse {
                panel: long,
                ..Default::default()
            }
            .valid()
        );
        let rows = (0..=MAX_ROWS).map(|i| Row::new(i.to_string()));
        let many = Panel::new().section(Section::new("").list(rows, ""));
        assert!(
            !PanelResponse {
                panel: many,
                ..Default::default()
            }
            .valid()
        );
        assert!(
            serde_json::from_value::<PanelResponse>(
                json!({"panel":{"sections":[{"items":[{"type":"script","src":"x"}]}]}})
            )
            .is_err()
        );
    }
}
