//! awaza TSF TIP本体。
//!
//! Phase 0 (`awaza-phase0-tasks.md`) の P0-2/P0-2.5/P0-3 を実装する。
//! - COM登録（CLSID/InprocServer32/カテゴリ3種/ja-JPプロファイル）: P0-2
//! - 隠しメッセージ専用ウィンドウでのタイマー駆動（T1保留・投機出力）: P0-2.5
//! - preedit(`ITfComposition`)への実文字表示: P0-3
//!
//! ## このセッションでの既知のリスク
//! Windows実機でのコンパイル確認はこのファイルの初回コミット時点では未実施。
//! `experiments/tsf-edit-session-spike`で検証済みのパターン（TIP/KeySink分離、
//! DllRegisterServer等）は踏襲しているが、composition関連のAPI呼び出しと
//! 隠しウィンドウ生成は今回が初めての実装であり、ビルドエラーが出る前提で
//! 反復修正する。

#![cfg(windows)]

use std::cell::{Cell, RefCell};
use std::fs::OpenOptions;
use std::io::Write as _;
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

use windows::core::{implement, w, Interface, Result, BOOL, GUID, HRESULT, PCWSTR};
use windows::Win32::Foundation::{
    E_NOINTERFACE, HINSTANCE, HMODULE, HWND, LPARAM, LRESULT, S_FALSE, S_OK, WPARAM,
};
use windows::Win32::System::Com::{
    CoCreateInstance, IClassFactory, IClassFactory_Impl, CLSCTX_INPROC_SERVER,
};
use windows::Win32::System::LibraryLoader::{
    GetModuleFileNameW, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
    GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegSetValueExW, HKEY, HKEY_CLASSES_ROOT,
    KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ,
};
use windows::Win32::UI::TextServices::{
    ITfCategoryMgr, ITfComposition, ITfCompositionSink, ITfCompositionSink_Impl, ITfContext,
    ITfContextOwnerCompositionServices, ITfEditSession, ITfEditSession_Impl,
    ITfInputProcessorProfiles, ITfInsertAtSelection, ITfKeyEventSink, ITfKeyEventSink_Impl,
    ITfKeystrokeMgr, ITfRange, ITfTextInputProcessor, ITfTextInputProcessor_Impl, ITfThreadMgr,
    CLSID_TF_CategoryMgr, CLSID_TF_InputProcessorProfiles, GUID_TFCAT_TIPCAP_IMMERSIVESUPPORT,
    GUID_TFCAT_TIPCAP_SYSTRAYSUPPORT, GUID_TFCAT_TIP_KEYBOARD, TF_ES_READWRITE, TF_ES_SYNC,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, KillTimer, RegisterClassExW, SetTimer,
    UnregisterClassW, CS_HREDRAW, CW_USEDEFAULT, HWND_MESSAGE, WM_DESTROY, WM_TIMER,
    WNDCLASSEXW, WS_DISABLED,
};

use awaza_chord::{ChordEngine, ChordResponse};
use awase::config::ConfirmMode;
use awase::engine::input_tracker::PhysicalKeyState;
use awase::engine::InputContext;
use awase::engine::ModifierState;
use awase::types::{ImeRelevance, KeyAction, KeyClassification, KeyEventType, RawKeyEvent, VkCode};

/// 簡易的な単調増加ミリ秒タイムスタンプ(`Timestamp`は`u64`のtype alias)。
fn now_ms() -> u64 {
    use std::sync::OnceLock;
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    let start = START.get_or_init(std::time::Instant::now);
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}
use awase::yab::YabLayout;

use libakaza::config::EngineConfig;
use libakaza::engine::base::HenkanEngine;
use libakaza::engine::bigram_word_viterbi_engine::{
    BigramWordViterbiEngine, BigramWordViterbiEngineBuilder,
};
use libakaza::kana_kanji::marisa_kana_kanji_dict::MarisaKanaKanjiDict;
use libakaza::lm::system_bigram::MarisaSystemBigramLM;
use libakaza::lm::system_unigram_lm::MarisaSystemUnigramLM;

/// P0-5: libakazaの具体的なエンジン型。`EngineConfig::default()`/
/// `.build()`の戻り値がこの3型パラメータで単相化される(2026-08-24、
/// akaza-im/akaza main の実ソースで確認)。
type AkazaEngine = BigramWordViterbiEngine<MarisaSystemUnigramLM, MarisaSystemBigramLM, MarisaKanaKanjiDict>;

// ── GUID(恒久。spikeの使い捨てGUIDとは別に採番) ──

/// awaza TIPのCLSID。
const CLSID_TIP: GUID = GUID::from_u128(0x8f2c1a6e_3d47_4b8a_9e12_5a6f7c8d9e0f);
/// 言語プロファイルGUID。
const GUID_PROFILE: GUID = GUID::from_u128(0x1b3d5f7a_9c2e_4f6b_8a1d_3e5f7a9c2e4f);
/// 日本語(ja-JP)。
const LANGID_JA_JP: u16 = 0x0411;

const LOG_PATH: &str = r"C:\Users\cuzic\awaza-tip.log";

fn log(msg: &str) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let pid = std::process::id();
    let mut line = String::new();
    use std::fmt::Write as _;
    let _ = writeln!(line, "[{now} pid={pid}] {msg}");
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(LOG_PATH) {
        let _ = f.write_all(line.as_bytes());
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn own_module_path() -> Result<String> {
    unsafe {
        let mut hmodule = HMODULE::default();
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            PCWSTR(own_module_path as *const () as *const u16),
            &mut hmodule,
        )?;
        let mut buf = [0u16; 1024];
        let len = GetModuleFileNameW(Some(hmodule), &mut buf);
        Ok(String::from_utf16_lossy(&buf[..len as usize]))
    }
}

fn own_hmodule() -> HMODULE {
    unsafe {
        let mut hmodule = HMODULE::default();
        let _ = GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            PCWSTR(own_hmodule as *const () as *const u16),
            &mut hmodule,
        );
        hmodule
    }
}

// ── 親指キーの暫定割り当て(P0-6で設定ファイル駆動化する前の仮値) ──
// NICOLA慣例に倣い、無変換=左親指、変換=右親指とする。
const VK_NONCONVERT: u16 = 0x1D; // 左親指(暫定)
const VK_CONVERT: u16 = 0x1C; // 右親指(暫定)

// ── P0-2.5: 隠しメッセージ専用ウィンドウでのタイマー駆動 ──
//
// awase本体は`SetTimer(None, 0, ms, None)`(hwnd無し)+自前のメッセージループで
// WM_TIMERを拾うが、TIPはホストアプリのメッセージループに寄生するだけで自前の
// ループを持たない。hwnd無しのWM_TIMERはhost側の一般的なループでは誰も拾って
// くれないため、TIP自身がHWND_MESSAGEの隠しウィンドウを作り、そのウィンドウに
// 対してSetTimer(hwnd, ...)する。DispatchMessageはhwndを見てウィンドウ
// プロシージャを呼ぶため、hostのメッセージループが(それと知らずに)中継してくれる、
// という定番のパターン。

const TIMER_WNDCLASS: PCWSTR = w!("AwazaTipTimerWindow");

/// `GWLP_USERDATA`に`Rc<TipState>`の生ポインタを積んで、WndProcから
/// `TipState`にアクセスする。
unsafe extern "system" fn timer_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_TIMER => {
            let ptr = windows::Win32::UI::WindowsAndMessaging::GetWindowLongPtrW(
                hwnd,
                windows::Win32::UI::WindowsAndMessaging::GWLP_USERDATA,
            );
            if ptr != 0 {
                let state = state_from_raw(ptr as *const TipState);
                let timer_id = wparam.0;
                state.on_timer_fired(timer_id);
            }
            LRESULT(0)
        }
        WM_DESTROY => LRESULT(0),
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn create_timer_window(state_ptr: *const TipState) -> Result<HWND> {
    unsafe {
        let hinstance: HINSTANCE = own_hmodule().into();
        let wc = WNDCLASSEXW {
            cbSize: u32::try_from(std::mem::size_of::<WNDCLASSEXW>()).unwrap_or_default(),
            style: CS_HREDRAW,
            lpfnWndProc: Some(timer_wndproc),
            hInstance: hinstance,
            lpszClassName: TIMER_WNDCLASS,
            ..Default::default()
        };
        // 既に登録済みでも害はない(複数プロセスにロードされるため毎回試みる)。
        let _ = RegisterClassExW(&wc);

        let hwnd = CreateWindowExW(
            Default::default(),
            TIMER_WNDCLASS,
            w!(""),
            WS_DISABLED,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            Some(HWND_MESSAGE),
            None,
            Some(hinstance),
            None,
        )?;

        windows::Win32::UI::WindowsAndMessaging::SetWindowLongPtrW(
            hwnd,
            windows::Win32::UI::WindowsAndMessaging::GWLP_USERDATA,
            state_ptr as isize,
        );

        log(&format!("create_timer_window: hwnd={:?}", hwnd.0));
        Ok(hwnd)
    }
}

// ── preedit/compositionと`NicolaFsm`をまとめて持つ共有状態 ──
//
// TIP本体(`TspTip`)・キーイベントシンク(`KeySink`)・composition終了シンク
// (`CompSink`)の3つのCOMオブジェクトから、Rcで共有してアクセスする。
// COMのスレッディングモデルはApartment(単一スレッド)なので`Rc<RefCell<_>>`で
// 問題ない(`Send`/`Sync`は不要)。

struct TipState {
    tid: Cell<u32>,
    hwnd: Cell<HWND>,
    /// 直近の`OnKeyDown`で渡された`ITfContext`。タイマーコールバックは
    /// キーイベント外で発生するため、compositionを更新するにはこれを
    /// 保持しておく必要がある(P0-2.5/P0-3で判明した設計上の要点)。
    context: RefCell<Option<ITfContext>>,
    chord: RefCell<ChordEngine>,
    composition: RefCell<Option<ITfComposition>>,
    preedit: RefCell<String>,
    /// P0-5: libakazaエンジン。辞書・言語モデルが未導入だと`None`のまま
    /// (design doc §9.1: 起動を拒否せず、未導入を明示するだけにする決定)。
    /// まだ実際の変換フローには配線していない(候補UI・確定キーがP0-7/P1)。
    /// ここでは「Windows MSVCでlibakazaがビルド・構築できるか」の検証が主目的。
    #[allow(dead_code)] // 変換フロー配線(P1)まではtry_convert経由のテスト用途のみ
    engine: RefCell<Option<AkazaEngine>>,
}

/// NICOLA配列(`layout/nicola.yab`)を埋め込む。design doc ADR-011の
/// `YabLayout::builtin()`(awase本体側)への移行はP1で行う。それまでは
/// awazaリポジトリ側にコピーを持つ(design doc §8.5の「二重化を避ける」
/// 方針との一時的な妥協。P1でawase側の切り出しと同時に解消する)。
const NICOLA_YAB: &str = include_str!("../layout/nicola.yab");

impl TipState {
    fn new() -> Self {
        let layout = YabLayout::parse(NICOLA_YAB, awase::scanmap::KeyboardModel::Jis)
            .expect("layout/nicola.yabは実機監査済み(2026-07-28)なので常にパースできるはず")
            .resolve_kana();
        let chord = ChordEngine::new(
            layout,
            VkCode(VK_NONCONVERT),
            VkCode(VK_CONVERT),
            100,
            ConfirmMode::default(),
            30,
        );
        let engine = match BigramWordViterbiEngineBuilder::new(EngineConfig::default()).build() {
            Ok(engine) => {
                log("libakaza engine構築成功(P0-5)");
                Some(engine)
            }
            Err(e) => {
                // design doc §9.1: 辞書・言語モデル未導入時は起動を継続する。
                // 実際の「取得手順への導線」UIはP3のタスク(ここではログのみ)。
                log(&format!(
                    "libakaza engine構築失敗(辞書・言語モデル未導入の可能性、起動は継続): {e}"
                ));
                None
            }
        };
        Self {
            tid: Cell::new(0),
            hwnd: Cell::new(HWND::default()),
            context: RefCell::new(None),
            chord: RefCell::new(chord),
            composition: RefCell::new(None),
            preedit: RefCell::new(String::new()),
            engine: RefCell::new(engine),
        }
    }

    fn on_timer_fired(self: &Rc<Self>, timer_id: usize) {
        log(&format!("on_timer_fired timer_id={timer_id}"));
        if self.context.borrow().is_none() {
            log("on_timer_fired: no context, ignoring");
            return;
        }
        let phys = PhysicalKeyState::empty();
        let composing = self.composition.borrow().is_some();
        let resp = self.chord.borrow_mut().on_timeout(timer_id, &phys, composing);
        self.apply_response(&resp);
        self.apply_timers(&resp.timers);
    }

    /// P0-5: libakazaで実際に変換を試す(まだ確定キー等のUIには未配線。
    /// エンジンが構築できているかどうかの動作確認のみが目的)。
    #[allow(dead_code)] // 変換フロー配線(P1)まで未呼び出し
    fn try_convert(&self, yomi: &str) {
        let engine_ref = self.engine.borrow();
        let Some(engine) = engine_ref.as_ref() else {
            log("try_convert: engine未構築のためスキップ");
            return;
        };
        match engine.convert(yomi, None) {
            Ok(candidates) => log(&format!("try_convert({yomi:?}) -> {} 文節", candidates.len())),
            Err(e) => log(&format!("try_convert({yomi:?}) failed: {e}")),
        }
    }

    /// `Response::timers`(`TimerCommand::Set`/`Kill`)を実際のWin32タイマーに反映する。
    /// `OnKeyDown`・`on_timer_fired`の両方から呼ぶ(P0-2.5のバグ修正: 以前は
    /// `on_timer_fired`側でこれを呼んでおらず、`Kill`要求が無視されて
    /// タイマーが無限に再発火し続けていた)。
    fn apply_timers(&self, timers: &[timed_fsm::TimerCommand<usize>]) {
        let hwnd = self.hwnd.get();
        for timer in timers {
            match timer {
                timed_fsm::TimerCommand::Set { id, duration } => unsafe {
                    let ms = u32::try_from(duration.as_millis()).unwrap_or(u32::MAX);
                    log(&format!("apply_timers: Set id={id} ms={ms}"));
                    SetTimer(Some(hwnd), *id, ms, None);
                },
                timed_fsm::TimerCommand::Kill { id } => unsafe {
                    log(&format!("apply_timers: Kill id={id}"));
                    let _ = KillTimer(Some(hwnd), *id);
                },
            }
        }
    }

    /// `ChordResponse`(かな確定・投機出力等)を実際のpreedit更新に反映する。
    /// 既存compositionがあれば`ITfRange::SetText`で置き換え、無ければ
    /// `ITfInsertAtSelection`+`StartComposition`で新規開始する
    /// (`TipEditSession::DoEditSession`参照)。
    fn apply_response(self: &Rc<Self>, resp: &ChordResponse) {
        if resp.actions.is_empty() && resp.timers.is_empty() {
            return;
        }
        log(&format!("apply_response: {resp:?}"));

        // KeyAction::Char由来のかな1文字をpreeditに追記する。
        //
        // design doc §7.3/D6: `SpecialKey(Backspace)`は`retract_and_replace`
        // (src/engine/nicola_fsm.rs)が「直前の投機出力1文字をBackspace 1発で
        // 取り消し、新しい面の文字に差し替える」ために生成する内部訂正シグナル
        // であり、ユーザーが物理的に押したBackspaceキーとは発生源が異なる。
        // 本FSMではこのバリアントは`retract_and_replace`(grep確認: 単一箇所)
        // からしか出現せず、必ず直後に差し替え用のactionが同じ`resp.actions`
        // 内に続く。一方、物理Backspaceキーは`classify_vk`で`Passthrough`に
        // 分類され`on_event`のaction生成経路に乗らない(そのままアプリへ転送
        // される)。したがって`resp.actions`中の`SpecialKey(Backspace)`は常に
        // 「preeditの末尾1文字を取り消す」という意味で安全に解釈でき、
        // 専用の`ChordOutcome`型を新設せずawaza側のこのマッピングだけで
        // D6の要件(内部訂正と物理Backspaceの混同回避)を満たせる
        // (design doc §7.3で保留されていたPhase 1確認事項の結論)。
        let mut text = self.preedit.borrow().clone();
        let mut changed = false;
        for action in &resp.actions {
            match action {
                KeyAction::Char(c) => {
                    text.push(*c);
                    changed = true;
                }
                KeyAction::SpecialKey(awase::types::SpecialKey::Backspace) => {
                    text.pop();
                    changed = true;
                }
                other => {
                    log(&format!("apply_response: unhandled action {other:?} (P1で実装)"));
                }
            }
        }

        if changed {
            if let Err(e) = self.update_composition(&text) {
                log(&format!("apply_response: update_composition failed: {e}"));
            } else {
                self.preedit.replace(text);
            }
        }
    }

    fn update_composition(self: &Rc<Self>, text: &str) -> Result<()> {
        let Some(ctx) = self.context.borrow().clone() else {
            return Ok(());
        };
        let session = TipEditSession {
            state: self.clone(),
            text: text.to_owned(),
        };
        let session_iface: ITfEditSession = session.into();
        let tid = self.tid.get();
        unsafe {
            let _: HRESULT =
                ctx.RequestEditSession(tid, &session_iface, TF_ES_SYNC | TF_ES_READWRITE)?;
        }
        Ok(())
    }

    /// 現在のcompositionを確定(終了)する。「今は他の未処理キーが来たら直前の
    /// compositionを確定する」という単純な安全弁(実際の確定キー・変換UIは
    /// P1で実装)。preedit状態もクリアする。
    fn confirm_composition(self: &Rc<Self>) {
        if self.composition.borrow().is_none() {
            return;
        }
        let Some(ctx) = self.context.borrow().clone() else {
            return;
        };
        let session = TipEndCompositionSession {
            state: self.clone(),
        };
        let session_iface: ITfEditSession = session.into();
        let tid = self.tid.get();
        unsafe {
            if let Err(e) = ctx.RequestEditSession(tid, &session_iface, TF_ES_SYNC | TF_ES_READWRITE)
            {
                log(&format!("confirm_composition: RequestEditSession failed: {e}"));
            }
        }
    }
}

/// `GWLP_USERDATA`に積んだ生ポインタから`Rc<TipState>`を安全に復元する。
/// `Rc::increment_strong_count`で参照カウントを先に増やしてから
/// `Rc::from_raw`するため、元の所有者(`TspTip.state`)の参照を消費しない
/// (両者が独立してdropできる)。
fn state_from_raw(ptr: *const TipState) -> Rc<TipState> {
    unsafe {
        Rc::increment_strong_count(ptr);
        Rc::from_raw(ptr)
    }
}

/// composition開始/更新を行うedit session。既存compositionがあれば
/// `ITfRange::SetText`で置き換え(preeditが正しく更新される)、無ければ
/// `ITfInsertAtSelection`+`StartComposition`で新規開始し、結果を
/// `TipState.composition`に保持する(以前は毎回新規挿入していたため、
/// 2文字目以降でcompositionが積み重なり操作不能になるバグがあった。
/// 実機テストで発見・修正)。
#[implement(ITfEditSession)]
struct TipEditSession {
    state: Rc<TipState>,
    text: String,
}

impl ITfEditSession_Impl for TipEditSession_Impl {
    fn DoEditSession(&self, ec: u32) -> Result<()> {
        let Some(ctx) = self.state.context.borrow().clone() else {
            return Ok(());
        };
        let wtext = wide(&self.text);
        // 末尾の\0は含めない(SetText/InsertTextAtSelectionはスライス長で判断する)。
        let wtext = &wtext[..wtext.len().saturating_sub(1)];

        let existing = self.state.composition.borrow().clone();
        if let Some(composition) = existing {
            unsafe {
                let range: ITfRange = composition.GetRange()?;
                range.SetText(ec, 0, wtext)?;
            }
            log("DoEditSession: SetText OK (既存compositionを更新)");
        } else {
            unsafe {
                let insert: ITfInsertAtSelection = ctx.cast()?;
                let comp_services: ITfContextOwnerCompositionServices = ctx.cast()?;
                let range: ITfRange = insert.InsertTextAtSelection(
                    ec,
                    windows::Win32::UI::TextServices::INSERT_TEXT_AT_SELECTION_FLAGS(0),
                    wtext,
                )?;
                let comp_sink = TipCompositionSink {
                    state: self.state.clone(),
                };
                let comp_sink_iface: ITfCompositionSink = comp_sink.into();
                let composition = comp_services.StartComposition(ec, &range, &comp_sink_iface)?;
                self.state.composition.replace(Some(composition));
            }
            log("DoEditSession: StartComposition OK (新規composition)");
        }
        Ok(())
    }
}

/// 直前のcompositionを確定(`EndComposition`)するだけのedit session。
/// `TipState::confirm_composition`から使う。
#[implement(ITfEditSession)]
struct TipEndCompositionSession {
    state: Rc<TipState>,
}

impl ITfEditSession_Impl for TipEndCompositionSession_Impl {
    fn DoEditSession(&self, ec: u32) -> Result<()> {
        if let Some(composition) = self.state.composition.borrow().clone() {
            unsafe {
                composition.EndComposition(ec)?;
            }
            log("DoEditSession: EndComposition OK (確定)");
        }
        self.state.composition.take();
        self.state.preedit.replace(String::new());
        Ok(())
    }
}

/// composition終了(自前のEndComposition・マウスクリック等の外部終了の両方)の
/// 通知シンク。P0-3の受け入れ基準の一つ(外部終了の検知)。
#[implement(ITfCompositionSink)]
struct TipCompositionSink {
    state: Rc<TipState>,
}

impl ITfCompositionSink_Impl for TipCompositionSink_Impl {
    fn OnCompositionTerminated(
        &self,
        _ecwrite: u32,
        _pcomposition: windows::core::Ref<'_, ITfComposition>,
    ) -> Result<()> {
        log("OnCompositionTerminated (外部からのcomposition終了、preedit状態をクリア)");
        self.state.composition.take();
        self.state.preedit.replace(String::new());
        Ok(())
    }
}

// ── ITfKeyEventSink ──

#[implement(ITfKeyEventSink)]
struct KeySink {
    state: Rc<TipState>,
}

impl ITfKeyEventSink_Impl for KeySink_Impl {
    fn OnSetFocus(&self, _fforeground: BOOL) -> Result<()> {
        Ok(())
    }

    fn OnTestKeyDown(
        &self,
        _pic: windows::core::Ref<'_, ITfContext>,
        _wparam: WPARAM,
        _lparam: LPARAM,
    ) -> Result<BOOL> {
        // 副作用なし(§6.1 ADR-D、MS公式サンプルのパターン)。
        Ok(BOOL(0))
    }

    fn OnKeyDown(
        &self,
        pic: windows::core::Ref<'_, ITfContext>,
        wparam: WPARAM,
        _lparam: LPARAM,
    ) -> Result<BOOL> {
        let Some(pic) = pic.as_ref() else {
            return Ok(BOOL(0));
        };
        self.state.context.replace(Some(pic.clone()));

        let vk = u16::try_from(wparam.0).unwrap_or(0);
        let (classification, physical_pos) = classify_vk(vk);
        let event = RawKeyEvent {
            vk_code: VkCode(vk),
            scan_code: awase::types::ScanCode(0),
            event_type: KeyEventType::KeyDown,
            extra_info: 0,
            timestamp: now_ms(),
            key_classification: classification,
            physical_pos,
            ime_relevance: ImeRelevance::default(),
            modifier_key: None,
            modifier_snapshot: ModifierState {
                ctrl: false,
                alt: false,
                shift: false,
                win: false,
            },
            injected: false,
        };
        let phys = ChordEngine::physical_key_state(
            &InputContext {
                ime_on: true,
                input_mode: awase::engine::InputModeState::ObservedKana,
                is_japanese_ime: true,
                composing: self.state.composition.borrow().is_some(),
                modifiers: ModifierState {
                    ctrl: false,
                    alt: false,
                    shift: false,
                    win: false,
                },
                left_thumb_down: None,
                right_thumb_down: None,
            },
            &event,
        );

        let resp = self.state.chord.borrow_mut().on_event(event, &phys);
        log(&format!("OnKeyDown vk=0x{vk:02X} class={classification:?} resp={resp:?}"));
        self.state.apply_response(&resp);
        self.state.apply_timers(&resp.timers);

        // このキーがchordと無関係(NicolaFsmが消費しなかった)なら、直前の
        // compositionが残っていれば確定する。本来の確定キー・変換UIはP1で
        // 実装するが、それまで「どのキーを押しても確定できず操作不能になる」
        // のを避ける安全弁として最低限これを入れる(実機テストで発覚した問題)。
        if !resp.consumed {
            self.state.confirm_composition();
        }

        // 現時点ではキーを消費しない(投機出力の実処理・確定処理はP1で本実装する)。
        Ok(BOOL(0))
    }

    fn OnTestKeyUp(
        &self,
        _pic: windows::core::Ref<'_, ITfContext>,
        _wparam: WPARAM,
        _lparam: LPARAM,
    ) -> Result<BOOL> {
        Ok(BOOL(0))
    }

    fn OnKeyUp(
        &self,
        _pic: windows::core::Ref<'_, ITfContext>,
        _wparam: WPARAM,
        _lparam: LPARAM,
    ) -> Result<BOOL> {
        Ok(BOOL(0))
    }

    fn OnPreservedKey(
        &self,
        _pic: windows::core::Ref<'_, ITfContext>,
        _rguid: *const GUID,
    ) -> Result<BOOL> {
        Ok(BOOL(0))
    }
}

/// P0-6完了: VK→物理位置は`awase-vkmap`crateに切り出し済み
/// (`refactor/awase-vkmap-extract`、rust-nicolaリポジトリ側でテスト・
/// クロスコンパイル確認済み、2026-08-25)。ここでの重複コピーは廃止した。
fn classify_vk(vk: u16) -> (KeyClassification, Option<awase::scanmap::PhysicalPos>) {
    match vk {
        VK_NONCONVERT => (KeyClassification::LeftThumb, None),
        VK_CONVERT => (KeyClassification::RightThumb, None),
        _ => match awase_vkmap::vk_to_pos(VkCode(vk)) {
            Some(pos) => (KeyClassification::Char, Some(pos)),
            None => (KeyClassification::Passthrough, None),
        },
    }
}

// ── ITfTextInputProcessor 本体 ──

#[implement(ITfTextInputProcessor)]
struct TspTip {
    thread_mgr: RefCell<Option<ITfThreadMgr>>,
    sink: RefCell<Option<ITfKeyEventSink>>,
    tid: Cell<u32>,
    state: Rc<TipState>,
}

impl Default for TspTip {
    fn default() -> Self {
        Self {
            thread_mgr: RefCell::new(None),
            sink: RefCell::new(None),
            tid: Cell::new(0),
            state: Rc::new(TipState::new()),
        }
    }
}

impl ITfTextInputProcessor_Impl for TspTip_Impl {
    fn Activate(&self, ptim: windows::core::Ref<'_, ITfThreadMgr>, tid: u32) -> Result<()> {
        log(&format!("Activate tid={tid}"));
        if let Some(tm) = ptim.as_ref() {
            let keystroke_mgr: ITfKeystrokeMgr = tm.cast()?;
            let key_sink = KeySink {
                state: self.state.clone(),
            };
            let sink: ITfKeyEventSink = key_sink.into();
            unsafe {
                keystroke_mgr.AdviseKeyEventSink(tid, &sink, true)?;
            }
            self.sink.replace(Some(sink));
            self.thread_mgr.replace(Some(tm.clone()));
            self.tid.set(tid);
            self.state.tid.set(tid);

            // P0-2.5: 隠しメッセージウィンドウを作成する。
            let state_ptr = Rc::as_ptr(&self.state);
            match create_timer_window(state_ptr) {
                Ok(hwnd) => self.state.hwnd.set(hwnd),
                Err(e) => log(&format!("create_timer_window failed: {e}")),
            }

            log("Activate: AdviseKeyEventSink OK");
        }
        Ok(())
    }

    fn Deactivate(&self) -> Result<()> {
        log("Deactivate");
        if let Some(tm) = self.thread_mgr.take() {
            let keystroke_mgr: ITfKeystrokeMgr = tm.cast()?;
            unsafe {
                keystroke_mgr.UnadviseKeyEventSink(self.tid.get())?;
            }
        }
        self.sink.take();
        let hwnd = self.state.hwnd.get();
        if hwnd.0 != std::ptr::null_mut() {
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
        }
        Ok(())
    }
}

// ── IClassFactory ──

#[implement(IClassFactory)]
struct ClassFactory;

impl IClassFactory_Impl for ClassFactory_Impl {
    fn CreateInstance(
        &self,
        punkouter: windows::core::Ref<'_, windows::core::IUnknown>,
        riid: *const GUID,
        ppvobject: *mut *mut core::ffi::c_void,
    ) -> Result<()> {
        if punkouter.as_ref().is_some() {
            return Err(windows::core::Error::from(E_NOINTERFACE));
        }
        let tip: ITfTextInputProcessor = TspTip::default().into();
        unsafe { tip.query(&*riid, ppvobject).ok() }
    }

    fn LockServer(&self, _flock: BOOL) -> Result<()> {
        Ok(())
    }
}

// ── DLLエクスポート ──

/// # Safety
/// COMのDllGetClassObject規約に従う。
#[no_mangle]
pub unsafe extern "system" fn DllGetClassObject(
    rclsid: *const GUID,
    riid: *const GUID,
    ppv: *mut *mut core::ffi::c_void,
) -> HRESULT {
    if rclsid.is_null() || riid.is_null() || ppv.is_null() {
        return windows::Win32::Foundation::E_POINTER;
    }
    if *rclsid != CLSID_TIP {
        return windows::Win32::Foundation::CLASS_E_CLASSNOTAVAILABLE;
    }
    let factory: IClassFactory = ClassFactory.into();
    match factory.query(&*riid, ppv).ok() {
        Ok(()) => S_OK,
        Err(e) => e.code(),
    }
}

#[no_mangle]
pub extern "system" fn DllCanUnloadNow() -> HRESULT {
    S_FALSE
}

fn reg_set_sz(hkey: HKEY, value_name: PCWSTR, data: &str) -> Result<()> {
    let wdata = wide(data);
    let bytes: &[u8] =
        unsafe { std::slice::from_raw_parts(wdata.as_ptr().cast::<u8>(), wdata.len() * 2) };
    unsafe { RegSetValueExW(hkey, value_name, None, REG_SZ, Some(bytes)) }.ok()
}

fn guid_to_reg_path(guid: &GUID) -> String {
    format!(
        "{{{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
        guid.data1,
        guid.data2,
        guid.data3,
        guid.data4[0],
        guid.data4[1],
        guid.data4[2],
        guid.data4[3],
        guid.data4[4],
        guid.data4[5],
        guid.data4[6],
        guid.data4[7],
    )
}

/// # Safety
/// COMのDllRegisterServer規約に従う。regsvr32経由(管理者権限)での呼び出しを想定。
#[no_mangle]
pub unsafe extern "system" fn DllRegisterServer() -> HRESULT {
    match register_inner() {
        Ok(()) => {
            log("DllRegisterServer: OK");
            S_OK
        }
        Err(e) => {
            log(&format!("DllRegisterServer: FAILED {e}"));
            e.code()
        }
    }
}

fn register_inner() -> Result<()> {
    let dll_path = own_module_path()?;
    let clsid_path = format!("CLSID\\{}", guid_to_reg_path(&CLSID_TIP));
    let inproc_path = format!("{clsid_path}\\InprocServer32");

    unsafe {
        let mut hkey = HKEY::default();
        let clsid_path_w = wide(&clsid_path);
        RegCreateKeyExW(
            HKEY_CLASSES_ROOT,
            PCWSTR(clsid_path_w.as_ptr()),
            None,
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut hkey,
            None,
        )
        .ok()?;
        reg_set_sz(hkey, PCWSTR::null(), "awaza TIP")?;
        RegCloseKey(hkey).ok()?;

        let mut hkey2 = HKEY::default();
        let inproc_path_w = wide(&inproc_path);
        RegCreateKeyExW(
            HKEY_CLASSES_ROOT,
            PCWSTR(inproc_path_w.as_ptr()),
            None,
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut hkey2,
            None,
        )
        .ok()?;
        reg_set_sz(hkey2, PCWSTR::null(), &dll_path)?;
        reg_set_sz(hkey2, w!("ThreadingModel"), "Apartment")?;
        RegCloseKey(hkey2).ok()?;
    }

    unsafe {
        let category_mgr: ITfCategoryMgr =
            CoCreateInstance(&CLSID_TF_CategoryMgr, None, CLSCTX_INPROC_SERVER)?;
        category_mgr.RegisterCategory(&CLSID_TIP, &GUID_TFCAT_TIP_KEYBOARD, &CLSID_TIP)?;
        // spikeで判明した必須セット(§6.1参照)。1つでも欠けると「一覧に出るが
        // 選択できない」症状になる。
        category_mgr.RegisterCategory(&CLSID_TIP, &GUID_TFCAT_TIPCAP_IMMERSIVESUPPORT, &CLSID_TIP)?;
        category_mgr.RegisterCategory(&CLSID_TIP, &GUID_TFCAT_TIPCAP_SYSTRAYSUPPORT, &CLSID_TIP)?;

        let profiles: ITfInputProcessorProfiles =
            CoCreateInstance(&CLSID_TF_InputProcessorProfiles, None, CLSCTX_INPROC_SERVER)?;
        profiles.Register(&CLSID_TIP)?;
        let desc = wide("awaza");
        let icon = wide(&dll_path);
        profiles.AddLanguageProfile(&CLSID_TIP, LANGID_JA_JP, &GUID_PROFILE, &desc, &icon, 0)?;
    }

    log(&format!(
        "registered CLSID={} dll={dll_path}",
        guid_to_reg_path(&CLSID_TIP)
    ));
    Ok(())
}

/// # Safety
/// COMのDllUnregisterServer規約に従う。
#[no_mangle]
pub unsafe extern "system" fn DllUnregisterServer() -> HRESULT {
    match unregister_inner() {
        Ok(()) => S_OK,
        Err(e) => e.code(),
    }
}

fn unregister_inner() -> Result<()> {
    unsafe {
        if let Ok(profiles) = CoCreateInstance::<_, ITfInputProcessorProfiles>(
            &CLSID_TF_InputProcessorProfiles,
            None,
            CLSCTX_INPROC_SERVER,
        ) {
            let _ = profiles.Unregister(&CLSID_TIP);
        }
        if let Ok(category_mgr) =
            CoCreateInstance::<_, ITfCategoryMgr>(&CLSID_TF_CategoryMgr, None, CLSCTX_INPROC_SERVER)
        {
            let _ = category_mgr.UnregisterCategory(&CLSID_TIP, &GUID_TFCAT_TIP_KEYBOARD, &CLSID_TIP);
            let _ = category_mgr.UnregisterCategory(
                &CLSID_TIP,
                &GUID_TFCAT_TIPCAP_IMMERSIVESUPPORT,
                &CLSID_TIP,
            );
            let _ = category_mgr.UnregisterCategory(
                &CLSID_TIP,
                &GUID_TFCAT_TIPCAP_SYSTRAYSUPPORT,
                &CLSID_TIP,
            );
        }
        let clsid_path = format!("CLSID\\{}", guid_to_reg_path(&CLSID_TIP));
        let clsid_path_w = wide(&clsid_path);
        let _ = RegDeleteTreeW(HKEY_CLASSES_ROOT, PCWSTR(clsid_path_w.as_ptr()));
        let _ = UnregisterClassW(TIMER_WNDCLASS, None);
    }
    Ok(())
}
