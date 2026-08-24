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
    GUID_TFCAT_TIPCAP_SYSTRAYSUPPORT, GUID_TFCAT_TIP_KEYBOARD, TF_ANCHOR_END, TF_ES_READWRITE,
    TF_ES_SYNC, TF_ST_CORRECTION,
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
use awase::engine::{ModifierState, TIMER_PENDING, TIMER_SPECULATIVE};
use awase::types::{ImeRelevance, KeyAction, KeyClassification, KeyEventType, RawKeyEvent, VkCode};

/// 簡易的な単調増加ミリ秒タイムスタンプ(`Timestamp`は`u64`のtype alias)。
fn now_ms() -> u64 {
    use std::sync::OnceLock;
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    let start = START.get_or_init(std::time::Instant::now);
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}
use awase::yab::YabLayout;

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
                let state = &*(ptr as *const TipState);
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
}

impl TipState {
    fn new() -> Self {
        // Phase 0時点では最小限のNICOLAレイアウトを埋め込みで使う。
        // 本来はP0-6で`.yab`ファイルから読む(または`YabLayout::builtin()`、
        // design doc ADR-011)。ここでは空レイアウトで起動だけ確認する。
        let layout = YabLayout::parse("", awase::scanmap::KeyboardModel::Jis)
            .expect("空文字列のレイアウトは常にパースできる(awase-linux/src/main.rsの前例踏襲)")
            .resolve_kana();
        let chord = ChordEngine::new(
            layout,
            VkCode(VK_NONCONVERT),
            VkCode(VK_CONVERT),
            100,
            ConfirmMode::default(),
            30,
        );
        Self {
            tid: Cell::new(0),
            hwnd: Cell::new(HWND::default()),
            context: RefCell::new(None),
            chord: RefCell::new(chord),
            composition: RefCell::new(None),
            preedit: RefCell::new(String::new()),
        }
    }

    fn on_timer_fired(&self, timer_id: usize) {
        log(&format!("on_timer_fired timer_id={timer_id}"));
        let Some(ctx) = self.context.borrow().clone() else {
            log("on_timer_fired: no context, ignoring");
            return;
        };
        let phys = PhysicalKeyState::empty();
        let composing = self.composition.borrow().is_some();
        let resp = self.chord.borrow_mut().on_timeout(timer_id, &phys, composing);
        self.apply_response(&ctx, &resp);
    }

    /// `ChordResponse`(かな確定・投機出力等)を実際のpreedit更新に反映する。
    /// P0-3: `ITfInsertAtSelection`+`StartComposition`で初回表示、以降は
    /// `ITfRange::SetText`で更新する。
    fn apply_response(&self, ctx: &ITfContext, resp: &ChordResponse) {
        if resp.actions.is_empty() && resp.timers.is_empty() {
            return;
        }
        log(&format!("apply_response: {resp:?}"));

        // 現時点(P0-2/P0-2.5/P0-3の最初のパス)では、KeyAction::Char由来の
        // かな1文字をpreeditにそのまま反映するだけの最小実装とする。
        // KeyAction::Romaji(拗音)・SpecialKey(Backspace)・その他のバリアントは
        // P0-6/P1で本実装する(ここでは無視してログに残すのみ)。
        let mut text_to_show: Option<String> = None;
        for action in &resp.actions {
            match action {
                KeyAction::Char(c) => {
                    let mut s = self.preedit.borrow().clone();
                    s.push(*c);
                    text_to_show = Some(s);
                }
                other => {
                    log(&format!("apply_response: unhandled action {other:?} (P1で実装)"));
                }
            }
        }

        if let Some(text) = text_to_show {
            if let Err(e) = self.update_composition(ctx, &text) {
                log(&format!("apply_response: update_composition failed: {e}"));
            } else {
                self.preedit.replace(text);
            }
        }
    }

    fn update_composition(&self, ctx: &ITfContext, text: &str) -> Result<()> {
        let session = TipEditSession {
            state_context: ctx.clone(),
            text: text.to_owned(),
        };
        let session_iface: ITfEditSession = session.into();
        let tid = self.tid.get();
        unsafe {
            ctx.RequestEditSession(tid, &session_iface, TF_ES_SYNC | TF_ES_READWRITE)?;
        }
        Ok(())
    }
}

/// composition開始/更新を行うedit session。`DoEditSession`の中で
/// `ITfInsertAtSelection`/`StartComposition`/`ITfRange::SetText`を呼ぶ。
/// 実際のcomposition保持・差し替えロジックは`TipState`が持つため、この
/// セッションオブジェクトは`TipState`への参照を`OnKeyDown`側から都度
/// 渡してもらう設計にはせず、まず「preeditに文字を出す」ことだけを最小限
/// 実装する(P0-3の第一段階)。
#[implement(ITfEditSession)]
struct TipEditSession {
    state_context: ITfContext,
    text: String,
}

impl ITfEditSession_Impl for TipEditSession_Impl {
    fn DoEditSession(&self, ec: u32) -> Result<()> {
        unsafe {
            let insert: ITfInsertAtSelection = self.state_context.cast()?;
            let comp_services: ITfContextOwnerCompositionServices = self.state_context.cast()?;
            let wtext = wide(&self.text);
            // 末尾の\0は含めない(SetText/InsertTextAtSelectionはスライス長で判断する)。
            let wtext = &wtext[..wtext.len().saturating_sub(1)];
            let range: ITfRange = insert.InsertTextAtSelection(
                ec,
                windows::Win32::UI::TextServices::INSERT_TEXT_AT_SELECTION_FLAGS(0),
                wtext,
            )?;
            let comp_sink = TipCompositionSink;
            let comp_sink_iface: ITfCompositionSink = comp_sink.into();
            let composition = comp_services.StartComposition(ec, &range, &comp_sink_iface)?;
            log("DoEditSession: StartComposition OK");
            // このパスではcompositionを`TipState`に保持し直す処理を省略している
            // (次のキーイベントでのSetText差し替えはP1で実装する)。
            let _ = composition;
        }
        Ok(())
    }
}

/// composition強制終了(マウスクリック等)の検知。P0-3の受け入れ基準の一つ。
/// 現時点ではログ出力のみ(preedit状態のクリーンアップはP1で実装)。
#[implement(ITfCompositionSink)]
struct TipCompositionSink;

impl ITfCompositionSink_Impl for TipCompositionSink_Impl {
    fn OnCompositionTerminated(
        &self,
        _ecwrite: u32,
        _pcomposition: windows::core::Ref<'_, ITfComposition>,
    ) -> Result<()> {
        log("OnCompositionTerminated (外部からのcomposition終了)");
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
        let classification = classify_vk(vk);
        let event = RawKeyEvent {
            vk_code: VkCode(vk),
            scan_code: awase::types::ScanCode(0),
            event_type: KeyEventType::KeyDown,
            extra_info: 0,
            timestamp: now_ms(),
            key_classification: classification,
            physical_pos: None,
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
        self.state.apply_response(pic, &resp);

        // タイマー要求をWM_TIMERへ変換する(P0-2.5)。
        let hwnd = self.state.hwnd.get();
        for timer in &resp.timers {
            match timer {
                timed_fsm::TimerCommand::Set { id, duration } => unsafe {
                    let ms = u32::try_from(duration.as_millis()).unwrap_or(u32::MAX);
                    SetTimer(Some(hwnd), *id, ms, None);
                },
                timed_fsm::TimerCommand::Kill { id } => unsafe {
                    let _ = KillTimer(Some(hwnd), *id);
                },
            }
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

/// P0-6の暫定版。VK→分類。恒久実装は`awase-vkmap`(切り出し予定crate)で行う。
fn classify_vk(vk: u16) -> KeyClassification {
    match vk {
        VK_NONCONVERT => KeyClassification::LeftThumb,
        VK_CONVERT => KeyClassification::RightThumb,
        _ => KeyClassification::Passthrough,
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
