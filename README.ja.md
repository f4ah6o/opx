# opx

1Password CLI ラッパー - コマンドへのシームレスなsecret注入のためのツール

## 機能

* キーワード検索でアイテムを検索
* 1Password アイテムの secret を環境変数としてコマンド実行
* 繰り返し実行を高速化するアイテムリストのキャッシュ
* 完全一致がない場合のファジーマッチ

## インストール

```bash
cargo install --path .
```

## 使い方

### アイテム検索

キーワードで 1Password アイテムを検索:

```bash
opx find <query>
```

例:
```bash
opx find z.ai
# 出力: vjzgubnmgber7mczrkhrq6lkei	Employee	z.ai
```

### Secret 付きでコマンド実行

1Password アイテムの secret を環境変数としてコマンドを実行:

```bash
opx [OPTIONS] <ITEM> -- <COMMAND>...
```

オプション:
* `--vault <NAME>` - Vault 名（省略時はすべての Vault を検索）
* `--out <PATH>` - 出力 env ファイルパス（デフォルト: `.1password`）
* `--keep` - 生成された env ファイルを残す

例:
```bash
# "z.ai" アイテムの secret で claude を実行
opx z.ai -- claude "hello"

# デバッグ用に env ファイルを残す
opx --keep z.ai -- env

# Vault を指定して env ファイルを残す
opx --vault Private --keep z.ai -- your-command
```

## 仕組み

1. 1Password からアイテムリストを取得（60秒間キャッシュ）
2. タイトルで一致するアイテムを検索（完全一致またはファジーマッチ）
3. フィールドを抽出して環境変数に変換
4. 一時的な `.env` ファイルを作成
5. `op run --env-file=...` 経由でコマンドを実行
6. env ファイルを削除（`--keep` 指定時を除く）

## 要件

* [1Password CLI](https://developer.1password.com/docs/cli/) (`op`) がインストールされ、認証されていること
