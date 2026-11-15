use std::error::Error;
use std::fmt;

use opentelemetry::logs::AnyValue;
use tracing::field::{Field, Visit};

const MESSAGE_FIELD: &str = "message";
const PLACEHOLDER_START: char = '{';
const PLACEHOLDER_END: char = '}';

#[derive(Clone, Debug, PartialEq)]
pub(super) enum FieldValue {
    Text(String),
    Integer(i64),
    Unsigned(u64),
    Float(f64),
    Boolean(bool),
}

impl fmt::Display for FieldValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Text(text) => formatter.write_str(text),
            Self::Integer(integer) => write!(formatter, "{integer}"),
            Self::Unsigned(unsigned) => write!(formatter, "{unsigned}"),
            Self::Float(float) => write!(formatter, "{float}"),
            Self::Boolean(boolean) => write!(formatter, "{boolean}"),
        }
    }
}

impl From<FieldValue> for AnyValue {
    fn from(value: FieldValue) -> Self {
        match value {
            FieldValue::Text(text) => AnyValue::from(text),
            FieldValue::Integer(integer) => AnyValue::Int(integer),
            FieldValue::Unsigned(unsigned) => i64::try_from(unsigned)
                .map_or_else(|_| AnyValue::from(unsigned.to_string()), AnyValue::Int),
            FieldValue::Float(float) => AnyValue::Double(float),
            FieldValue::Boolean(boolean) => AnyValue::Boolean(boolean),
        }
    }
}

#[derive(Debug, Default)]
pub(super) struct EventFields {
    pub(super) template: Option<String>,
    pub(super) values: Vec<(&'static str, FieldValue)>,
}

impl EventFields {
    pub(super) fn rendered_message(&self) -> String {
        self.template
            .as_deref()
            .map(|template| render(template, &self.values))
            .unwrap_or_default()
    }

    fn record(&mut self, field: &Field, value: FieldValue) {
        if field.name() == MESSAGE_FIELD {
            self.template = Some(value.to_string());
        } else {
            self.values.push((field.name(), value));
        }
    }
}

impl Visit for EventFields {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.record(field, FieldValue::Text(value.to_owned()));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.record(field, FieldValue::Integer(value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.record(field, FieldValue::Unsigned(value));
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.record(field, FieldValue::Float(value));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.record(field, FieldValue::Boolean(value));
    }

    fn record_error(&mut self, field: &Field, value: &(dyn Error + 'static)) {
        self.record(field, FieldValue::Text(value.to_string()));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.record(field, FieldValue::Text(format!("{value:?}")));
    }
}

fn render(template: &str, values: &[(&'static str, FieldValue)]) -> String {
    let mut rendered = String::with_capacity(template.len());
    let mut remaining = template;
    while let Some((literal, after_start)) = remaining.split_once(PLACEHOLDER_START) {
        rendered.push_str(literal);
        let Some((name, after_end)) = after_start.split_once(PLACEHOLDER_END) else {
            rendered.push(PLACEHOLDER_START);
            remaining = after_start;
            continue;
        };
        push_placeholder(&mut rendered, name, values);
        remaining = after_end;
    }
    rendered.push_str(remaining);
    rendered
}

fn push_placeholder(rendered: &mut String, name: &str, values: &[(&'static str, FieldValue)]) {
    if let Some((_, value)) = values.iter().find(|(field_name, _)| *field_name == name) {
        rendered.push_str(&value.to_string());
    } else {
        rendered.push(PLACEHOLDER_START);
        rendered.push_str(name);
        rendered.push(PLACEHOLDER_END);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn heartbeat_values() -> Vec<(&'static str, FieldValue)> {
        vec![
            ("edi.switch_state", FieldValue::Text("running".to_owned())),
            ("edi.unfinished_count", FieldValue::Unsigned(3)),
        ]
    }

    #[test]
    fn placeholders_take_the_values_of_their_fields() {
        let rendered = render(
            "EDI transfer is {edi.switch_state}, {edi.unfinished_count} in flight",
            &heartbeat_values(),
        );

        assert_eq!(rendered, "EDI transfer is running, 3 in flight");
    }

    #[test]
    fn a_placeholder_without_a_field_stays_as_written() {
        let rendered = render("{edi.switch_state} with {missing}", &heartbeat_values());

        assert_eq!(rendered, "running with {missing}");
    }

    #[test]
    fn an_unclosed_brace_stays_as_written() {
        let rendered = render("set {edi.switch_state} or {unclosed", &heartbeat_values());

        assert_eq!(rendered, "set running or {unclosed");
    }

    #[test]
    fn a_template_without_placeholders_is_unchanged() {
        assert_eq!(
            render("sync finished", &heartbeat_values()),
            "sync finished"
        );
    }

    #[test]
    fn unsigned_values_beyond_i64_become_text() {
        assert_eq!(
            AnyValue::from(FieldValue::Unsigned(u64::MAX)),
            AnyValue::from(u64::MAX.to_string())
        );
        assert_eq!(AnyValue::from(FieldValue::Unsigned(7)), AnyValue::Int(7));
    }
}
