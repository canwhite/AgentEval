use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Html,
    Json,
};
use serde::Serialize;
use serde_json::Value;

use crate::eval::types::SessionView;
use crate::grader::types::GradeReport;
use crate::proxy::AppState;

pub async fn serve_ui() -> Html<&'static str> {
    Html(include_str!("ui.html"))
}

#[derive(Serialize)]
struct SessionSummary {
    session_id: String,
    model: String,
    turn_count: usize,
    jsonl_ids: Vec<u64>,
    overall: Option<f64>,
    graded: bool,
    dimensions: Vec<DimensionSummary>,
}

#[derive(Serialize)]
struct DimensionSummary {
    metric: String,
    score: f64,
    source: String,
    weight: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

pub async fn list_sessions(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let mut summaries: Vec<SessionSummary> = Vec::new();

    let dir = match std::fs::read_dir(&state.log_dir) {
        Ok(d) => d,
        Err(_) => return Ok(Json(serde_json::json!({ "sessions": [] }))),
    };

    let mut session_ids = std::collections::BTreeSet::new();

    for entry in dir.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if let Some(stem) = name.strip_suffix(".view.json") {
            session_ids.insert(stem.to_string());
        } else if let Some(stem) = name.strip_suffix(".grade.json") {
            session_ids.insert(stem.to_string());
        }
    }

    for sid in session_ids.iter().rev() {
        let grade_path = format!("{}/{}.grade.json", state.log_dir, sid);
        let view_path = format!("{}/{}.view.json", state.log_dir, sid);

        if let Ok(grade_json) = std::fs::read_to_string(&grade_path) {
            if let Ok(report) = serde_json::from_str::<GradeReport>(&grade_json) {
                let dims = report
                    .dimensions
                    .iter()
                    .map(|d| DimensionSummary {
                        metric: d.metric.clone(),
                        score: d.score,
                        source: d.source.clone(),
                        weight: d.weight,
                        reason: None,
                    })
                    .collect();
                summaries.push(SessionSummary {
                    session_id: sid.clone(),
                    model: report.model,
                    turn_count: report.turn_count,
                    jsonl_ids: report.jsonl_ids.clone(),
                    overall: Some(report.overall),
                    graded: true,
                    dimensions: dims,
                });
                continue;
            }
        }

        // No grade yet — try view
        if let Ok(view_json) = std::fs::read_to_string(&view_path) {
            if let Ok(view) = serde_json::from_str::<SessionView>(&view_json) {
                summaries.push(SessionSummary {
                    session_id: sid.clone(),
                    model: view.model,
                    turn_count: view.turns.len(),
                    jsonl_ids: view.jsonl_ids.clone(),
                    overall: None,
                    graded: false,
                    dimensions: Vec::new(),
                });
            }
        }
    }

    Ok(Json(serde_json::json!({ "sessions": summaries })))
}

pub async fn get_session(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    if !is_safe_session_id(&session_id) {
        return Err((StatusCode::BAD_REQUEST, "invalid session_id".into()));
    }

    let view_path = format!("{}/{}.view.json", state.log_dir, session_id);
    let grade_path = format!("{}/{}.grade.json", state.log_dir, session_id);

    let view_json =
        std::fs::read_to_string(&view_path).map_err(|_| (StatusCode::NOT_FOUND, "session not found".into()))?;
    let view: SessionView = serde_json::from_str(&view_json)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("failed to parse view: {}", e)))?;

    let grade: Option<GradeReport> = std::fs::read_to_string(&grade_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok());

    Ok(Json(serde_json::json!({
        "view": view,
        "grade": grade,
    })))
}

fn is_safe_session_id(s: &str) -> bool {
    !s.contains("..") && !s.contains('/') && !s.contains('\\')
}
