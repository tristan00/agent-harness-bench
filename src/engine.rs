use std::{error::Error, fmt, fmt::Write};

use shakmaty::{
    CastlingMode, Chess, Color, EnPassantMode, MoveList, Position, fen::Fen, uci::UciMove,
};

const MODE: CastlingMode = CastlingMode::Chess960;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Side {
    White,
    Black,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Outcome {
    WhiteWin,
    BlackWin,
    Draw,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutcomeReason {
    Checkmate,
    Stalemate,
    InsufficientMaterial,
    ThreefoldRepetition,
    FiftyMoveRule,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
pub struct MoveMetrics {
    pub capture: bool,
    pub en_passant: bool,
    pub castling: bool,
    pub promotion: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EngineError {
    InvalidStartIndex(u16),
    InvalidMove,
    IllegalMove,
    GameOver,
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidStartIndex(index) => write!(f, "Chess960 index {index} is outside 0..960"),
            Self::InvalidMove => f.write_str("invalid UCI move"),
            Self::IllegalMove => f.write_str("illegal Chess960 move"),
            Self::GameOver => f.write_str("game has ended"),
        }
    }
}

impl Error for EngineError {}

/// A Chess960 game with position history since the last pawn move or capture.
///
/// The arena claims draws automatically when a position occurs for the third
/// time or the halfmove clock reaches 100. Claims apply to the position reached
/// after a move. Checkmate, stalemate and insufficient material take precedence.
#[derive(Clone, Debug)]
pub struct Game {
    history: Vec<Chess>,
    legal: MoveList,
    reason: Option<OutcomeReason>,
}

impl Game {
    pub fn new(index: u16) -> Result<Self, EngineError> {
        if index >= 960 {
            return Err(EngineError::InvalidStartIndex(index));
        }
        let white = back_rank(usize::from(index));
        let black = white.to_ascii_lowercase();
        let fen = format!("{black}/pppppppp/8/8/8/8/PPPPPPPP/{white} w KQkq - 0 1");
        let position = fen
            .parse::<Fen>()
            .expect("generated start FEN")
            .into_position(MODE)
            .expect("generated legal Chess960 start");
        Ok(Self::from_position(position))
    }

    fn from_position(position: Chess) -> Self {
        let mut game = Self {
            legal: position.legal_moves(),
            history: vec![position],
            reason: None,
        };
        game.reason = game.detect_outcome();
        game
    }

    fn position(&self) -> &Chess {
        self.history.last().expect("game has an initial position")
    }

    /// Returns Shredder-FEN with rook-file castling rights and the FEN en passant target.
    pub fn fen(&self) -> String {
        let position = self.position();
        let mut fen = String::with_capacity(96);
        position.board().board_fen().append_to_string(&mut fen);
        fen.push_str(match position.turn() {
            Color::White => " w ",
            Color::Black => " b ",
        });
        let rights = position.castles().castling_rights();
        for color in [Color::White, Color::Black] {
            for rook in (rights & color.backrank()).into_iter().rev() {
                let file = rook.file().char();
                fen.push(if color == Color::White {
                    file.to_ascii_uppercase()
                } else {
                    file
                });
            }
        }
        if rights.is_empty() {
            fen.push('-');
        }
        fen.push(' ');
        if let Some(square) = position.ep_square(EnPassantMode::Always) {
            square.append_to_string(&mut fen);
        } else {
            fen.push('-');
        }
        write!(fen, " {} {}", position.halfmoves(), position.fullmoves())
            .expect("writing to a String cannot fail");
        fen
    }

    pub fn turn(&self) -> Side {
        match self.position().turn() {
            Color::White => Side::White,
            Color::Black => Side::Black,
        }
    }

    /// Returns the playable UCI960 moves. A finished game has no playable moves.
    pub fn legal_moves(&self) -> Vec<String> {
        if self.reason.is_some() {
            return Vec::new();
        }
        self.legal
            .iter()
            .map(|m| {
                let mut uci = String::with_capacity(5);
                m.to_uci(MODE).append_to_string(&mut uci);
                uci
            })
            .collect()
    }

    pub fn play(&mut self, uci: &str) -> Result<(), EngineError> {
        self.play_with_metrics(uci).map(|_| ())
    }

    pub fn play_with_metrics(&mut self, uci: &str) -> Result<MoveMetrics, EngineError> {
        if self.reason.is_some() {
            return Err(EngineError::GameOver);
        }
        let notation = uci
            .parse::<UciMove>()
            .map_err(|_| EngineError::InvalidMove)?;
        let position = self.position();
        let m = notation
            .to_move(position)
            .map_err(|_| EngineError::IllegalMove)?;
        // Shakmaty also accepts standard UCI castling. Require the UCI960 spelling.
        if notation != m.to_uci(MODE) {
            return Err(EngineError::IllegalMove);
        }

        let mut next = position.clone();
        next.play_unchecked(m);
        if next.halfmoves() == 0 {
            self.history.clear();
        }
        self.legal = next.legal_moves();
        self.history.push(next);
        self.reason = self.detect_outcome();
        Ok(MoveMetrics {
            capture: m.is_capture(),
            en_passant: m.is_en_passant(),
            castling: m.is_castle(),
            promotion: m.is_promotion(),
        })
    }

    pub fn outcome(&self) -> Option<Outcome> {
        Some(match self.reason? {
            OutcomeReason::Checkmate => match self.turn() {
                Side::White => Outcome::BlackWin,
                Side::Black => Outcome::WhiteWin,
            },
            _ => Outcome::Draw,
        })
    }

    pub fn outcome_reason(&self) -> Option<OutcomeReason> {
        self.reason
    }

    fn detect_outcome(&self) -> Option<OutcomeReason> {
        let position = self.position();
        if self.legal.is_empty() {
            return Some(if position.is_check() {
                OutcomeReason::Checkmate
            } else {
                OutcomeReason::Stalemate
            });
        }
        if position.is_insufficient_material() {
            return Some(OutcomeReason::InsufficientMaterial);
        }
        // Only earlier positions with the same turn can repeat. Chess equality
        // includes castling rights and legal en passant availability.
        if self
            .history
            .iter()
            .rev()
            .skip(2)
            .step_by(2)
            .filter(|past| *past == position)
            .nth(1)
            .is_some()
        {
            return Some(OutcomeReason::ThreefoldRepetition);
        }
        if position.halfmoves() >= 100 {
            return Some(OutcomeReason::FiftyMoveRule);
        }
        None
    }
}

fn back_rank(index: usize) -> String {
    // Scharnagl numbering places the bishops, queen, and knight pair in that order.
    const KNIGHTS: [(usize, usize); 10] = [
        (0, 1),
        (0, 2),
        (0, 3),
        (0, 4),
        (1, 2),
        (1, 3),
        (1, 4),
        (2, 3),
        (2, 4),
        (3, 4),
    ];
    let mut rank = [' '; 8];
    rank[2 * (index % 4) + 1] = 'B';
    rank[2 * (index / 4 % 4)] = 'B';
    let mut open: Vec<_> = (0..8).filter(|&file| rank[file] == ' ').collect();
    rank[open.remove(index / 16 % 6)] = 'Q';
    let (first, second) = KNIGHTS[index / 96];
    rank[open.remove(second)] = 'N';
    rank[open.remove(first)] = 'N';
    for (file, piece) in open.into_iter().zip(['R', 'K', 'R']) {
        rank[file] = piece;
    }
    rank.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn position(fen: &str) -> Game {
        Game::from_position(fen.parse::<Fen>().unwrap().into_position(MODE).unwrap())
    }

    fn play(game: &mut Game, moves: &[&str]) {
        for m in moves {
            game.play(m).unwrap_or_else(|error| panic!("{m}: {error}"));
        }
    }

    #[test]
    fn all_scharnagl_starts() {
        let mut seen = HashSet::new();
        for index in 0..960 {
            let game = Game::new(index).unwrap();
            let fen = game.fen();
            let fields: Vec<_> = fen.split_whitespace().collect();
            let ranks: Vec<_> = fields[0].split('/').collect();
            let rank = ranks[7];
            assert!(seen.insert(rank.to_owned()));
            assert_eq!(ranks[0], rank.to_ascii_lowercase());
            assert_eq!(&ranks[1..7], &["pppppppp", "8", "8", "8", "8", "PPPPPPPP"]);
            let mut pieces: Vec<_> = rank.chars().collect();
            pieces.sort_unstable();
            assert!(pieces.into_iter().eq("BBKNNQRR".chars()));
            let bishops: Vec<_> = rank.match_indices('B').map(|(file, _)| file).collect();
            assert_ne!(bishops[0] % 2, bishops[1] % 2);
            let left = rank.find('R').unwrap();
            let right = rank.rfind('R').unwrap();
            assert!((left + 1..right).contains(&rank.find('K').unwrap()));
            let rights: String = (*b"Aa")
                .into_iter()
                .flat_map(|base| [right, left].map(|file| char::from(base + file as u8)))
                .collect();
            assert_eq!(fields[2], rights);
            assert_eq!(&fields[3..], &["-", "0", "1"]);
            assert_eq!(game.turn(), Side::White);
            assert_eq!(game.outcome(), None);
            assert!(
                game.legal_moves().into_iter().eq(game
                    .position()
                    .legal_moves()
                    .into_iter()
                    .map(|m| m.to_uci(MODE).to_string()))
            );
        }
        assert_eq!(seen.len(), 960);
        assert_eq!(back_rank(0), "BBQNNRKR");
        assert_eq!(
            Game::new(518).unwrap().fen(),
            "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w HAha - 0 1"
        );
        assert_eq!(back_rank(959), "RKRNNQBB");
        assert_eq!(
            Game::new(960).unwrap_err(),
            EngineError::InvalidStartIndex(960)
        );
    }

    fn perft(game: &Game, depth: u8) -> u64 {
        if depth == 0 {
            return 1;
        }
        game.legal_moves()
            .iter()
            .map(|m| {
                let mut child = game.clone();
                child.play(m).unwrap();
                perft(&child, depth - 1)
            })
            .sum()
    }

    #[test]
    fn perft_through_public_move_api() {
        let orthodox = Game::new(518).unwrap();
        // First fixture from the Chess960 perft suite distributed with shakmaty.
        let fischer = position("bqnb1rkr/pp3ppp/3ppn2/2p5/5P2/P2P4/NPP1P1PP/BQ1BNRKR w HFhf - 2 9");
        for (game, counts) in [(orthodox, [20, 400, 8902]), (fischer, [21, 528, 12189])] {
            for (depth, expected) in (1..=3).zip(counts) {
                assert_eq!(perft(&game, depth), expected);
            }
        }
    }

    #[test]
    fn overlapping_castles_and_rights() {
        for (rank, right, m, final_rank) in [
            ("6KR", "H", "g1h1", "5RK1"),
            ("4KR2", "F", "e1f1", "5RK1"),
            ("4K1R1", "G", "e1g1", "5RK1"),
            ("5K1R", "H", "f1h1", "5RK1"),
            ("5KR1", "G", "f1g1", "5RK1"),
            ("2RK4", "C", "d1c1", "2KR4"),
        ] {
            let mut game = position(&format!("k7/8/8/8/8/8/8/{rank} w {right} - 0 1"));
            assert!(game.legal_moves().contains(&m.to_owned()));
            assert_eq!(
                game.play_with_metrics(m).unwrap(),
                MoveMetrics {
                    capture: false,
                    en_passant: false,
                    castling: true,
                    promotion: false,
                }
            );
            assert_eq!(game.fen(), format!("k7/8/8/8/8/8/8/{final_rank} b - - 1 1"));
        }
        let mut game = position("r3k2r/8/8/8/8/8/8/R3K2R w HAha - 0 1");
        assert_eq!(game.play("e1g1"), Err(EngineError::IllegalMove));
        play(&mut game, &["h1h2", "h8h7", "h2h1", "h7h8"]);
        assert_eq!(game.fen().split_whitespace().nth(2), Some("Aa"));
        assert_ne!(game.history.first(), game.history.last());
        assert_eq!(
            game.play_with_metrics("a1a8").unwrap(),
            MoveMetrics {
                capture: true,
                en_passant: false,
                castling: false,
                promotion: false,
            }
        );
        assert_eq!(game.fen().split_whitespace().nth(2), Some("-"));
    }

    #[test]
    fn en_passant_promotion_and_illegal_moves() {
        let mut game = Game::new(518).unwrap();
        let original = game.fen();
        assert_eq!(game.play("e2e5"), Err(EngineError::IllegalMove));
        assert_eq!(game.play("e7e5"), Err(EngineError::IllegalMove));
        assert_eq!(game.play("bad"), Err(EngineError::InvalidMove));
        assert_eq!(game.play("0000"), Err(EngineError::IllegalMove));
        assert_eq!(game.history.len(), 1);
        assert_eq!(game.fen(), original);

        play(&mut game, &["e2e4"]);
        assert_eq!(game.turn(), Side::Black);
        assert_eq!(game.fen().split_whitespace().nth(3), Some("e3"));
        play(&mut game, &["a7a6", "e4e5", "d7d5"]);
        assert_eq!(
            game.play_with_metrics("e5d6").unwrap(),
            MoveMetrics {
                capture: true,
                en_passant: true,
                castling: false,
                promotion: false,
            }
        );
        assert_eq!(
            game.fen(),
            "rnbqkbnr/1pp1pppp/p2P4/8/8/8/PPPP1PPP/RNBQKBNR b HAha - 0 3"
        );

        let pinned = position("k3r3/8/8/3pP3/8/8/8/4K3 w - d6 0 1");
        let no_target = position("k3r3/8/8/3pP3/8/8/8/4K3 w - - 0 1");
        assert!(!pinned.legal_moves().contains(&"e5d6".to_owned()));
        assert_eq!(pinned.position(), no_target.position());
        let legal = position("k7/8/8/3pP3/8/8/8/4K3 w - d6 0 1");
        let expired = position("k7/8/8/3pP3/8/8/8/4K3 w - - 0 1");
        assert_ne!(legal.position(), expired.position());

        let promotion = position("7k/P7/8/8/8/8/8/7K w - - 0 1");
        for piece in ['q', 'r', 'b', 'n'] {
            let mut game = promotion.clone();
            assert_eq!(
                game.play_with_metrics(&format!("a7a8{piece}")).unwrap(),
                MoveMetrics {
                    capture: false,
                    en_passant: false,
                    castling: false,
                    promotion: true,
                }
            );
            assert!(game.fen().starts_with(piece.to_ascii_uppercase()));
        }
        assert_eq!(
            promotion.clone().play("a7a8"),
            Err(EngineError::IllegalMove)
        );
    }

    #[test]
    fn draws_and_terminal_precedence() {
        let mut game = Game::new(518).unwrap();
        let cycle = ["g1f3", "g8f6", "f3g1", "f6g8"];
        play(&mut game, &cycle);
        assert_eq!(game.outcome(), None);
        play(&mut game, &cycle);
        assert_eq!(game.history.len(), 9);
        assert_eq!(game.outcome(), Some(Outcome::Draw));
        assert_eq!(
            game.outcome_reason(),
            Some(OutcomeReason::ThreefoldRepetition)
        );
        assert!(game.legal_moves().is_empty());
        assert_eq!(game.play("g1f3"), Err(EngineError::GameOver));

        let mut game = Game::new(518).unwrap();
        play(&mut game, &cycle);
        play(&mut game, &["e2e4", "e7e5"]);
        assert_eq!(game.history.len(), 1);
        play(&mut game, &cycle);
        assert_eq!(game.outcome(), None);
        play(&mut game, &cycle);
        assert_eq!(
            game.outcome_reason(),
            Some(OutcomeReason::ThreefoldRepetition)
        );

        let mut game = position("7k/8/8/8/8/8/P7/R3K3 w - - 98 50");
        play(&mut game, &["a1b1"]);
        assert_eq!(game.outcome(), None);
        play(&mut game, &["h8g8"]);
        assert_eq!(game.outcome(), Some(Outcome::Draw));
        assert_eq!(game.outcome_reason(), Some(OutcomeReason::FiftyMoveRule));
        let mut reset = position("7k/8/8/8/8/8/P7/R3K3 w - - 99 50");
        play(&mut reset, &["a2a3"]);
        assert_eq!(reset.outcome(), None);
        assert_eq!(reset.position().halfmoves(), 0);

        let mut mate = position("7k/8/5KQ1/8/8/8/8/8 w - - 99 50");
        play(&mut mate, &["g6g7"]);
        assert_eq!(mate.outcome(), Some(Outcome::WhiteWin));
        assert_eq!(mate.outcome_reason(), Some(OutcomeReason::Checkmate));
        let mut fools_mate = Game::new(518).unwrap();
        play(&mut fools_mate, &["f2f3", "e7e5", "g2g4", "d8h4"]);
        assert_eq!(fools_mate.outcome(), Some(Outcome::BlackWin));
        for (fen, reason) in [
            (
                "7k/5Q2/6K1/8/8/8/8/8 b - - 100 50",
                OutcomeReason::Stalemate,
            ),
            (
                "7k/8/8/8/8/8/8/7K w - - 0 1",
                OutcomeReason::InsufficientMaterial,
            ),
        ] {
            let game = position(fen);
            assert_eq!(game.outcome(), Some(Outcome::Draw));
            assert_eq!(game.outcome_reason(), Some(reason));
        }
    }
}
