use once_cell::sync::Lazy;
use std::collections::HashMap;
use tokio::sync::Mutex;

/// Represents the type of a GitHub Action as declared in its action.yml `runs.using` field.
#[derive(Debug, Clone)]
pub enum ActionType {
    Node { version: u32 },
    Docker { image: String },
    Composite,
}

/// Result of resolving a remote action's action.yml.
#[derive(Debug, Clone)]
pub struct ResolvedAction {
    pub action_type: ActionType,
    /// The raw parsed action.yml, available for composite action execution.
    pub definition: Option<serde_yaml::Value>,
}

/// In-memory cache for resolved actions keyed by "owner/repo@version".
static ACTION_CACHE: Lazy<Mutex<HashMap<String, Option<ResolvedAction>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// Fetch and parse `action.yml` (or `action.yaml`) from a remote GitHub repository.
///
/// Returns `Ok(ResolvedAction)` on success, or `Err` if the action metadata cannot be
/// fetched or parsed. Callers should fall back to hardcoded image mappings on error.
pub async fn resolve_remote_action(repo: &str, version: &str) -> Result<ResolvedAction, String> {
    let cache_key = format!("{}@{}", repo, version);

    // Check cache first
    {
        let cache = ACTION_CACHE.lock().await;
        if let Some(cached) = cache.get(&cache_key) {
            return cached
                .clone()
                .ok_or_else(|| format!("Previously failed to resolve {}", cache_key));
        }
    }

    // Try action.yml first, then action.yaml
    let result = match fetch_and_parse(repo, version, "action.yml").await {
        Ok(resolved) => Ok(resolved),
        Err(_) => fetch_and_parse(repo, version, "action.yaml").await,
    };

    // Cache the result (including failures as None)
    {
        let mut cache = ACTION_CACHE.lock().await;
        cache.insert(cache_key, result.as_ref().ok().cloned());
    }

    result
}

async fn fetch_and_parse(
    repo: &str,
    version: &str,
    filename: &str,
) -> Result<ResolvedAction, String> {
    let url = format!(
        "https://raw.githubusercontent.com/{}/{}/{}",
        repo, version, filename
    );

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {}", e))?;

    let mut request = client.get(&url);

    // Use GITHUB_TOKEN if available for private repos / rate limiting
    if let Ok(token) = std::env::var("GITHUB_TOKEN") {
        request = request.header("Authorization", format!("token {}", token));
    }

    let response = request
        .send()
        .await
        .map_err(|e| format!("Failed to fetch {}: {}", url, e))?;

    if !response.status().is_success() {
        return Err(format!("HTTP {} fetching {}", response.status(), url));
    }

    let body = response
        .text()
        .await
        .map_err(|e| format!("Failed to read response body: {}", e))?;

    parse_action_definition(&body)
}

/// Parse an action.yml body and extract the action type from the `runs` section.
fn parse_action_definition(content: &str) -> Result<ResolvedAction, String> {
    let def: serde_yaml::Value =
        serde_yaml::from_str(content).map_err(|e| format!("Invalid action YAML: {}", e))?;

    let runs = def
        .get("runs")
        .ok_or_else(|| "action.yml missing 'runs' section".to_string())?;

    let using = runs
        .get("using")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "action.yml missing 'runs.using' field".to_string())?;

    let action_type = parse_using(using, runs)?;

    Ok(ResolvedAction {
        action_type,
        definition: Some(def),
    })
}

/// Map the `runs.using` value to an `ActionType`.
fn parse_using(using: &str, runs: &serde_yaml::Value) -> Result<ActionType, String> {
    match using {
        "composite" => Ok(ActionType::Composite),

        "docker" => {
            let image = runs
                .get("image")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "Docker action missing 'runs.image' field".to_string())?;

            // Strip "docker://" prefix if present (some actions use it, some don't)
            let image = image.trim_start_matches("docker://").to_string();
            Ok(ActionType::Docker { image })
        }

        s if s.starts_with("node") => {
            // Parse "node12", "node16", "node20", etc.
            let version: u32 = s.trim_start_matches("node").parse().unwrap_or(20);
            Ok(ActionType::Node { version })
        }

        other => Err(format!("Unknown runs.using value: {}", other)),
    }
}

/// Return the appropriate Docker image for a resolved action type.
pub fn image_for_action(action_type: &ActionType) -> String {
    match action_type {
        ActionType::Node { version } => format!("node:{}-slim", version),
        ActionType::Docker { image } => image.clone(),
        ActionType::Composite => "composite".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_node_action() {
        let yaml = r#"
name: 'My Action'
runs:
  using: 'node20'
  main: 'index.js'
"#;
        let resolved = parse_action_definition(yaml).unwrap();
        match resolved.action_type {
            ActionType::Node { version } => assert_eq!(version, 20),
            other => panic!("Expected Node action, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_docker_action() {
        let yaml = r#"
name: 'Docker Action'
runs:
  using: 'docker'
  image: 'docker://rust:latest'
"#;
        let resolved = parse_action_definition(yaml).unwrap();
        match &resolved.action_type {
            ActionType::Docker { image } => assert_eq!(image, "rust:latest"),
            other => panic!("Expected Docker action, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_docker_action_without_prefix() {
        let yaml = r#"
name: 'Docker Action'
runs:
  using: 'docker'
  image: 'Dockerfile'
"#;
        let resolved = parse_action_definition(yaml).unwrap();
        match &resolved.action_type {
            ActionType::Docker { image } => assert_eq!(image, "Dockerfile"),
            other => panic!("Expected Docker action, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_composite_action() {
        let yaml = r#"
name: 'Composite Action'
runs:
  using: 'composite'
  steps:
    - run: echo hello
"#;
        let resolved = parse_action_definition(yaml).unwrap();
        assert!(matches!(resolved.action_type, ActionType::Composite));
    }

    #[test]
    fn test_parse_missing_runs() {
        let yaml = r#"
name: 'Bad Action'
"#;
        assert!(parse_action_definition(yaml).is_err());
    }

    #[test]
    fn test_image_for_node() {
        assert_eq!(
            image_for_action(&ActionType::Node { version: 20 }),
            "node:20-slim"
        );
        assert_eq!(
            image_for_action(&ActionType::Node { version: 16 }),
            "node:16-slim"
        );
    }

    #[test]
    fn test_image_for_docker() {
        assert_eq!(
            image_for_action(&ActionType::Docker {
                image: "rust:latest".to_string()
            }),
            "rust:latest"
        );
    }

    #[test]
    fn test_image_for_composite() {
        assert_eq!(image_for_action(&ActionType::Composite), "composite");
    }
}
