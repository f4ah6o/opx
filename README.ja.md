# opz

1Password CLI ラッパー - コマンドへのシームレスな secret 注入のためのツール

## 機能

* キーワード検索でアイテムを検索
* 1Password アイテムの secret を環境変数としてコマンド実行
* `gen` サブコマンドで env ファイル生成（既存ファイルに追記、重複キーは上書き）
* 繰り返し実行を高速化するアイテムリストのキャッシュ
* 完全一致がない場合のファジーマッチ

## インストール

```bash
cargo install opz
```

## Trusted publishing

このリポジトリは [crates.io trusted publishing](https://crates.io/docs/trusted-publishing) に対応しています。
`v2025.12.0` のようなタグを作成してプッシュすると、`Publish to crates.io` ワークフローがトリガーされ、OIDC経由で短期間有効なトークンを取得し、`cargo publish --locked` を実行します。
ワークフローがトークンをリクエストできるようにするには、crates.io UI で `opz` クレートに対して trusted publishing を有効にする必要があります（リンクされたリポジトリ: `f4ah6o/opx`）。

## 使い方

### アイテム検索

キーワードで 1Password アイテムを検索:

```bash
opz find <query>
```

例:
```bash
opz find baz
# 出力: foo   bar     baz
```

### Secret 付きでコマンド実行

1Password アイテムの secret を環境変数としてコマンドを実行:

```bash
opz [OPTIONS] <ITEM> [ENV] -- <COMMAND>...
```

オプション:
* `--vault <NAME>` - Vault 名（省略時はすべての Vault を検索）

引数:
* `<ITEM>` - secret を取得するアイテムタイトル
* `[ENV]` - 出力 env ファイルパス（省略時はファイル生成なし）

`[ENV]` を指定した場合、env ファイルはコマンド実行後も保持されます。既存ファイルがある場合は追記され、重複キーは上書きされます。

例:
```bash
# secret 付きでコマンド実行（.env ファイルは生成されない）
opz example-item -- your-command

# secret を注入して .env ファイルも生成
opz example-item .env -- your-command

# カスタム env ファイルパスを指定
opz example-item .env.local -- your-command

# Vault を指定
opz --vault Private example-item -- your-command
```

### Env ファイル生成

コマンド実行なしで env ファイルのみを生成:

```bash
opz gen <ITEM> [ENV]
```

例:
```bash
# 標準出力に出力
opz gen example-item

# .env ファイルを生成
opz gen example-item .env

# カスタムパスに生成
opz gen example-item .env.production

# Vault を指定
opz --vault Private gen example-item
```

## 仕組み

1. 1Password からアイテムリストを取得（60秒間キャッシュ）
2. タイトルで一致するアイテムを検索（完全一致またはファジーマッチ）
3. 各フィールドの secret 値を取得
4. env ファイルパスが指定されている場合はファイルに書き込み（既存ファイルにマージ、重複キーは上書き）；指定がない場合は標準出力に出力
5. 環境変数として secret を注入してコマンドを実行

`gen` サブコマンドの場合、ステップ 1-4 のみ実行されます（コマンド実行なし）。

## `op` コマンドの利用

セキュリティの透明性のため、`opz` が `op` CLI をどのように利用するかを示します:

```mermaid
sequenceDiagram
    participant opz
    participant op as op CLI

    Note over opz: ユーザー実行: opz example-item -- claude "hello"

    opz->>op: op item list --format json
    op-->>opz: [{id, title, vault}, ...]
    Note over opz: "example-item" にマッチ → アイテム ID を取得

    opz->>op: op item get <id> --format json
    op-->>opz: {fields: [{label, value}, ...]}
    Note over opz: secret 値を解決<br/>（環境変数として注入）

    Note over opz: オプション: .env ファイルを指定時は書き込み

    opz->>op: sh -c "claude \"hello\""
    Note over opz: secret を含む環境変数で実行
    op-->>opz: 終了ステータス
```

**セキュリティ**: `opz` は secret へのアクセスと認証をすべて `op` CLI に委任します。アイテムリストはメタデータのみを 60 秒間キャッシュします。

## 要件

* [1Password CLI](https://developer.1password.com/docs/cli/) (`op`) がインストールされ、認証されていること
