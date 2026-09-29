// src-tauri/src/paste_tx/linux.rs
use tauri::AppHandle;
use crate::settings::{AutoSubmitKey, ClipboardHandling, PasteMethod};

pub(super) fn run(
    _text: &str,
    _app_handle: &AppHandle,
    _paste_method: &PasteMethod,
    _enigo: &mut enigo::Enigo,
    _auto_submit: bool,
    _auto_submit_key: AutoSubmitKey,
    _clipboard_handling: ClipboardHandling,
) -> Result<(), String> {
    log::info!("[linux-paste] Linux reliable paste module loaded. Falling back to legacy behavior.");
    Err("Linux reliable paste is not yet implemented. Falling back.".to_string())
}
