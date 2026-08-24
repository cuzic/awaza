//! NICOLA同時打鍵判定のラッパ。`awase::engine::NicolaFsm` を直接使い、
//! `awase::engine::Engine`（belief活性ゲート）は経由しない
//! （design doc §8.1: awazaはpreeditを自前で所有するためbeliefが不要）。

use awase::config::ConfirmMode;
use awase::engine::input_tracker::PhysicalKeyState;
use awase::engine::{InputContext, NicolaFsm};
use awase::scanmap::PhysicalPos;
use awase::types::{RawKeyEvent, VkCode};
use awase::yab::YabLayout;

pub use awase::engine::{TIMER_PENDING, TIMER_SPECULATIVE};

/// `NicolaFsm::on_event` の戻り値型（`timed-fsm` の `Response`）。
pub type ChordResponse = timed_fsm::Response<awase::types::KeyAction, usize>;

/// NICOLA同時打鍵エンジンのラッパ。
/// `NicolaFsm`自体が`Debug`を実装していないため`derive(Debug)`は付けない。
pub struct ChordEngine {
    fsm: NicolaFsm,
}

impl ChordEngine {
    /// # 引数
    /// - `layout`: `.yab` レイアウト（`resolve_kana` 済みであること）
    /// - `left_thumb_vk` / `right_thumb_vk`: 親指キーのVK（P0-6で設定ファイルから読む）
    /// - `threshold_ms`: T1（既定100ms）
    /// - `confirm_mode`: 確定方式
    /// - `speculative_delay_ms`: 投機出力までの遅延（既定30ms）
    #[must_use]
    pub fn new(
        layout: YabLayout,
        left_thumb_vk: VkCode,
        right_thumb_vk: VkCode,
        threshold_ms: u32,
        confirm_mode: ConfirmMode,
        speculative_delay_ms: u32,
    ) -> Self {
        Self {
            fsm: NicolaFsm::new(
                layout,
                left_thumb_vk,
                right_thumb_vk,
                threshold_ms,
                confirm_mode,
                speculative_delay_ms,
            ),
        }
    }

    /// 1キーイベントを処理する。`phys` は `PhysicalKeyState::from_ctx` で
    /// `InputContext` から構築する（P0-2でTIP側から呼ぶ想定）。
    pub fn on_event(&mut self, event: RawKeyEvent, phys: &PhysicalKeyState) -> ChordResponse {
        self.fsm.on_event(event, phys)
    }

    /// `TimerCommand`が発火した際に呼ぶ（P0-2.5: 隠しウィンドウのWM_TIMER経由）。
    /// `timer_id`は`TIMER_PENDING`/`TIMER_SPECULATIVE`のいずれか。
    pub fn on_timeout(
        &mut self,
        timer_id: usize,
        phys: &PhysicalKeyState,
        composing: bool,
    ) -> ChordResponse {
        self.fsm.on_timeout(timer_id, phys, composing)
    }

    /// `InputContext` + `RawKeyEvent` から `PhysicalKeyState` を構築するヘルパ。
    #[must_use]
    pub fn physical_key_state(ctx: &InputContext, event: &RawKeyEvent) -> PhysicalKeyState {
        PhysicalKeyState::from_ctx(ctx, event)
    }
}

/// P0-6で実装する、TIPコンテキストでの物理キー分類の入口（仮スタブ）。
/// 実体はP0-6で `awase-vkmap`（切り出し予定crate）を使って実装する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TipKeyClassification {
    LeftThumb,
    RightThumb,
    Char(PhysicalPos),
    ImeControl,
    Passthrough,
}
