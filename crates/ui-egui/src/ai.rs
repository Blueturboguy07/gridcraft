//! AI tools through publik API or the person's own key, desktop only. The setup flow, the
//! credential file and the HTTP calls live in the `craft-publik` crate (shared by the Craft
//! apps). This module is GridCraft's thin host: the first-launch dialog, File › Options › AI,
//! and the Formulas tab's "AI Formula" and "Explain Selection".
//!
//! Nothing is sent, and nothing is charged, until the person clicks a button in one of the two
//! tool dialogs. Programmatic commands never reach these dialogs.

use std::sync::mpsc::{Receiver, TryRecvError};

pub use craft_publik::AppInfo;
use craft_publik::client::{self, Fail, Reply};
use craft_publik::ui::{Action, AiState, spawn};
use egui::{RichText, Ui};
use gridcraft_engine::core::CellRef;
use serde_json::{Value as Json, json};

use crate::SheetApp;

const FORMULA_SYSTEM: &str = "You write spreadsheet formulas for GridCraft, which uses Excel's function names and A1 references. Reply with exactly one formula that starts with = and nothing else: no explanation, no code fence, no cell address before it.";
const EXPLAIN_SYSTEM: &str = "You explain spreadsheet data and formulas in plain words for GridCraft, a spreadsheet that uses Excel's function names. Say what the selection holds, what any formulas compute, and anything that looks wrong. At most six short sentences. Plain text, no markdown.";

/// How much of the sheet a request carries, so a big sheet stays a small request.
const CONTEXT_ROWS: u32 = 30;
const CONTEXT_COLS: u32 = 12;
const CELL_CHARS: usize = 40;

pub struct Ai {
    pub state: AiState,
    frames: u32,
    first_run_checked: bool,
    /// Options (or a tool dialog) asked for the setup flow; `frame` opens it when nothing else shows.
    open_setup: bool,
    formula: Job,
    explain: Job,
}

/// One tool's request in flight and its last answer.
#[derive(Default)]
struct Job {
    prompt: String,
    /// The cell or range the request was made for.
    target: String,
    result: Option<String>,
    error: Option<String>,
    rx: Option<Receiver<Result<Reply, Fail>>>,
}

impl Job {
    fn busy(&self) -> bool {
        self.rx.is_some()
    }

    fn poll(&mut self, state: &mut AiState) {
        let Some(rx) = self.rx.take() else { return };
        match rx.try_recv() {
            Ok(Ok(r)) => {
                state.note_reply(&r);
                self.result = Some(r.text.trim().to_string());
                self.error = None;
            }
            Ok(Err(f)) => {
                state.note_fail(&f);
                // A 402 or a retired key is the notice bar's job (one message, one link); the rest is ours.
                if state.notice.is_none() {
                    self.error = Some(f.message);
                }
            }
            Err(TryRecvError::Empty) => self.rx = Some(rx),
            Err(TryRecvError::Disconnected) => self.error = Some("The request stopped unexpectedly. Try again.".into()),
        }
    }

    fn start(&mut self, ctx: &egui::Context, provider: craft_publik::Provider, system: &'static str, user: String) {
        self.result = None;
        self.error = None;
        self.rx = Some(spawn(ctx, move || client::chat(&provider, system, &user)));
    }
}

impl Ai {
    pub fn new(app: AppInfo) -> Self {
        Ai { state: AiState::new(app), frames: 0, first_run_checked: false, open_setup: false, formula: Job::default(), explain: Job::default() }
    }
}

/// `PUBLIK_SELF_TEST=1`: provision for real and print one line (publikhq.com/skills/setup.md).
pub fn self_test(app: &AppInfo) -> String {
    craft_publik::self_test(app)
}

/// Once per frame, before drawing: finish background work and open the setup dialog when due.
pub fn frame(app: &mut SheetApp, _ctx: &egui::Context) {
    let Some(ai) = app.ai.as_mut() else { return };
    ai.state.poll();
    ai.formula.poll(&mut ai.state);
    ai.explain.poll(&mut ai.state);
    ai.frames = ai.frames.saturating_add(1);
    // The first launch asks once the window is up (a few frames in). "Later" ends that for good.
    if !ai.first_run_checked && ai.frames > 3 {
        ai.first_run_checked = true;
        if ai.state.wants_first_run() || ai.state.owes_first_card() {
            ai.open_setup = true;
        }
    }
    if !ai.open_setup || app.dialog.is_some() {
        return;
    }
    ai.open_setup = false;
    ai.state.begin();
    app.backstage = None;
    app.open_dialog("aiSetup", json!({}));
}

fn unavailable(ui: &mut Ui) {
    ui.label("AI tools are off in this session (GRIDCRAFT_NO_PREFS).");
}

fn open_url(app: &SheetApp, url: &str) {
    if let Some(f) = &app.services.open_url {
        f(url);
    }
}

fn apply(app: &SheetApp, action: Action, open: &mut bool) {
    match action {
        Action::None => {}
        Action::OpenUrl(url) => open_url(app, &url),
        Action::Close => *open = false,
    }
}

/// The active cell and the selection, as A1 text.
fn selection_of(app: &SheetApp) -> (String, String) {
    app.session.active().map(|d| (d.selection.active.a1(), d.selection.current().a1())).unwrap_or_default()
}

/// The selection clipped to the context window, as A1 text.
fn clipped_selection(app: &SheetApp) -> Option<String> {
    let r = app.session.active()?.selection.current();
    let end = CellRef::new(r.end.row.min(r.start.row.saturating_add(CONTEXT_ROWS - 1)), r.end.col.min(r.start.col.saturating_add(CONTEXT_COLS - 1)));
    Some(format!("{}:{}", r.start.a1(), end.a1()))
}

fn cell_text(v: &Json) -> String {
    let s = match v {
        Json::Null => String::new(),
        Json::String(s) => s.clone(),
        Json::Object(o) => o.get("error").and_then(|e| e.as_str()).unwrap_or("#ERR").to_string(),
        other => other.to_string(),
    };
    let s = s.replace(['\t', '\n', '\r'], " ");
    if s.chars().count() > CELL_CHARS { format!("{}…", s.chars().take(CELL_CHARS).collect::<String>()) } else { s }
}

/// A range as tab-separated rows: formulas where a cell has one, display text elsewhere.
fn range_text(app: &mut SheetApp, range: &str) -> Result<String, String> {
    let out = app.run("sheet.read", json!({"range": range, "formulas": true, "formatted": true}))?;
    let rows = out.get("values").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let mut lines = Vec::new();
    for row in rows.iter().take(CONTEXT_ROWS as usize) {
        let cells: Vec<String> = row.as_array().map(|r| r.iter().take(CONTEXT_COLS as usize).map(cell_text).collect()).unwrap_or_default();
        lines.push(cells.join("\t"));
    }
    Ok(lines.join("\n"))
}

/// One formula out of a chat answer: no fences, no label, a leading `=`.
fn clean_formula(text: &str) -> String {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty() && !l.starts_with("```")).unwrap_or("").trim_matches('`').trim();
    let line = match line.find('=') {
        Some(i) => line.get(i..).unwrap_or(line),
        None => line,
    };
    if line.starts_with('=') { line.to_string() } else { format!("={line}") }
}

fn needs_setup(ui: &mut Ui) -> bool {
    ui.label("This tool needs AI. Set it up first: publik API (no key to paste) or your own key.");
    ui.add_space(6.0);
    ui.button("Set up AI…").clicked()
}

// ---- dialogs ------------------------------------------------------------------------------------

pub fn setup_dialog(app: &mut SheetApp, ui: &mut Ui, open: &mut bool) {
    let Some(ai) = app.ai.as_mut() else {
        unavailable(ui);
        return;
    };
    let action = ai.state.setup_ui(ui);
    apply(app, action, open);
}

pub fn formula_dialog(app: &mut SheetApp, ui: &mut Ui, open: &mut bool) {
    let (active, _sel) = selection_of(app);
    let ctx = ui.ctx().clone();
    let Some(ai) = app.ai.as_mut() else {
        unavailable(ui);
        return;
    };
    let provider = ai.state.provider();
    if !provider.is_set_up() {
        if needs_setup(ui) {
            ai.state.begin();
            *open = false;
            app.open_dialog("aiSetup", json!({}));
        }
        return;
    }
    let mut action = Action::None;
    let mut start = false;
    let mut insert: Option<(String, String)> = None;
    ui.label(format!("Describe the formula for {active}. Name the cells or ranges it should use."));
    ui.add_space(4.0);
    ui.add_enabled(
        !ai.formula.busy(),
        egui::TextEdit::multiline(&mut ai.formula.prompt)
            .desired_rows(3)
            .desired_width(f32::INFINITY)
            .hint_text("total of column B where column A says Paid"),
    );
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        if ai.formula.busy() {
            ui.spinner();
            ui.label("Writing…");
        } else if ui.add_enabled(!ai.formula.prompt.trim().is_empty(), egui::Button::new("Write formula")).clicked() {
            start = true;
        }
    });
    if let Some(e) = &ai.formula.error {
        ui.add_space(4.0);
        ui.colored_label(ui.visuals().error_fg_color, e);
    }
    if let Some(r) = ai.formula.result.clone() {
        let formula = clean_formula(&r);
        ui.add_space(6.0);
        ui.label(RichText::new(&formula).monospace());
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if ui.button(format!("Insert into {}", ai.formula.target)).clicked() {
                insert = Some((ai.formula.target.clone(), formula.clone()));
            }
            if ui.button("Copy").clicked() {
                ctx.copy_text(formula.clone());
            }
        });
    }
    ui.add_space(4.0);
    let bar = ai.state.result_bar(ui);
    if bar != Action::None {
        action = bar;
    }
    ui.add_space(4.0);
    if ui.button("Close").clicked() {
        action = Action::Close;
    }
    if start {
        // The top-left window of the sheet gives the model the headers; the active cell is the target.
        let context = range_text(app, &format!("A1:{}", CellRef::new(CONTEXT_ROWS - 1, CONTEXT_COLS - 1).a1())).unwrap_or_default();
        if let Some(ai) = app.ai.as_mut() {
            ai.formula.target = active.clone();
            let user = format!(
                "Target cell: {active}\nRequest: {}\n\nTop of the sheet, tab-separated rows starting at A1 (formulas shown where present):\n{context}",
                ai.formula.prompt.trim()
            );
            ai.formula.start(&ctx, provider, FORMULA_SYSTEM, user);
        }
    }
    if let Some((cell, formula)) = insert {
        match app.run("cell.set", json!({"cell": cell, "input": formula})) {
            Ok(_) => action = Action::Close,
            Err(e) => {
                if let Some(ai) = app.ai.as_mut() {
                    ai.formula.error = Some(crate::clean_error(&e));
                }
            }
        }
    }
    apply(app, action, open);
}

pub fn explain_dialog(app: &mut SheetApp, ui: &mut Ui, open: &mut bool) {
    let (_active, sel) = selection_of(app);
    let clipped = clipped_selection(app);
    let ctx = ui.ctx().clone();
    let Some(ai) = app.ai.as_mut() else {
        unavailable(ui);
        return;
    };
    let provider = ai.state.provider();
    if !provider.is_set_up() {
        if needs_setup(ui) {
            ai.state.begin();
            *open = false;
            app.open_dialog("aiSetup", json!({}));
        }
        return;
    }
    let mut action = Action::None;
    let mut start = false;
    let shown = clipped.clone().unwrap_or_else(|| sel.clone());
    if shown != sel {
        ui.label(format!("Selection {sel}; the first {CONTEXT_ROWS} rows and {CONTEXT_COLS} columns ({shown}) go to the model."));
    } else {
        ui.label(format!("Explain {sel} in plain words."));
    }
    ui.add_space(4.0);
    ui.add_enabled(
        !ai.explain.busy(),
        egui::TextEdit::singleline(&mut ai.explain.prompt).desired_width(f32::INFINITY).hint_text("Optional: a question about it"),
    );
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        if ai.explain.busy() {
            ui.spinner();
            ui.label("Reading…");
        } else if ui.button("Explain").clicked() {
            start = true;
        }
    });
    if let Some(e) = &ai.explain.error {
        ui.add_space(4.0);
        ui.colored_label(ui.visuals().error_fg_color, e);
    }
    if let Some(r) = ai.explain.result.clone() {
        ui.add_space(6.0);
        egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
            ui.label(&r);
        });
        ui.add_space(4.0);
        if ui.button("Copy").clicked() {
            ctx.copy_text(r);
        }
    }
    ui.add_space(4.0);
    let bar = ai.state.result_bar(ui);
    if bar != Action::None {
        action = bar;
    }
    ui.add_space(4.0);
    if ui.button("Close").clicked() {
        action = Action::Close;
    }
    if start {
        let text = range_text(app, &shown);
        if let Some(ai) = app.ai.as_mut() {
            match text {
                Ok(context) => {
                    ai.explain.target = shown.clone();
                    let question = ai.explain.prompt.trim();
                    let question = if question.is_empty() { String::new() } else { format!("Question: {question}\n") };
                    let user = format!("Selection {shown}, tab-separated rows (formulas shown where present):\n{context}\n{question}");
                    ai.explain.start(&ctx, provider, EXPLAIN_SYSTEM, user);
                }
                Err(e) => ai.explain.error = Some(crate::clean_error(&e)),
            }
        }
    }
    apply(app, action, open);
}

// ---- File › Options › AI -------------------------------------------------------------------------

pub fn options_section(app: &mut SheetApp, ui: &mut Ui) {
    let Some(ai) = app.ai.as_mut() else {
        ui.label(RichText::new("AI tools are off in this session (GRIDCRAFT_NO_PREFS).").weak());
        return;
    };
    ui.label(RichText::new("AI Formula and Explain Selection (Formulas tab) run through publik API or your own key.").weak());
    ui.add_space(4.0);
    let mut open_setup = false;
    let action = ai.state.options_ui(ui, &mut open_setup);
    if open_setup {
        ai.open_setup = true;
    }
    let mut unused = true;
    apply(app, action, &mut unused);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formulas_come_out_clean() {
        assert_eq!(clean_formula("```\n=SUM(A1:A9)\n```"), "=SUM(A1:A9)");
        assert_eq!(clean_formula("B3: =SUMIF(A:A,\"Paid\",B:B)"), "=SUMIF(A:A,\"Paid\",B:B)");
        assert_eq!(clean_formula("`SUM(A1:A9)`"), "=SUM(A1:A9)");
        assert_eq!(clean_formula(""), "=");
    }

    #[test]
    fn cells_are_flattened_and_capped() {
        assert_eq!(cell_text(&json!(null)), "");
        assert_eq!(cell_text(&json!(12.5)), "12.5");
        assert_eq!(cell_text(&json!({"error": "#DIV/0!"})), "#DIV/0!");
        assert_eq!(cell_text(&json!("a\tb\nc")), "a b c");
        assert_eq!(cell_text(&json!("x".repeat(50))).chars().count(), CELL_CHARS + 1);
    }
}
