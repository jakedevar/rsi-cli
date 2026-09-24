use crate::app::App;
use crate::types::{OverlayState, PopupMode};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

const DECLINE_RESPONSE: &str = "(declined — no preference; use your best judgment)";

pub fn open_question_modal(app: &mut App) {
    // Find the target session — prefer currently focused, fall back to first WaitingApproval
    let session_id = app.selected_session_id();
    let session = if let Some(sid) = session_id {
        if let Some(state) = app.sessions.get(&sid) {
            if state.session.pending_question.is_some() {
                Some(state.session.clone())
            } else {
                app.sessions
                    .values()
                    .find(|s| s.session.pending_question.is_some())
                    .map(|s| s.session.clone())
            }
        } else {
            app.sessions
                .values()
                .find(|s| s.session.pending_question.is_some())
                .map(|s| s.session.clone())
        }
    } else {
        app.sessions
            .values()
            .find(|s| s.session.pending_question.is_some())
            .map(|s| s.session.clone())
    };

    let session = match session {
        Some(s) => s,
        None => {
            app.notify("No sessions waiting for input");
            return;
        }
    };

    let questions = match &session.pending_question {
        Some(pq) => pq.questions.clone(),
        None => {
            app.notify("No pending question for this session");
            return;
        }
    };

    let num_questions = questions.len();
    let mut textarea = tui_textarea::TextArea::default();
    textarea.set_cursor_line_style(ratatui::style::Style::default());

    let selections = questions
        .iter()
        .map(|q| {
            if q.multi_select {
                crate::types::QuestionSelection::Multi(vec![])
            } else {
                crate::types::QuestionSelection::Single(None)
            }
        })
        .collect();

    app.overlay = OverlayState::QuestionModal {
        session_id: session.id,
        questions,
        current_question: 0,
        cursor: vec![0; num_questions],
        selections,
        textarea: Box::new(textarea),
        mode: PopupMode::Normal,
        pending_operator: None,
    };
}

pub async fn handle_question_modal_key(app: &mut App, key: KeyEvent) {
    let submit_on_enter = app.settings.submit_on_enter;

    // Read current popup mode without holding a borrow across the submit await.
    let current_mode = match &app.overlay {
        OverlayState::QuestionModal { mode, .. } => *mode,
        _ => return,
    };

    // Ctrl+Enter submits from any mode (legacy accelerator).
    let ctrl_submit = key.code == KeyCode::Enter && key.modifiers.contains(KeyModifiers::CONTROL);

    // Plain Enter submits when:
    //   - submit_on_enter is true (default), AND
    //   - we are in Insert mode (so the Normal-mode "advance question" branch
    //     below at the `KeyCode::Enter` arm still works), AND
    //   - no modifiers (Shift+Enter falls through to textarea.input(key) for
    //     newline insertion — same mechanism as input_surface chokepoint).
    let plain_enter_submit = submit_on_enter
        && current_mode == PopupMode::Insert
        && key.code == KeyCode::Enter
        && key.modifiers == KeyModifiers::NONE;

    if ctrl_submit || plain_enter_submit {
        submit_question_response(app).await;
        return;
    }

    if current_mode == PopupMode::Normal
        && key.code == KeyCode::Char('d')
        && key.modifiers == KeyModifiers::NONE
    {
        decline_question_response(app).await;
        return;
    }

    if let OverlayState::QuestionModal {
        mode,
        current_question,
        questions,
        cursor,
        selections,
        textarea,
        ..
    } = &mut app.overlay
    {
        match mode {
            PopupMode::Normal => match key.code {
                KeyCode::Char('q') | KeyCode::Esc => {
                    app.restore_previous_overlay();
                }
                KeyCode::Char('i') => {
                    *mode = PopupMode::Insert;
                    let cq = *current_question;
                    if questions[cq].multi_select {
                        selections[cq] = crate::types::QuestionSelection::Multi(vec![]);
                    } else {
                        selections[cq] = crate::types::QuestionSelection::Single(None);
                    }
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    let cq = *current_question;
                    let num_options = questions[cq].options.len();
                    if num_options > 0 {
                        cursor[cq] = (cursor[cq] + 1) % num_options;
                        // Single-select commits option immediately on cursor move
                        if !questions[cq].multi_select {
                            selections[cq] =
                                crate::types::QuestionSelection::Single(Some(cursor[cq]));
                            // Clear textarea when option is selected
                            *textarea = Box::new(tui_textarea::TextArea::default());
                            textarea.set_cursor_line_style(ratatui::style::Style::default());
                        }
                    }
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    let cq = *current_question;
                    let num_options = questions[cq].options.len();
                    if num_options > 0 {
                        cursor[cq] = (cursor[cq] + num_options - 1) % num_options;
                        // Single-select commits option immediately on cursor move
                        if !questions[cq].multi_select {
                            selections[cq] =
                                crate::types::QuestionSelection::Single(Some(cursor[cq]));
                            // Clear textarea when option is selected
                            *textarea = Box::new(tui_textarea::TextArea::default());
                            textarea.set_cursor_line_style(ratatui::style::Style::default());
                        }
                    }
                }
                KeyCode::Char(' ') => {
                    let cq = *current_question;
                    let num_options = questions[cq].options.len();
                    if num_options > 0 {
                        let opt_idx = cursor[cq];
                        match &mut selections[cq] {
                            crate::types::QuestionSelection::Single(opt) => {
                                *opt = Some(opt_idx);
                            }
                            crate::types::QuestionSelection::Multi(opts) => {
                                if let Some(pos) = opts.iter().position(|&x| x == opt_idx) {
                                    opts.remove(pos);
                                } else {
                                    opts.push(opt_idx);
                                }
                            }
                        }
                        // Clear textarea when option is toggled/selected
                        *textarea = Box::new(tui_textarea::TextArea::default());
                        textarea.set_cursor_line_style(ratatui::style::Style::default());
                    }
                }
                KeyCode::Char(c) if c.is_ascii_digit() => {
                    if let Some(digit) = c.to_digit(10) {
                        if digit > 0 {
                            let idx = (digit - 1) as usize;
                            let cq = *current_question;
                            if idx < questions[cq].options.len() {
                                cursor[cq] = idx;
                                match &mut selections[cq] {
                                    crate::types::QuestionSelection::Single(opt) => {
                                        *opt = Some(idx);
                                    }
                                    crate::types::QuestionSelection::Multi(opts) => {
                                        if let Some(pos) = opts.iter().position(|&x| x == idx) {
                                            opts.remove(pos);
                                        } else {
                                            opts.push(idx);
                                        }
                                    }
                                }
                                // Clear textarea when option is selected
                                *textarea = Box::new(tui_textarea::TextArea::default());
                                textarea.set_cursor_line_style(ratatui::style::Style::default());
                            }
                        }
                    }
                }
                KeyCode::Enter => {
                    let total_questions = questions.len();
                    if *current_question + 1 < total_questions {
                        *current_question += 1;
                        *textarea = Box::new(tui_textarea::TextArea::default());
                        textarea.set_cursor_line_style(ratatui::style::Style::default());
                    }
                }
                KeyCode::Backspace => {
                    if *current_question > 0 {
                        *current_question -= 1;
                    }
                }
                _ => {}
            },
            PopupMode::Insert => match key.code {
                KeyCode::Esc => {
                    *mode = PopupMode::Normal;
                }
                _ => {
                    textarea.input(key);
                    // Deselect options if user types
                    let cq = *current_question;
                    if questions[cq].multi_select {
                        selections[cq] = crate::types::QuestionSelection::Multi(vec![]);
                    } else {
                        selections[cq] = crate::types::QuestionSelection::Single(None);
                    }
                }
            },
        }
    }
}

pub fn format_answers(
    questions: &[rsi_common::types::QuestionItem],
    selections: &[crate::types::QuestionSelection],
    textarea_text: &str,
) -> Result<String, String> {
    let mut answers = Vec::new();

    for (idx, q) in questions.iter().enumerate() {
        let response = match &selections[idx] {
            crate::types::QuestionSelection::Single(Some(opt_idx)) => {
                if *opt_idx < q.options.len() {
                    q.options[*opt_idx].label.clone()
                } else {
                    return Err(format!("Invalid option index for question {}", idx + 1));
                }
            }
            crate::types::QuestionSelection::Multi(opts) if !opts.is_empty() => {
                let mut sorted_opts = opts.clone();
                sorted_opts.sort_unstable();
                let mut labels = Vec::new();
                for &o in &sorted_opts {
                    if o < q.options.len() {
                        labels.push(q.options[o].label.clone());
                    } else {
                        return Err(format!("Invalid option index for question {}", idx + 1));
                    }
                }
                labels.join(", ")
            }
            _ => {
                if idx == questions.len() - 1 {
                    textarea_text.trim().to_string()
                } else {
                    String::new()
                }
            }
        };

        if response.is_empty() {
            return Err(format!("Please answer question {}", idx + 1));
        }

        let header = if q.header.is_empty() {
            "Question".to_string()
        } else {
            q.header.clone()
        };
        answers.push((header, response));
    }

    let final_response = if answers.len() == 1 {
        answers[0].1.clone()
    } else {
        answers
            .iter()
            .enumerate()
            .map(|(i, (header, ans))| format!("Q{} ({}): {}", i + 1, header, ans))
            .collect::<Vec<_>>()
            .join("\n")
    };

    if final_response.is_empty() {
        return Err("Cannot submit empty response".to_string());
    }

    Ok(final_response)
}

pub async fn submit_question_response(app: &mut App) {
    if let OverlayState::QuestionModal {
        session_id,
        questions,
        selections,
        textarea,
        ..
    } = &mut app.overlay
    {
        let textarea_text = textarea.lines().join("\n");
        let final_response = match format_answers(questions, selections, &textarea_text) {
            Ok(resp) => resp,
            Err(err_msg) => {
                app.notify(err_msg);
                return;
            }
        };

        let sid = *session_id;
        app.restore_previous_overlay();

        if let Err(e) = app.client.answer_question(sid, final_response).await {
            app.notify(format!("Failed to answer: {}", e));
        }
    }
}

pub async fn decline_question_response(app: &mut App) {
    if let OverlayState::QuestionModal { session_id, .. } = &app.overlay {
        let sid = *session_id;
        app.restore_previous_overlay();

        if let Err(e) = app
            .client
            .answer_question(sid, DECLINE_RESPONSE.to_string())
            .await
        {
            app.notify(format!("Failed to decline: {}", e));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::QuestionSelection;
    use rsi_common::types::{QuestionItem, QuestionOption};

    fn mk_opt(label: &str) -> QuestionOption {
        QuestionOption {
            label: label.to_string(),
            description: "desc".to_string(),
        }
    }

    #[test]
    fn decline_response_matches_contract() {
        assert_eq!(
            DECLINE_RESPONSE,
            "(declined — no preference; use your best judgment)"
        );
    }

    #[test]
    fn test_format_answers_single_select_bare() {
        let questions = vec![QuestionItem {
            question: "Q1".to_string(),
            header: "Header1".to_string(),
            options: vec![mk_opt("Yes"), mk_opt("No")],
            multi_select: false,
        }];
        let selections = vec![QuestionSelection::Single(Some(0))];
        let res = format_answers(&questions, &selections, "").unwrap();
        assert_eq!(res, "Yes");
    }

    #[test]
    fn test_format_answers_multi_select_joined() {
        let questions = vec![QuestionItem {
            question: "Q1".to_string(),
            header: "Header1".to_string(),
            options: vec![mk_opt("A"), mk_opt("B"), mk_opt("C")],
            multi_select: true,
        }];
        let selections = vec![QuestionSelection::Multi(vec![2, 0])]; // C and A
        let res = format_answers(&questions, &selections, "").unwrap();
        // Option indices sorted to 0 and 2 => A and C
        assert_eq!(res, "A, C");
    }

    #[test]
    fn test_format_answers_multiple_questions_multiline() {
        let questions = vec![
            QuestionItem {
                question: "Q1".to_string(),
                header: "Header1".to_string(),
                options: vec![mk_opt("A"), mk_opt("B")],
                multi_select: false,
            },
            QuestionItem {
                question: "Q2".to_string(),
                header: "Header2".to_string(),
                options: vec![mk_opt("C"), mk_opt("D")],
                multi_select: true,
            },
        ];
        let selections = vec![
            QuestionSelection::Single(Some(1)),   // B
            QuestionSelection::Multi(vec![0, 1]), // C and D
        ];
        let res = format_answers(&questions, &selections, "").unwrap();
        assert_eq!(res, "Q1 (Header1): B\nQ2 (Header2): C, D");
    }

    #[test]
    fn test_format_answers_textarea_fallback() {
        let questions = vec![QuestionItem {
            question: "Q1".to_string(),
            header: "".to_string(),
            options: vec![mk_opt("A")],
            multi_select: false,
        }];
        let selections = vec![QuestionSelection::Single(None)];
        let res = format_answers(&questions, &selections, "  custom text  ").unwrap();
        assert_eq!(res, "custom text");
    }
}
