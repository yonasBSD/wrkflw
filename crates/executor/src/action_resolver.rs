use once_cell::sync::Lazy;
use std::collections::{HashMap, VecDeque};
use tokio::sync::RwLock;

/// Maximum number of entries in the action resolution cache.
const MAX_CACHE_ENTRIES: usize = 256;

/// Represents the type of a GitHub Action as declared in its action.yml `runs.using` field.
#[derive(Debug, Clone)]
pub enum ActionType {
    Node {
        version: u32,
    },
    /// A Docker action that references a registry image (e.g., `rust:latest`).
    Docker {
        image: String,
    },
    /// A Docker action that bundles its own Dockerfile and needs to be built.
    DockerBuild,
    Composite,
}

/// Result of resolving a remote action's action.yml.
#[derive(Debug, Clone)]
pub struct ResolvedAction {
    pub action_type: ActionType,
    /// The raw parsed action.yml, available for composite action execution.
    pub definition: Option<serde_yaml::Value>,
}

/// Bounded LRU-style cache for successfully resolved actions keyed by "owner/repo@version".
/// Only successful resolutions are cached — transient failures are not persisted
/// so that retries can succeed if network conditions improve.
struct BoundedCache {
    map: HashMap<String, ResolvedAction>,
    /// Insertion order for LRU eviction (oldest at front).
    order: VecDeque<String>,
}

impl BoundedCache {
    fn new() -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn get(&self, key: &str) -> Option<&ResolvedAction> {
        self.map.get(key)
    }

    #[allow(clippy::map_entry)]
    fn insert(&mut self, key: String, value: ResolvedAction) {
        if self.map.contains_key(&key) {
            // Already cached — update value, don't change LRU order
            self.map.insert(key, value);
            return;
        }
        // Evict oldest entries if at capacity
        while self.map.len() >= MAX_CACHE_ENTRIES {
            if let Some(oldest) = self.order.pop_front() {
                self.map.remove(&oldest);
            }
        }
        self.order.push_back(key.clone());
        self.map.insert(key, value);
    }
}

static ACTION_CACHE: Lazy<RwLock<BoundedCache>> = Lazy::new(|| RwLock::new(BoundedCache::new()));

/// Shared HTTP client to avoid repeated TLS initialization.
/// Timeout is kept low (5s) since resolution is best-effort with a fallback.
static HTTP_CLIENT: Lazy<reqwest::Client> = Lazy::new(|| {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .expect("Failed to create HTTP client")
});

/// Fetch and parse `action.yml` (or `action.yaml`) from a remote GitHub repository.
///
/// Returns `Ok(ResolvedAction)` on success, or `Err` if the action metadata cannot be
/// fetched or parsed. Callers should fall back to hardcoded image mappings on error.
pub async fn resolve_remote_action(repo: &str, version: &str) -> Result<ResolvedAction, String> {
    let cache_key = format!("{}@{}", repo, version);

    // Check cache first (read lock — allows concurrent reads)
    {
        let cache = ACTION_CACHE.read().await;
        if let Some(cached) = cache.get(&cache_key) {
            return Ok(cached.clone());
        }
    }

    // Try action.yml first, then action.yaml
    let result = match fetch_and_parse(repo, version, "action.yml").await {
        Ok(resolved) => Ok(resolved),
        Err(_) => fetch_and_parse(repo, version, "action.yaml").await,
    };

    // Only cache successful resolutions — transient failures should be retryable
    if let Ok(ref resolved) = result {
        let mut cache = ACTION_CACHE.write().await;
        cache.insert(cache_key, resolved.clone());
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

    // Try unauthenticated first; only send GITHUB_TOKEN on 404 (private repos)
    let response = HTTP_CLIENT
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("Failed to fetch {}: {}", url, e))?;

    let response = if response.status() == reqwest::StatusCode::NOT_FOUND {
        // Retry with auth if token is available — the repo may be private
        if let Ok(token) = std::env::var("GITHUB_TOKEN") {
            HTTP_CLIENT
                .get(&url)
                .header("Authorization", format!("token {}", token))
                .send()
                .await
                .map_err(|e| format!("Failed to fetch {}: {}", url, e))?
        } else {
            response
        }
    } else {
        response
    };

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
            let image = image.trim_start_matches("docker://");

            // If the image is "Dockerfile" or a relative path, it means the action
            // bundles its own Dockerfile that needs to be built — not pulled from a registry.
            if image == "Dockerfile"
                || image.starts_with("./")
                || image.starts_with("../")
                || image.ends_with("/Dockerfile")
            {
                Ok(ActionType::DockerBuild)
            } else {
                Ok(ActionType::Docker {
                    image: image.to_string(),
                })
            }
        }

        s if s.starts_with("node") => {
            let version_str = s.trim_start_matches("node");
            let version: u32 = version_str.parse().map_err(|_| {
                format!(
                    "Invalid node version in runs.using '{}': expected 'node<N>' (e.g., 'node20')",
                    s
                )
            })?;
            Ok(ActionType::Node { version })
        }

        other => Err(format!("Unknown runs.using value: {}", other)),
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
    fn test_parse_docker_action_with_dockerfile() {
        let yaml = r#"
name: 'Docker Action'
runs:
  using: 'docker'
  image: 'Dockerfile'
"#;
        let resolved = parse_action_definition(yaml).unwrap();
        assert!(
            matches!(resolved.action_type, ActionType::DockerBuild),
            "Expected DockerBuild, got {:?}",
            resolved.action_type
        );
    }

    #[test]
    fn test_parse_docker_action_with_relative_dockerfile() {
        let yaml = r#"
name: 'Docker Action'
runs:
  using: 'docker'
  image: './docker/Dockerfile'
"#;
        let resolved = parse_action_definition(yaml).unwrap();
        assert!(
            matches!(resolved.action_type, ActionType::DockerBuild),
            "Expected DockerBuild, got {:?}",
            resolved.action_type
        );
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
    fn test_parse_node16_action() {
        let yaml = r#"
name: 'Legacy Node Action'
runs:
  using: 'node16'
  main: 'index.js'
"#;
        let resolved = parse_action_definition(yaml).unwrap();
        match resolved.action_type {
            ActionType::Node { version } => assert_eq!(version, 16),
            other => panic!("Expected Node 16, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_unknown_using_value() {
        let yaml = r#"
name: 'Unknown Action'
runs:
  using: 'python3'
"#;
        let err = parse_action_definition(yaml).unwrap_err();
        assert!(err.contains("Unknown runs.using value"));
    }

    #[test]
    fn test_parse_missing_using_field() {
        let yaml = r#"
name: 'Bad Action'
runs:
  main: 'index.js'
"#;
        let err = parse_action_definition(yaml).unwrap_err();
        assert!(err.contains("runs.using"));
    }

    #[test]
    fn test_parse_docker_missing_image() {
        let yaml = r#"
name: 'Bad Docker Action'
runs:
  using: 'docker'
"#;
        let err = parse_action_definition(yaml).unwrap_err();
        assert!(err.contains("runs.image"));
    }

    #[test]
    fn test_parse_docker_with_docker_prefix_and_dockerfile() {
        let yaml = r#"
name: 'Docker Action'
runs:
  using: 'docker'
  image: 'docker://Dockerfile'
"#;
        let resolved = parse_action_definition(yaml).unwrap();
        assert!(
            matches!(resolved.action_type, ActionType::DockerBuild),
            "docker://Dockerfile should be DockerBuild, got {:?}",
            resolved.action_type
        );
    }

    #[test]
    fn test_resolved_action_has_definition() {
        let yaml = r#"
name: 'My Action'
description: 'Test'
runs:
  using: 'node20'
  main: 'index.js'
"#;
        let resolved = parse_action_definition(yaml).unwrap();
        let def = resolved.definition.unwrap();
        assert_eq!(def.get("name").unwrap().as_str().unwrap(), "My Action");
    }

    #[test]
    fn test_parse_malformed_node_version_returns_error() {
        let yaml = r#"
name: 'Bad Node Action'
runs:
  using: 'nodefoo'
  main: 'index.js'
"#;
        let err = parse_action_definition(yaml).unwrap_err();
        assert!(
            err.contains("Invalid node version"),
            "Expected error about invalid node version, got: {}",
            err
        );
    }

    #[test]
    fn test_parse_bare_node_returns_error() {
        let yaml = r#"
name: 'Bare Node Action'
runs:
  using: 'node'
  main: 'index.js'
"#;
        let err = parse_action_definition(yaml).unwrap_err();
        assert!(
            err.contains("Invalid node version"),
            "Expected error about invalid node version, got: {}",
            err
        );
    }

    #[tokio::test]
    async fn test_cache_respects_max_capacity() {
        let mut cache = BoundedCache::new();
        // Fill beyond capacity
        for i in 0..MAX_CACHE_ENTRIES + 10 {
            cache.insert(
                format!("owner/repo@v{}", i),
                ResolvedAction {
                    action_type: ActionType::Node { version: 20 },
                    definition: None,
                },
            );
        }
        assert!(
            cache.map.len() <= MAX_CACHE_ENTRIES,
            "Cache size {} exceeds max {}",
            cache.map.len(),
            MAX_CACHE_ENTRIES
        );
        // Oldest entries should have been evicted
        assert!(cache.get("owner/repo@v0").is_none());
        // Newest entries should still be present
        assert!(cache
            .get(&format!("owner/repo@v{}", MAX_CACHE_ENTRIES + 9))
            .is_some());
    }

    #[tokio::test]
    async fn test_cache_duplicate_insert_does_not_grow() {
        let mut cache = BoundedCache::new();
        cache.insert(
            "owner/repo@v1".to_string(),
            ResolvedAction {
                action_type: ActionType::Node { version: 20 },
                definition: None,
            },
        );
        cache.insert(
            "owner/repo@v1".to_string(),
            ResolvedAction {
                action_type: ActionType::Node { version: 16 },
                definition: None,
            },
        );
        assert_eq!(cache.map.len(), 1);
        // Value should be updated
        match &cache.get("owner/repo@v1").unwrap().action_type {
            ActionType::Node { version } => assert_eq!(*version, 16),
            other => panic!("Expected Node, got {:?}", other),
        }
    }
}
