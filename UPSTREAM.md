# UPSTREAM — この fork の上流追従と差分

`crcrworks/wordcraft` は [`storytold/wordcraft`](https://github.com/storytold/wordcraft) の
fork。bace（[`crcrworks/bace`](https://github.com/crcrworks/bace)）が LiveWindow に WordCraft を
プロセス内で埋め込み、上流には無いもの（journal など）を足すために持っている。

目的は**改造を bace 側の資産にしないこと**。上流の取り込みを作り直しにしないための規則を
ここに置く。決定の全文と却下した選択肢は bace 側の
[`docs/decisions/0031-craft-fork.md`](https://github.com/crcrworks/bace/blob/main/docs/decisions/0031-craft-fork.md)。

## ブランチ

| ブランチ | 役割 | 動かし方 |
| --- | --- | --- |
| `main` | 上流の mirror。追従専用で、ここには改造を積まない | `git fetch upstream && git merge --ff-only upstream/main`、`git push origin main` |
| `bace` | bace 用の改造を積む唯一のブランチ。bace の依存はここを指す | `git fetch upstream && git merge upstream/main`（merge commit で取り込む） |

`bace` は上流の `eae242176108d328c5dd81fbc10370c51ab90706`（bace が rev で固定していた版）から
始めた。`main` はそれより先へ上流に追従しているので、`bace` が追いつくのは次の
`git merge upstream/main` のとき。

- `bace` の起点にした上流の rev: `eae242176108d328c5dd81fbc10370c51ab90706`
  (2026-10-15, "Merge pull request #2307 from dylanmtaylor/ci/nix-workflow")
- `main` が mirror している上流の rev: `fab8793ce791763761353b78caad6a63a46e5b01`
  (2026-11-13, "Merge pull request #2703 from vangel76/feat/style-pr-change-2697")
- `bace` が最後に取り込んだ上流の rev: `fab8793ce791763761353b78caad6a63a46e5b01`
  (egui 0.37。上流が `crates/engine/src/session.rs` を `session/` のモジュールに分割したので、
  journal のフックは分割後のモジュールに付け直した)

`main` は bace のビルドには使わない。bace が使うのは `bace` だけ。

## 差分の一覧

`bace` が上流と違うファイル。merge のたびに更新する。

| ファイル | 理由 | `// bace:` |
| --- | --- | --- |
| `UPSTREAM.md` | この文書。コードの差分ではない | — |
| `crates/engine/src/journal.rs` | **新しいモジュール。** セッションが実行したコマンドの記録（journal）と、それを別のセッションで実行し直す `Session::apply_journal`。bace 側の設計は [`0032-wordcraft-journal.md`](https://github.com/crcrworks/bace/blob/main/docs/decisions/0032-wordcraft-journal.md) | — |
| `crates/engine/src/lib.rs` | `CommandSpec` に `journal` を足し、モジュールを公開するフック | 4 |
| `crates/engine/src/cmd.rs` | テキスト・書式・段落の編集コマンドを journal 対象にするフック（上流が `cmd/mod.rs` から改名） | 4 |
| `crates/engine/src/session.rs` | `Session` に `journal` / `applying` を持つ | 1 |
| `crates/engine/src/doc_state.rs` | `Undo` に、そのステップを差分として送れるかを持つ | 1 |
| `crates/engine/src/session/history.rs` | ステップの journal 印と、undo / redo が差分（`journal.revert`）を記録するフック | 7 |
| `crates/engine/src/session/run.rs` | `run` の成功時に journal へ記録するフック、差分の適用、別の文書へ移ったコマンドでログを切るフックと、`take_journal` / `skip_journal` / `apply_journal` の入口 | 17 |
| `crates/engine/src/session/snapshot.rs` | `edit_snapshot` / `restore` で記録を巻き戻すフック | 3 |
| `crates/engine/src/session/documents.rs` | 文書を切り替えたらログを切るフック（ログは表示中の文書のもの） | 2 |
| `crates/engine/src/session/lifecycle.rs` | `Session::new` の初期化 | 1 |
| `crates/engine/src/snapshot.rs` | `EditSnapshot` に journal の位置を持つ | 1 |

2026-11-11 時点（`bace` の起点）のコードの差分はゼロだった。journal を足した時点で:

```bash
git diff eae242176108d328c5dd81fbc10370c51ab90706 bace --stat
git grep -c '// bace:'
```

`fab8793c`（egui 0.37）を取り込んだ時点の差は 9 ファイル・+690/−5、`// bace:` は 25 個。
うち `crates/engine/src/session/*` の 16 個は、上流の `session.rs` 分割にフックを付け直したもの。
undo/redo を差分で送る変更（`0046`）の後は 11 ファイル・+1139/−8、`// bace:` は 41 個
（`session/*` は 30 個）。:

```bash
git diff fab8793ce791763761353b78caad6a63a46e5b01 bace --stat -- . ':!UPSTREAM.md'
git grep -c '// bace:'
```

## 既存ファイルへの変更はフックだけにする

上流が同じ関数を書き換えるたびに rebase が壊れるのを避けるため、改造は次の形に限る。

1. **bace 用の処理は新しいモジュールに置く。** 上流に無いファイル（例:
   `crates/engine/src/journal.rs`）を足し、ロジックはそこに閉じる。bace の field・presence・
   転送・id の概念は持ち込まない。
2. **既存ファイルへの変更はフックだけ。** 上流の関数の呼び出しを数行足すだけにして、上流の
   ロジックそのものは書き換えない。上流の公開 API を変える必要が出たら、まず新しいモジュール
   側で吸収できないか考える。
3. **既存ファイルへの差の1行コメントで印を付ける。** 既存ファイルに足した行のそばに
   `// bace: <理由>` を置く。理由は短く（例: `// bace: journal records undo/redo bounds`）。
   上流のファイルに印が無い差があってはいけない。
4. **差の総量を数えられる状態を保つ。**

   ```bash
   git grep -c '// bace:'                 # ファイルごとの印の数
   git diff <上流 rev> bace --stat        # 差分の全体
   ```

   数えた結果はこの文書の「差分の一覧」に反映する。件数が増えるほど上流に追従するコストが
   上がるので、印が多いファイルは上流に返せる変更か（案 C）を一度考える。

## 上流を取り込む

```bash
git fetch upstream

# 1. main を上流の mirror に戻す（ff できなければ main に何かが積まれている）
git switch main
git merge --ff-only upstream/main
git push origin main

# 2. bace に上流を取り込む
git switch bace
git merge upstream/main

# 3. 差を数え直し、この文書の「差分の一覧」と件数を更新する
git grep -c '// bace:'
git diff upstream/main bace --stat

# 4. ビルドして push する
cargo build --workspace --all-targets
cargo test --workspace
git push origin bace
```

衝突したときは次の順で解く。

1. 上流側の変更を正とする。bace の改造は新しいモジュールに閉じているので、上流のロジックを
   こちらに合わせて書き換えない。
2. フックを上流の新しい関数・型に付け直す。上流が関数を消した・改名したときは、呼び出しを
   新しい場所へ移す。
3. `// bace:` が消えた・重複した箇所を探す。

   ```bash
   git grep -n '// bace:'
   git diff --name-only --diff-filter=U
   ```

4. 設計に迷ったら bace 側の決定
   [`0031-craft-fork.md`](https://github.com/crcrworks/bace/blob/main/docs/decisions/0031-craft-fork.md)
   に戻る。決まっていなければ bace の issue で決めてから merge する。

## bace の依存をこの fork に向ける

bace の `apps/bace/Cargo.toml` と `apps/craft-probe/Cargo.toml` は 4 クレート
（`wordcraft-engine` / `wordcraft-ui-egui`）を `branch = "bace"` で指す。正確な rev は
bace の `Cargo.lock` に残るので、`--locked` の CI は同じ rev で再現する。

fork を進めたら bace 側で lock を進める。

```bash
cargo update -p wordcraft-engine -p wordcraft-ui-egui
```

`[patch.crates-io]` の `krilla`（`storytold/craft-libs`）と `zip`（`zip-rs/zip2`）は上流の
まま。この fork では改造しないので fork しない。`craft-pdf` が `krilla` を要求し続ける限り、
bace のルートの patch も残す。

## ライセンス

上流と同じ **MIT OR Apache-2.0**（fork しても変わらない）。`LICENSE-MIT` /
`LICENSE-APACHE` / `ATTRIBUTION.md` / `NOTICE` を消さない。上流のアセットは
`ATTRIBUTION.md` の表のとおりで、fork 側でアセットを足すときは同じ表に著作者・出典・
ライセンスを書く。
