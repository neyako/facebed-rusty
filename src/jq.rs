use serde_json::Value;

/// Walk a JSON value and yield every nested object as `&Map<...>`-equivalent `&Value::Object`.
/// Python `Jq.enumerate` order is depth-first, lists-of-dicts before dicts before scalars.
/// We don't preserve that subtle order — callers must not depend on iteration order.
pub fn enumerate<'a>(root: &'a Value, out: &mut Vec<&'a Value>) {
    match root {
        Value::Object(map) => {
            out.push(root);
            for v in map.values() {
                enumerate(v, out);
            }
        }
        Value::Array(arr) => {
            for v in arr {
                enumerate(v, out);
            }
        }
        _ => {}
    }
}

/// Find the first value associated with `key` anywhere in the tree.
pub fn first<'a>(root: &'a Value, key: &str) -> Option<&'a Value> {
    match root {
        Value::Object(map) => {
            if let Some(v) = map.get(key) {
                return Some(v);
            }
            for v in map.values() {
                if let Some(found) = first(v, key) {
                    return Some(found);
                }
            }
            None
        }
        Value::Array(arr) => {
            for v in arr {
                if let Some(found) = first(v, key) {
                    return Some(found);
                }
            }
            None
        }
        _ => None,
    }
}

/// Find every value associated with `key` anywhere in the tree.
pub fn all<'a>(root: &'a Value, key: &str) -> Vec<&'a Value> {
    let mut out = Vec::new();
    walk_all(root, key, &mut out);
    out
}

fn walk_all<'a>(root: &'a Value, key: &str, out: &mut Vec<&'a Value>) {
    match root {
        Value::Object(map) => {
            for (k, v) in map {
                if k == key {
                    out.push(v);
                }
                walk_all(v, key, out);
            }
        }
        Value::Array(arr) => {
            for v in arr {
                walk_all(v, key, out);
            }
        }
        _ => {}
    }
}

/// Find the last occurrence of `key` (in walk order).
pub fn last<'a>(root: &'a Value, key: &str) -> Option<&'a Value> {
    all(root, key).into_iter().last()
}

/// True iff *every* key exists somewhere in the tree.
pub fn has(root: &Value, keys: &[&str]) -> bool {
    keys.iter().all(|k| first(root, k).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn first_finds_nested() {
        let v = json!({"a": {"b": {"c": 42}}});
        assert_eq!(first(&v, "c"), Some(&json!(42)));
    }

    #[test]
    fn all_collects() {
        let v = json!({"id": 1, "child": {"id": 2, "grandchild": {"id": 3}}});
        let mut ids: Vec<i64> = all(&v, "id")
            .into_iter()
            .filter_map(|x| x.as_i64())
            .collect();
        ids.sort();
        assert_eq!(ids, vec![1, 2, 3]);
    }

    #[test]
    fn has_all_or_nothing() {
        let v = json!({"a": 1, "nested": {"b": 2}});
        assert!(has(&v, &["a", "b"]));
        assert!(!has(&v, &["a", "z"]));
    }
}
