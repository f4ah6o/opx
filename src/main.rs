use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use directories::ProjectDirs;
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Seek, SeekFrom, Write},
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

    /// Output env file path (default: .env in current dir)
    #[arg(
        long = "env-file",
        alias = "out",
        global = true,
        default_value = ".env"
    )]
    env_file: PathBuf,

    /// Keep the generated env file
    #[arg(long, global = true)]
    keep: bool,

    #[command(subcommand)]
    cmd: Option<Cmd>,

    /// Item title (when not using 'find' subcommand)
    #[arg(value_name = "ITEM")]
    item_title: Option<String>,

    /// Command to run (after --)
    #[arg(last = true)]
    command: Vec<String>,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Find items by keyword (title contains)
    Find { query: String },
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
        None => {
            // Default: run mode
            let item_title = cli.item_title.as_ref().ok_or_else(|| {
                anyhow!("Item title required. Usage: opz [OPTIONS] <ITEM> -- <COMMAND>...")
            })?;
            if cli.command.is_empty() {
                return Err(anyhow!(
                    "Command required after '--'. Usage: opz [OPTIONS] <ITEM> -- <COMMAND>..."
                ));
            }
            run_with_item(&cli, item_title, &cli.command)
        }
    }
}

fn run_with_item(cli: &Cli, item_title: &str, command: &[String]) -> Result<()> {
    let items = item_list_cached(cli.vault.as_deref())?;

    let mut matches: Vec<ItemListEntry> = items
        .into_iter()
        .filter(|x| x.title == item_title)
        .collect();

    // If exact match not found, fallback to contains (simple fuzzy)
    if matches.is_empty() {
        let q = item_title.to_lowercase();
        matches = item_list_cached(cli.vault.as_deref())?
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

    let item_id = &matches[0].id;
    let item = item_get(item_id)?;
    let vault_name = matches
        .get(0)
        .and_then(|m| m.vault.as_ref())
        .or_else(|| item.vault.as_ref())
        .map(|v| v.name.clone())
        .ok_or_else(|| anyhow!("Vault name is required. Try specifying --vault."))?;

    let env_lines = item_to_env_lines(&item, &vault_name, &matches[0].title)?;

    let existing_env_content = if cli.env_file.exists() {
        Some(
            fs::read(&cli.env_file)
                .with_context(|| format!("failed to read {}", cli.env_file.display()))?,
        )
    } else {
        None
    };

    write_env_file(&cli.env_file, &env_lines)?;
    let status = Command::new("op")
        .arg("run")
        .arg(format!("--env-file={}", cli.env_file.display()))
        .arg("--")
        .args(command)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("failed to run `op run`")?;

    if !cli.keep {
        match existing_env_content {
            Some(original) => {
                let _ = fs::write(&cli.env_file, original);
            }
            None => {
                let _ = fs::remove_file(&cli.env_file);
            }
        }
    }

    if !status.success() {
        return Err(anyhow!("command failed with status: {}", status));
    }
    Ok(())
}

fn item_to_env_lines(item: &ItemGet, vault_name: &str, item_title: &str) -> Result<Vec<String>> {
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

        let reference = format!("op://{}/{}/{}", vault_name, item_title, label);

        // .env safe quoting
        out.push(format!(
            r#"{k}="{v}""#,
            k = label,
            v = escape_env_value(&reference)
        ));
    }

    Ok(out)
}

fn escape_env_value(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

fn write_env_file(path: &Path, lines: &[String]) -> Result<()> {
    let mut f = if path.exists() {
        let mut f = fs::OpenOptions::new()
            .read(true)
            .append(true)
            .open(path)
            .with_context(|| format!("open {}", path.display()))?;

        // Ensure we start appending on a new line
        let needs_newline = {
            let meta = f.metadata()?;
            if meta.len() == 0 {
                false
            } else {
                f.seek(SeekFrom::End(-1))?;
                let mut buf = [0u8; 1];
                f.read_exact(&mut buf)?;
                buf[0] != b'\n'
            }
        };

        if needs_newline {
            writeln!(f)?;
        }

        f
    } else {
        fs::File::create(path).with_context(|| format!("create {}", path.display()))?
    };

    for l in lines {
        writeln!(f, "{l}")?;
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
    // Tests for escape_env_value()
    // ============================================

    #[test]
    fn test_escape_env_value_plain_text() {
        assert_eq!(escape_env_value("hello"), "hello");
        assert_eq!(escape_env_value("simple text"), "simple text");
    }

    #[test]
    fn test_escape_env_value_with_backslash() {
        assert_eq!(escape_env_value(r"path\to\file"), r"path\\to\\file");
        assert_eq!(escape_env_value(r"\\server\share"), r"\\\\server\\share");
    }

    #[test]
    fn test_escape_env_value_with_quotes() {
        assert_eq!(escape_env_value(r#"say "hello""#), r#"say \"hello\""#);
        assert_eq!(escape_env_value(r#""""#), r#"\"\""#); // two quotes -> two escaped quotes
    }

    #[test]
    fn test_escape_env_value_with_newlines() {
        assert_eq!(escape_env_value("line1\nline2"), r"line1\nline2");
        assert_eq!(escape_env_value("line1\r\nline2"), r"line1\r\nline2");
    }

    #[test]
    fn test_escape_env_value_combined() {
        // Test multiple escape characters together
        assert_eq!(
            escape_env_value("path\\to\n\"file\""),
            r#"path\\to\n\"file\""#
        );
    }

    #[test]
    fn test_escape_env_value_empty() {
        assert_eq!(escape_env_value(""), "");
    }

    // ============================================
    // Tests for item_to_env_lines()
    // ============================================

    fn make_field(label: Option<&str>) -> ItemField {
        ItemField {
            label: label.map(String::from),
        }
    }

    fn make_item(fields: Vec<ItemField>) -> ItemGet {
        ItemGet {
            fields,
            vault: None,
        }
    }

    fn env_lines(item: &ItemGet) -> Vec<String> {
        item_to_env_lines(item, "Vault", "Item").unwrap()
    }

    #[test]
    fn test_item_to_env_lines_basic() {
        let item = make_item(vec![
            make_field(Some("API_KEY")),
            make_field(Some("DB_HOST")),
        ]);
        let lines = env_lines(&item);
        assert_eq!(lines.len(), 2);
        assert!(lines.contains(&r#"API_KEY="op://Vault/Item/API_KEY""#.to_string()));
        assert!(lines.contains(&r#"DB_HOST="op://Vault/Item/DB_HOST""#.to_string()));
    }

    #[test]
    fn test_item_to_env_lines_skips_invalid_labels() {
        let item = make_item(vec![
            make_field(Some("VALID_KEY")),
            make_field(Some("invalid-key")), // dash not allowed
            make_field(Some("123_START")),   // can't start with number
            make_field(Some("has space")),   // space not allowed
        ]);
        let lines = env_lines(&item);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0], r#"VALID_KEY="op://Vault/Item/VALID_KEY""#);
    }

    #[test]
    fn test_item_to_env_lines_valid_label_patterns() {
        let item = make_item(vec![
            make_field(Some("_UNDERSCORE_START")),
            make_field(Some("lowercase")),
            make_field(Some("MixedCase123")),
            make_field(Some("WITH_123_NUMBERS")),
        ]);
        let lines = env_lines(&item);
        assert_eq!(lines.len(), 4);
    }

    #[test]
    fn test_item_to_env_lines_skips_no_label() {
        let item = make_item(vec![make_field(None), make_field(Some("VALID"))]);
        let lines = env_lines(&item);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0], r#"VALID="op://Vault/Item/VALID""#);
    }

    #[test]
    fn test_item_to_env_lines_empty_fields() {
        let item = make_item(vec![]);
        let lines = env_lines(&item);
        assert!(lines.is_empty());
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
    fn test_write_env_file_appends_existing() {
        let tmp_dir = TempDir::new().unwrap();
        let file_path = tmp_dir.path().join(".env");

        // Write initial content without trailing newline
        fs::write(&file_path, "OLD_CONTENT").unwrap();

        // Append with new content, ensuring newline is added automatically
        let lines = vec![r#"NEW_KEY="new_value""#.to_string()];
        write_env_file(&file_path, &lines).unwrap();

        let content = fs::read_to_string(&file_path).unwrap();
        assert!(content.starts_with("OLD_CONTENT"));
        assert!(content.contains("\nNEW_KEY=\"new_value\""));
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
}
