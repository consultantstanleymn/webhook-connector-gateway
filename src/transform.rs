//! Per-tenant JSON transform rules.
//!
//! Rules run in order. Field names can use dots for nested objects,
//! for example `customer.email`.

use serde::Deserialize;
use serde_json::{Map, Value};

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Rule {
    /// Move a field to a new name.
    Rename { from: String, to: String },
    /// Delete a field.
    Remove { field: String },
    /// Set a field. String values `$tenant_id` and `$event_id` are replaced.
    Enrich { field: String, value: Value },
    /// Keep the event only when the field equals the value.
    Keep { field: String, equals: Value },
    /// Drop the event when the field equals the value.
    Drop { field: String, equals: Value },
}

pub struct Context<'a> {
    pub tenant_id: &'a str,
    pub event_id: &'a str,
}

#[derive(Debug, PartialEq)]
pub enum Outcome {
    Deliver(Value),
    Filtered,
}

pub fn apply(rules: &[Rule], ctx: &Context, mut doc: Value) -> Outcome {
    for rule in rules {
        match rule {
            Rule::Rename { from, to } => {
                if let Some(v) = remove_path(&mut doc, from) {
                    set_path(&mut doc, to, v);
                }
            }
            Rule::Remove { field } => {
                remove_path(&mut doc, field);
            }
            Rule::Enrich { field, value } => {
                let v = match value.as_str() {
                    Some("$tenant_id") => Value::String(ctx.tenant_id.to_string()),
                    Some("$event_id") => Value::String(ctx.event_id.to_string()),
                    _ => value.clone(),
                };
                set_path(&mut doc, field, v);
            }
            Rule::Keep { field, equals } => {
                if get_path(&doc, field) != Some(equals) {
                    return Outcome::Filtered;
                }
            }
            Rule::Drop { field, equals } => {
                if get_path(&doc, field) == Some(equals) {
                    return Outcome::Filtered;
                }
            }
        }
    }
    Outcome::Deliver(doc)
}

fn get_path<'a>(doc: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.').try_fold(doc, |cur, key| cur.get(key))
}

fn remove_path(doc: &mut Value, path: &str) -> Option<Value> {
    let (parent, last) = match path.rsplit_once('.') {
        Some((p, l)) => (get_path_mut(doc, p)?, l),
        None => (doc, path),
    };
    parent.as_object_mut()?.remove(last)
}

fn get_path_mut<'a>(doc: &'a mut Value, path: &str) -> Option<&'a mut Value> {
    path.split('.').try_fold(doc, |cur, key| cur.get_mut(key))
}

fn set_path(doc: &mut Value, path: &str, value: Value) {
    let mut cur = doc;
    let mut parts = path.split('.').peekable();
    while let Some(key) = parts.next() {
        if !cur.is_object() {
            *cur = Value::Object(Map::new());
        }
        let obj = cur.as_object_mut().expect("made an object above");
        if parts.peek().is_none() {
            obj.insert(key.to_string(), value);
            return;
        }
        cur = obj
            .entry(key.to_string())
            .or_insert_with(|| Value::Object(Map::new()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const CTX: Context = Context {
        tenant_id: "acme",
        event_id: "ev1",
    };

    fn run(rules: Vec<Rule>, doc: Value) -> Outcome {
        apply(&rules, &CTX, doc)
    }

    #[test]
    fn rename_nested_to_flat() {
        let out = run(
            vec![Rule::Rename {
                from: "customer.email".into(),
                to: "email".into(),
            }],
            json!({"customer": {"email": "a@b.c", "id": 1}}),
        );
        assert_eq!(
            out,
            Outcome::Deliver(json!({"customer": {"id": 1}, "email": "a@b.c"}))
        );
    }

    #[test]
    fn rename_missing_is_noop() {
        let out = run(
            vec![Rule::Rename {
                from: "x".into(),
                to: "y".into(),
            }],
            json!({"a": 1}),
        );
        assert_eq!(out, Outcome::Deliver(json!({"a": 1})));
    }

    #[test]
    fn enrich_with_placeholders_and_nested_create() {
        let out = run(
            vec![
                Rule::Enrich {
                    field: "meta.tenant".into(),
                    value: json!("$tenant_id"),
                },
                Rule::Enrich {
                    field: "meta.event".into(),
                    value: json!("$event_id"),
                },
                Rule::Enrich {
                    field: "source".into(),
                    value: json!("gateway"),
                },
            ],
            json!({}),
        );
        assert_eq!(
            out,
            Outcome::Deliver(
                json!({"meta": {"tenant": "acme", "event": "ev1"}, "source": "gateway"})
            )
        );
    }

    #[test]
    fn filters() {
        let keep = Rule::Keep {
            field: "type".into(),
            equals: json!("order.created"),
        };
        assert_eq!(
            run(vec![keep.clone()], json!({"type": "ping"})),
            Outcome::Filtered
        );
        assert_eq!(run(vec![keep.clone()], json!({})), Outcome::Filtered);
        assert!(matches!(
            run(vec![keep], json!({"type": "order.created"})),
            Outcome::Deliver(_)
        ));
        let drop = Rule::Drop {
            field: "test".into(),
            equals: json!(true),
        };
        assert_eq!(run(vec![drop], json!({"test": true})), Outcome::Filtered);
    }

    #[test]
    fn remove_and_order_of_rules() {
        let out = run(
            vec![
                Rule::Rename {
                    from: "a".into(),
                    to: "b".into(),
                },
                Rule::Remove { field: "b".into() },
            ],
            json!({"a": 1, "c": 2}),
        );
        assert_eq!(out, Outcome::Deliver(json!({"c": 2})));
    }

    #[test]
    fn rules_parse_from_toml() {
        #[derive(Deserialize)]
        struct W {
            rules: Vec<Rule>,
        }
        let w: W = toml::from_str(
            "rules = [{op='rename', from='a', to='b'}, {op='keep', field='t', equals='x'}]",
        )
        .unwrap();
        assert_eq!(w.rules.len(), 2);
    }
}
