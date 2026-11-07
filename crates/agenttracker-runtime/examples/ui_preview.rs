//! Headless UI server for development/screenshots on machines without the Tauri GUI libraries.
//! Serves the same core server + runtime routes as the desktop shell's `--headless` mode.
//!
//!   cargo run -p agenttracker-runtime --example ui_preview -- <data_dir>
use agenttracker_core::collector::Collector;
use agenttracker_core::db::Db;
use agenttracker_core::engine::Engine;
use agenttracker_core::server::{serve_with, AppState};
use std::path::PathBuf;
use tracker_common::security;

fn main() {
    let data_dir = PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| "ui_preview_data".into()));
    let home = data_dir.join("home");
    std::fs::create_dir_all(&home).expect("create data dir");
    let db = Db::open(&data_dir.join("agenttracker.db")).expect("open database");
    let collector = Collector::new(&db);
    let engine = Engine::new(db, vec![data_dir.join("agenttracker.db"), home.clone()]);
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    // AT_PREVIEW_TOKEN lets local tooling open the UI with a known one-time token (dev only).
    let token = std::env::var("AT_PREVIEW_TOKEN").unwrap_or_else(|_| security::token_urlsafe(24));
    let state = AppState::new(engine, collector, port, Some(token.clone()), security::token_urlsafe(32), None, data_dir.clone(), home);
    println!("Open the UI (one-time link): http://127.0.0.1:{port}/launch?token={token}");
    tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap().block_on(async move {
        let l = tokio::net::TcpListener::from_std(listener).unwrap();
        serve_with(l, state, agenttracker_runtime::http::routes()).await.unwrap();
    });
}
