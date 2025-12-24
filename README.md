# opz

1Password CLI wrapper for seamless secret injection into commands.

## Features

* Find items by keyword search
* Run commands with secrets from 1Password items as environment variables
* Generate env files with `gen` subcommand (appends to existing, overwrites duplicates)
* Item list caching for faster repeated runs
* Fuzzy matching when exact title match is not found

## Installation

```bash
cargo install opz
```

## Trusted publishing

This repository is configured for [crates.io trusted publishing](https://crates.io/docs/trusted-publishing).
Create a tag such as `v2025.12.0` and push it to trigger the `Publish to crates.io` workflow, which mints a short-lived token via OIDC and runs `cargo publish --locked`.
You must enable trusted publishing for the `opz` crate in the crates.io UI (linked repository: `f4ah6o/opx`) before the workflow is allowed to request tokens.

## Usage

### Find Items

Search for 1Password items by keyword:

```bash
opz find <query>
```

Example:
```bash
opz find baz
# Output: foo	bar	baz
```

### Run Commands with Secrets

Run a command with secrets from a 1Password item as environment variables:

```bash
opz [OPTIONS] <ITEM> [ENV] -- <COMMAND>...
```

Options:
* `--vault <NAME>` - Vault name (optional, searches all vaults if omitted)

Arguments:
* `<ITEM>` - Item title to fetch secrets from
* `[ENV]` - Output env file path (default: `.env`)

The env file is preserved after command execution. If the file already exists, new entries are appended and duplicate keys are overwritten.

Examples:
```bash
# Run claude with secrets from "example-item" item
opz example-item -- claude "hello"

# Specify custom env file path
opz example-item .env.local -- your-command

# Specify vault
opz --vault Private example-item -- your-command
```

### Generate Env File

Generate env file only without running a command:

```bash
opz gen <ITEM> [ENV]
```

Examples:
```bash
# Generate .env file
opz gen example-item

# Generate to custom path
opz gen example-item .env.production

# Specify vault
opz --vault Private gen example-item
```

## How It Works

1. Fetches item list from 1Password (cached for 60 seconds)
2. Finds the matching item by title (exact or fuzzy match)
3. Builds `op://<vault>/<item>/<field>` references for each field
4. Writes `.env` file with references (appends to existing, overwrites duplicate keys)
5. Runs the command via `op run --env-file=...` (secrets resolved by `op`)

With `gen` subcommand, only steps 1-4 are executed (no command run).

## `op` Command Usage

For security transparency, here's how `opz` uses the `op` CLI:

```mermaid
sequenceDiagram
    participant opz
    participant op as op CLI

    Note over opz: User runs: opz example-item -- claude "hello"

    opz->>op: op item list --format json
    op-->>opz: [{id, title, vault}, ...]
    Note over opz: Match "example-item" → get item ID

    opz->>op: op item get <id> --format json
    op-->>opz: {fields: [{label, value}, ...]}
    Note over opz: Convert to env refs<br/>(API_KEY="op://vault/item/API_KEY", ...)

    opz->>opz: Write .env (merge with existing)

    opz->>op: op run --env-file=.env -- claude "hello"
    Note over op: Inject secrets & execute
    op-->>opz: Exit status
```

**Security**: `opz` delegates all secret access and authentication to `op` CLI. Item list is cached (60s) with metadata only.

## Requirements

* [1Password CLI](https://developer.1password.com/docs/cli/) (`op`) installed and authenticated
