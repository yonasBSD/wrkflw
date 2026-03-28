use serde::{Deserialize, Deserializer, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use wrkflw_matrix::MatrixConfig;

use super::schema::SchemaValidator;

// Custom deserializer for needs field that handles both string and array formats
fn deserialize_needs<'de, D>(deserializer: D) -> Result<Option<Vec<String>>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringOrVec {
        String(String),
        Vec(Vec<String>),
    }

    let value = Option::<StringOrVec>::deserialize(deserializer)?;
    match value {
        Some(StringOrVec::String(s)) => Ok(Some(vec![s])),
        Some(StringOrVec::Vec(v)) => Ok(Some(v)),
        None => Ok(None),
    }
}

// Custom deserializer for runs-on field that handles both string and array formats
fn deserialize_runs_on<'de, D>(deserializer: D) -> Result<Option<Vec<String>>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringOrVec {
        String(String),
        Vec(Vec<String>),
    }

    let value = Option::<StringOrVec>::deserialize(deserializer)?;
    match value {
        Some(StringOrVec::String(s)) => Ok(Some(vec![s])),
        Some(StringOrVec::Vec(v)) => Ok(Some(v)),
        None => Ok(None),
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct WorkflowDefinition {
    pub name: String,
    #[serde(skip, default)] // Skip deserialization of the 'on' field directly
    pub on: Vec<String>,
    #[serde(rename = "on")] // Raw access to the 'on' field for custom handling
    pub on_raw: serde_yaml::Value,
    pub jobs: HashMap<String, Job>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Job {
    #[serde(rename = "runs-on", default, deserialize_with = "deserialize_runs_on")]
    pub runs_on: Option<Vec<String>>,
    #[serde(default, deserialize_with = "deserialize_needs")]
    pub needs: Option<Vec<String>>,
    #[serde(default)]
    pub steps: Vec<Step>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub matrix: Option<MatrixConfig>,
    #[serde(default)]
    pub services: HashMap<String, Service>,
    #[serde(default, rename = "if")]
    pub if_condition: Option<String>,
    #[serde(default)]
    pub outputs: Option<HashMap<String, String>>,
    #[serde(default)]
    pub permissions: Option<HashMap<String, String>>,
    // Reusable workflow (job-level 'uses') support
    #[serde(default)]
    pub uses: Option<String>,
    #[serde(default)]
    pub with: Option<HashMap<String, String>>,
    #[serde(default)]
    pub secrets: Option<serde_yaml::Value>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Service {
    pub image: String,
    #[serde(default)]
    pub ports: Option<Vec<String>>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub volumes: Option<Vec<String>>,
    #[serde(default)]
    pub options: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Step {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub uses: Option<String>,
    #[serde(default)]
    pub run: Option<String>,
    #[serde(default)]
    pub with: Option<HashMap<String, String>>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub continue_on_error: Option<bool>,
}

impl WorkflowDefinition {
    pub fn resolve_action(&self, action_ref: &str) -> ActionInfo {
        // Parse GitHub action reference like "actions/checkout@v3"
        let parts: Vec<&str> = action_ref.split('@').collect();

        let (repo, version) = if parts.len() > 1 {
            (parts[0], parts[1])
        } else {
            (parts[0], "main") // Default to main if no version specified
        };

        ActionInfo {
            repository: repo.to_string(),
            version: version.to_string(),
            is_docker: repo.starts_with("docker://"),
            is_local: repo.starts_with("./"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ActionInfo {
    pub repository: String,
    pub version: String,
    pub is_docker: bool,
    pub is_local: bool,
}

pub fn parse_workflow(path: &Path) -> Result<WorkflowDefinition, String> {
    // First validate against schema
    let validator = SchemaValidator::new()?;
    validator.validate_workflow(path)?;

    // If validation passes, parse the workflow
    let content =
        fs::read_to_string(path).map_err(|e| format!("Failed to read workflow file: {}", e))?;

    // Parse the YAML content
    let mut workflow: WorkflowDefinition = serde_yaml::from_str(&content)
        .map_err(|e| format!("Failed to parse workflow structure: {}", e))?;

    // Normalize the trigger events
    workflow.on = normalize_triggers(&workflow.on_raw)?;

    Ok(workflow)
}

fn normalize_triggers(on_value: &serde_yaml::Value) -> Result<Vec<String>, String> {
    let mut triggers = Vec::new();

    match on_value {
        // Simple string trigger: on: push
        serde_yaml::Value::String(event) => {
            triggers.push(event.clone());
        }
        // Array of triggers: on: [push, pull_request]
        serde_yaml::Value::Sequence(events) => {
            for event in events {
                if let Some(event_str) = event.as_str() {
                    triggers.push(event_str.to_string());
                }
            }
        }
        // Map of triggers with configuration: on: {push: {branches: [main]}}
        serde_yaml::Value::Mapping(events_map) => {
            for (event, _) in events_map {
                if let Some(event_str) = event.as_str() {
                    triggers.push(event_str.to_string());
                }
            }
        }
        _ => {
            return Err("'on' section has invalid format".to_string());
        }
    }

    Ok(triggers)
}

#[cfg(test)]
mod tests {
    use super::parse_workflow;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn parse_workflow_allows_null_workflow_dispatch_with_other_triggers() {
        let temp_dir = tempdir().unwrap();
        let workflow_path = temp_dir.path().join("workflow.yml");

        let content = r#"
name: trigger-test
on:
  push:
    branches: []
    tags-ignore: []
  release:
    types: [prereleased, published]
  workflow_dispatch:

jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - run: echo hi
"#;

        fs::write(&workflow_path, content).unwrap();

        let parsed = parse_workflow(&workflow_path);
        assert!(
            parsed.is_ok(),
            "Expected workflow to parse successfully, got: {:?}",
            parsed.err()
        );
    }
}
