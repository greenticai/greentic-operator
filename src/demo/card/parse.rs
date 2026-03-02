use serde_json::Value as JsonValue;

/// A simplified representation of an Adaptive Card that exposes the inputs/actions we care about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CardView {
    pub version: Option<String>,
    pub title: Option<String>,
    pub summary_text: Option<String>,
    pub body_texts: Vec<String>,
    pub inputs: Vec<CardInput>,
    pub actions: Vec<CardAction>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CardInput {
    pub id: String,
    pub label: Option<String>,
    pub placeholder: Option<String>,
    pub input_type: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CardAction {
    pub id: String,
    pub title: Option<String>,
    pub action_type: Option<String>,
}

const CARD_TYPE: &str = "AdaptiveCard";

pub fn detect_adaptive_card_view(value: &JsonValue) -> Option<CardView> {
    let card = extract_card_object(value)?;
    let card_type = card
        .get("type")
        .and_then(JsonValue::as_str)
        .unwrap_or_default();
    if !card_type.eq_ignore_ascii_case(CARD_TYPE) {
        return None;
    }
    let version = card
        .get("version")
        .and_then(JsonValue::as_str)
        .map(|v| v.to_string());
    let title = card
        .get("title")
        .and_then(JsonValue::as_str)
        .map(|value| value.to_string());
    let summary_text = card
        .get("summary")
        .and_then(JsonValue::as_str)
        .map(|value| value.to_string())
        .or_else(|| title.clone());

    let body_texts = card
        .get("body")
        .and_then(JsonValue::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let item_obj = item.as_object()?;
                    let item_type = item_obj
                        .get("type")
                        .and_then(JsonValue::as_str)?
                        .to_ascii_lowercase();
                    if item_type == "textblock" {
                        item_obj
                            .get("text")
                            .and_then(JsonValue::as_str)
                            .map(|text| text.to_string())
                    } else {
                        None
                    }
                })
                .collect()
        })
        .unwrap_or_default();

    let inputs = card
        .get("body")
        .and_then(JsonValue::as_array)
        .map(|items| collect_inputs(items))
        .unwrap_or_default();

    let actions = card
        .get("actions")
        .and_then(JsonValue::as_array)
        .map(|items| items.iter().filter_map(parse_action).collect())
        .unwrap_or_default();

    Some(CardView {
        version,
        title,
        summary_text,
        body_texts,
        inputs,
        actions,
    })
}

fn extract_card_object(value: &JsonValue) -> Option<&JsonValue> {
    if let Some(card) = value.get("card") {
        return card.as_object().map(|_| card);
    }
    if let Some(payload) = value.get("payload") {
        if let Some(card) = payload.get("card") {
            return card.as_object().map(|_| card);
        }
        if let Some(outputs) = payload.get("outputs")
            && let Some(card) = outputs.get("card")
        {
            return card.as_object().map(|_| card);
        }
    }
    if let Some(outputs) = value.get("outputs")
        && let Some(card) = outputs.get("card")
    {
        return card.as_object().map(|_| card);
    }
    if value
        .get("type")
        .and_then(JsonValue::as_str)
        .map(|kind| kind.eq_ignore_ascii_case(CARD_TYPE))
        .unwrap_or(false)
    {
        return Some(value);
    }
    None
}

/// Recursively collect Input.* elements from body items, including those
/// nested inside Container, ColumnSet, and Column elements.
fn collect_inputs(items: &[JsonValue]) -> Vec<CardInput> {
    let mut inputs = Vec::new();
    for item in items {
        if let Some(input) = parse_input(item) {
            inputs.push(input);
        }
        // Recurse into Container.items
        if let Some(children) = item.get("items").and_then(JsonValue::as_array) {
            inputs.extend(collect_inputs(children));
        }
        // Recurse into ColumnSet.columns[].items
        if let Some(columns) = item.get("columns").and_then(JsonValue::as_array) {
            for col in columns {
                if let Some(col_items) = col.get("items").and_then(JsonValue::as_array) {
                    inputs.extend(collect_inputs(col_items));
                }
            }
        }
    }
    inputs
}

fn parse_input(value: &JsonValue) -> Option<CardInput> {
    let item = value.as_object()?;
    let input_type = item
        .get("type")
        .and_then(JsonValue::as_str)
        .map(|value| value.to_string());
    let lower_type = input_type
        .as_ref()
        .map(|value| value.to_ascii_lowercase())
        .unwrap_or_default();
    if !lower_type.starts_with("input") {
        return None;
    }
    let id = item
        .get("id")
        .and_then(JsonValue::as_str)
        .or_else(|| item.get("dataId").and_then(JsonValue::as_str))
        .map(|value| value.to_string())
        .unwrap_or_else(|| "<unnamed-input>".to_string());
    let label = item
        .get("title")
        .and_then(JsonValue::as_str)
        .map(|value| value.to_string())
        .or_else(|| {
            item.get("label")
                .and_then(JsonValue::as_str)
                .map(|value| value.to_string())
        });
    let placeholder = item
        .get("placeholder")
        .and_then(JsonValue::as_str)
        .map(|value| value.to_string());

    Some(CardInput {
        id,
        label,
        placeholder,
        input_type,
    })
}

fn parse_action(value: &JsonValue) -> Option<CardAction> {
    let obj = value.as_object()?;
    let id = obj
        .get("id")
        .and_then(JsonValue::as_str)
        .or_else(|| obj.get("actionId").and_then(JsonValue::as_str))
        .map(|value| value.to_string())
        .unwrap_or_else(|| "<unnamed-action>".to_string());
    let action_type = obj
        .get("type")
        .and_then(JsonValue::as_str)
        .map(|value| value.to_string());
    let title = obj
        .get("title")
        .and_then(JsonValue::as_str)
        .map(|value| value.to_string());
    Some(CardAction {
        id,
        title,
        action_type,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn detects_card_with_inputs_and_actions() {
        let adaptive_card = json!({
            "type": "AdaptiveCard",
            "version": "1.4",
            "summary": "Please confirm",
            "title": "Adaptive widget",
            "body": [
                {"type": "TextBlock", "text": "Hello"},
                {"type": "Input.Text", "id": "comment", "placeholder": "Add a comment"},
                {"type": "Input.Toggle", "id": "opt_in", "title": "Opt in"}
            ],
            "actions": [
                {"type": "Action.Submit", "title": "Submit", "id": "submit"},
                {"type": "Action.ShowCard", "title": "More", "actionId": "more-info"}
            ]
        });
        let payload = json!({ "card": adaptive_card });
        let view = detect_adaptive_card_view(&payload).expect("card should be detected");
        assert_eq!(view.version.as_deref(), Some("1.4"));
        assert_eq!(view.summary_text.as_deref(), Some("Please confirm"));
        assert_eq!(view.inputs.len(), 2);
        assert_eq!(view.actions.len(), 2);
        assert!(view.body_texts.contains(&"Hello".to_string()));
    }

    #[test]
    fn ignores_non_adaptive_card() {
        let payload = json!({ "card": { "type": "SomeOtherCard" } });
        assert!(detect_adaptive_card_view(&payload).is_none());
    }
}
