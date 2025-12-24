use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use directories::ProjectDirs;
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
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

    /// Output env file path (default: .1password in current dir)
    #[arg(long, global = true, default_value = ".1password")]
    out: PathBuf,

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
            for it in items.into_iter().filter(|x| x.title.to_lowercase().contains(&q)) {
                let vault = it.vault.as_ref().map(|v| v.name.as_str()).unwrap_or("-");
                println!("{}\t{}\t{}", it.id, vault, it.title);
            }
            Ok(())
        }
        None => {
            // Default: run mode
            let item_title = cli.item_title.as_ref()
                .ok_or_else(|| anyhow!("Item title required. Usage: opz [OPTIONS] <ITEM> -- <COMMAND>..."))?;
            if cli.command.is_empty() {
                return Err(anyhow!("Command required after '--'. Usage: opz [OPTIONS] <ITEM> -- <COMMAND>..."));
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
        return Err(anyhow!("Please be more specific or use `opz find <query>` and pass exact title."));
    }

    let item_id = &matches[0].id;
    let item = item_get(item_id)?;
    let env_lines = item_to_env_lines(&item)?;

    write_env_file(&cli.out, &env_lines)?;
    let status = Command::new("op")
        .arg("run")
        .arg(format!("--env-file={}", cli.out.display()))
        .arg("--")
        .args(command)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("failed to run `op run`")?;

    if !cli.keep {
        let _ = fs::remove_file(&cli.out);
    }

    if !status.success() {
        return Err(anyhow!("command failed with status: {}", status));
    }
    Ok(())
}

fn item_to_env_lines(item: &ItemGet) -> Result<Vec<String>> {
    let re = Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*$")?;
    let mut out = Vec::new();

    for f in &item.fields {
        let Some(label) = f.label.as_ref() else { continue };
        if !re.is_match(label) {
            // env var invalid -> skip
            continue;
        }

        // Use actual value, not reference (reference is for op to resolve)
        let Some(v) = &f.value else { continue };
        let val = match v {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Number(n) => n.to_string(),
            serde_json::Value::Bool(b) => b.to_string(),
            // For objects/arrays, encode as JSON string
            other => other.to_string(),
        };

        // Skip empty values
        if val.is_empty() {
            continue;
        }

        // .env safe quoting
        out.push(format!(r#"{k}="{v}""#, k = label, v = escape_env_value(&val)));
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
    let mut f = fs::File::create(path).with_context(|| format!("create {}", path.display()))?;
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

    let v: serde_json::Value = serde_json::from_slice(&out.stdout)
        .context("failed to parse op JSON output")?;
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

