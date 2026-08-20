use std::{collections::{BTreeMap, BTreeSet, HashMap}, sync::Arc};

use axum::http::Method;
use serde_json::Value;

use crate::config::AuthRole;

const MAX_PROFILE_OPERATIONS: usize = 30;
const CANONICAL_OPENAPI: &str = include_str!("../openapi.yaml");
const PROFILE_MANIFEST: &str = include_str!("../openapi-profiles.json");

#[derive(Clone)]
pub struct RolePolicy {
    rules: Arc<[RouteRule]>,
}

#[derive(Debug, Clone)]
struct RouteRule {
    method: String,
    template: String,
    roles: BTreeSet<AuthRole>,
}

impl RolePolicy {
    pub fn embedded() -> Result<Self, String> {
        let canonical: Value = serde_json::from_str(CANONICAL_OPENAPI)
            .map_err(|error| format!("canonical openapi.yaml is not JSON-compatible: {error}"))?;
        let profiles: BTreeMap<String, Vec<String>> = serde_json::from_str(PROFILE_MANIFEST)
            .map_err(|error| format!("openapi-profiles.json is invalid: {error}"))?;
        let operation_index = operation_index(&canonical)?;
        let mut route_roles: BTreeMap<(String, String), BTreeSet<AuthRole>> = BTreeMap::new();

        for (profile, operation_ids) in profiles {
            let role = profile_role(&profile)?;
            if operation_ids.is_empty() || operation_ids.len() > MAX_PROFILE_OPERATIONS {
                return Err(format!(
                    "profile {profile:?} must contain 1..={MAX_PROFILE_OPERATIONS} operations, found {}",
                    operation_ids.len()
                ));
            }
            let mut seen = BTreeSet::new();
            for operation_id in operation_ids {
                if !seen.insert(operation_id.clone()) {
                    return Err(format!("profile {profile:?} duplicates operationId {operation_id:?}"));
                }
                let (method, path) = operation_index.get(&operation_id)
                    .ok_or_else(|| format!("profile {profile:?} references missing operationId {operation_id:?}"))?;
                route_roles.entry((method.clone(), path.clone())).or_default().insert(role);
            }
        }

        let rules = route_roles.into_iter().map(|((method, template), roles)| RouteRule {
            method,
            template,
            roles,
        }).collect::<Vec<_>>();
        Ok(Self { rules: Arc::from(rules) })
    }

    pub fn allows(&self, role: AuthRole, method: &Method, path: &str) -> bool {
        if role == AuthRole::Admin {
            return true;
        }
        let method = method.as_str().to_ascii_lowercase();
        self.rules.iter().any(|rule| {
            rule.method == method && rule.roles.contains(&role) && route_matches(&rule.template, path)
        })
    }
}

fn operation_index(canonical: &Value) -> Result<HashMap<String, (String, String)>, String> {
    let paths = canonical.get("paths").and_then(Value::as_object)
        .ok_or_else(|| "canonical OpenAPI has no paths object".to_owned())?;
    let mut index = HashMap::new();
    for (path, path_item) in paths {
        let Some(methods) = path_item.as_object() else { continue; };
        for (method, operation) in methods {
            if !is_http_method(method) { continue; }
            let Some(operation_id) = operation.get("operationId").and_then(Value::as_str) else { continue; };
            if index.insert(operation_id.to_owned(), (method.to_ascii_lowercase(), path.clone())).is_some() {
                return Err(format!("canonical OpenAPI duplicates operationId {operation_id:?}"));
            }
        }
    }
    Ok(index)
}

fn profile_role(profile: &str) -> Result<AuthRole, String> {
    match profile {
        "dev" => Ok(AuthRole::Developer),
        "review" => Ok(AuthRole::Reviewer),
        "ops" => Ok(AuthRole::Operator),
        other => Err(format!("unknown OpenAPI profile {other:?}")),
    }
}

fn is_http_method(method: &str) -> bool {
    matches!(method, "get" | "put" | "post" | "delete" | "patch" | "head" | "options" | "trace")
}

fn route_matches(template: &str, actual: &str) -> bool {
    let template_parts = template.trim_matches('/').split('/').collect::<Vec<_>>();
    let actual_parts = actual.trim_matches('/').split('/').collect::<Vec<_>>();
    template_parts.len() == actual_parts.len()
        && template_parts.iter().zip(actual_parts).all(|(expected, found)| {
            if expected.starts_with('{') && expected.ends_with('}') {
                !found.is_empty()
            } else {
                *expected == found
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_policy_loads_and_respects_roles() {
        let policy = RolePolicy::embedded().unwrap();
        assert!(policy.allows(AuthRole::Developer, &Method::POST, "/v1/code/edit-plan"));
        assert!(policy.allows(AuthRole::Developer, &Method::POST, "/v1/git/commit"));
        assert!(!policy.allows(AuthRole::Developer, &Method::GET, "/v1/system/processes"));
        assert!(policy.allows(AuthRole::Reviewer, &Method::POST, "/v1/git/diff"));
        assert!(!policy.allows(AuthRole::Reviewer, &Method::POST, "/v1/git/commit"));
        assert!(policy.allows(AuthRole::Operator, &Method::DELETE, "/v1/jobs/job-1"));
        assert!(policy.allows(AuthRole::Operator, &Method::POST, "/v1/approvals/a-1/approve"));
        assert!(!policy.allows(AuthRole::Operator, &Method::POST, "/v1/lsp/rename"));
        assert!(policy.allows(AuthRole::Admin, &Method::POST, "/anything"));
    }

    #[test]
    fn templates_match_single_segments_only() {
        assert!(route_matches("/v1/jobs/{id}", "/v1/jobs/job-1"));
        assert!(!route_matches("/v1/jobs/{id}", "/v1/jobs/a/b"));
        assert!(!route_matches("/v1/jobs/{id}", "/v1/jobs/"));
    }
}
