use anyhow::{Result, bail};
use std::sync::atomic::Ordering;

/// Admission stays closed until this process exits; only an idle server restarts.
pub async fn request(app: &crate::app::App) -> Result<()> {
    let _gate = app.admission.lock().await;
    if app.maintenance.load(Ordering::Acquire)
        || app.draining.load(Ordering::Acquire)
        || app.update_busy.load(Ordering::Acquire)
        || app.incoming.load(Ordering::Acquire) > 0
        || !app.store.active_jobs()?.is_empty()
        || !app.running.lock().unwrap().is_empty()
    {
        bail!("CONFLICT: 処理・待機・転送・モデル操作・更新中は再起動できません。")
    }
    app.maintenance.store(true, Ordering::Release);
    app.restart_requested.store(true, Ordering::Release);
    let stop = app.stop.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        stop.cancel();
    });
    Ok(())
}

/// Called after the Tokio runtime drops every task, model and SQLite/instance lock.
pub fn relaunch() -> Result<()> {
    let executable = std::env::current_exe()?;
    let mut args = std::env::args_os()
        .skip(1)
        .filter(|arg| arg != "--open")
        .collect::<Vec<_>>();
    if !args.iter().any(|arg| arg == "--no-browser") {
        args.push("--no-browser".into());
    }
    let mut command = std::process::Command::new(executable);
    command.args(args).stdin(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        if unsafe { windows_sys::Win32::System::Console::GetConsoleWindow() }.is_null() {
            command.creation_flags(0x08000000);
        }
    }
    command.spawn()?;
    Ok(())
}
