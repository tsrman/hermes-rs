//! The `clarify` tool: structured questions put to the person at the terminal.
//!
//! Follows `tools/clarify_tool.py`: validation of the questions, the "(Recommended)" label on
//! the first choice, and the shape of the result. The terminal itself is the caller's: the
//! questions travel as an approval-style request and the answers come back as a reply.

use serde_json::{json, Map, Value};

pub(crate) const CLARIFY: &str = "clarify";
const MAX_CHOICES: usize = 4;
const MAX_QUESTIONS: usize = 5;
const MAX_CHOICE_CHARS: usize = 8000;
const RECOMMENDED_LABEL: &str = "(Recommended)";
const SHAPE: &str =
    "Pass questions=[{question, choices?, multi_select?}]; a single question is a one-entry array.";
pub(crate) const UNAVAILABLE: &str = "Clarify tool is not available in this execution context.";

pub(crate) fn schema() -> Value {
    json!({
        "type": "function",
        "function": serde_json::from_str::<Value>(include_str!("schemas/clarify.json")).unwrap_or_default()
    })
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Question {
    pub qid: String,
    pub text: String,
    /// The choices as shown, the first one labelled as recommended.
    pub choices: Option<Vec<String>>,
    /// The bare choices, which the result echoes.
    pub offered: Option<Vec<String>>,
    pub multi_select: bool,
}

/// Suffix the first choice with the recommended label, unless it already has it or stands alone.
fn mark_recommended(choices: &[String]) -> Vec<String> {
    let first = choices.first().map_or("", |first| first.trim());
    if choices.len() < 2 || first != strip_recommended(first) {
        return choices.to_vec();
    }
    let mut marked = vec![format!("{first} {RECOMMENDED_LABEL}")];
    marked.extend_from_slice(&choices[1..]);
    marked
}

/// Remove the label so that presentation never leaks into the answer.
pub(crate) fn strip_recommended(text: &str) -> String {
    let trimmed = text.trim();
    let lower = trimmed.to_lowercase();
    if lower.ends_with(&RECOMMENDED_LABEL.to_lowercase()) {
        let cut = trimmed.chars().count() - RECOMMENDED_LABEL.chars().count();
        return trimmed
            .chars()
            .take(cut)
            .collect::<String>()
            .trim()
            .to_owned();
    }
    trimmed.to_owned()
}

pub(crate) fn normalize(args: &Value) -> Result<Vec<Question>, String> {
    let list = match &args["questions"] {
        Value::Array(list) if !list.is_empty() => list,
        _ => return Err(format!("questions must be a non-empty array. {SHAPE}")),
    };
    if list.len() > MAX_QUESTIONS {
        return Err(format!("questions supports at most {MAX_QUESTIONS} items."));
    }
    let mut questions = Vec::new();
    for (index, item) in list.iter().enumerate() {
        let Some(object) = item.as_object() else {
            return Err(format!("questions[{index}] must be an object. {SHAPE}"));
        };
        let text = object
            .get("question")
            .map(|value| {
                value
                    .as_str()
                    .map_or_else(|| value.to_string(), str::to_owned)
            })
            .unwrap_or_default();
        let text = text.trim().to_owned();
        if text.is_empty() {
            return Err(format!(
                "questions[{index}].question must be non-empty text."
            ));
        }
        let mut choices = None;
        if let Some(raw) = object.get("choices").filter(|value| !value.is_null()) {
            let Some(items) = raw
                .as_array()
                .filter(|items| items.iter().all(Value::is_string))
            else {
                return Err(format!(
                    "questions[{index}].choices must be a list of strings."
                ));
            };
            let strings: Vec<&str> = items.iter().filter_map(Value::as_str).collect();
            for (position, choice) in strings.iter().enumerate() {
                let length = choice.trim().chars().count();
                if length > MAX_CHOICE_CHARS {
                    return Err(format!(
                        "questions[{index}].choices[{position}] is {length} characters; the limit is {MAX_CHOICE_CHARS}. Move detail into the question text and resend."
                    ));
                }
            }
            let cleaned: Vec<String> = strings
                .iter()
                .map(|choice| choice.trim())
                .filter(|choice| !choice.is_empty())
                .take(MAX_CHOICES)
                .map(str::to_owned)
                .collect();
            if !strings.is_empty() && cleaned.is_empty() {
                return Err(format!(
                    "questions[{index}].choices has {} entries but all are blank. Send real labels, or omit choices to ask open-ended.",
                    strings.len()
                ));
            }
            choices = (!cleaned.is_empty()).then_some(cleaned);
        }
        let multi_select = choices.is_some()
            && match object.get("multi_select") {
                Some(Value::Bool(flag)) => *flag,
                Some(Value::Null) | None => false,
                Some(Value::String(text)) => !text.is_empty(),
                Some(Value::Number(number)) => number.as_f64() != Some(0.0),
                Some(_) => true,
            };
        questions.push(Question {
            qid: format!("q{index}"),
            text,
            choices: choices.as_deref().map(mark_recommended),
            offered: choices,
            multi_select,
        });
    }
    Ok(questions)
}

/// The questions in the wire form the terminal asks.
pub(crate) fn request_payload(questions: &[Question]) -> String {
    let items: Vec<Value> = questions
        .iter()
        .map(|q| json!({"qid": q.qid, "question": q.text, "choices": q.choices, "multi_select": q.multi_select}))
        .collect();
    Value::Array(items).to_string()
}

fn clean_answer(raw: &Value, multi: bool) -> Option<Value> {
    if !multi {
        let text = strip_recommended(
            raw.as_str()
                .map_or_else(|| raw.to_string(), str::to_owned)
                .as_str(),
        );
        return (!text.is_empty()).then_some(Value::String(text));
    }
    let items: Vec<Value> = match raw {
        Value::Array(list) => list.clone(),
        Value::String(text) => match serde_json::from_str::<Value>(text) {
            Ok(Value::Array(list)) => list,
            _ => vec![Value::String(text.clone())],
        },
        other => vec![other.clone()],
    };
    let cleaned: Vec<Value> = items
        .iter()
        .map(|item| {
            item.as_str()
                .map_or_else(|| item.to_string(), str::to_owned)
        })
        .filter(|item| !item.trim().is_empty())
        .map(|item| Value::String(strip_recommended(&item)))
        .collect();
    (!cleaned.is_empty()).then_some(Value::Array(cleaned))
}

/// The result for a reply `{"answers": {qid: answer | null}, "outcome": ..., "notice"?}`.
pub(crate) fn result(questions: &[Question], reply: &Value) -> Value {
    let answers = reply["answers"].as_object().cloned().unwrap_or_default();
    let responses: Vec<Value> = questions
        .iter()
        .map(|question| {
            let raw = answers.get(&question.qid);
            let cleaned = raw
                .filter(|raw| !raw.is_null() && raw.as_str() != Some(""))
                .and_then(|raw| clean_answer(raw, question.multi_select));
            let status = if cleaned.is_some() {
                "answered"
            } else if raw.is_some() {
                "skipped"
            } else {
                "unanswered"
            };
            json!({
                "question": question.text,
                "choices_offered": question.offered,
                "status": status,
                "user_response": cleaned,
            })
        })
        .collect();
    let mut result = Map::new();
    result.insert("responses".into(), Value::Array(responses));
    result.insert("outcome".into(), reply["outcome"].clone());
    if let Some(notice) = reply["notice"].as_str().filter(|notice| !notice.is_empty()) {
        result.insert("notice".into(), json!(notice));
    }
    Value::Object(result)
}

/// A reply for a wait that ended without answers.
pub(crate) fn empty_reply(outcome: &str, notice: &str) -> Value {
    json!({"answers": {}, "outcome": outcome, "notice": notice})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_questions_and_labels_the_first_choice() {
        let ok = normalize(&json!({"questions": [
            {"question": " Which? ", "choices": ["A", " B ", "", "C", "D", "E"], "multi_select": true},
            {"question": "Name?"},
            {"question": "One", "choices": ["only"], "multi_select": true}
        ]}))
        .unwrap();
        assert_eq!(ok[0].text, "Which?");
        assert_eq!(ok[0].offered.as_deref().unwrap(), ["A", "B", "C", "D"]);
        assert_eq!(ok[0].choices.as_deref().unwrap()[0], "A (Recommended)");
        assert_eq!(ok[0].qid, "q0");
        assert!(ok[0].multi_select);
        assert!(!ok[1].multi_select && ok[1].choices.is_none());
        assert_eq!(ok[2].choices.as_deref().unwrap(), ["only"]);

        for (args, needle) in [
            (json!({}), "non-empty array"),
            (json!({"questions": []}), "non-empty array"),
            (json!({"questions": [1]}), "questions[0] must be an object"),
            (json!({"questions": [{"question": " "}]}), "non-empty text"),
            (
                json!({"questions": [{"question": "q", "choices": "x"}]}),
                "list of strings",
            ),
            (
                json!({"questions": [{"question": "q", "choices": [" ", ""]}]}),
                "all are blank",
            ),
            (
                json!({"questions": (0..6).map(|_| json!({"question": "q"})).collect::<Vec<_>>()}),
                "at most 5",
            ),
        ] {
            let error = normalize(&args).unwrap_err();
            assert!(error.contains(needle), "{error}");
        }
    }

    #[test]
    fn results_report_status_and_strip_the_recommended_label() {
        let questions = normalize(&json!({"questions": [
            {"question": "a", "choices": ["x", "y"]},
            {"question": "b", "choices": ["p", "q"], "multi_select": true},
            {"question": "c"},
            {"question": "d"}
        ]}))
        .unwrap();
        let reply = json!({"outcome": "submitted", "answers": {
            "q0": "x (Recommended)", "q1": ["q", "p (Recommended)", " "], "q2": null
        }});
        let result = result(&questions, &reply);
        let responses = result["responses"].as_array().unwrap();
        assert_eq!(
            (
                responses[0]["status"].as_str(),
                responses[0]["user_response"].as_str()
            ),
            (Some("answered"), Some("x"))
        );
        assert_eq!(responses[1]["user_response"], json!(["q", "p"]));
        assert_eq!(responses[2]["status"], "skipped");
        assert_eq!(responses[3]["status"], "unanswered");
        assert_eq!(responses[0]["choices_offered"], json!(["x", "y"]));
        assert!(result.get("notice").is_none());
        let timed_out = super::result(
            &questions,
            &empty_reply("timed_out", "[user did not respond within 2m]"),
        );
        assert_eq!(timed_out["outcome"], "timed_out");
        assert_eq!(timed_out["notice"], "[user did not respond within 2m]");
        assert!(timed_out["responses"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["status"] == "unanswered"));
    }
}
