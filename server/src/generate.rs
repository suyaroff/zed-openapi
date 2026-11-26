//! "Generate requests for tag X": request blocks for spec operations the file doesn't have yet.

use crate::completion::request_text;
use crate::http_parser::HttpFile;
use crate::spec::{Op, Spec};
use serde_json::Value;

pub const UNTAGGED: &str = "untagged";

/// Operations missing from `file`, grouped by their first tag in spec `tags` order.
pub fn missing_by_tag<'a>(spec: &'a Spec, file: &HttpFile) -> Vec<(String, Vec<&'a Op>)> {
    let present: Vec<(&str, &str)> = file
        .requests
        .iter()
        .filter_map(|r| spec.find_exact(&r.method, &r.url))
        .map(|op| (op.method.as_str(), op.path.as_str()))
        .collect();
    let mut groups: Vec<(String, Vec<&Op>)> = spec
        .root
        .get("tags")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|t| t.get("name").and_then(Value::as_str))
        .map(|n| (n.to_string(), Vec::new()))
        .collect();
    for op in &spec.ops {
        if present.contains(&(op.method.as_str(), op.path.as_str())) {
            continue;
        }
        let tag = spec
            .operation(op)
            .pointer("/tags/0")
            .and_then(Value::as_str)
            .unwrap_or(UNTAGGED);
        match groups.iter_mut().find(|(t, _)| t == tag) {
            Some((_, ops)) => ops.push(op),
            None => groups.push((tag.to_string(), vec![op])),
        }
    }
    groups.retain(|(_, ops)| !ops.is_empty());
    groups
}

fn request_name(op_id: &str) -> String {
    op_id
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect()
}

pub fn blocks(spec: &Spec, ops: &[&Op], file: &HttpFile) -> String {
    let mut taken: Vec<String> = file.requests.iter().filter_map(|r| r.name.clone()).collect();
    let mut out = Vec::new();
    for op in ops {
        let o = spec.operation(op);
        let title = o
            .get("summary")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .map_or_else(|| format!("{} {}", op.method, op.path), |s| s.trim().replace('\n', " "));
        let mut block = format!("### {title}\n");
        if let Some(id) = o.get("operationId").and_then(Value::as_str).map(request_name) {
            if !id.is_empty() && !taken.contains(&id) {
                block.push_str(&format!("# @name {id}\n"));
                taken.push(id);
            }
        }
        block.push_str(&request_text(spec, op));
        out.push(block);
    }
    out.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http_parser::parse;
    use crate::test_util::spec;

    fn groups(text: &str) -> Vec<(String, Vec<String>)> {
        let s = spec();
        missing_by_tag(&s, &parse(text))
            .into_iter()
            .map(|(t, ops)| (t, ops.iter().map(|o| format!("{} {}", o.method, o.path)).collect()))
            .collect()
    }

    #[test]
    fn groups_missing_ops_by_tag_order() {
        let g = groups("GET {{baseUrl}}/pets?limit=1\n\n###\nDELETE {{baseUrl}}/pets/{{id}}\n\n###\nGET {{baseUrl}}/owners/1\n");
        assert_eq!(
            g,
            [
                (
                    "pets".to_string(),
                    vec!["POST /pets".to_string(), "GET /pets/{id}".into(), "GET /pets/{id}/photos".into(), "GET /pets/mine/photos".into()]
                ),
                (UNTAGGED.to_string(), vec!["POST /login".to_string(), "GET /health".into()]),
            ]
        );
    }

    #[test]
    fn nothing_missing() {
        let all: String = spec().ops.iter().map(|o| format!("{} {}\n\n###\n", o.method, o.path)).collect();
        assert!(groups(&all).is_empty());
    }

    #[test]
    fn blocks_name_requests_uniquely() {
        let s = spec();
        let file = parse("# @name getPet\nGET /pets/1\n");
        let ops: Vec<&Op> = ["GET /pets/{id}", "DELETE /pets/{id}", "GET /health"]
            .iter()
            .map(|l| s.ops.iter().find(|o| format!("{} {}", o.method, o.path) == *l).unwrap())
            .collect();
        assert_eq!(
            blocks(&s, &ops, &file),
            "### Get pet\nGET {{baseUrl}}/pets/{{id}}\n\n\
             ### DELETE /pets/{id}\n# @name delete_pet_\nDELETE {{baseUrl}}/pets/{{id}}\n\n\
             ### Health\nGET {{baseUrl}}/health"
        );
        assert_eq!(request_name("a.b c-d_é"), "a_b_c-d_é");
    }
}
