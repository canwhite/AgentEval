//! Four read-only file tools for the probe agent.
//!
//! All paths are resolved relative to `source_dir` (set from PROBE_SOURCE_PROJECT_DIR).
//! Paths containing `..` are rejected to prevent directory traversal.

use std::path::PathBuf;

use serde_json::Value;

use super::tool::Tool;

/// Resolve a tool argument path relative to the source project directory.
///
/// Returns an error if the path contains `..` or resolves outside the source dir.
fn resolve_path(source_dir: &str, path: &str) -> Result<PathBuf, String> {
    if path.contains("..") {
        return Err(format!("path traversal rejected: '{}'", path));
    }
    let base = PathBuf::from(source_dir);
    let resolved = base.join(path.trim_start_matches('/'));
    // Canonicalize to catch indirect traversal, but only if the path exists.
    // For non-existent paths (e.g. glob patterns), do a prefix check.
    if resolved.exists() {
        let canon = std::fs::canonicalize(&resolved)
            .map_err(|e| format!("failed to resolve path '{}': {}", path, e))?;
        let base_canon = std::fs::canonicalize(source_dir)
            .map_err(|e| format!("failed to resolve base dir: {}", e))?;
        if !canon.starts_with(&base_canon) {
            return Err(format!("path escapes source dir: '{}'", path));
        }
        Ok(canon)
    } else {
        // For non-existent paths, just check the string doesn't escape
        let base_canon = std::fs::canonicalize(source_dir)
            .map_err(|e| format!("failed to resolve base dir: {}", e))?;
        // Strip the source dir prefix + slash, re-join to get canonical form
        let mut normalized = base_canon.clone();
        for component in path.trim_start_matches('/').split('/') {
            if component == ".." || component == "." && path != "." {
                continue;
            }
            normalized.push(component);
        }
        // If the path tries to go above, canonicalize would have fewer components
        // than base_canon, but since we rejected `..` this is just a safety check.
        Ok(resolved)
    }
}

// ── ReadFile ──

pub struct ReadFile {
    source_dir: String,
}

impl ReadFile {
    pub fn new(source_dir: &str) -> Self {
        Self {
            source_dir: source_dir.to_string(),
        }
    }
}

impl Tool for ReadFile {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "Read the contents of a file. Returns the file content as text. \
         Files larger than 1MB are truncated with a marker."
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Relative path to the file to read (e.g. CLAUDE.md, skills/search.md)"
                }
            },
            "required": ["path"]
        })
    }

    fn call(&self, args: Value) -> Result<String, String> {
        let path_str = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or("missing 'path' argument")?;

        let resolved = resolve_path(&self.source_dir, path_str)?;

        let content = std::fs::read_to_string(&resolved)
            .map_err(|e| format!("failed to read '{}': {}", path_str, e))?;

        const MAX_BYTES: usize = 1_000_000;
        if content.len() > MAX_BYTES {
            let truncated = &content[..MAX_BYTES];
            Ok(format!(
                "{}\n\n[truncated at {} bytes, {} bytes omitted]",
                truncated,
                MAX_BYTES,
                content.len() - MAX_BYTES
            ))
        } else {
            Ok(content)
        }
    }
}

// ── Grep ──

pub struct Grep {
    source_dir: String,
}

impl Grep {
    pub fn new(source_dir: &str) -> Self {
        Self {
            source_dir: source_dir.to_string(),
        }
    }
}

impl Tool for Grep {
    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        "Search for a pattern in files under a directory. \
         Uses system grep -rn (recursive, line numbers). \
         Excludes binary files. Results are truncated at 1000 lines. \
         Use this to find where a keyword, tool name, or skill is referenced."
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "The search pattern (grep-compatible regex)"
                },
                "path": {
                    "type": "string",
                    "description": "Directory or file to search in, relative to project root. Use '.' for the entire project."
                }
            },
            "required": ["pattern", "path"]
        })
    }

    fn call(&self, args: Value) -> Result<String, String> {
        let pattern = args
            .get("pattern")
            .and_then(|v| v.as_str())
            .ok_or("missing 'pattern' argument")?;
        let path_str = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or("missing 'path' argument")?;

        let resolved = resolve_path(&self.source_dir, path_str)?;

        let output = std::process::Command::new("grep")
            .args(["-rn", "--binary-files=without-match"])
            .arg(pattern)
            .arg(&resolved)
            .output()
            .map_err(|e| format!("grep failed to run: {}", e))?;

        if !output.status.success() && output.status.code() != Some(1) {
            // grep returns 1 for "no matches", 2+ for errors
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("grep error: {}", stderr.trim()));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        if stdout.trim().is_empty() {
            return Ok("(no matches found)".to_string());
        }

        const MAX_LINES: usize = 1000;
        let lines: Vec<&str> = stdout.lines().collect();
        if lines.len() > MAX_LINES {
            Ok(format!(
                "{}\n\n[truncated at {} lines, {} lines omitted]",
                lines[..MAX_LINES].join("\n"),
                MAX_LINES,
                lines.len() - MAX_LINES
            ))
        } else {
            Ok(stdout.to_string())
        }
    }
}

// ── ListDir ──

pub struct ListDir {
    source_dir: String,
}

impl ListDir {
    pub fn new(source_dir: &str) -> Self {
        Self {
            source_dir: source_dir.to_string(),
        }
    }
}

impl Tool for ListDir {
    fn name(&self) -> &str {
        "list_dir"
    }

    fn description(&self) -> &str {
        "List files and subdirectories in a directory. \
         Returns entries in 'TYPE  NAME' format where TYPE is DIR or FILE. \
         Use this to explore the project structure."
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Directory path relative to project root. Use '/' for the project root."
                }
            },
            "required": ["path"]
        })
    }

    fn call(&self, args: Value) -> Result<String, String> {
        let path_str = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or("missing 'path' argument")?;

        let resolved = resolve_path(&self.source_dir, path_str)?;

        let dir = std::fs::read_dir(&resolved)
            .map_err(|e| format!("failed to list '{}': {}", path_str, e))?;

        let mut entries: Vec<String> = Vec::new();
        for entry in dir {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            let name = entry.file_name().to_string_lossy().to_string();
            let prefix = if entry.path().is_dir() { "DIR " } else { "FILE" };
            entries.push(format!("{}  {}", prefix, name));
        }
        entries.sort();

        if entries.is_empty() {
            Ok("(empty directory)".to_string())
        } else {
            Ok(entries.join("\n"))
        }
    }
}

// ── Glob ──

pub struct Glob {
    source_dir: String,
}

impl Glob {
    pub fn new(source_dir: &str) -> Self {
        Self {
            source_dir: source_dir.to_string(),
        }
    }
}

impl Tool for Glob {
    fn name(&self) -> &str {
        "glob"
    }

    fn description(&self) -> &str {
        "Find files matching a name pattern. Uses find -name under the hood. \
         Returns matching file paths relative to the project root. \
         Results are truncated at 2000 entries. \
         Use this to find files by extension or naming convention \
         (e.g. '*.md' for skill files, '*.json' for config files)."
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "File name pattern (e.g. '*.md', 'CLAUDE.md', '*.json')"
                },
                "path": {
                    "type": "string",
                    "description": "Directory to search in, relative to project root. Use '.' for the entire project."
                }
            },
            "required": ["pattern", "path"]
        })
    }

    fn call(&self, args: Value) -> Result<String, String> {
        let pattern = args
            .get("pattern")
            .and_then(|v| v.as_str())
            .ok_or("missing 'pattern' argument")?;
        let path_str = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or("missing 'path' argument")?;

        let resolved = resolve_path(&self.source_dir, path_str)?;

        let output = std::process::Command::new("find")
            .arg(&resolved)
            .arg("-name")
            .arg(pattern)
            .arg("-type")
            .arg("f")
            .output()
            .map_err(|e| format!("find failed to run: {}", e))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("find error: {}", stderr.trim()));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        if stdout.trim().is_empty() {
            return Ok("(no files found)".to_string());
        }

        // Strip source_dir prefix from results for readability
        let base = format!("{}/", self.source_dir.trim_end_matches('/'));
        let lines: Vec<&str> = stdout.lines().collect();

        const MAX_LINES: usize = 2000;
        let display_lines: Vec<String> = lines
            .iter()
            .take(MAX_LINES)
            .map(|l| {
                if l.starts_with(&base) {
                    l[base.len()..].to_string()
                } else {
                    l.to_string()
                }
            })
            .collect();

        let mut result = display_lines.join("\n");
        if lines.len() > MAX_LINES {
            result.push_str(&format!(
                "\n\n[truncated at {} entries, {} entries omitted]",
                MAX_LINES,
                lines.len() - MAX_LINES
            ));
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_parent_traversal() {
        let result = resolve_path("/tmp", "../etc/passwd");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("traversal"));
    }

    #[test]
    fn rejects_dotdot_in_middle() {
        let result = resolve_path("/tmp", "foo/../../etc/passwd");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("traversal"));
    }

    #[test]
    fn accepts_normal_path() {
        let result = resolve_path("/tmp", "foo/bar.txt");
        assert!(result.is_ok());
    }
}
