use async_std::channel::{bounded, Receiver, Sender};
use async_std::fs;
use async_std::io;
use async_std::io::prelude::*;
use async_std::io::BufReader;
use async_std::process::Command;
use async_std::stream::StreamExt;
use async_std::sync::{Arc, Mutex};
use async_std::task;
use comfy_table::{presets::UTF8_FULL, Cell, CellAlignment, Color, ContentArrangement, Table};
use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
struct MinerOutput {
    balance: String,
    claimable: String,
    status: String,
    public_key: String,
    attempts: u64,
    wallet_path: String,
    instance_id: usize,
    last_success: Option<Instant>,
    total_ore_mined: f64,
    total_hits: u64,
    tracking_start: Instant,
}

#[async_std::main]
async fn main() {
    let json_folder = Path::new("wallets");
    let rpc_urls: Vec<String> = vec![
        "YOUR_RPCS_HERE",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let mut handles: Vec<task::JoinHandle<()>> = Vec::new();

    let mut dir_entries = fs::read_dir(json_folder).await.expect("Read dir failed");
    let mut paths = Vec::new();
    while let Some(entry) = dir_entries.next().await {
        let entry = entry.expect("Failed to read dir entry");
        let path = entry.path();
        if path.extension().map_or(false, |ext| ext == "json") {
            paths.push(path);
        }
    }

    let session_start = Instant::now();
    let (miner_tx, miner_rx) = bounded(100);
    let state = Arc::new(Mutex::new(HashMap::new()));
    let dashboard_state = Arc::clone(&state);

    task::spawn(async move {
        update_dashboard(dashboard_state, miner_rx, session_start).await;
    });

    let num_instances_per_wallet = 3;
    let num_instances = paths.len() * num_instances_per_wallet;
    let distributed_rpc_urls = round_robin_rpc_urls(&rpc_urls, num_instances_per_wallet * paths.len());

    for j in 0..num_instances_per_wallet {
        for (i, path) in paths.iter().enumerate() {
            let rpc_url = distributed_rpc_urls[i * num_instances_per_wallet + j].clone();
            let wallet_path = path.to_string_lossy().into_owned();
            let miner_tx = miner_tx.clone();

            let handle = task::spawn(async move {
                mine_keypair(&rpc_url, &wallet_path, i * num_instances_per_wallet + j, miner_tx).await;
            });
            handles.push(handle);
        }
    }

    futures::future::join_all(handles).await;
}

async fn mine_keypair(rpc_url: &str, wallet_path: &str, instance_id: usize, miner_tx: Sender<(String, MinerOutput)>) {
    let public_key = get_public_key_from_keypair(wallet_path).await;
    let max_attempts = 10;

    loop {
        let mut child = Command::new("ore")
            .args(&["--rpc", rpc_url, "--keypair", wallet_path, "mine"])
            .stdout(Stdio::piped())
            .spawn()
            .expect("Failed to start mining process");

        let stdout = child.stdout.take().expect("Failed to take stdout");
        let mut reader = BufReader::new(stdout).lines();

        let mut attempts = 0;
        let mut miner_output = MinerOutput {
            balance: "0 ORE".into(),
            claimable: "0 ORE".into(),
            status: "Starting...".into(),
            public_key: public_key.clone(),
            attempts: 0,
            wallet_path: wallet_path.to_owned(),
            instance_id,
            last_success: None,
            total_ore_mined: 0.0,
            total_hits: 0,
            tracking_start: Instant::now(),
        };

        let mut line_buffer = String::new();
        while let Some(result) = reader.next().await {
            let line = result.expect("Failed to read line");
            line_buffer.push_str(&line);
            line_buffer.push('\n');

            if line.contains("Transaction did not land") {
                attempts += 1;
                miner_output.attempts = attempts;
                if attempts > max_attempts {
                    if let Err(e) = child.kill() {
                        eprintln!("Failed to kill mining process: {}", e);
                    }
                    let _ = child.status().await;
                    break;
                }
            } else {
                attempts = 0;
            }

            if line_buffer.len() >= 1000 {
                parse_lines(&line_buffer, &mut miner_output);
                miner_tx.send((wallet_path.to_owned(), miner_output.clone())).await;
                line_buffer.clear();
            }
        }

        if !line_buffer.is_empty() {
            parse_lines(&line_buffer, &mut miner_output);
            miner_tx.send((wallet_path.to_owned(), miner_output.clone())).await;
        }

        let _ = child.status().await;
    }
}

fn parse_lines(lines: &str, miner_output: &mut MinerOutput) {
    for line in lines.lines() {
        parse_line(line, miner_output);
    }
}

fn parse_line(line: &str, miner_output: &mut MinerOutput) {
    if line.starts_with("Balance:") {
        miner_output.balance = line.split_whitespace().nth(1).unwrap_or_default().into();
    } else if line.starts_with("Claimable:") {
        let new_claimable = parse_amount(line.split_whitespace().nth(1).unwrap_or_default());
        let claimable_diff = new_claimable - parse_amount(&miner_output.claimable);
        miner_output.total_ore_mined += claimable_diff;
        miner_output.claimable = format!("{:.9} ORE", new_claimable);
    } else if line.contains("valid hash") {
        miner_output.status = "Mining...".into();
    } else if line.contains("Transaction landed") {
        miner_output.status = "Success".into();
        miner_output.attempts = 0;
        miner_output.last_success = Some(Instant::now());
        miner_output.total_hits += 1;
    } else if line.contains("Transaction did not land") {
        miner_output.attempts += 1;
        miner_output.status = "Sending...".into();
    }
}

async fn update_dashboard(state: Arc<Mutex<HashMap<String, Vec<MinerOutput>>>>, mut miner_rx: Receiver<(String, MinerOutput)>, session_start: Instant) {
    let mut interval = async_std::stream::interval(Duration::from_millis(1000));
    while let Some(_) = interval.next().await {
        let mut batch = Vec::new();
        while let Ok((wallet_path, miner_output)) = miner_rx.try_recv() {
            batch.push((wallet_path, miner_output));
        }

        if !batch.is_empty() {
            let mut state_lock = state.lock().await;
            for (wallet_path, miner_output) in batch {
                let miner_outputs = state_lock.entry(wallet_path).or_insert_with(Vec::new);
                if let Some(index) = miner_outputs.iter().position(|o| o.instance_id == miner_output.instance_id) {
                    miner_outputs[index] = miner_output;
                } else {
                    miner_outputs.push(miner_output);
                }
            }
        }

        let state_lock = state.lock().await;
        render_dashboard(&*state_lock, session_start).await;
    }
}

async fn render_dashboard(state: &HashMap<String, Vec<MinerOutput>>, session_start: Instant) {
    let mut table = Table::new();
    table.set_content_arrangement(ContentArrangement::Dynamic)
         .load_preset(UTF8_FULL)
         .set_width(120)
         .set_header(vec![
             Cell::new("Wallet").fg(Color::Cyan).set_alignment(CellAlignment::Center),
             Cell::new("Balance").fg(Color::Cyan).set_alignment(CellAlignment::Center),
             Cell::new("Claimable").fg(Color::Cyan).set_alignment(CellAlignment::Center),
             Cell::new("Instances").fg(Color::Cyan).set_alignment(CellAlignment::Center),
         ]);

    let mut total_balance: f64 = 0.0;
    let mut total_claimable: f64 = 0.0;

    let mut rows = Vec::new();

    for (wallet_path, outputs) in state.iter() {
        let mut instance_status_cells = Vec::new();
        let mut wallet_balance = 0.0;
        let mut wallet_claimable = 0.0;

        let mut sending_attempts = Vec::new();
        let mut mining_count = 0;

        for output in outputs {
            let balance = parse_amount(&output.balance);
            let claimable = parse_amount(&output.claimable);

            wallet_balance = balance;
            wallet_claimable = claimable;

            match output.status.as_str() {
                "Success" => {
                    if let Some(last_success) = output.last_success {
                        if last_success.elapsed() <= Duration::from_secs(2) {
                            instance_status_cells.push(Cell::new("Success").fg(Color::Green));
                        }
                    }
                }
                "Sending..." => {
                    sending_attempts.push(output.attempts);
                }
                "Mining..." => {
                    mining_count += 1;
                }
                _ => {}
            }
        }

        if !sending_attempts.is_empty() {
            let sending_status = format!(
                "Sending... ({})",
                sending_attempts.iter().map(|a| a.to_string()).collect::<Vec<_>>().join(",")
            );
            instance_status_cells.push(Cell::new(sending_status).fg(Color::Yellow));
        }

        if mining_count > 0 {
            let mining_status = format!("Mining... ({} instances)", mining_count);
            instance_status_cells.push(Cell::new(mining_status).fg(Color::White));
        }

        total_balance += wallet_balance;
        total_claimable += wallet_claimable;

        rows.push(vec![
            Cell::new(wallet_path).fg(Color::Magenta),
            Cell::new(&format!("{:.9}", wallet_balance)).fg(Color::Yellow),
            Cell::new(&format!("{:.9}", wallet_claimable)).fg(Color::Yellow),
            Cell::new(instance_status_cells.iter().map(|cell| cell.content().to_string()).collect::<Vec<_>>().join("\n")).set_alignment(CellAlignment::Left),
        ]);
    }

    let elapsed_minutes = session_start.elapsed().as_secs_f64() / 60.0;
    let total_ore_mined = state.values().flatten().map(|o| o.total_ore_mined).sum::<f64>();
    let ore_per_minute = (total_ore_mined / elapsed_minutes) / 10.0;
    let hits_per_minute = state.values().flatten().map(|o| o.total_hits).sum::<u64>() as f64 / elapsed_minutes;

    rows.push(vec![
        Cell::new("Total").fg(Color::Cyan).set_alignment(CellAlignment::Right),
        Cell::new(&format!("{:.9}", total_balance)).fg(Color::Yellow),
        Cell::new(&format!("{:.9}", total_claimable)).fg(Color::Yellow),
        Cell::new("").fg(Color::White),
    ]);

    rows.push(vec![
        Cell::new("Rates").fg(Color::Cyan).set_alignment(CellAlignment::Right),
        Cell::new(&format!("{:.4}", ore_per_minute)).fg(Color::Yellow),
        Cell::new(&format!("{:.2} Hits/min", hits_per_minute)).fg(Color::Yellow),
        Cell::new("").fg(Color::White),
    ]);

    for row in rows {
        table.add_row(row);
    }

    print!("\x1b[H\x1b[J{}", table);
    io::stdout().flush().await.expect("Failed to flush stdout");
}

fn parse_amount(amount_str: &str) -> f64 {
    let cleaned_amount = amount_str.chars().filter(|&c| c.is_digit(10) || c == '.').collect::<String>();
    cleaned_amount.parse::<f64>().unwrap_or(0.0)
}

async fn get_public_key_from_keypair(wallet_path: &str) -> String {
    let output = Command::new("solana-keygen")
        .args(&["pubkey", wallet_path])
        .output()
        .await
        .expect("Failed to execute solana-keygen");

    if output.status.success() {
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    } else {
        eprintln!("Error getting public key: {}", String::from_utf8_lossy(&output.stderr));
        String::new()
    }
}

fn round_robin_rpc_urls(rpc_urls: &[String], num_instances: usize) -> Vec<String> {
    let mut distributed_urls = Vec::with_capacity(num_instances);
    for i in 0..num_instances {
        distributed_urls.push(rpc_urls[i % rpc_urls.len()].clone());
    }
    distributed_urls
}