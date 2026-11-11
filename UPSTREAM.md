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
- `main` が mirror している上流の rev: `060768f1ed7f310e639bc49d7af7256e129f7409`
  (2026-11-11, "Preserve header/footer content when adding page numbers (#2201)")

`main` は bace のビルドには使わない。bace が使うのは `bace` だけ。

## 差分の一覧

`bace` が上流と違うファイル。merge のたびに更新する。

| ファイル | 理由 |
| --- | --- |
| `UPSTREAM.md` | この文書。コードの差分ではない |

2026-11-11 時点（`bace` の起点）で、コードの差分は**ゼロ**:

```bash
git diff eae242176108d328c5dd81fbc10370c51ab90706 bace -- . ':!UPSTREAM.md'   # 空
git grep -c '// bace:' bace -- . ':!UPSTREAM.md'                              # 何も出ない
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
