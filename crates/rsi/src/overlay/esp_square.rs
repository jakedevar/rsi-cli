use crate::app::App;
use crate::types::OverlayState;
use crossterm::event::{KeyCode, KeyEvent};
use rand::Rng;
use std::time::{Duration, Instant};

/// Flash duration for hit/miss/pass feedback.
const FLASH_DURATION: Duration = Duration::from_millis(300);

/// Minimum delay before the same key can register a second guess.
const SAME_KEY_DEBOUNCE: Duration = Duration::from_millis(600);

/// Map key to grid index (0-8).
/// Layout: u=top-left(0), i=top-center(1), o=top-right(2),
///         j=mid-left(3), k=mid-center(4), l=mid-right(5),
///         m=bot-left(6), ,=bot-center(7), .=bot-right(8).
fn key_to_grid_index(c: char) -> Option<usize> {
    match c {
        'u' => Some(0),
        'i' => Some(1),
        'o' => Some(2),
        'j' => Some(3),
        'k' => Some(4),
        'l' => Some(5),
        'm' => Some(6),
        ',' => Some(7),
        '.' => Some(8),
        _ => None,
    }
}

/// Move cursor in the 3×3 grid. Returns new index after moving.
fn move_cursor(current: usize, direction: KeyCode) -> usize {
    let row = current / 3;
    let col = current % 3;
    match direction {
        KeyCode::Up => {
            if row > 0 {
                (row - 1) * 3 + col
            } else {
                current
            }
        }
        KeyCode::Down => {
            if row < 2 {
                (row + 1) * 3 + col
            } else {
                current
            }
        }
        KeyCode::Left => {
            if col > 0 {
                row * 3 + (col - 1)
            } else {
                current
            }
        }
        KeyCode::Right => {
            if col < 2 {
                row * 3 + (col + 1)
            } else {
                current
            }
        }
        _ => current,
    }
}

/// Binomial p-value: P(X >= k) for n=12, p=1/9.
pub fn esp_p_value(k: u8) -> f64 {
    let n = 12u8;
    let p: f64 = 1.0 / 9.0;
    let q: f64 = 1.0 - p;
    let mut cdf: f64 = 0.0;
    for i in 0..k {
        let coeff = binomial_coeff(n, i);
        cdf += coeff * p.powi(i32::from(i)) * q.powi(i32::from(n - i));
    }
    1.0 - cdf
}

fn binomial_coeff(n: u8, k: u8) -> f64 {
    if k > n {
        return 0.0;
    }
    let mut result = 1.0;
    for i in 0..k as u64 {
        result *= (n as u64 - i) as f64 / (i + 1) as f64;
    }
    result
}

/// Interpretation text for a p-value.
pub fn p_value_interpretation(p: f64) -> &'static str {
    if p <= 0.0001 {
        "Psychic"
    } else if p <= 0.001 {
        "Extremely significant"
    } else if p <= 0.01 {
        "Highly significant"
    } else if p <= 0.05 {
        "Significant"
    } else if p <= 0.15 {
        "Lucky"
    } else {
        "Normal"
    }
}

/// Build the game-over message and set it on the overlay.
fn set_game_over_message(correct: u8, message: &mut String) {
    let pv = esp_p_value(correct);
    let interp = p_value_interpretation(pv);
    *message = format!(
        "Game Over — Score: {}/12 — p = {:.4} ({})",
        correct, pv, interp
    );
}

/// Serialize round details to JSON for persistence.
/// Each entry includes pick, target, hit, and guess_ms (milliseconds since game start).
fn serialize_round_details(details: &[Option<(usize, usize, u64)>]) -> String {
    let json_vals: Vec<serde_json::Value> = details
        .iter()
        .map(|r| match r {
            Some((pick, target, guess_ms)) => serde_json::json!({
                "pick": pick,
                "target": target,
                "hit": pick == target,
                "guess_ms": guess_ms
            }),
            None => serde_json::json!({"pass": true}),
        })
        .collect();
    serde_json::to_string(&json_vals).unwrap_or_default()
}

/// Set flash with 300ms auto-clear deadline.
fn set_flash(
    flash: &mut Option<(usize, bool)>,
    flash_deadline: &mut Option<Instant>,
    idx: usize,
    hit: bool,
) {
    *flash = Some((idx, hit));
    *flash_deadline = Some(Instant::now() + FLASH_DURATION);
}

pub async fn handle_esp_square_key(app: &mut App, key: KeyEvent) {
    // Extract a pick if this is a guess action (numpad or Enter with cursor).
    // We determine the pick first, then process it uniformly.
    let pick = match key.code {
        KeyCode::Enter => {
            if let OverlayState::EspSquare {
                cursor: Some(pos),
                round,
                ..
            } = &app.overlay
            {
                if *round < 12 { Some(*pos) } else { None }
            } else {
                None
            }
        }
        KeyCode::Char(c) => {
            if let OverlayState::EspSquare { round, .. } = &app.overlay {
                if *round < 12 {
                    key_to_grid_index(c)
                } else {
                    None
                }
            } else {
                None
            }
        }
        _ => None,
    };

    let (
        round,
        correct,
        interactive,
        rounds,
        message,
        flash,
        flash_deadline,
        target,
        round_details,
        cursor,
        last_guess,
        started_at,
    ) = match &mut app.overlay {
        OverlayState::EspSquare {
            round,
            correct,
            interactive,
            rounds,
            message,
            flash,
            flash_deadline,
            target,
            round_details,
            cursor,
            last_guess,
            started_at,
        } => (
            round,
            correct,
            interactive,
            rounds,
            message,
            flash,
            flash_deadline,
            target,
            round_details,
            cursor,
            last_guess,
            started_at,
        ),
        _ => return,
    };

    // The hint offers r at every stage; Enter starts a new game only when it
    // is not selecting a square.
    if key.code == KeyCode::Char('r') || (key.code == KeyCode::Enter && pick.is_none()) {
        *round = 0;
        *correct = 0;
        rounds.clear();
        round_details.clear();
        message.clear();
        *flash = None;
        *flash_deadline = None;
        *target = rand::thread_rng().gen_range(0..9usize);
        *cursor = None;
        *last_guess = None;
        *started_at = Instant::now();
        return;
    }

    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.overlay = OverlayState::None;
            return;
        }
        // Toggle interactive
        KeyCode::Char('t') if *round < 12 => {
            *interactive = !*interactive;
        }
        // Pass — flash the correct square in red, then pick a new target
        KeyCode::Char(';') if *round < 12 => {
            set_flash(flash, flash_deadline, *target, false);
            *target = rand::thread_rng().gen_range(0..9usize);
        }
        // Arrow keys — show/move cursor
        KeyCode::Up | KeyCode::Down | KeyCode::Left | KeyCode::Right if *round < 12 => {
            match *cursor {
                None => {
                    // First arrow press: show cursor at square 7 (index 0, top-left)
                    *cursor = Some(0);
                }
                Some(pos) => {
                    *cursor = Some(move_cursor(pos, key.code));
                }
            }
        }
        // Enter or numpad guess — handled via `pick` computed above
        KeyCode::Enter | KeyCode::Char(_) if pick.is_some() => {
            // Debounce: ignore same-key repeats within SAME_KEY_DEBOUNCE window.
            if let KeyCode::Char(c) = key.code {
                let now = Instant::now();
                if let Some((last_c, last_t)) = *last_guess {
                    if last_c == c && now.duration_since(last_t) < SAME_KEY_DEBOUNCE {
                        return;
                    }
                }
                *last_guess = Some((c, now));
            }
            let pick = pick.unwrap();
            let current_target = *target;
            let guess_ms = started_at.elapsed().as_millis() as u64;
            let hit = pick == current_target;
            if hit {
                *correct += 1;
                set_flash(flash, flash_deadline, pick, true);
            } else {
                set_flash(flash, flash_deadline, current_target, false);
            }
            rounds.push(Some(hit));
            round_details.push(Some((pick, current_target, guess_ms)));
            *round += 1;
            *target = rand::thread_rng().gen_range(0..9usize);
            if *round == 12 {
                set_game_over_message(*correct, message);
                let details_str = serialize_round_details(round_details);
                let rounds_played = rounds.iter().filter(|r| r.is_some()).count() as u8;
                let pv = esp_p_value(*correct);
                let _ = app
                    .client
                    .save_esp_game(*correct, rounds_played, 12, pv, &details_str)
                    .await;
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::DaemonClient;
    use crossterm::event::KeyModifiers;
    use std::path::PathBuf;

    #[tokio::test]
    async fn r_resets_active_and_finished_games() {
        for round in [5, 12] {
            let mut app = App::new(DaemonClient::new(PathBuf::from("/tmp/test.sock")));
            app.overlay = OverlayState::EspSquare {
                round,
                correct: 3,
                interactive: true,
                rounds: vec![Some(true)],
                message: "played".into(),
                flash: Some((1, true)),
                flash_deadline: Some(Instant::now()),
                target: 0,
                round_details: vec![Some((0, 0, 1))],
                cursor: Some(2),
                last_guess: Some(('u', Instant::now())),
                started_at: Instant::now(),
            };
            handle_esp_square_key(
                &mut app,
                KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE),
            )
            .await;
            let OverlayState::EspSquare {
                round,
                correct,
                rounds,
                message,
                cursor,
                ..
            } = &app.overlay
            else {
                panic!("esp square")
            };
            assert_eq!((*round, *correct), (0, 0));
            assert!(rounds.is_empty());
            assert!(message.is_empty());
            assert_eq!(*cursor, None);
        }
    }

    #[test]
    fn test_key_to_grid_index() {
        assert_eq!(key_to_grid_index('u'), Some(0));
        assert_eq!(key_to_grid_index('i'), Some(1));
        assert_eq!(key_to_grid_index('o'), Some(2));
        assert_eq!(key_to_grid_index('j'), Some(3));
        assert_eq!(key_to_grid_index('k'), Some(4));
        assert_eq!(key_to_grid_index('l'), Some(5));
        assert_eq!(key_to_grid_index('m'), Some(6));
        assert_eq!(key_to_grid_index(','), Some(7));
        assert_eq!(key_to_grid_index('.'), Some(8));
        assert_eq!(key_to_grid_index('a'), None);
        assert_eq!(key_to_grid_index('1'), None);
    }

    #[test]
    fn test_binomial_coeff() {
        assert!((binomial_coeff(12, 0) - 1.0).abs() < 1e-10);
        assert!((binomial_coeff(12, 1) - 12.0).abs() < 1e-10);
        assert!((binomial_coeff(12, 6) - 924.0).abs() < 1e-6);
        assert!((binomial_coeff(12, 12) - 1.0).abs() < 1e-10);
        assert_eq!(binomial_coeff(5, 7), 0.0); // k > n
    }

    #[test]
    fn test_esp_p_value() {
        let p0 = esp_p_value(0);
        assert!((p0 - 1.0).abs() < 1e-10, "p(0) = {}", p0);

        let p1 = esp_p_value(1);
        assert!(p1 > 0.7 && p1 < 0.8, "p(1) = {}", p1);

        let p4 = esp_p_value(4);
        assert!(p4 > 0.02 && p4 < 0.06, "p(4) = {}", p4);

        let p12 = esp_p_value(12);
        assert!(p12 > 0.0, "p(12) = {}", p12);

        for k in 1..=12u8 {
            assert!(
                esp_p_value(k) <= esp_p_value(k - 1),
                "p({}) > p({})",
                k,
                k - 1
            );
        }
    }

    #[test]
    fn test_p_value_interpretation() {
        assert_eq!(p_value_interpretation(1.0), "Normal");
        assert_eq!(p_value_interpretation(0.10), "Lucky");
        assert_eq!(p_value_interpretation(0.03), "Significant");
        assert_eq!(p_value_interpretation(0.005), "Highly significant");
        assert_eq!(p_value_interpretation(0.0005), "Extremely significant");
        assert_eq!(p_value_interpretation(0.00001), "Psychic");
    }

    #[test]
    fn test_serialize_round_details() {
        let details = vec![Some((4, 7, 1200)), None, Some((2, 2, 5400))];
        let json = serialize_round_details(&details);
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0]["pick"], 4);
        assert_eq!(parsed[0]["target"], 7);
        assert_eq!(parsed[0]["hit"], false);
        assert_eq!(parsed[0]["guess_ms"], 1200);
        assert_eq!(parsed[1]["pass"], true);
        assert_eq!(parsed[2]["pick"], 2);
        assert_eq!(parsed[2]["target"], 2);
        assert_eq!(parsed[2]["hit"], true);
        assert_eq!(parsed[2]["guess_ms"], 5400);
    }

    #[test]
    fn test_move_cursor() {
        // Top-left corner (0) — can't go up or left
        assert_eq!(move_cursor(0, KeyCode::Up), 0);
        assert_eq!(move_cursor(0, KeyCode::Left), 0);
        assert_eq!(move_cursor(0, KeyCode::Down), 3);
        assert_eq!(move_cursor(0, KeyCode::Right), 1);

        // Center (4)
        assert_eq!(move_cursor(4, KeyCode::Up), 1);
        assert_eq!(move_cursor(4, KeyCode::Down), 7);
        assert_eq!(move_cursor(4, KeyCode::Left), 3);
        assert_eq!(move_cursor(4, KeyCode::Right), 5);

        // Bottom-right corner (8) — can't go down or right
        assert_eq!(move_cursor(8, KeyCode::Down), 8);
        assert_eq!(move_cursor(8, KeyCode::Right), 8);
        assert_eq!(move_cursor(8, KeyCode::Up), 5);
        assert_eq!(move_cursor(8, KeyCode::Left), 7);
    }
}
