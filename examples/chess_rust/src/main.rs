use std::io::{self,BufRead,Write};
use serde_json::{Value,json};

fn pick_move(_fen:&str,allowed_moves:&[Value])->String {
    allowed_moves[0].as_str().unwrap().into()
}

fn main()->Result<(),Box<dyn std::error::Error>> {
    let mut out=io::stdout().lock();
    writeln!(out,"{{\"ready\":true}}")?;out.flush()?;
    for line in io::stdin().lock().lines() {
        let input:Value=serde_json::from_str(&line?)?;
        let choice=pick_move(input["fen"].as_str().unwrap(),input["allowed_moves"].as_array().unwrap());
        writeln!(out,"{}",json!({"id":input["id"],"move":choice}))?;out.flush()?;
    }
    Ok(())
}
