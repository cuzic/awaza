# awaza

NICOLA親指シフトを内蔵した、Windows専用・`libakaza`ベースのRust製日本語IME。

**状態: Phase 0（実装初期段階）。まだ日常使用できるIMEではない。**

## 背景

[awase](https://github.com/cuzic/awase)（同じ開発者によるWindows向けNICOLA同時打鍵
エミュレータ）は、外部IME（Google日本語入力/MS-IME）にVKを注入し、変換自体は
外部IMEに任せる設計だった。この設計の代償は、外部IMEの非公開内部状態を推測し
続ける「belief」問題であり、awaseの再発バグの大半の根にある。

awazaは「自分がTSF TIP（Text Input Processor）そのものになり、preeditも変換も
すべて自分で持つ」ことでこの推測を構造的に無くす。かな漢字変換エンジンは自前で
書かず[akaza](https://github.com/akaza-im/akaza)の`libakaza`を使うことで、
開発コストを「TSF実装 + NICOLA統合」に絞る。

設計の詳細な経緯・根拠は設計書を参照（現時点ではリポジトリ外の作業ドキュメント。
Phase 0進行中にこのリポジトリへ移す予定）。

## クレート構成

- `crates/awaza-chord/` — NICOLA同時打鍵判定のラッパ。`awase::engine::NicolaFsm`
  を直接使う（`awase::engine::Engine`のbelief活性ゲートは経由しない）。
  OS非依存で、Linux上でも`cargo check`が通る。
- `crates/awaza-tip/` — TSF TIP本体（COM DLL）。Windows専用。

## 開発状況・既知の制約

- `awase`/`timed-fsm`はgitタグピン依存（`v1.16.0`、`branch = "main"`は禁止）。
- 親指キーの割り当ては現時点で暫定値（無変換=左親指、変換=右親指、ハードコード）。
  P0-6で設定ファイル駆動化する。
- `.yab`レイアウトは現時点で空（埋め込み前）。P0-6/ADR-011で本実装する。
- preedit表示・composition更新は最小限の実装（かな1文字を表示するだけ）。
  拗音・確定処理・候補選択は未実装（P1以降）。
- ビルド・実機動作確認はWindows実機（clipwire経由）で行う。このリポジトリ自体は
  Linux上でも`awaza-chord`のみ`cargo check`可能。

## ライセンス

MIT OR Apache-2.0
