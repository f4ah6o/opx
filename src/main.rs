use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use directories::ProjectDirs;
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, SystemTime},
};

#[derive(Parser, Debug)]
#[command(author, version, about)]
#[command(args_conflicts_with_subcommands = true)]
struct Cli {
    /// Vault name (optional). If omitted, search all items and pick best match.
    #[arg(long, global = true)]
    vault: Option<String>,

    #[command(subcommand)]
    cmd: Option<Cmd>,

    /// Item title (when not using subcommand)
    #[arg(value_name = "ITEM")]
    item_title: Option<String>,

    /// Output env file path (optional, no file generated if omitted)
    #[arg(value_name = "ENV")]
    env_file: Option<PathBuf>,

    /// Command to run (after --)
    #[arg(last = true)]
    command: Vec<String>,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Find items by keyword (title contains)
    Find { query: String },

    /// Generate env file only (do not run command). Appends to existing file, overwrites duplicate keys.
    Gen {
        /// Item title
        #[arg(value_name = "ITEM")]
        item: String,

        /// Output env file path (optional, no file generated if omitted)
        #[arg(value_name = "ENV")]
        env_file: Option<PathBuf>,
    },

    /// Create a 1Password item from .env style file values
    Create {
        /// Item title to create
        #[arg(value_name = "ITEM")]
        item: String,

        /// Source env file path (defaults to .env)
        #[arg(value_name = "ENV")]
        env_file: Option<PathBuf>,
    },

    /// Run command with secrets from 1Password item
    Run {
        /// Item title
        #[arg(value_name = "ITEM")]
        item: String,

        /// Output env file path (optional, no file generated if omitted)
        #[arg(value_name = "ENV")]
        env_file: Option<PathBuf>,

        /// Command to run (after --)
        #[arg(last = true)]
        command: Vec<String>,
    },
}

#[derive(Deserialize, Serialize, Debug)]
struct ItemListEntry {
    id: String,
    title: String,
    #[serde(default)]
    vault: Option<ItemVault>,
}
#[derive(Deserialize, Serialize, Debug)]
struct ItemVault {
    id: String,
    name: String,
}

#[derive(Deserialize, Debug)]
struct ItemGet {
    #[serde(default)]
    fields: Vec<ItemField>,
    #[serde(default)]
    vault: Option<ItemVault>,
}
#[derive(Deserialize, Debug)]
struct ItemField {
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    value: Option<serde_json::Value>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match &cli.cmd {
        Some(Cmd::Find { query }) => {
            let items = item_list_cached(cli.vault.as_deref())?;
            let q = query.to_lowercase();
            for it in items
                .into_iter()
                .filter(|x| x.title.to_lowercase().contains(&q))
            {
                let vault = it.vault.as_ref().map(|v| v.name.as_str()).unwrap_or("-");
                println!("{}\t{}\t{}", it.id, vault, it.title);
            }
            Ok(())
        }
        Some(Cmd::Gen { item, env_file }) => generate_env_output(&cli, item, env_file.as_deref()),
        Some(Cmd::Create { item, env_file }) => {
            let env_path = env_file.as_deref().unwrap_or_else(|| Path::new(".env"));
            create_item_from_env(&cli, item, env_path)
        }
        Some(Cmd::Run {
            item,
            env_file,
            command,
        }) => {
            if command.is_empty() {
                return Err(anyhow!(
                    "Command required after '--'. Usage: opz run <ITEM> [ENV] -- <COMMAND>..."
                ));
            }
            run_with_item(&cli, item, env_file.as_deref(), command)
        }
        None => {
            let item_title = cli.item_title.as_ref().ok_or_else(|| {
                anyhow!("Item title required. Usage: opz [OPTIONS] <ITEM> [ENV] -- <COMMAND>...")
            })?;

            if cli.command.is_empty() {
                return Err(anyhow!(
                    "Command required after '--'. Usage: opz [OPTIONS] <ITEM> [ENV] -- <COMMAND>..."
                ));
            }
            run_with_item(&cli, item_title, cli.env_file.as_deref(), &cli.command)
        }
    }
}

fn create_item_from_env(cli: &Cli, item_title: &str, env_file: &Path) -> Result<()> {
    let env_pairs = parse_env_file(env_file)?;
    if env_pairs.is_empty() {
        return Err(anyhow!(
            "No valid env entries found in {}",
            env_file.display()
        ));
    }

    let args = build_create_item_args(cli.vault.as_deref(), item_title, &env_pairs);
    let mut cmd = Command::new("op");
    cmd.args(&args);

    let status = cmd
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("failed to run `op item create`")?;

    if !status.success() {
        return Err(anyhow!("op item create failed with status: {}", status));
    }

    Ok(())
}

fn build_create_item_args(
    vault: Option<&str>,
    item_title: &str,
    env_pairs: &[(String, String)],
) -> Vec<String> {
    let mut args = vec![
        "item".to_string(),
        "create".to_string(),
        "--category".to_string(),
        "API Credential".to_string(),
        "--title".to_string(),
        item_title.to_string(),
    ];

    if let Some(v) = vault {
        args.push("--vault".to_string());
        args.push(v.to_string());
    }

    // key[text]=value creates a custom text field where the field label is the key.
    for (key, value) in env_pairs {
        args.push(format!("{}[text]={}", key, value));
    }

    args
}

fn parse_env_file(path: &Path) -> Result<Vec<(String, String)>> {
    let content = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let label_re = Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*$")?;
    let mut pairs = Vec::new();

    for raw_line in content.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let normalized = match line.strip_prefix("export") {
            Some(rest) if rest.chars().next().is_some_and(char::is_whitespace) => rest.trim_start(),
            _ => line,
        };
        let Some((raw_key, raw_value)) = normalized.split_once('=') else {
            continue;
        };
        let key = raw_key.trim();
        if !label_re.is_match(key) {
            eprintln!("Skipped invalid key in env file: {key}");
            continue;
        }

        let value = normalize_env_value(raw_value);

        // Last occurrence wins for duplicate keys.
        if let Some(pos) = pairs
            .iter()
            .position(|(existing_key, _)| existing_key == key)
        {
            pairs.remove(pos);
        }

        pairs.push((key.to_string(), value));
    }

    Ok(pairs)
}

fn normalize_env_value(raw_value: &str) -> String {
    let mut value = strip_inline_comment(raw_value).trim().to_string();
    if value.len() >= 2
        && ((value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\'')))
    {
        value = value[1..value.len() - 1].to_string();
    }
    value
}

fn strip_inline_comment(value: &str) -> &str {
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut escaped_in_double = false;

    for (idx, ch) in value.char_indices() {
        if in_double_quote {
            if escaped_in_double {
                escaped_in_double = false;
                continue;
            }
            if ch == '\\' {
                escaped_in_double = true;
                continue;
            }
            if ch == '"' {
                in_double_quote = false;
            }
            continue;
        }

        if in_single_quote {
            if ch == '\'' {
                in_single_quote = false;
            }
            continue;
        }

        match ch {
            '"' => in_double_quote = true,
            '\'' => in_single_quote = true,
            '#' => {
                if idx == 0 || value[..idx].chars().last().is_some_and(char::is_whitespace) {
                    return value[..idx].trim_end();
                }
            }
            _ => {}
        }
    }

    value
}

/// Find and match item by title, returns (item_id, vault_id, item_title)
fn find_item(vault: Option<&str>, item_title: &str) -> Result<(String, String, String)> {
    let items = item_list_cached(vault)?;

    let mut matches: Vec<ItemListEntry> = items
        .into_iter()
        .filter(|x| x.title == item_title)
        .collect();

    // If exact match not found, fallback to contains (simple fuzzy)
    if matches.is_empty() {
        let q = item_title.to_lowercase();
        matches = item_list_cached(vault)?
            .into_iter()
            .filter(|x| x.title.to_lowercase().contains(&q))
            .collect();
    }

    if matches.is_empty() {
        return Err(anyhow!("No item matched title: {}", item_title));
    }
    if matches.len() > 1 {
        eprintln!("Ambiguous item title. Candidates:");
        for it in matches.iter().take(20) {
            let vault = it.vault.as_ref().map(|v| v.name.as_str()).unwrap_or("-");
            eprintln!("  {}  [{}]  {}", it.id, vault, it.title);
        }
        return Err(anyhow!(
            "Please be more specific or use `opz find <query>` and pass exact title."
        ));
    }

    let item_id = matches[0].id.clone();
    let item = item_get(&item_id)?;
    let vault_id = resolve_vault_id(
        matches.first().and_then(|m| m.vault.as_ref()),
        item.vault.as_ref(),
    )
    .ok_or_else(|| anyhow!("Vault ID is required. Try specifying --vault."))?;

    Ok((item_id, vault_id, matches[0].title.clone()))
}

fn resolve_vault_id(
    list_vault: Option<&ItemVault>,
    item_vault: Option<&ItemVault>,
) -> Option<String> {
    list_vault.or(item_vault).map(|v| v.id.clone())
}

fn generate_env_output(cli: &Cli, item_title: &str, env_file: Option<&Path>) -> Result<()> {
    let (item_id, vault_id, _) = find_item(cli.vault.as_deref(), item_title)?;
    let item = item_get(&item_id)?;
    let env_lines = item_to_env_lines(&item, &vault_id, &item_id)?;

    if let Some(path) = env_file {
        write_env_file(path, &env_lines)?;
        eprintln!("Generated: {}", path.display());
    } else {
        // 標準出力に出力
        for line in env_lines {
            println!("{}", line);
        }
    }
    Ok(())
}

/// Expand $VAR and ${VAR} references in a string using provided environment variables.
/// Only expands variables that exist in the provided map; others are left as-is
/// (e.g., $HOME, $PATH).
fn expand_vars(s: &str, env_vars: &HashMap<String, String>) -> String {
    let mut result = String::with_capacity(s.len() * 2);
    let mut chars = s.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '$' {
            // Try to parse ${VAR} or $VAR
            let mut var_name = String::new();
            let mut is_braced = false;

            if chars.peek() == Some(&'{') {
                is_braced = true;
                chars.next(); // consume '{'
            }

            // Collect variable name (ASCII alphanumeric + underscore only)
            // This matches shell variable naming rules
            while let Some(&next) = chars.peek() {
                match next {
                    'a'..='z' | 'A'..='Z' | '0'..='9' | '_' => {
                        var_name.push(chars.next().unwrap());
                    }
                    _ => break,
                }
            }

            if is_braced {
                if chars.peek() == Some(&'}') {
                    chars.next(); // consume '}'
                } else {
                    // Invalid ${ syntax, treat as literal
                    result.push_str("$\\{");
                    result.push_str(&var_name);
                    continue;
                }
            }

            // Look up the variable and replace, or keep original literal form
            if let Some(value) = env_vars.get(&var_name) {
                result.push_str(value);
            } else {
                // Variable not found in our env, keep $VAR as-is
                result.push('$');
                result.push_str(&var_name);
            }
        } else {
            result.push(c);
        }
    }

    result
}

fn run_with_item(
    cli: &Cli,
    item_title: &str,
    env_file: Option<&Path>,
    command: &[String],
) -> Result<()> {
    let (item_id, vault_id, _) = find_item(cli.vault.as_deref(), item_title)?;
    let item = item_get(&item_id)?;
    let env_lines = item_to_env_lines(&item, &vault_id, &item_id)?;

    if let Some(path) = env_file {
        write_env_file(path, &env_lines)?;
        eprintln!("Generated: {}", path.display());
    }

    // First pass: collect all environment variable values
    let mut env_vars: HashMap<String, String> = HashMap::new();
    for line in &env_lines {
        if let Some((key, reference)) = parse_env_line_kv(line) {
            let value = op_read(&reference)?;
            env_vars.insert(key.to_string(), value);
        }
    }

    // Second pass: expand $VAR references in command arguments
    let expanded_args: Vec<String> = command
        .iter()
        .map(|arg| expand_vars(arg, &env_vars))
        .collect();

    let mut cmd = Command::new("sh");
    cmd.arg("-c");
    cmd.arg("exec \"$@\"");
    cmd.arg("sh");
    cmd.args(&expanded_args);

    // Set environment variables for the child process
    for (key, value) in &env_vars {
        cmd.env(key, value);
    }

    let status = cmd
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("failed to run command")?;

    if !status.success() {
        return Err(anyhow!("command failed with status: {}", status));
    }
    Ok(())
}

fn item_to_env_lines(item: &ItemGet, vault_id: &str, item_id: &str) -> Result<Vec<String>> {
    let re = Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*$")?;
    let mut out = Vec::new();

    for f in &item.fields {
        let Some(label) = f.label.as_ref() else {
            continue;
        };
        if !re.is_match(label) {
            // env var invalid -> skip
            continue;
        }
        // Skip fields without value
        if f.value.is_none() {
            continue;
        }

        let reference = format!("op://{}/{}/{}", vault_id, item_id, label);
        out.push(format!("{k}={v}", k = label, v = reference));
    }

    Ok(out)
}

/// Parse env line to extract key name (e.g., "KEY=value" -> "KEY")
fn parse_env_key(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }
    trimmed.split('=').next()
}

/// Parse env line to extract key and value (e.g., "KEY=value" -> ("KEY", "value"))
fn parse_env_line_kv(line: &str) -> Option<(&str, &str)> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }
    let mut parts = trimmed.splitn(2, '=');
    let key = parts.next()?;
    let value = parts.next()?;
    Some((key, value))
}

/// Read a secret from 1Password using op read
fn op_read(reference: &str) -> Result<String> {
    let out = Command::new("op")
        .arg("read")
        .arg(reference)
        .output()
        .context("failed to run `op read`")?;

    if !out.status.success() {
        return Err(anyhow!(
            "op read failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }

    Ok(String::from_utf8(out.stdout)?.trim().to_string())
}

fn write_env_file(path: &Path, new_lines: &[String]) -> Result<()> {
    use std::collections::HashMap;

    // Build a map of new keys for quick lookup
    let new_keys: HashMap<String, &str> = new_lines
        .iter()
        .filter_map(|line| parse_env_key(line).map(|key| (key.to_string(), line.as_str())))
        .collect();

    let mut result_lines: Vec<String> = Vec::new();
    let mut written_keys: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Read existing file and merge
    if path.exists() {
        let content =
            fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;

        for line in content.lines() {
            if let Some(key) = parse_env_key(line) {
                if let Some(&new_line) = new_keys.get(key) {
                    // Overwrite with new value
                    result_lines.push(new_line.to_string());
                    written_keys.insert(key.to_string());
                } else {
                    // Keep existing line
                    result_lines.push(line.to_string());
                }
            } else {
                // Comment or empty line - keep as is
                result_lines.push(line.to_string());
            }
        }
    }

    // Append new keys that weren't already in the file
    for line in new_lines {
        if let Some(key) = parse_env_key(line) {
            if !written_keys.contains(key) {
                result_lines.push(line.clone());
            }
        }
    }

    // Write result
    let mut f = fs::File::create(path).with_context(|| format!("create {}", path.display()))?;
    for line in &result_lines {
        writeln!(f, "{line}")?;
    }
    Ok(())
}

fn op_json(args: &[&str]) -> Result<serde_json::Value> {
    let out = Command::new("op")
        .args(args)
        .output()
        .with_context(|| format!("failed to run op {}", args.join(" ")))?;

    if !out.status.success() {
        return Err(anyhow!(
            "op error ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        ));
    }

    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).context("failed to parse op JSON output")?;
    Ok(v)
}

/// Cache `op item list --format json` to speed up repeated runs.
fn item_list_cached(vault: Option<&str>) -> Result<Vec<ItemListEntry>> {
    let cache_path = cache_file_path(vault)?;
    let ttl = Duration::from_secs(60); // 60秒程度で十分（好みで調整）

    if let Ok(meta) = fs::metadata(&cache_path) {
        if let Ok(mtime) = meta.modified() {
            if SystemTime::now().duration_since(mtime).unwrap_or_default() < ttl {
                let bytes = fs::read(&cache_path)?;
                let items: Vec<ItemListEntry> = serde_json::from_slice(&bytes)?;
                return Ok(items);
            }
        }
    }

    let mut args = vec!["item", "list", "--format", "json"];
    if let Some(v) = vault {
        // `op item list --vault <name>` が使える環境想定（未対応なら削る）
        args.push("--vault");
        args.push(v);
    }

    let v = op_json(&args)?;
    let items: Vec<ItemListEntry> = serde_json::from_value(v)?;
    fs::create_dir_all(cache_path.parent().unwrap())?;
    fs::write(&cache_path, serde_json::to_vec(&items)?)?;
    Ok(items)
}

fn cache_file_path(vault: Option<&str>) -> Result<PathBuf> {
    let proj = ProjectDirs::from("dev", "opz", "opz").ok_or_else(|| anyhow!("no cache dir"))?;
    let base = proj.cache_dir().to_path_buf();
    let key = vault.unwrap_or("_all_");
    let mut hasher = Sha256::new();
    hasher.update(key.as_bytes());
    let name = format!("item_list_{}.json", hex::encode(hasher.finalize()));
    Ok(base.join(name))
}

fn item_get(item_id: &str) -> Result<ItemGet> {
    let v = op_json(&["item", "get", item_id, "--format", "json"])?;
    let item: ItemGet = serde_json::from_value(v)?;
    Ok(item)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    // ============================================
    // Tests for item_to_env_lines()
    // ============================================

    fn make_field(label: Option<&str>, has_value: bool) -> ItemField {
        ItemField {
            label: label.map(String::from),
            value: if has_value {
                Some(serde_json::Value::String("test".to_string()))
            } else {
                None
            },
        }
    }

    fn make_item(fields: Vec<ItemField>) -> ItemGet {
        ItemGet {
            fields,
            vault: None,
        }
    }

    fn env_lines(item: &ItemGet) -> Vec<String> {
        item_to_env_lines(item, "vault-id", "abc123").unwrap()
    }

    #[test]
    fn test_item_to_env_lines_basic() {
        let item = make_item(vec![
            make_field(Some("API_KEY"), true),
            make_field(Some("DB_HOST"), true),
        ]);
        let lines = env_lines(&item);
        assert_eq!(lines.len(), 2);
        assert!(lines.contains(&"API_KEY=op://vault-id/abc123/API_KEY".to_string()));
        assert!(lines.contains(&"DB_HOST=op://vault-id/abc123/DB_HOST".to_string()));
    }

    #[test]
    fn test_item_to_env_lines_skips_invalid_labels() {
        let item = make_item(vec![
            make_field(Some("VALID_KEY"), true),
            make_field(Some("invalid-key"), true), // dash not allowed
            make_field(Some("123_START"), true),   // can't start with number
            make_field(Some("has space"), true),   // space not allowed
        ]);
        let lines = env_lines(&item);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0], "VALID_KEY=op://vault-id/abc123/VALID_KEY");
    }

    #[test]
    fn test_item_to_env_lines_valid_label_patterns() {
        let item = make_item(vec![
            make_field(Some("_UNDERSCORE_START"), true),
            make_field(Some("lowercase"), true),
            make_field(Some("MixedCase123"), true),
            make_field(Some("WITH_123_NUMBERS"), true),
        ]);
        let lines = env_lines(&item);
        assert_eq!(lines.len(), 4);
    }

    #[test]
    fn test_item_to_env_lines_skips_no_label() {
        let item = make_item(vec![
            make_field(None, true),
            make_field(Some("VALID"), true),
        ]);
        let lines = env_lines(&item);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0], "VALID=op://vault-id/abc123/VALID");
    }

    #[test]
    fn test_item_to_env_lines_empty_fields() {
        let item = make_item(vec![]);
        let lines = env_lines(&item);
        assert!(lines.is_empty());
    }

    #[test]
    fn test_item_to_env_lines_skips_no_value() {
        let item = make_item(vec![
            make_field(Some("NO_VALUE"), false),
            make_field(Some("HAS_VALUE"), true),
        ]);
        let lines = env_lines(&item);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0], "HAS_VALUE=op://vault-id/abc123/HAS_VALUE");
    }

    #[test]
    fn test_resolve_vault_id_prefers_id_even_with_unicode_name() {
        let list_vault = ItemVault {
            id: "vault-123".to_string(),
            name: "情報管理共有".to_string(),
        };
        let item_vault = ItemVault {
            id: "vault-fallback".to_string(),
            name: "別名".to_string(),
        };

        let resolved = resolve_vault_id(Some(&list_vault), Some(&item_vault));
        assert_eq!(resolved.as_deref(), Some("vault-123"));
    }

    // ============================================
    // Tests for parse_env_key()
    // ============================================

    #[test]
    fn test_parse_env_key_basic() {
        assert_eq!(parse_env_key("KEY=value"), Some("KEY"));
        assert_eq!(parse_env_key("FOO_BAR=baz"), Some("FOO_BAR"));
    }

    #[test]
    fn test_parse_env_key_with_quotes() {
        assert_eq!(parse_env_key(r#"KEY="value""#), Some("KEY"));
    }

    #[test]
    fn test_parse_env_key_comments_and_empty() {
        assert_eq!(parse_env_key("# comment"), None);
        assert_eq!(parse_env_key(""), None);
        assert_eq!(parse_env_key("   "), None);
        assert_eq!(parse_env_key("  # indented comment"), None);
    }

    // ============================================
    // Tests for write_env_file()
    // ============================================

    #[test]
    fn test_write_env_file_creates_file() {
        let tmp_dir = TempDir::new().unwrap();
        let file_path = tmp_dir.path().join(".env");

        let lines = vec![
            r#"KEY1="value1""#.to_string(),
            r#"KEY2="value2""#.to_string(),
        ];

        write_env_file(&file_path, &lines).unwrap();

        assert!(file_path.exists());
        let content = fs::read_to_string(&file_path).unwrap();
        assert!(content.contains(r#"KEY1="value1""#));
        assert!(content.contains(r#"KEY2="value2""#));
    }

    #[test]
    fn test_write_env_file_with_newlines() {
        let tmp_dir = TempDir::new().unwrap();
        let file_path = tmp_dir.path().join(".env");

        let lines = vec![r#"MULTI="line1\nline2""#.to_string()];

        write_env_file(&file_path, &lines).unwrap();

        let content = fs::read_to_string(&file_path).unwrap();
        assert!(content.contains(r#"MULTI="line1\nline2""#));
    }

    #[test]
    fn test_write_env_file_empty_lines() {
        let tmp_dir = TempDir::new().unwrap();
        let file_path = tmp_dir.path().join(".env");

        let lines: Vec<String> = vec![];
        write_env_file(&file_path, &lines).unwrap();

        let content = fs::read_to_string(&file_path).unwrap();
        assert!(content.is_empty());
    }

    #[test]
    fn test_write_env_file_appends_new_keys() {
        let tmp_dir = TempDir::new().unwrap();
        let file_path = tmp_dir.path().join(".env");

        // Write initial content
        fs::write(&file_path, "OLD_KEY=old_value\n").unwrap();

        // Append with new content
        let lines = vec![r#"NEW_KEY="new_value""#.to_string()];
        write_env_file(&file_path, &lines).unwrap();

        let content = fs::read_to_string(&file_path).unwrap();
        assert!(content.contains("OLD_KEY=old_value"));
        assert!(content.contains(r#"NEW_KEY="new_value""#));
    }

    #[test]
    fn test_write_env_file_overwrites_duplicates() {
        let tmp_dir = TempDir::new().unwrap();
        let file_path = tmp_dir.path().join(".env");

        // Write initial content with a key we'll overwrite
        fs::write(&file_path, "API_KEY=old_secret\nOTHER_KEY=keep_me\n").unwrap();

        // Overwrite API_KEY
        let lines = vec![r#"API_KEY="new_secret""#.to_string()];
        write_env_file(&file_path, &lines).unwrap();

        let content = fs::read_to_string(&file_path).unwrap();
        // Should have new value, not old
        assert!(content.contains(r#"API_KEY="new_secret""#));
        assert!(!content.contains("API_KEY=old_secret"));
        // Other key should be preserved
        assert!(content.contains("OTHER_KEY=keep_me"));
    }

    #[test]
    fn test_write_env_file_preserves_comments() {
        let tmp_dir = TempDir::new().unwrap();
        let file_path = tmp_dir.path().join(".env");

        // Write initial content with comments
        fs::write(
            &file_path,
            "# This is a comment\nKEY1=value1\n\n# Another comment\n",
        )
        .unwrap();

        // Add new key
        let lines = vec![r#"KEY2="value2""#.to_string()];
        write_env_file(&file_path, &lines).unwrap();

        let content = fs::read_to_string(&file_path).unwrap();
        assert!(content.contains("# This is a comment"));
        assert!(content.contains("# Another comment"));
        assert!(content.contains("KEY1=value1"));
        assert!(content.contains(r#"KEY2="value2""#));
    }

    #[test]
    fn test_write_env_file_mixed_overwrite_and_append() {
        let tmp_dir = TempDir::new().unwrap();
        let file_path = tmp_dir.path().join(".env");

        // Initial content
        fs::write(&file_path, "KEY1=original1\nKEY2=original2\n").unwrap();

        // Overwrite KEY1 and add KEY3
        let lines = vec![
            r#"KEY1="updated1""#.to_string(),
            r#"KEY3="new3""#.to_string(),
        ];
        write_env_file(&file_path, &lines).unwrap();

        let content = fs::read_to_string(&file_path).unwrap();
        let content_lines: Vec<&str> = content.lines().collect();

        // KEY1 should be updated (in its original position)
        assert!(content_lines[0].contains(r#"KEY1="updated1""#));
        // KEY2 should be preserved
        assert!(content_lines[1].contains("KEY2=original2"));
        // KEY3 should be appended
        assert!(content_lines[2].contains(r#"KEY3="new3""#));
    }

    // ============================================
    // Tests for cache_file_path()
    // ============================================

    #[test]
    fn test_cache_file_path_with_vault() {
        let path1 = cache_file_path(Some("my-vault")).unwrap();
        let path2 = cache_file_path(Some("other-vault")).unwrap();

        // Different vaults should produce different paths
        assert_ne!(path1, path2);

        // Path should end with .json
        assert!(path1.extension().unwrap() == "json");
        assert!(path2.extension().unwrap() == "json");

        // Filename should start with item_list_
        let name1 = path1.file_name().unwrap().to_str().unwrap();
        assert!(name1.starts_with("item_list_"));
    }

    #[test]
    fn test_cache_file_path_without_vault() {
        let path = cache_file_path(None).unwrap();

        // Should produce a valid path
        assert!(path.extension().unwrap() == "json");

        let name = path.file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with("item_list_"));
    }

    #[test]
    fn test_cache_file_path_deterministic() {
        // Same input should produce same output
        let path1 = cache_file_path(Some("test-vault")).unwrap();
        let path2 = cache_file_path(Some("test-vault")).unwrap();
        assert_eq!(path1, path2);

        let path3 = cache_file_path(None).unwrap();
        let path4 = cache_file_path(None).unwrap();
        assert_eq!(path3, path4);
    }

    // ============================================
    // Tests for ItemListEntry and ItemGet deserialization
    // ============================================

    #[test]
    fn test_item_list_entry_deserialization() {
        let json =
            r#"{"id": "abc123", "title": "My Item", "vault": {"id": "v1", "name": "Personal"}}"#;
        let item: ItemListEntry = serde_json::from_str(json).unwrap();
        assert_eq!(item.id, "abc123");
        assert_eq!(item.title, "My Item");
        assert!(item.vault.is_some());
        assert_eq!(item.vault.as_ref().unwrap().name, "Personal");
    }

    #[test]
    fn test_item_list_entry_without_vault() {
        let json = r#"{"id": "abc123", "title": "My Item"}"#;
        let item: ItemListEntry = serde_json::from_str(json).unwrap();
        assert_eq!(item.id, "abc123");
        assert_eq!(item.title, "My Item");
        assert!(item.vault.is_none());
    }

    #[test]
    fn test_item_get_deserialization() {
        let json = r#"{
            "fields": [
                {"label": "username", "value": "user@example.com"},
                {"label": "password", "value": "secret"}
            ]
        }"#;
        let item: ItemGet = serde_json::from_str(json).unwrap();
        assert_eq!(item.fields.len(), 2);
        assert_eq!(item.fields[0].label, Some("username".to_string()));
    }

    #[test]
    fn test_item_get_empty_fields() {
        let json = r#"{}"#;
        let item: ItemGet = serde_json::from_str(json).unwrap();
        assert!(item.fields.is_empty());
    }

    #[test]
    fn test_item_field_with_null_value() {
        // Unknown fields (like "value") are ignored during deserialization
        let json = r#"{"label": "empty_field", "value": null}"#;
        let field: ItemField = serde_json::from_str(json).unwrap();
        assert_eq!(field.label, Some("empty_field".to_string()));
    }

    #[test]
    fn test_item_field_missing_value() {
        let json = r#"{"label": "no_value_field"}"#;
        let field: ItemField = serde_json::from_str(json).unwrap();
        assert_eq!(field.label, Some("no_value_field".to_string()));
    }

    // ============================================
    // Tests for parse_env_file()
    // ============================================

    #[test]
    fn test_parse_env_file_basic() {
        let tmp_dir = TempDir::new().unwrap();
        let file_path = tmp_dir.path().join(".env");
        fs::write(&file_path, "API_KEY=secret\nDB_HOST=localhost\n").unwrap();

        let pairs = parse_env_file(&file_path).unwrap();
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0], ("API_KEY".to_string(), "secret".to_string()));
        assert_eq!(pairs[1], ("DB_HOST".to_string(), "localhost".to_string()));
    }

    #[test]
    fn test_parse_env_file_handles_comments_export_and_quotes() {
        let tmp_dir = TempDir::new().unwrap();
        let file_path = tmp_dir.path().join(".env");
        fs::write(
            &file_path,
            r#"# comment
export TOKEN=abc
QUOTED="hello"
SINGLE='world'
"#,
        )
        .unwrap();

        let pairs = parse_env_file(&file_path).unwrap();
        assert_eq!(pairs.len(), 3);
        assert_eq!(pairs[0], ("TOKEN".to_string(), "abc".to_string()));
        assert_eq!(pairs[1], ("QUOTED".to_string(), "hello".to_string()));
        assert_eq!(pairs[2], ("SINGLE".to_string(), "world".to_string()));
    }

    #[test]
    fn test_parse_env_file_skips_invalid_keys() {
        let tmp_dir = TempDir::new().unwrap();
        let file_path = tmp_dir.path().join(".env");
        fs::write(
            &file_path,
            "VALID=value\nINVALID-KEY=value\n1INVALID=value\n",
        )
        .unwrap();

        let pairs = parse_env_file(&file_path).unwrap();
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0], ("VALID".to_string(), "value".to_string()));
    }

    #[test]
    fn test_parse_env_file_supports_inline_comments_and_hash_in_quotes() {
        let tmp_dir = TempDir::new().unwrap();
        let file_path = tmp_dir.path().join(".env");
        fs::write(
            &file_path,
            r#"PLAIN=value # comment
NO_COMMENT=value#hash
DOUBLE="value # kept"
SINGLE='value # kept'
"#,
        )
        .unwrap();

        let pairs = parse_env_file(&file_path).unwrap();
        assert_eq!(pairs.len(), 4);
        assert_eq!(pairs[0], ("PLAIN".to_string(), "value".to_string()));
        assert_eq!(
            pairs[1],
            ("NO_COMMENT".to_string(), "value#hash".to_string())
        );
        assert_eq!(pairs[2], ("DOUBLE".to_string(), "value # kept".to_string()));
        assert_eq!(pairs[3], ("SINGLE".to_string(), "value # kept".to_string()));
    }

    #[test]
    fn test_parse_env_file_allows_export_with_multiple_spaces() {
        let tmp_dir = TempDir::new().unwrap();
        let file_path = tmp_dir.path().join(".env");
        fs::write(&file_path, "export   TOKEN=abc\n").unwrap();

        let pairs = parse_env_file(&file_path).unwrap();
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0], ("TOKEN".to_string(), "abc".to_string()));
    }

    #[test]
    fn test_parse_env_file_duplicate_keys_last_wins() {
        let tmp_dir = TempDir::new().unwrap();
        let file_path = tmp_dir.path().join(".env");
        fs::write(&file_path, "A=first\nB=keep\nA=last\n").unwrap();

        let pairs = parse_env_file(&file_path).unwrap();
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0], ("B".to_string(), "keep".to_string()));
        assert_eq!(pairs[1], ("A".to_string(), "last".to_string()));
    }

    #[test]
    fn test_build_create_item_args_uses_api_credential_category_and_text_fields() {
        let env_pairs = vec![
            ("API_KEY".to_string(), "secret".to_string()),
            ("DB_HOST".to_string(), "localhost".to_string()),
        ];

        let args = build_create_item_args(Some("Private"), "my-item", &env_pairs);

        assert_eq!(args[0], "item");
        assert_eq!(args[1], "create");
        assert!(args.contains(&"--category".to_string()));
        assert!(args.contains(&"API Credential".to_string()));
        assert!(args.contains(&"--title".to_string()));
        assert!(args.contains(&"my-item".to_string()));
        assert!(args.contains(&"--vault".to_string()));
        assert!(args.contains(&"Private".to_string()));
        assert!(args.contains(&"API_KEY[text]=secret".to_string()));
        assert!(args.contains(&"DB_HOST[text]=localhost".to_string()));
    }

    // ============================================
    // Tests for expand_vars()
    // ============================================

    #[test]
    fn test_expand_vars_simple() {
        let mut env = HashMap::new();
        env.insert("API_TOKEN".to_string(), "secret123".to_string());
        assert_eq!(expand_vars("Bearer $API_TOKEN", &env), "Bearer secret123");
    }

    #[test]
    fn test_expand_vars_braced() {
        let mut env = HashMap::new();
        env.insert("HOST".to_string(), "example.com".to_string());
        assert_eq!(
            expand_vars("https://${HOST}/api", &env),
            "https://example.com/api"
        );
    }

    #[test]
    fn test_expand_vars_multiple() {
        let mut env = HashMap::new();
        env.insert("USER".to_string(), "alice".to_string());
        env.insert("HOST".to_string(), "server.com".to_string());
        assert_eq!(expand_vars("$USER@$HOST", &env), "alice@server.com");
    }

    #[test]
    fn test_expand_vars_unknown_var() {
        let env = HashMap::new();
        // Unknown vars should be preserved as-is
        assert_eq!(expand_vars("$HOME/dir", &env), "$HOME/dir");
        assert_eq!(expand_vars("$PATH", &env), "$PATH");
    }

    #[test]
    fn test_expand_vars_mixed_known_unknown() {
        let mut env = HashMap::new();
        env.insert("API_TOKEN".to_string(), "secret".to_string());
        assert_eq!(
            expand_vars("Authorization: $API_TOKEN for $HOME", &env),
            "Authorization: secret for $HOME"
        );
    }

    #[test]
    fn test_expand_vars_with_special_chars() {
        let mut env = HashMap::new();
        env.insert("TOKEN".to_string(), "a$b\"c`d".to_string());
        let result = expand_vars("$TOKEN", &env);
        assert_eq!(result, r#"a$b"c`d"#);
    }

    #[test]
    fn test_expand_vars_empty_value() {
        let mut env = HashMap::new();
        env.insert("EMPTY".to_string(), "".to_string());
        // $EMPTYsuffix looks for "EMPTYsuffix" variable, not "EMPTY"
        // Since EMPTYsuffix doesn't exist, it remains as-is for shell expansion
        assert_eq!(
            expand_vars("prefix$EMPTYsuffix", &env),
            "prefix$EMPTYsuffix"
        );
        // Use ${EMPTY} to explicitly mark variable boundaries
        assert_eq!(expand_vars("prefix${EMPTY}suffix", &env), "prefixsuffix");
        // Direct usage should expand to empty string
        assert_eq!(expand_vars("$EMPTY", &env), "");
    }

    #[test]
    fn test_expand_vars_partial_name() {
        let mut env = HashMap::new();
        env.insert("API".to_string(), "test".to_string());
        // $API_TOKEN looks for "API_TOKEN" variable, not "API"
        // Since API_TOKEN doesn't exist, it remains as-is
        assert_eq!(expand_vars("$API_TOKEN", &env), "$API_TOKEN");
    }

    #[test]
    fn test_expand_vars_no_vars() {
        let env = HashMap::new();
        assert_eq!(expand_vars("hello world", &env), "hello world");
    }

    #[test]
    fn test_expand_vars_consecutive_dollars() {
        let mut env = HashMap::new();
        env.insert("A".to_string(), "1".to_string());
        env.insert("B".to_string(), "2".to_string());
        assert_eq!(expand_vars("$A$B", &env), "12");
    }

    #[test]
    fn test_expand_vars_underscore_in_name() {
        let mut env = HashMap::new();
        env.insert("API_TOKEN".to_string(), "secret".to_string());
        assert_eq!(expand_vars("$API_TOKEN", &env), "secret");
        assert_eq!(expand_vars("${API_TOKEN}", &env), "secret");
    }
}
