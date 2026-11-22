#![deny(unsafe_op_in_unsafe_fn)]
mod app;
#[cfg(all(test, windows))]
mod auto_device_tests;
mod catalog;
mod config;
mod deletion;
#[cfg(all(test, windows))]
mod diarization_tests;
mod http;
mod integrated;
mod jobs;
mod lifecycle;
mod migration;
mod model_files;
mod model_info;
mod models;
mod nemo;
mod network;
mod platform;
mod recognition;
mod restart;
mod runtime_bootstrap;
#[cfg(all(test, windows))]
mod runtime_bootstrap_tests;
mod runtime_install;
mod runtime_storage;
mod scheduler;
#[cfg(all(test, windows))]
mod scheduler_tests;
mod setup_control;
#[cfg(test)]
mod sqlite_compat_tests;
mod store;
mod streaming;
mod telemetry;
mod timing;
mod tls;
mod update_apply;
mod updates;
use clap::Parser;
fn main() -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()?;
    let restart = runtime.block_on(run())?;
    drop(runtime);
    if restart {
        restart::relaunch()?;
    }
    Ok(())
}
async fn run() -> anyhow::Result<bool> {
    let internal = std::env::args().collect::<Vec<_>>();
    if internal.get(1).is_some_and(|s| s == "__prepare-setup") {
        setup_control::run(&internal).await?;
        return Ok(false);
    }
    if internal.get(1).is_some_and(|s| s == "__apply-update") {
        let path = std::path::PathBuf::from(
            internal
                .get(2)
                .ok_or_else(|| anyhow::anyhow!("更新計画を指定してください"))?,
        );
        let result = update_apply::apply(&path).await;
        let code = match result {
            Ok(code) => code,
            Err(error) => {
                if let Some(parent) = path.parent() {
                    let _ = config::write_json(
                        &parent.join("result.json"),
                        &serde_json::json!({"state":"failed","message":"更新の準備または復旧を完了できません。"}),
                    );
                    let _ = std::fs::write(parent.join("error.log"), error.to_string());
                }
                1
            }
        };
        std::process::exit(code);
    }
    let args = config::Args::parse();
    if args.compact_media {
        let dir = args.data_dir.as_ref().unwrap();
        anyhow::ensure!(
            dir.is_absolute() && dir.join("workbench.sqlite3").is_file(),
            "既存データの絶対パスを指定してください"
        );
        // Do not let Store::open's normal startup recovery alter unfinished jobs.
        let db = rusqlite::Connection::open_with_flags(
            dir.join("workbench.sqlite3"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        let unfinished: i64 = db.query_row("SELECT count(*) FROM jobs WHERE json_extract(payload,'$.status') IN ('uploading','queued','running')", [], |r| r.get(0))?;
        anyhow::ensure!(
            unfinished == 0,
            "処理・待機・転送中のジョブがあります。完了後に実行してください"
        );
        drop(db);
        let store = store::Store::open(dir)?; // Holds the same exclusive lock as the HTTP server.
        println!("{}", store.compact_media(args.apply)?);
        return Ok(false);
    }
    let connection_guide = args.connection_guide;
    let runtime = config::Runtime::load(args)?;
    platform::set_title();
    let _instance = if runtime.installed {
        let instance = platform::Instance::new()?;
        if instance.existing {
            if runtime.open {
                platform::browser(&format!(
                    "{}://127.0.0.1:{}/{}",
                    if runtime.config.tls_cert.is_some() {
                        "https"
                    } else {
                        "http"
                    },
                    runtime.config.port,
                    if connection_guide { "?guide=1" } else { "" }
                ))
            }
            return Ok(false);
        }
        Some(instance)
    } else {
        None
    };
    let transport = tls::configuration(&runtime.config)?;
    let listener =
        tokio::net::TcpListener::bind((runtime.config.host.as_str(), runtime.config.port)).await?;
    let stop = tokio_util::sync::CancellationToken::new();
    let signal = stop.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            signal.cancel()
        }
    });
    let app = app::App::start(runtime, stop).await?;
    app.runtime.save()?;
    if let Some(path) = &app.runtime.process_record {
        platform::process_record(
            path,
            app.runtime.config.port,
            &app.runtime.config.instance_id,
        )?;
    }
    app.print_startup();
    updates::background(app.clone());
    if app.runtime.open {
        platform::browser(&format!(
            "{}://127.0.0.1:{}/{}",
            if app.policy.https { "https" } else { "http" },
            app.runtime.config.port,
            if connection_guide { "?guide=1" } else { "" }
        ))
    }
    let listener = tls::Listener::new(listener, transport, app.runtime.config.https_only);
    let serving = async {
        use axum::serve::ListenerExt;
        axum::serve(
            listener.tap_io(|_| {}),
            http::router(app.clone()).into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .with_graceful_shutdown(app.stop.clone().cancelled_owned())
        .await
    };
    let stop = app.stop.clone();
    // 停止要求後も応答中の接続を最大5秒待つ。
    let grace = async {
        stop.cancelled().await;
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    };
    tokio::select! {
        result = serving => result?,
        _ = grace => (),
    };
    app.stop.cancel();
    for job in app.store.active_jobs()? {
        app.store.update(
            job["id"].as_str().unwrap(),
            serde_json::json!({"message":"サーバーを停止しました。保存完了した未完了ジョブは再起動後に再実行します。",
                "metadata":{"restartPending":true}}),
        )?;
    }
    for _ in 0..50 {
        if app.running.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    Ok(app
        .restart_requested
        .load(std::sync::atomic::Ordering::Acquire))
}
