use ashpd::desktop::global_shortcuts::{
    BindShortcutsOptions, GlobalShortcuts, NewShortcut,
};
use ashpd::desktop::CreateSessionOptions;
use futures_util::StreamExt;
use log::{error, info};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use tauri::AppHandle;
use tokio::sync::{mpsc, RwLock};

use crate::settings::{get_settings, KeyboardImplementation, ShortcutBinding};
use super::handler::handle_shortcut_event;

// Commands we can send to our background worker
enum Command {
    Register(ShortcutBinding),
    Unregister(String),
    RebuildSession, 
}

static COMMAND_TX: OnceLock<mpsc::UnboundedSender<Command>> = OnceLock::new();
static STARTED: AtomicBool = AtomicBool::new(false);

pub fn init_shortcuts(app: &AppHandle) {
    if STARTED.swap(true, Ordering::SeqCst) {
        return; // Already started
    }

    let (tx, rx) = mpsc::unbounded_channel();
    if COMMAND_TX.set(tx).is_err() {
        STARTED.store(false, Ordering::SeqCst);
        return;
    }

    let app = app.clone();
    // Spawn the background worker (The "Mailbox" reader)
    tauri::async_runtime::spawn(async move {
        if let Err(e) = run_portal(app.clone(), rx).await {
            error!("XDG Global Shortcuts portal unavailable: {e}; falling back to Tauri");
            
            // Fallback to Tauri if the Linux Portal isn't available
            let mut settings = get_settings(&app);
            if settings.keyboard_implementation == KeyboardImplementation::WaylandNative {
                settings.keyboard_implementation = KeyboardImplementation::Tauri;
                crate::settings::write_settings(&app, settings);
            }
            super::tauri_impl::init_shortcuts(&app);
        }
    });
}

// Top-level async function to avoid closure lifetime inference issues
async fn bind_session(
    bindings: &[ShortcutBinding],
    proxy: &GlobalShortcuts,
) -> Option<ashpd::desktop::Session<GlobalShortcuts>> {
    let new_session = match proxy.create_session(CreateSessionOptions::default()).await {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to create XDG session: {e}");
            return None;
        }
    };

    let shortcuts: Vec<NewShortcut> = bindings
        .iter()
        .map(|b| {
            let trigger = b.current_binding.replace('+', "-");
            NewShortcut::new(b.id.clone(), b.description.clone())
                .preferred_trigger(Some(trigger.as_str()))
        })
        .collect();

    // FIX #3: Properly handle the ashpd Request object
    let request = match proxy
        .bind_shortcuts(
            &new_session,
            &shortcuts,
            None, // WindowIdentifier is optional
            BindShortcutsOptions::default(),
        )
        .await
    {
        Ok(req) => req,
        Err(e) => {
            error!("Failed to bind XDG shortcuts: {e}");
            return None;
        }
    };

    // Await the portal's response (handles potential user dialogs or rejections)
    match request.response().await {
        Ok(_) => {
            info!("XDG Global Shortcuts session created and bound.");
            Some(new_session)
        }
        Err(e) => {
            error!("XDG Portal rejected shortcut binding or failed: {e}");
            None
        }
    }
}

async fn run_portal(
    app: AppHandle,
    mut rx: mpsc::UnboundedReceiver<Command>,
) -> Result<(), String> {
    // 1. Connect to the Portal
    let proxy = GlobalShortcuts::new()
        .await
        .map_err(|e| format!("failed to create GlobalShortcuts proxy: {e}"))?;

    // The "Guest List" to soft-unregister shortcuts
    let active_ids = Arc::new(RwLock::new(HashSet::<String>::new()));
    let mut current_bindings: Vec<ShortcutBinding> = Vec::new();

    // CRITICAL: Keep the session alive! If this drops, the portal destroys the shortcuts.
    let mut current_session: Option<ashpd::desktop::Session<GlobalShortcuts>> = None;

    // 2. Spawn Activated Listener (Key Pressed)
    let act_proxy = proxy.clone();
    let act_app = app.clone();
    let act_ids = active_ids.clone();
    tauri::async_runtime::spawn(async move {
        if let Ok(mut stream) = act_proxy.receive_activated().await {
            while let Some(sig) = stream.next().await {
                let id = sig.shortcut_id().to_owned();
                if act_ids.read().await.contains(&id) {
                    // Passing &id for both arguments is functionally correct for the handler
                    handle_shortcut_event(&act_app, &id, &id, true);
                }
            }
        }
    });

    // 3. Spawn Deactivated Listener (Key Released)
    let deact_proxy = proxy.clone();
    let deact_app = app.clone();
    let deact_ids = active_ids.clone();
    tauri::async_runtime::spawn(async move {
        if let Ok(mut stream) = deact_proxy.receive_deactivated().await {
            while let Some(sig) = stream.next().await {
                let id = sig.shortcut_id().to_owned();
                if deact_ids.read().await.contains(&id) {
                    handle_shortcut_event(&deact_app, &id, &id, false);
                }
            }
        }
    });

    // Initial load from settings
    let settings = get_settings(&app);
    for (id, binding) in &settings.bindings {
        // FIX #2: DO NOT SKIP CANCEL! 
        // We must bind it at startup so the XDG portal knows it exists and emits signals.
        // if id == "cancel" { continue; } // <--- REMOVED
        
        if id == "transcribe_with_post_process" && !settings.post_process_enabled { continue; }
        current_bindings.push(binding.clone());
        active_ids.write().await.insert(id.clone());
    }
    
    // Store the session so it doesn't drop immediately
    current_session = bind_session(&current_bindings, &proxy).await;

    // 4. Process the Mailbox (Commands from the main app)
    while let Some(cmd) = rx.recv().await {
        match cmd {
            Command::Register(b) => {
                active_ids.write().await.insert(b.id.clone());
            }
            Command::Unregister(id) => {
                active_ids.write().await.remove(&id);
            }
            Command::RebuildSession => {
                let settings = get_settings(&app);
                current_bindings.clear();
                let mut new_active = HashSet::new();
                
                // Optimization: Clone set ONCE to avoid lock contention in the loop
                let currently_active = active_ids.read().await.clone();
                
                for (id, binding) in &settings.bindings {
                    // FIX #2: DO NOT SKIP CANCEL HERE EITHER!
                    // if id == "cancel" && !currently_active.contains(id) { continue; } // <--- REMOVED
                    
                    if id == "transcribe_with_post_process" && !settings.post_process_enabled { continue; }
                    current_bindings.push(binding.clone());
                    new_active.insert(id.clone());
                }
                
                *active_ids.write().await = new_active;
                
                // FIX #4: Explicitly close the old session to prevent DBus leaks
                if let Some(old_session) = current_session.take() {
                    if let Err(e) = old_session.close().await {
                        error!("Failed to close old XDG session: {e}");
                    }
                }
                
                // Rebuild the session (this cleanly drops the old session and keeps the new one alive)
                current_session = bind_session(&current_bindings, &proxy).await;
            }
        }
    }
    Ok(())
}

// Synchronous wrappers (No freezing/blocking!)
pub fn register_shortcut(_app: &AppHandle, binding: ShortcutBinding) -> Result<(), String> {
    COMMAND_TX
        .get()
        .ok_or_else(|| "Wayland shortcut backend is not initialized".to_string())?
        .send(Command::Register(binding))
        .map_err(|e| e.to_string())
}

pub fn unregister_shortcut(_app: &AppHandle, binding: ShortcutBinding) -> Result<(), String> {
    COMMAND_TX
        .get()
        .ok_or_else(|| "Wayland shortcut backend is not initialized".to_string())?
        .send(Command::Unregister(binding.id))
        .map_err(|e| e.to_string())
}

pub fn rebuild_session() -> Result<(), String> {
    COMMAND_TX
        .get()
        .ok_or_else(|| "Wayland shortcut backend is not initialized".to_string())?
        .send(Command::RebuildSession)
        .map_err(|e| e.to_string())
}

pub fn validate_shortcut(raw: &str) -> Result<(), String> {
    if raw.trim().is_empty() {
        return Err("Shortcut cannot be empty".into());
    }
    Ok(())
}
