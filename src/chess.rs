use crate::{
    Result,
    engine::{Game, Outcome, Side},
    model::{Config, Manifest},
    process::{MOVE_TIMEOUT, Protocol},
    random,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::PathBuf;

#[derive(Clone)]
pub struct Entry {
    pub id: String,
    pub agent: String,
    pub manifest: Option<Manifest>,
    pub directory: PathBuf,
    pub error: Option<String>,
    pub lines_of_code: Option<usize>,
    pub artifact_sha256: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Pairing {
    pub white: usize,
    pub black: usize,
    pub position: u16,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Ply {
    pub fen: String,
    pub chosen_move: String,
    pub elapsed_ms: f64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GameRecord {
    pub pairing: Pairing,
    pub white: String,
    pub black: String,
    pub white_score: f64,
    pub black_score: f64,
    pub reason: String,
    pub error: Option<String>,
    pub moves: Vec<Ply>,
    pub final_fen: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Standing {
    pub id: String,
    pub agent: String,
    pub initial_elo: f64,
    pub final_elo: f64,
    pub elo_change: f64,
    pub games: usize,
    pub wins: usize,
    pub draws: usize,
    pub losses: usize,
    pub forfeits: usize,
    pub lines_of_code: Option<usize>,
    pub artifact_sha256: Option<String>,
}
pub fn schedule(config: &Config, players: usize) -> Vec<Pairing> {
    let mut rng = config.seed;
    let mut positions: Vec<u16> = (0..960).collect();
    shuffle(&mut positions, &mut rng);
    let mut games = Vec::new();
    for &position in positions.iter().take(config.positions as usize) {
        for a in 0..players {
            for b in a + 1..players {
                games.push(Pairing {
                    white: a,
                    black: b,
                    position,
                });
                games.push(Pairing {
                    white: b,
                    black: a,
                    position,
                });
            }
        }
    }
    shuffle(&mut games, &mut rng);
    games
}
fn shuffle<T>(items: &mut [T], rng: &mut u64) {
    for i in (1..items.len()).rev() {
        items.swap(i, (random(rng) % (i as u64 + 1)) as usize);
    }
}
pub fn standings(entries: &[Entry], config: &Config) -> Vec<Standing> {
    entries
        .iter()
        .map(|e| Standing {
            id: e.id.clone(),
            agent: e.agent.clone(),
            initial_elo: config.initial_elo,
            final_elo: config.initial_elo,
            elo_change: 0.0,
            games: 0,
            wins: 0,
            draws: 0,
            losses: 0,
            forfeits: 0,
            lines_of_code: e.lines_of_code,
            artifact_sha256: e.artifact_sha256.clone(),
        })
        .collect()
}
pub fn rate(table: &mut [Standing], game: &GameRecord, k: f64) {
    let a = game.pairing.white;
    let b = game.pairing.black;
    let expected = 1.0 / (1.0 + 10_f64.powf((table[b].final_elo - table[a].final_elo) / 400.0));
    let delta_a = k * (game.white_score - expected);
    let delta_b = k * (game.black_score - (1.0 - expected));
    for (i, score, delta) in [
        (a, game.white_score, delta_a),
        (b, game.black_score, delta_b),
    ] {
        let s = &mut table[i];
        s.final_elo += delta;
        s.elo_change = s.final_elo - s.initial_elo;
        s.games += 1;
        if score == 1.0 {
            s.wins += 1;
        } else if score == 0.5 {
            s.draws += 1;
        } else {
            s.losses += 1;
            if game.reason.contains("forfeit") {
                s.forfeits += 1;
            }
        }
    }
}
fn launch(entry: &Entry) -> Result<Protocol> {
    if let Some(e) = &entry.error {
        return Err(e.clone().into());
    }
    Protocol::launch(
        entry.manifest.as_ref().ok_or("no submission")?,
        &entry.directory,
        None,
    )
}
pub fn play(entries: &[Entry], pairing: Pairing, max_plies: u16) -> Result<GameRecord> {
    let mut game = Game::new(pairing.position)?;
    let mut white = launch(&entries[pairing.white]);
    let mut black = launch(&entries[pairing.black]);
    let mut result = GameRecord {
        white: entries[pairing.white].id.clone(),
        black: entries[pairing.black].id.clone(),
        pairing,
        white_score: 0.5,
        black_score: 0.5,
        reason: "ply_limit".into(),
        error: None,
        moves: Vec::new(),
        final_fen: game.fen(),
    };
    match (&white, &black) {
        (Err(a), Err(b)) => {
            result.white_score = 0.0;
            result.black_score = 0.0;
            result.reason = "double_forfeit".into();
            result.error = Some(format!("white: {a}; black: {b}"));
            return Ok(result);
        }
        (Err(e), _) => {
            result.white_score = 0.0;
            result.black_score = 1.0;
            result.reason = "white_start_forfeit".into();
            result.error = Some(e.to_string());
            return Ok(result);
        }
        (_, Err(e)) => {
            result.white_score = 1.0;
            result.black_score = 0.0;
            result.reason = "black_start_forfeit".into();
            result.error = Some(e.to_string());
            return Ok(result);
        }
        _ => {}
    }
    for _ in 0..max_plies {
        let turn = game.turn();
        let moves = game.legal_moves();
        let fen = game.fen();
        let player = if turn == Side::White {
            white.as_mut().unwrap()
        } else {
            black.as_mut().unwrap()
        };
        let picked = (|| -> Result<(String, f64)> {
            let (response, elapsed) =
                player.exchange(json!({"fen":fen,"allowed_moves":moves}), MOVE_TIMEOUT)?;
            let chosen = response
                .get("move")
                .and_then(|m| m.as_str())
                .ok_or("response requires a move string")?;
            if !moves.iter().any(|m| m == chosen) {
                return Err("move is not in allowed_moves".into());
            }
            Ok((chosen.into(), elapsed))
        })();
        match picked {
            Err(e) => {
                result.white_score = if turn == Side::White { 0.0 } else { 1.0 };
                result.black_score = 1.0 - result.white_score;
                result.reason = if turn == Side::White {
                    "white_move_forfeit"
                } else {
                    "black_move_forfeit"
                }
                .into();
                result.error = Some(e.to_string());
                break;
            }
            Ok((chosen_move, elapsed_ms)) => {
                game.play(&chosen_move)?;
                result.moves.push(Ply {
                    fen,
                    chosen_move,
                    elapsed_ms,
                });
                if let Some(outcome) = game.outcome() {
                    result.white_score = match outcome {
                        Outcome::WhiteWin => 1.0,
                        Outcome::BlackWin => 0.0,
                        Outcome::Draw => 0.5,
                    };
                    result.black_score = 1.0 - result.white_score;
                    result.reason = format!("{:?}", game.outcome_reason().unwrap());
                    break;
                }
            }
        }
    }
    result.final_fen = game.fen();
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn schedule_balances_colors_and_replays() {
        let c = Config {
            positions: 3,
            ..Config::default()
        };
        let games = schedule(&c, 4);
        assert_eq!(games.len(), 36);
        assert_eq!(
            serde_json::to_string(&games).unwrap(),
            serde_json::to_string(&schedule(&c, 4)).unwrap()
        );
        for g in &games {
            assert_eq!(
                games
                    .iter()
                    .filter(|r| r.position == g.position
                        && r.white == g.black
                        && r.black == g.white)
                    .count(),
                1
            );
        }
    }
    #[test]
    fn elo_uses_pre_game_ratings_and_records_double_losses() {
        let entries = (0..2)
            .map(|i| Entry {
                id: i.to_string(),
                agent: i.to_string(),
                manifest: None,
                directory: PathBuf::new(),
                error: None,
                lines_of_code: None,
                artifact_sha256: None,
            })
            .collect::<Vec<_>>();
        let mut table = standings(&entries, &Config::default());
        let mut g = GameRecord {
            pairing: Pairing {
                white: 0,
                black: 1,
                position: 518,
            },
            white: "0".into(),
            black: "1".into(),
            white_score: 1.0,
            black_score: 0.0,
            reason: "black_move_forfeit".into(),
            error: None,
            moves: vec![],
            final_fen: String::new(),
        };
        rate(&mut table, &g, 32.0);
        assert_eq!(table[0].elo_change, 16.0);
        assert_eq!(table[1].elo_change, -16.0);
        assert_eq!(table[1].forfeits, 1);
        g.white_score = 0.0;
        g.black_score = 0.0;
        g.reason = "double_forfeit".into();
        let before = table[0].final_elo + table[1].final_elo;
        rate(&mut table, &g, 32.0);
        assert!((before - table[0].final_elo - table[1].final_elo - 32.0).abs() < 1e-8);
    }
}
