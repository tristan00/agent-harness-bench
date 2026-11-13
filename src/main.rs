use agent_harness_bench::{Result, service::Service};
use serde_json::{Value, json};
use std::{
    io::{self, BufRead, Write},
    path::PathBuf,
};

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("baseline") {
        return baseline(args.get(2).map(String::as_str).unwrap_or("random"));
    }
    if args.iter().any(|s| s == "--help" || s == "-h") {
        println!(
            "harness-bench serve [--bind 127.0.0.1:8741] [--data data]\nLocal Rust benchmark evaluator. Admin/judge tokens are generated in the data directory.\nUse scripts/harness.py for batches, tasks and submissions."
        );
        return Ok(());
    }
    let mut bind = "127.0.0.1:8741".to_owned();
    let mut data = PathBuf::from("data");
    let mut i = 1;
    if args.get(i).map(String::as_str) == Some("serve") {
        i += 1;
    }
    while i < args.len() {
        match args[i].as_str() {
            "--bind" => {
                i += 1;
                bind = args.get(i).ok_or("missing --bind value")?.clone();
            }
            "--data" => {
                i += 1;
                data = PathBuf::from(args.get(i).ok_or("missing --data value")?);
            }
            arg => return Err(format!("unknown argument: {arg}").into()),
        }
        i += 1;
    }
    Service::open(&data)?.serve(bind.parse()?)
}
fn baseline(kind: &str) -> Result<()> {
    let stdin = io::stdin();
    let mut out = io::stdout().lock();
    writeln!(out, "{{\"ready\":true}}")?;
    out.flush()?;
    for line in stdin.lock().lines() {
        let value: Value = serde_json::from_str(&line?)?;
        let moves = value["allowed_moves"].as_array().ok_or("missing moves")?;
        let fen = value["fen"].as_str().ok_or("missing FEN")?;
        let mut seed = 0xcbf29ce484222325u64;
        for byte in fen.bytes() {
            seed = (seed ^ u64::from(byte)).wrapping_mul(0x100000001b3);
        }
        let chosen = if kind == "capture" {
            let mut squares = [b' '; 64];
            let mut n = 0;
            for ch in fen.split_whitespace().next().unwrap_or("").bytes() {
                if ch == b'/' {
                    continue;
                }
                if ch.is_ascii_digit() {
                    n += (ch - b'0') as usize;
                } else {
                    if n < 64 {
                        squares[n] = ch;
                    }
                    n += 1;
                }
            }
            moves
                .iter()
                .max_by_key(|m| {
                    let text = m.as_str().unwrap_or("").as_bytes();
                    if text.len() < 4 {
                        return 0;
                    }
                    let file = (text[2] - b'a') as usize;
                    let rank = (b'8' - text[3]) as usize;
                    let piece = squares[rank * 8 + file];
                    let white = fen.split_whitespace().nth(1) == Some("w");
                    let capture = if piece != b' ' && piece.is_ascii_uppercase() != white {
                        match piece.to_ascii_lowercase() {
                            b'q' => 9,
                            b'r' => 5,
                            b'b' | b'n' => 3,
                            b'p' => 1,
                            _ => 0,
                        }
                    } else {
                        0
                    };
                    capture * 10 + if text.get(4) == Some(&b'q') { 9 } else { 0 }
                })
                .ok_or("no moves")?
        } else {
            &moves[(agent_harness_bench::random(&mut seed) % moves.len() as u64) as usize]
        };
        writeln!(out, "{}", json!({"id":value["id"],"move":chosen}))?;
        out.flush()?;
    }
    Ok(())
}
